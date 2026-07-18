use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const ENV_SOCKET: &str = "HANDTERM_NATIVE_SCROLL_SOCKET";
const ENV_WINDOW_ID: &str = "HANDTERM_WINDOW_ID";
const ENV_TERM_PROGRAM: &str = "TERM_PROGRAM";
const TERM_PROGRAM_VALUE: &str = "handterm";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneKind {
    Chat,
    SidePanel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneState {
    pub kind: PaneKind,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub position: usize,
    pub content_length: usize,
    pub viewport_length: usize,
}

impl PaneState {
    pub fn contains(&self, col: usize, row: usize) -> bool {
        let x = usize::from(self.x);
        let y = usize::from(self.y);
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        col >= x && col < x.saturating_add(width) && row >= y && row < y.saturating_add(height)
    }

    pub fn scrollable(&self) -> bool {
        self.content_length > self.viewport_length && self.viewport_length > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PaneSnapshot {
    pub panes: Vec<PaneState>,
}

impl PaneSnapshot {
    pub fn hovered_pane(&self, col: usize, row: usize) -> Option<PaneKind> {
        self.panes
            .iter()
            .find(|pane| pane.scrollable() && pane.contains(col, row))
            .map(|pane| pane.kind)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppToHost {
    PaneSnapshot { panes: Vec<PaneState> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToApp {
    Scroll { pane: PaneKind, delta: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneVisualScroll {
    pub kind: PaneKind,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    /// Pending visual motion relative to the latest Jcode pane snapshot.
    /// Positive values mean scrolling toward later content, so rendered pane
    /// pixels move upward.
    pub offset_rows: f32,
}

#[derive(Debug, Clone, Copy, Default)]
struct PaneMotion {
    offset_rows: f32,
    outstanding_rows: i32,
    last_position: Option<usize>,
}

#[derive(Debug)]
pub struct NativeScrollBridge {
    socket_path: PathBuf,
    snapshot: Arc<Mutex<PaneSnapshot>>,
    command_tx: Sender<HostToApp>,
    connected: Arc<AtomicBool>,
    connection_generation: Arc<AtomicU64>,
    observed_connection_generation: u64,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    chat_motion: PaneMotion,
    side_panel_motion: PaneMotion,
}

impl NativeScrollBridge {
    pub fn new(window_id: u64) -> Result<Self> {
        let socket_path = socket_path_for_window(window_id);
        if socket_path.exists() {
            let _ = std::fs::remove_file(&socket_path);
        }

        let listener = UnixListener::bind(&socket_path).with_context(|| {
            format!(
                "failed binding native scroll socket {}",
                socket_path.display()
            )
        })?;
        listener
            .set_nonblocking(true)
            .context("failed setting native scroll listener nonblocking")?;

        let snapshot = Arc::new(Mutex::new(PaneSnapshot::default()));
        let connected = Arc::new(AtomicBool::new(false));
        let connection_generation = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (command_tx, command_rx) = mpsc::channel();

        let thread = spawn_bridge_thread(
            listener,
            snapshot.clone(),
            connected.clone(),
            connection_generation.clone(),
            stop.clone(),
            command_rx,
        );

        Ok(Self {
            socket_path,
            snapshot,
            command_tx,
            connected,
            connection_generation,
            observed_connection_generation: 0,
            stop,
            thread: Some(thread),
            chat_motion: PaneMotion::default(),
            side_panel_motion: PaneMotion::default(),
        })
    }

    pub fn child_envs(&self, window_id: u64) -> [(&'static str, String); 3] {
        [
            (ENV_SOCKET, self.socket_path.display().to_string()),
            (ENV_WINDOW_ID, window_id.to_string()),
            (ENV_TERM_PROGRAM, TERM_PROGRAM_VALUE.to_string()),
        ]
    }

    pub fn hovered_pane(&self, col: usize, row: usize) -> Option<PaneKind> {
        self.snapshot.lock().ok()?.hovered_pane(col, row)
    }

    fn motion_mut(&mut self, pane: PaneKind) -> &mut PaneMotion {
        match pane {
            PaneKind::Chat => &mut self.chat_motion,
            PaneKind::SidePanel => &mut self.side_panel_motion,
        }
    }

    fn pane_state(&self, pane: PaneKind) -> Option<PaneState> {
        self.snapshot
            .lock()
            .ok()?
            .panes
            .iter()
            .find(|state| state.kind == pane)
            .cloned()
    }

    fn reset_motion(&mut self) {
        self.chat_motion = PaneMotion::default();
        self.side_panel_motion = PaneMotion::default();
    }

    fn sync_connection_generation(&mut self) {
        let generation = self.connection_generation.load(Ordering::Relaxed);
        if generation != self.observed_connection_generation {
            self.observed_connection_generation = generation;
            self.reset_motion();
        }
    }

    fn reconcile_motion(&mut self, pane: PaneKind, state: &PaneState) {
        let motion = self.motion_mut(pane);
        if let Some(previous) = motion.last_position {
            let acknowledged = if state.position >= previous {
                i32::try_from(state.position - previous).unwrap_or(i32::MAX)
            } else {
                -i32::try_from(previous - state.position).unwrap_or(i32::MAX)
            };
            if acknowledged != 0 {
                motion.offset_rows -= acknowledged as f32;
                motion.outstanding_rows = motion.outstanding_rows.saturating_sub(acknowledged);
            }
        }
        motion.last_position = Some(state.position);

        let max_position = state.content_length.saturating_sub(state.viewport_length);
        let min_offset = -(state.position as f32);
        let max_offset = max_position.saturating_sub(state.position) as f32;
        motion.offset_rows = motion.offset_rows.clamp(min_offset, max_offset);
        if motion.offset_rows.abs() < 0.0001 {
            motion.offset_rows = 0.0;
        }
    }

    pub fn visual_scroll(&mut self, pane: PaneKind) -> Option<PaneVisualScroll> {
        self.sync_connection_generation();
        let state = self.pane_state(pane)?;
        self.reconcile_motion(pane, &state);
        let offset_rows = self.motion_mut(pane).offset_rows;
        (offset_rows != 0.0).then_some(PaneVisualScroll {
            kind: pane,
            x: state.x,
            y: state.y,
            width: state.width,
            height: state.height,
            offset_rows,
        })
    }

    pub fn send_scroll_delta(&mut self, pane: PaneKind, delta_rows: f32) -> bool {
        if !self.connected.load(Ordering::Acquire) {
            return false;
        }

        if !delta_rows.is_finite() {
            return true;
        }
        self.sync_connection_generation();
        let Some(state) = self.pane_state(pane) else {
            return false;
        };
        self.reconcile_motion(pane, &state);

        let max_position = state.content_length.saturating_sub(state.viewport_length);
        let min_offset = -(state.position as f32);
        let max_offset = max_position.saturating_sub(state.position) as f32;
        let steps = {
            let motion = self.motion_mut(pane);
            motion.offset_rows = (motion.offset_rows + delta_rows).clamp(min_offset, max_offset);
            let desired_rows = motion
                .offset_rows
                .trunc()
                .clamp(i32::MIN as f32, i32::MAX as f32) as i32;
            desired_rows.saturating_sub(motion.outstanding_rows)
        };
        if steps == 0 {
            return true;
        }
        if self
            .command_tx
            .send(HostToApp::Scroll { pane, delta: steps })
            .is_ok()
        {
            let motion = self.motion_mut(pane);
            motion.outstanding_rows = motion.outstanding_rows.saturating_add(steps);
            true
        } else {
            false
        }
    }
}

/// Environment variables passed to a handterm child process.
///
/// Terminal identity is independent of the optional native-scroll bridge, so
/// children still receive `TERM_PROGRAM=handterm` if socket setup fails.
pub(crate) fn child_process_envs(
    bridge: Option<&NativeScrollBridge>,
    window_id: u64,
) -> Vec<(&'static str, String)> {
    let mut envs = vec![(ENV_TERM_PROGRAM, TERM_PROGRAM_VALUE.to_string())];
    if let Some(bridge) = bridge {
        envs.extend([
            (ENV_SOCKET, bridge.socket_path.display().to_string()),
            (ENV_WINDOW_ID, window_id.to_string()),
        ]);
    }
    envs
}

/// Fold `delta_rows` into the running fractional `residual` and return the
/// whole number of scroll steps to emit, leaving the sub-row remainder in
/// `residual` (always in the open interval `(-1.0, 1.0)`).
///
/// This is the integer part of the accumulated value, computed in O(1) with a
/// single `trunc` instead of subtracting 1.0 in a loop. The loop form was both
/// slower for large deltas and could spin forever on a non-finite delta; this
/// form treats any non-finite accumulation as "no movement" and resets the
/// residual so a stray NaN/inf cannot poison later events.
#[cfg(test)]
fn accumulate_scroll_steps(residual: &mut f32, delta_rows: f32) -> i32 {
    let total = *residual + delta_rows;
    if !total.is_finite() {
        *residual = 0.0;
        return 0;
    }

    let whole = total.trunc();
    *residual = total - whole;
    // Clamp into i32 range; real scroll deltas are tiny, but this keeps an
    // adversarial value from wrapping on the cast.
    whole.clamp(i32::MIN as f32, i32::MAX as f32) as i32
}

impl Drop for NativeScrollBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn socket_path_for_window(window_id: u64) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!(
        "handterm-scroll-{}-{}.sock",
        std::process::id(),
        window_id
    ))
}

fn spawn_bridge_thread(
    listener: UnixListener,
    snapshot: Arc<Mutex<PaneSnapshot>>,
    connected: Arc<AtomicBool>,
    connection_generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    command_rx: Receiver<HostToApp>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("handterm-native-scroll".to_string())
        .spawn(move || {
            bridge_thread(
                listener,
                snapshot,
                connected,
                connection_generation,
                stop,
                command_rx,
            )
        })
        .expect("native scroll bridge thread should spawn")
}

fn bridge_thread(
    listener: UnixListener,
    snapshot: Arc<Mutex<PaneSnapshot>>,
    connected: Arc<AtomicBool>,
    connection_generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    command_rx: Receiver<HostToApp>,
) {
    let mut stream = None::<UnixStream>;
    let mut read_buf = Vec::<u8>::new();

    while !stop.load(Ordering::Relaxed) {
        if stream.is_none() {
            match listener.accept() {
                Ok((accepted, _)) => {
                    let _ = accepted.set_nonblocking(true);
                    read_buf.clear();
                    while command_rx.try_recv().is_ok() {}
                    if let Ok(mut current) = snapshot.lock() {
                        current.panes.clear();
                    }
                    connection_generation.fetch_add(1, Ordering::Relaxed);
                    connected.store(true, Ordering::Release);
                    stream = Some(accepted);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }

        if let Some(active) = stream.as_mut() {
            while let Ok(message) = command_rx.try_recv() {
                if write_line(active, &message).is_err() {
                    connected.store(false, Ordering::Relaxed);
                    stream = None;
                    read_buf.clear();
                    if let Ok(mut current) = snapshot.lock() {
                        current.panes.clear();
                    }
                    break;
                }
            }
        }

        if let Some(active) = stream.as_mut() {
            match read_messages(active, &mut read_buf) {
                Ok(messages) => {
                    for message in messages {
                        let AppToHost::PaneSnapshot { panes } = message;
                        if let Ok(mut current) = snapshot.lock() {
                            current.panes = panes;
                        }
                    }
                }
                Err(_) => {
                    connected.store(false, Ordering::Relaxed);
                    stream = None;
                    read_buf.clear();
                    if let Ok(mut current) = snapshot.lock() {
                        current.panes.clear();
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(8));
    }
}

fn read_messages(stream: &mut UnixStream, buffer: &mut Vec<u8>) -> Result<Vec<AppToHost>> {
    let mut chunk = [0u8; 4096];
    let mut messages = Vec::new();

    loop {
        match stream.read(&mut chunk) {
            Ok(0) => anyhow::bail!("native scroll peer closed"),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => return Err(err).context("failed reading native scroll stream"),
        }
    }

    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        let line = buffer.drain(..=pos).collect::<Vec<_>>();
        let line = &line[..line.len().saturating_sub(1)];
        if line.is_empty() {
            continue;
        }
        let message = serde_json::from_slice::<AppToHost>(line)
            .context("failed decoding native scroll message")?;
        messages.push(message);
    }

    Ok(messages)
}

fn write_line<T: Serialize>(stream: &mut UnixStream, message: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec(message).context("failed encoding native scroll message")?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .context("failed writing native scroll command")
}

// Child-side environment helpers.
//
// Handterm has no in-repo consumer for these: they are public library API for
// out-of-repo child applications (spawned inside a handterm window, see the
// `child_envs` wiring in app.rs/gpu_app.rs) to discover the native scroll
// bridge socket and their host window id from the environment handterm sets.

/// Path of the native scroll bridge socket, read from the environment
/// handterm sets for its child processes. For use by child applications.
pub fn child_socket_path() -> Option<PathBuf> {
    std::env::var_os(ENV_SOCKET).map(PathBuf::from)
}

/// Host window id of the handterm window this child runs in, read from the
/// environment handterm sets for its child processes.
pub fn child_window_id() -> Option<u64> {
    std::env::var(ENV_WINDOW_ID).ok()?.parse().ok()
}

/// Name of the env var carrying the native scroll bridge socket path.
pub fn socket_env_key() -> &'static str {
    ENV_SOCKET
}

/// Name of the env var children can check to detect they run under handterm.
pub fn term_program_env_key() -> &'static str {
    ENV_TERM_PROGRAM
}

/// Value handterm sets for `TERM_PROGRAM` in child environments.
pub fn term_program_env_value() -> &'static str {
    TERM_PROGRAM_VALUE
}

/// Name of the env var carrying the host window id.
pub fn window_env_key() -> &'static str {
    ENV_WINDOW_ID
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn install_scrollable_chat_snapshot(bridge: &NativeScrollBridge, position: usize) {
        *bridge.snapshot.lock().expect("snapshot lock") = PaneSnapshot {
            panes: vec![PaneState {
                kind: PaneKind::Chat,
                x: 2,
                y: 3,
                width: 10,
                height: 4,
                position,
                content_length: 20,
                viewport_length: 4,
            }],
        };
    }

    #[test]
    fn accumulate_scroll_steps_matches_reference_loop() {
        // Reference implementation: the previous loop-based behavior.
        fn reference(residual: &mut f32, delta: f32) -> i32 {
            *residual += delta;
            let mut steps = 0i32;
            while *residual >= 1.0 {
                steps += 1;
                *residual -= 1.0;
            }
            while *residual <= -1.0 {
                steps -= 1;
                *residual += 1.0;
            }
            steps
        }

        let deltas = [
            0.4, 0.7, -0.3, 1.0, -1.0, 2.5, -2.5, 0.0, 0.999, -0.999, 3.2, -4.8, 0.1, 0.1, 0.1,
        ];
        let mut residual_new = 0.0f32;
        let mut residual_ref = 0.0f32;
        for delta in deltas {
            let steps_new = accumulate_scroll_steps(&mut residual_new, delta);
            let steps_ref = reference(&mut residual_ref, delta);
            assert_eq!(
                steps_new, steps_ref,
                "step mismatch for delta {delta}: new={steps_new} ref={steps_ref}"
            );
            assert!(
                (residual_new - residual_ref).abs() < 1e-5,
                "residual drift for delta {delta}: new={residual_new} ref={residual_ref}"
            );
            // The residual must always stay a sub-row remainder.
            assert!(residual_new.abs() < 1.0);
        }
    }

    #[test]
    fn accumulate_scroll_steps_handles_large_delta_in_constant_time() {
        // The old loop would spin a million times for this; the closed form
        // returns immediately. Also confirms the big jump is reported exactly.
        let mut residual = 0.0f32;
        let steps = accumulate_scroll_steps(&mut residual, 1_000_000.0);
        assert_eq!(steps, 1_000_000);
        assert!(residual.abs() < 1.0);
    }

    #[test]
    fn accumulate_scroll_steps_rejects_non_finite_delta() {
        let mut residual = 0.5f32;
        assert_eq!(accumulate_scroll_steps(&mut residual, f32::NAN), 0);
        assert_eq!(residual, 0.0, "non-finite delta must reset residual");

        let mut residual = 0.5f32;
        assert_eq!(accumulate_scroll_steps(&mut residual, f32::INFINITY), 0);
        assert_eq!(residual, 0.0);
    }

    #[test]
    fn pane_hit_testing_uses_cell_rect() {
        let snapshot = PaneSnapshot {
            panes: vec![PaneState {
                kind: PaneKind::Chat,
                x: 2,
                y: 3,
                width: 10,
                height: 4,
                position: 0,
                content_length: 20,
                viewport_length: 4,
            }],
        };
        assert_eq!(snapshot.hovered_pane(2, 3), Some(PaneKind::Chat));
        assert_eq!(snapshot.hovered_pane(11, 6), Some(PaneKind::Chat));
        assert_eq!(snapshot.hovered_pane(12, 6), None);
    }

    #[test]
    fn scroll_delta_accumulates_fractional_rows() {
        let mut bridge = NativeScrollBridge::new(999_991).expect("bridge should initialize");
        let socket_path = bridge
            .child_envs(999_991)
            .into_iter()
            .find_map(|(key, value)| (key == ENV_SOCKET).then_some(value))
            .expect("socket env should be present");

        let deadline = Instant::now() + Duration::from_secs(2);
        let _stream = loop {
            match UnixStream::connect(&socket_path) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Err(err) => panic!("failed to connect to native scroll socket: {err}"),
            }
        };

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline && !bridge.connected.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(bridge.connected.load(Ordering::Relaxed));
        install_scrollable_chat_snapshot(&bridge, 0);

        let _ = bridge.send_scroll_delta(PaneKind::Chat, 0.4);
        assert!((bridge.chat_motion.offset_rows - 0.4).abs() < f32::EPSILON);
        assert_eq!(bridge.chat_motion.outstanding_rows, 0);
        let _ = bridge.send_scroll_delta(PaneKind::Chat, 0.7);
        assert!((bridge.chat_motion.offset_rows - 1.1).abs() < 1e-5);
        assert_eq!(bridge.chat_motion.outstanding_rows, 1);
        let visual = bridge.visual_scroll(PaneKind::Chat).expect("visual motion");
        assert!((visual.offset_rows - 1.1).abs() < 1e-5);

        install_scrollable_chat_snapshot(&bridge, 1);
        let visual = bridge
            .visual_scroll(PaneKind::Chat)
            .expect("fractional remainder remains after acknowledgement");
        assert!((visual.offset_rows - 0.1).abs() < 1e-5);
        assert_eq!(bridge.chat_motion.outstanding_rows, 0);
    }

    #[test]
    fn bridge_does_not_claim_scroll_delivery_without_child_connection() {
        let mut bridge = NativeScrollBridge::new(999_993).expect("bridge should initialize");
        assert!(!bridge.connected.load(Ordering::Relaxed));
        assert!(!bridge.send_scroll_delta(PaneKind::Chat, 1.0));
        assert_eq!(bridge.chat_motion.offset_rows, 0.0);
    }

    #[test]
    fn new_connection_discards_stale_commands_and_fractional_residuals() {
        let mut bridge = NativeScrollBridge::new(999_995).expect("bridge should initialize");
        let socket_path = bridge
            .child_envs(999_995)
            .into_iter()
            .find_map(|(key, value)| (key == ENV_SOCKET).then_some(value))
            .expect("socket env should be present");

        bridge
            .command_tx
            .send(HostToApp::Scroll {
                pane: PaneKind::Chat,
                delta: 7,
            })
            .expect("stale command should queue before connection");
        bridge.chat_motion.offset_rows = 0.75;

        let mut stream = UnixStream::connect(socket_path).expect("client should connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("read timeout should set");
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline && !bridge.connected.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(bridge.connected.load(Ordering::Acquire));
        install_scrollable_chat_snapshot(&bridge, 0);

        assert!(bridge.send_scroll_delta(PaneKind::Chat, 0.25));
        assert!((bridge.chat_motion.offset_rows - 0.25).abs() < f32::EPSILON);

        let mut byte = [0u8; 1];
        let error = stream
            .read(&mut byte)
            .expect_err("stale command must not replay to the new client");
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
    }

    #[test]
    fn child_process_envs_preserve_term_program_when_bridge_setup_fails() {
        let window_id = 999_994;
        let socket_path = socket_path_for_window(window_id);
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_dir_all(&socket_path);
        std::fs::create_dir(&socket_path).expect("blocking socket directory should be created");

        let bridge = NativeScrollBridge::new(window_id);
        assert!(
            bridge.is_err(),
            "socket bind should fail when its path is a directory"
        );

        let envs = child_process_envs(bridge.as_ref().ok(), window_id);
        assert!(
            envs.iter()
                .any(|(key, value)| { *key == ENV_TERM_PROGRAM && value == TERM_PROGRAM_VALUE })
        );
        assert!(!envs.iter().any(|(key, _)| *key == ENV_SOCKET));
        assert!(!envs.iter().any(|(key, _)| *key == ENV_WINDOW_ID));

        std::fs::remove_dir(&socket_path).expect("blocking socket directory should be removed");
    }

    #[test]
    fn bridge_roundtrips_snapshot_and_scroll_command_over_socket() {
        let mut bridge = NativeScrollBridge::new(999_992).expect("bridge should initialize");
        let socket_path = bridge
            .child_envs(999_992)
            .into_iter()
            .find_map(|(key, value)| (key == ENV_SOCKET).then_some(value))
            .expect("socket env should be present");

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            match UnixStream::connect(&socket_path) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Err(err) => panic!("failed to connect to native scroll socket: {err}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("read timeout should set");

        let panes = vec![PaneState {
            kind: PaneKind::Chat,
            x: 1,
            y: 2,
            width: 8,
            height: 5,
            position: 3,
            content_length: 20,
            viewport_length: 5,
        }];
        write_line(
            &mut stream,
            &AppToHost::PaneSnapshot {
                panes: panes.clone(),
            },
        )
        .expect("snapshot write should succeed");

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline && bridge.hovered_pane(2, 3) != Some(PaneKind::Chat) {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(bridge.hovered_pane(2, 3), Some(PaneKind::Chat));

        assert!(bridge.send_scroll_delta(PaneKind::Chat, 2.0));
        let command = read_host_command(&mut stream).expect("should read host scroll command");
        assert_eq!(
            command,
            HostToApp::Scroll {
                pane: PaneKind::Chat,
                delta: 2,
            }
        );
    }

    fn read_host_command(stream: &mut UnixStream) -> Option<HostToApp> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let line = &buffer[..pos];
                return serde_json::from_slice(line).ok();
            }
        }
    }
}
