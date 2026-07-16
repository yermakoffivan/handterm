use crate::grid::{COLOR_FLAG_RGB, Grid};

/// Unicode placeholder used by the Kitty graphics protocol for virtual placements.
pub const KITTY_UNICODE_PLACEHOLDER: u32 = 0x10_EEEE;

/// One visible terminal cell backed by a cell-sized region of a Kitty image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyVirtualCell {
    pub image_id: u32,
    pub image_col: usize,
    pub image_row: usize,
    pub col: usize,
    pub row: usize,
}

#[derive(Debug, Clone, Copy)]
struct PlaceholderRun {
    fg: u32,
    underline_color: u32,
    id_high: u8,
    image_row: usize,
    image_col: usize,
    next_col: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct PlaceholderMetadata {
    image_row: Option<usize>,
    image_col: Option<usize>,
    id_high: Option<u8>,
}

/// Returns true when a grid cell contains a Kitty Unicode image placeholder.
#[inline]
pub fn is_kitty_unicode_placeholder(ch: u32, grapheme: Option<&str>) -> bool {
    ch == KITTY_UNICODE_PLACEHOLDER
        || grapheme
            .and_then(|value| value.chars().next())
            .is_some_and(|value| value as u32 == KITTY_UNICODE_PLACEHOLDER)
}

/// Reconstruct visible Kitty virtual-placement cells from the terminal grid.
///
/// The first placeholder in each run carries row, column, and high image-ID
/// metadata as combining marks. Following bare placeholders inherit the row and
/// ID and advance the image column by one, as required by the Kitty protocol.
pub fn fill_kitty_virtual_cells(
    grid: &Grid,
    scrollback_offset: usize,
    visible_rows: usize,
    cells: &mut Vec<KittyVirtualCell>,
) {
    cells.clear();
    cells.reserve(visible_rows.saturating_mul(grid.cols));

    for row in 0..visible_rows {
        let mut run: Option<PlaceholderRun> = None;
        for col in 0..grid.cols {
            let cell = grid.cell_at_scrollback_offset(scrollback_offset, row, col);
            let grapheme = grid.cell_grapheme_at_scrollback_offset(scrollback_offset, row, col);
            if !is_kitty_unicode_placeholder(cell.ch, grapheme) {
                run = None;
                continue;
            }
            let Some(low_id) = kitty_color_id(cell.fg) else {
                run = None;
                continue;
            };
            let Some(_placement_id) = kitty_color_id(cell.underline_color) else {
                run = None;
                continue;
            };

            let Some(metadata) = placeholder_metadata(cell.ch, grapheme) else {
                run = None;
                continue;
            };
            let previous = run.filter(|previous| {
                previous.fg == cell.fg && previous.underline_color == cell.underline_color
            });
            let (image_row, image_col, id_high) =
                match (metadata.image_row, metadata.image_col, metadata.id_high) {
                    (None, None, None) => {
                        let Some(previous) = previous else {
                            continue;
                        };
                        (previous.image_row, previous.next_col, previous.id_high)
                    }
                    (Some(image_row), None, None) => {
                        let Some(previous) =
                            previous.filter(|previous| previous.image_row == image_row)
                        else {
                            continue;
                        };
                        (image_row, previous.next_col, previous.id_high)
                    }
                    (Some(image_row), Some(image_col), None) => {
                        let id_high = previous
                            .filter(|previous| {
                                previous.image_row == image_row
                                    && previous.image_col.saturating_add(1) == image_col
                            })
                            .map_or(0, |previous| previous.id_high);
                        (image_row, image_col, id_high)
                    }
                    (Some(image_row), Some(image_col), Some(id_high)) => {
                        (image_row, image_col, id_high)
                    }
                    _ => {
                        run = None;
                        continue;
                    }
                };

            let image_id = ((id_high as u32) << 24) | low_id;
            if image_id == 0 {
                run = None;
                continue;
            }
            cells.push(KittyVirtualCell {
                image_id,
                image_col,
                image_row,
                col,
                row,
            });
            run = Some(PlaceholderRun {
                fg: cell.fg,
                underline_color: cell.underline_color,
                id_high,
                image_row,
                image_col,
                next_col: image_col.saturating_add(1),
            });
        }
    }
}

fn placeholder_metadata(ch: u32, grapheme: Option<&str>) -> Option<PlaceholderMetadata> {
    let Some(grapheme) = grapheme else {
        return (ch == KITTY_UNICODE_PLACEHOLDER).then(PlaceholderMetadata::default);
    };
    let mut chars = grapheme.chars();
    if chars.next()? as u32 != KITTY_UNICODE_PLACEHOLDER {
        return None;
    }

    let mut metadata = PlaceholderMetadata::default();
    if let Some(value) = chars.next() {
        metadata.image_row = Some(kitty_diacritic_index(value)?);
    }
    if let Some(value) = chars.next() {
        metadata.image_col = Some(kitty_diacritic_index(value)?);
    }
    if let Some(value) = chars.next() {
        metadata.id_high = Some(u8::try_from(kitty_diacritic_index(value)?).ok()?);
    }
    if chars.next().is_some() {
        return None;
    }
    Some(metadata)
}

fn kitty_color_id(color: u32) -> Option<u32> {
    if color & COLOR_FLAG_RGB != 0 {
        Some(color & 0x00ff_ffff)
    } else if color <= u8::MAX as u32 {
        Some(color)
    } else {
        None
    }
}

fn kitty_diacritic_index(value: char) -> Option<usize> {
    KITTY_DIACRITICS
        .iter()
        .position(|candidate| *candidate == value)
}

static KITTY_DIACRITICS: [char; 297] = [
    '\u{305}',
    '\u{30D}',
    '\u{30E}',
    '\u{310}',
    '\u{312}',
    '\u{33D}',
    '\u{33E}',
    '\u{33F}',
    '\u{346}',
    '\u{34A}',
    '\u{34B}',
    '\u{34C}',
    '\u{350}',
    '\u{351}',
    '\u{352}',
    '\u{357}',
    '\u{35B}',
    '\u{363}',
    '\u{364}',
    '\u{365}',
    '\u{366}',
    '\u{367}',
    '\u{368}',
    '\u{369}',
    '\u{36A}',
    '\u{36B}',
    '\u{36C}',
    '\u{36D}',
    '\u{36E}',
    '\u{36F}',
    '\u{483}',
    '\u{484}',
    '\u{485}',
    '\u{486}',
    '\u{487}',
    '\u{592}',
    '\u{593}',
    '\u{594}',
    '\u{595}',
    '\u{597}',
    '\u{598}',
    '\u{599}',
    '\u{59C}',
    '\u{59D}',
    '\u{59E}',
    '\u{59F}',
    '\u{5A0}',
    '\u{5A1}',
    '\u{5A8}',
    '\u{5A9}',
    '\u{5AB}',
    '\u{5AC}',
    '\u{5AF}',
    '\u{5C4}',
    '\u{610}',
    '\u{611}',
    '\u{612}',
    '\u{613}',
    '\u{614}',
    '\u{615}',
    '\u{616}',
    '\u{617}',
    '\u{657}',
    '\u{658}',
    '\u{659}',
    '\u{65A}',
    '\u{65B}',
    '\u{65D}',
    '\u{65E}',
    '\u{6D6}',
    '\u{6D7}',
    '\u{6D8}',
    '\u{6D9}',
    '\u{6DA}',
    '\u{6DB}',
    '\u{6DC}',
    '\u{6DF}',
    '\u{6E0}',
    '\u{6E1}',
    '\u{6E2}',
    '\u{6E4}',
    '\u{6E7}',
    '\u{6E8}',
    '\u{6EB}',
    '\u{6EC}',
    '\u{730}',
    '\u{732}',
    '\u{733}',
    '\u{735}',
    '\u{736}',
    '\u{73A}',
    '\u{73D}',
    '\u{73F}',
    '\u{740}',
    '\u{741}',
    '\u{743}',
    '\u{745}',
    '\u{747}',
    '\u{749}',
    '\u{74A}',
    '\u{7EB}',
    '\u{7EC}',
    '\u{7ED}',
    '\u{7EE}',
    '\u{7EF}',
    '\u{7F0}',
    '\u{7F1}',
    '\u{7F3}',
    '\u{816}',
    '\u{817}',
    '\u{818}',
    '\u{819}',
    '\u{81B}',
    '\u{81C}',
    '\u{81D}',
    '\u{81E}',
    '\u{81F}',
    '\u{820}',
    '\u{821}',
    '\u{822}',
    '\u{823}',
    '\u{825}',
    '\u{826}',
    '\u{827}',
    '\u{829}',
    '\u{82A}',
    '\u{82B}',
    '\u{82C}',
    '\u{82D}',
    '\u{951}',
    '\u{953}',
    '\u{954}',
    '\u{F82}',
    '\u{F83}',
    '\u{F86}',
    '\u{F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;

    #[test]
    fn reconstructs_explicit_and_inherited_placeholder_cells() {
        let image_id = 0x12_345678u32;
        let [id_high, red, green, blue] = image_id.to_be_bytes();
        let mut bytes = format!("\x1b[38;2;{red};{green};{blue}m").into_bytes();
        let first = format!(
            "{}{}{}{}{}",
            char::from_u32(KITTY_UNICODE_PLACEHOLDER).unwrap(),
            KITTY_DIACRITICS[4],
            KITTY_DIACRITICS[7],
            KITTY_DIACRITICS[id_high as usize],
            char::from_u32(KITTY_UNICODE_PLACEHOLDER).unwrap(),
        );
        bytes.extend_from_slice(first.as_bytes());

        let mut terminal = Terminal::new(8, 2);
        terminal.process(&bytes);
        let mut cells = Vec::new();
        fill_kitty_virtual_cells(&terminal.grid, 0, terminal.grid.rows, &mut cells);

        assert_eq!(
            cells,
            vec![
                KittyVirtualCell {
                    image_id,
                    image_col: 7,
                    image_row: 4,
                    col: 0,
                    row: 0,
                },
                KittyVirtualCell {
                    image_id,
                    image_col: 8,
                    image_row: 4,
                    col: 1,
                    row: 0,
                },
            ]
        );
    }

    #[test]
    fn bare_placeholder_without_metadata_is_not_treated_as_an_image() {
        let mut terminal = Terminal::new(4, 1);
        terminal.process("\u{10eeee}".as_bytes());
        let mut cells = Vec::new();
        fill_kitty_virtual_cells(&terminal.grid, 0, 1, &mut cells);
        assert!(cells.is_empty());
    }

    #[test]
    fn decodes_indexed_image_ids_and_requires_both_colors_to_inherit() {
        let placeholder = char::from_u32(KITTY_UNICODE_PLACEHOLDER).unwrap();
        let first = format!(
            "{placeholder}{}{}{placeholder}",
            KITTY_DIACRITICS[0], KITTY_DIACRITICS[0]
        );
        let mut bytes = b"\x1b[38;5;42;58;5;7m".to_vec();
        bytes.extend_from_slice(first.as_bytes());
        bytes.extend_from_slice(b"\x1b[58;5;8m");
        bytes.extend_from_slice(placeholder.to_string().as_bytes());

        let mut terminal = Terminal::new(4, 1);
        terminal.process(&bytes);
        let mut cells = Vec::new();
        fill_kitty_virtual_cells(&terminal.grid, 0, 1, &mut cells);

        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].image_id, 42);
        assert_eq!(cells[1].image_col, 1);
    }

    #[test]
    fn inherits_omitted_column_and_high_id_diacritics() {
        let image_id = 0x12_345678u32;
        let [id_high, red, green, blue] = image_id.to_be_bytes();
        let placeholder = char::from_u32(KITTY_UNICODE_PLACEHOLDER).unwrap();
        let text = format!(
            "\x1b[38;2;{red};{green};{blue}m\
             {placeholder}{}{}{}\
             {placeholder}{}\
             {placeholder}{}{}",
            KITTY_DIACRITICS[4],
            KITTY_DIACRITICS[7],
            KITTY_DIACRITICS[id_high as usize],
            KITTY_DIACRITICS[4],
            KITTY_DIACRITICS[4],
            KITTY_DIACRITICS[9],
        );

        let mut terminal = Terminal::new(4, 1);
        terminal.process(text.as_bytes());
        let mut cells = Vec::new();
        fill_kitty_virtual_cells(&terminal.grid, 0, 1, &mut cells);

        assert_eq!(cells.len(), 3);
        assert_eq!(cells[1].image_id, image_id);
        assert_eq!(cells[1].image_col, 8);
        assert_eq!(cells[2].image_id, image_id);
        assert_eq!(cells[2].image_col, 9);
    }
}
