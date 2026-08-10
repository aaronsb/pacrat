//! The petit chef — pacrat's mascot, drawn in terminal half-blocks.
//!
//! `art/petit-chef.html` is the source of truth: [`GRID`] is its `const ART`
//! block copied verbatim, and [`PALETTE`] is its `--c-*` colours. Nothing
//! parses the HTML at runtime — redraw him there, then copy the grid back.
//!
//! One character of the grid is one pixel, and a terminal cell is two pixels
//! tall, so rows are paired and each pair becomes one line of half-blocks:
//! both pixels coloured is `▀` with the lower pixel as the background, one
//! pixel alone is the matching half-block in the foreground only, and
//! neither is a space. The pairing lives in one place ([`cells`]) and both
//! faces — ANSI strings for the CLI, [`Line`]s for the TUI — are projections
//! of it, so the two surfaces cannot disagree about the rat.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// The grid's columns, as the HTML declares them. The row count (32) is
/// pinned by the tests rather than named here, because nothing outside them
/// needs the number — the faces are as tall as the pairing makes them.
pub const WIDTH: usize = 38;
#[cfg(test)]
const HEIGHT: usize = 32;

/// The `const ART` grid from `art/petit-chef.html`, verbatim. `.` is
/// transparent; every other character is a [`PALETTE`] key.
const GRID: &str = "\
......................................
......................................
......................................
................w.....................
.............wwwwwww..................
...........wwwwwwwwwww................
.........wwwwwwwwwwwwwww..............
.........wwwwwwwwwwwwwww..............
........wwwwwwwwwwwwwwwww.............
.........wwwwwwwwwwwwwww..............
.......ffwwWWWWWWWWWWWwwff............
......fffffWWWWWWWWWWWfffff...........
.....ffpppfWWWWWWWWWWWfpppff..........
....ffpppppfffffffffffpppppff.........
....ffppppffheefffheeffppppff.........
....ffppppffeeefffeeeffppppff.........
.....ffpppffeeefffeeeffpppff.t........
......fffffffflllllffffffff..tt.......
.......ffffffllnnnllffffff....tt......
.....kkkkk.fllllnllllf.kkkkk...t......
......kkkkk.flllllllf.kkkkk....t......
.......kkkkkfflccclffkkkkk.....tt.....
...........fffccKccfff..........t.....
.........PPPfcccccccfPPP.......tt.....
.........PPPPccKccccPPPP.......t......
.........PPPccccccKccPPP......tt......
.........ffflllllllllfff....ttt.......
.........ffflllllllllfffttttttt.......
.........fflllllllllllffttttt.........
..........fflllllllllfftt.............
...........flllllllllf................
............fflllllff.................";

pub type Rgb = (u8, u8, u8);

/// The HTML's `--c-*` variables, key for key.
const PALETTE: [(char, Rgb); 13] = [
    ('w', (0xf4, 0xeb, 0xdd)), // toque
    ('W', (0xd9, 0xcc, 0xb9)), // toque band
    ('f', (0x8a, 0x90, 0xae)), // fur
    ('l', (0xcf, 0xc8, 0xdc)), // belly + muzzle
    ('p', (0xe8, 0x94, 0xa6)), // inner ear
    ('P', (0xe3, 0xbc, 0xc5)), // paws
    ('e', (0x1b, 0x15, 0x21)), // eye
    ('h', (0xff, 0xff, 0xff)), // eye shine
    ('n', (0xef, 0x7f, 0x95)), // nose
    ('k', (0x6a, 0x6f, 0x8d)), // whiskers
    ('c', (0xef, 0xc0, 0x4a)), // cheese
    ('K', (0xb8, 0x87, 0x1f)), // cheese holes
    ('t', (0xe0, 0xaa, 0xb4)), // tail
];

fn colour(pixel: char) -> Option<Rgb> {
    PALETTE
        .iter()
        .find(|(key, _)| *key == pixel)
        .map(|(_, rgb)| *rgb)
}

/// How much colour the terminal admits to.
///
/// Truecolor only on the terminal's own word for it — COLORTERM saying
/// `truecolor` or `24bit` is the convention — and anything less falls back
/// to the nearest xterm-256 colour rather than to no rat at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    TrueColor,
    Indexed,
}

/// Read a COLORTERM value. The value arrives as a parameter rather than
/// being read here, so a test can try every spelling without mutating the
/// process environment.
pub fn depth(colorterm: Option<&str>) -> Depth {
    match colorterm {
        Some(v) if v.contains("truecolor") || v.contains("24bit") => Depth::TrueColor,
        _ => Depth::Indexed,
    }
}

/// Whether the art is drawn at all: only at a terminal, and never under
/// NO_COLOR — which its spec reads as "set at all", an empty value included,
/// so the parameter is "is it set" rather than what it says. Parameters
/// rather than reads, for the same testability reason as [`depth`].
pub fn wanted(stdout_is_terminal: bool, no_color_set: bool) -> bool {
    stdout_is_terminal && !no_color_set
}

/// The upper and lower pixel behind one terminal cell.
#[derive(Debug, Clone, Copy)]
struct Cell {
    upper: Option<Rgb>,
    lower: Option<Rgb>,
}

/// The half-block core: the grid's row pairs as terminal cells. Everything
/// either face draws is a rendering of this.
fn cells() -> Vec<Vec<Cell>> {
    let rows: Vec<&[u8]> = GRID.lines().map(str::as_bytes).collect();
    rows.chunks(2)
        .map(|pair| {
            (0..WIDTH)
                .map(|x| Cell {
                    upper: colour(pair[0][x] as char),
                    lower: pair.get(1).and_then(|row| colour(row[x] as char)),
                })
                .collect()
        })
        .collect()
}

/// One cell as both faces must draw it: glyph, foreground, background.
fn resolve(cell: Cell) -> (char, Option<Rgb>, Option<Rgb>) {
    match (cell.upper, cell.lower) {
        (None, None) => (' ', None, None),
        (Some(upper), None) => ('▀', Some(upper), None),
        (None, Some(lower)) => ('▄', Some(lower), None),
        (Some(upper), Some(lower)) => ('▀', Some(upper), Some(lower)),
    }
}

/// The CLI's face: one escape-coded string per terminal line, reset at the
/// end so nothing leaks into whatever prints next.
pub fn ansi_lines(depth: Depth) -> Vec<String> {
    cells()
        .iter()
        .map(|row| {
            let mut line = String::new();
            for &cell in row {
                let (glyph, fg, bg) = resolve(cell);
                // A reset before every cell rather than a diff against the
                // previous one: it is what keeps a background from bleeding
                // under the transparent cells, and sixteen lines drawn once
                // are not worth a state machine.
                line.push_str("\x1b[0m");
                if let Some(rgb) = fg {
                    line.push_str(&code(depth, 38, rgb));
                }
                if let Some(rgb) = bg {
                    line.push_str(&code(depth, 48, rgb));
                }
                line.push(glyph);
            }
            line.push_str("\x1b[0m");
            line
        })
        .collect()
}

fn code(depth: Depth, plane: u8, (r, g, b): Rgb) -> String {
    match depth {
        Depth::TrueColor => format!("\x1b[{plane};2;{r};{g};{b}m"),
        Depth::Indexed => format!("\x1b[{plane};5;{}m", xterm256((r, g, b))),
    }
}

/// The TUI's face: the same cells as ratatui spans.
pub fn tui_lines(depth: Depth) -> Vec<Line<'static>> {
    let paint = |(r, g, b): Rgb| match depth {
        Depth::TrueColor => Color::Rgb(r, g, b),
        Depth::Indexed => Color::Indexed(xterm256((r, g, b))),
    };
    cells()
        .iter()
        .map(|row| {
            Line::from(
                row.iter()
                    .map(|&cell| {
                        let (glyph, fg, bg) = resolve(cell);
                        let mut style = Style::new();
                        if let Some(rgb) = fg {
                            style = style.fg(paint(rgb));
                        }
                        if let Some(rgb) = bg {
                            style = style.bg(paint(rgb));
                        }
                        Span::styled(glyph.to_string(), style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// Nearest xterm-256 colour: the 6×6×6 cube against the grayscale ramp,
/// whichever is closer by squared RGB distance. The cube wins ties, so the
/// mapping is deterministic.
fn xterm256((r, g, b): Rgb) -> u8 {
    const CUBE: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| -> usize {
        CUBE.iter()
            .enumerate()
            .min_by_key(|(_, &level)| (level - i32::from(v)).abs())
            .map_or(0, |(index, _)| index)
    };
    let dist = |cr: i32, cg: i32, cb: i32| -> i32 {
        let d = |level: i32, v: u8| (level - i32::from(v)).pow(2);
        d(cr, r) + d(cg, g) + d(cb, b)
    };
    let (ri, gi, bi) = (near(r), near(g), near(b));
    let cube_dist = dist(CUBE[ri], CUBE[gi], CUBE[bi]);
    // The ramp runs 8, 18, … 238 at indices 232-255.
    let avg = (i32::from(r) + i32::from(g) + i32::from(b)) / 3;
    let gray_i = ((avg - 8 + 5) / 10).clamp(0, 23);
    let gray = 8 + 10 * gray_i;
    if dist(gray, gray, gray) < cube_dist {
        (232 + gray_i) as u8
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grid really is the HTML's: 38×32, and every non-transparent pixel
    /// names a palette colour. A stray character here would be an invisible
    /// pixel in one face and a panic in neither, which is the worst kind.
    #[test]
    fn the_grid_is_38_by_32_and_speaks_only_the_palette() {
        let rows: Vec<&str> = GRID.lines().collect();
        assert_eq!(rows.len(), HEIGHT);
        for row in &rows {
            assert_eq!(row.len(), WIDTH, "{row}");
            for pixel in row.chars() {
                assert!(
                    pixel == '.' || colour(pixel).is_some(),
                    "unknown pixel {pixel:?}"
                );
            }
        }
        // 13 colours, as the HTML's own footer says.
        assert_eq!(PALETTE.len(), 13);
    }

    /// Two grid rows per terminal line: 32 rows become 16 lines of 38 cells,
    /// and both faces keep that shape.
    #[test]
    fn pairing_yields_16_lines_of_38_cells() {
        let core = cells();
        assert_eq!(core.len(), HEIGHT / 2);
        for row in &core {
            assert_eq!(row.len(), WIDTH);
        }
        for depth in [Depth::TrueColor, Depth::Indexed] {
            assert_eq!(ansi_lines(depth).len(), HEIGHT / 2);
            assert_eq!(tui_lines(depth).len(), HEIGHT / 2);
        }
    }

    /// The four half-block cases, straight from the rendering rule.
    #[test]
    fn a_cell_resolves_to_the_documented_glyph() {
        let of = |upper, lower| resolve(Cell { upper, lower });
        let red = Some((255, 0, 0));
        let blue = Some((0, 0, 255));
        assert_eq!(of(None, None), (' ', None, None));
        assert_eq!(of(red, None), ('▀', red, None));
        assert_eq!(of(None, blue), ('▄', blue, None));
        assert_eq!(of(red, blue), ('▀', red, blue));
    }

    /// Both faces are projections of one core, so the characters a terminal
    /// would show — escapes stripped — must be identical, line for line.
    #[test]
    fn the_two_faces_draw_the_same_characters() {
        for depth in [Depth::TrueColor, Depth::Indexed] {
            let ansi = ansi_lines(depth);
            let tui = tui_lines(depth);
            for (cli_line, tui_line) in ansi.iter().zip(&tui) {
                let cli: String = stripped(cli_line);
                let tui: String = tui_line.spans.iter().map(|s| s.content.as_ref()).collect();
                assert_eq!(cli, tui);
                assert_eq!(cli.chars().count(), WIDTH);
            }
        }
    }

    /// Everything between an ESC and the `m` that ends it, removed.
    fn stripped(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for escape in chars.by_ref() {
                    if escape == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// The depth codes really differ: truecolor is `38;2;r;g;b`, the
    /// fallback is `38;5;n`, and neither leaks into the other.
    #[test]
    fn each_depth_emits_only_its_own_codes() {
        let truecolor = ansi_lines(Depth::TrueColor).join("\n");
        assert!(truecolor.contains("\x1b[38;2;"));
        assert!(!truecolor.contains(";5;"));
        let indexed = ansi_lines(Depth::Indexed).join("\n");
        assert!(indexed.contains("\x1b[38;5;"));
        assert!(!indexed.contains(";2;"));
    }

    /// The 256-colour fallback is arithmetic, so it can be pinned: exact
    /// cube colours land on their cube index, an exact ramp gray lands on
    /// the ramp, and the cheese maps where the distance says it must.
    #[test]
    fn the_256_fallback_maps_known_colours_deterministically() {
        assert_eq!(xterm256((0, 0, 0)), 16);
        assert_eq!(xterm256((255, 255, 255)), 231);
        assert_eq!(xterm256((95, 135, 175)), 67);
        assert_eq!(xterm256((128, 128, 128)), 244);
        // The cheese, `--c-c: #efc04a`: nearest cube corner (255,175,95).
        assert_eq!(xterm256((0xef, 0xc0, 0x4a)), 215);
    }

    /// COLORTERM's two spellings and nothing else mean truecolor. The value
    /// is a parameter precisely so this test never touches the environment.
    #[test]
    fn truecolor_needs_the_terminals_own_word_for_it() {
        assert_eq!(depth(Some("truecolor")), Depth::TrueColor);
        assert_eq!(depth(Some("24bit")), Depth::TrueColor);
        assert_eq!(depth(Some("xterm-truecolor")), Depth::TrueColor);
        assert_eq!(depth(Some("")), Depth::Indexed);
        assert_eq!(depth(Some("yes")), Depth::Indexed);
        assert_eq!(depth(Some("256color")), Depth::Indexed);
        assert_eq!(depth(None), Depth::Indexed);
    }

    /// The gate: a pipe gets no art, and NO_COLOR set at all — even empty —
    /// gets no art. The info block beside it is the caller's business and
    /// prints regardless.
    #[test]
    fn art_is_for_terminals_that_have_not_said_no() {
        assert!(wanted(true, false));
        assert!(!wanted(false, false), "a pipe got escape codes");
        assert!(!wanted(true, true), "NO_COLOR was ignored");
        assert!(!wanted(false, true));
    }
}
