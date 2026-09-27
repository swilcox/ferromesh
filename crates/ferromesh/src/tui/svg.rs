//! A rendered screen as an SVG picture of a terminal, for `--snapshot --svg`.
//!
//! Every run of text is placed at its own column and stretched to exactly
//! the cells it covers, so the grid holds whatever monospace font the viewer
//! has, and wide characters like emoji keep their two columns.

use std::fmt::Write;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::text::Span;

const FONT_SIZE: f32 = 14.0;
const CELL_W: f32 = 8.4;
const CELL_H: f32 = 18.0;
/// Room around the grid, and above it for the title bar.
const PAD: f32 = 16.0;
const BAR: f32 = 32.0;

const FG: &str = "#c9d1d9";
const BG: &str = "#0d1117";

/// GitHub's dark terminal palette, ANSI 0–15, with a darker black so black
/// text on a coloured background stays legible.
const ANSI: [&str; 16] = [
    "#161b22", "#ff7b72", "#3fb950", "#d29922", "#58a6ff", "#bc8cff", "#39c5cf", "#b1bac4",
    "#6e7681", "#ffa198", "#56d364", "#e3b341", "#79c0ff", "#d2a8ff", "#56d4dd", "#ffffff",
];

/// A cell's look once reversal and defaults are resolved.
#[derive(Clone, PartialEq)]
struct Look {
    fg: String,
    bg: Option<String>,
    modifier: Modifier,
}

pub fn render(buffer: &Buffer, title: &str) -> String {
    let (cols, rows) = (buffer.area.width, buffer.area.height);
    let width = PAD * 2.0 + f32::from(cols) * CELL_W;
    let height = BAR + PAD + f32::from(rows) * CELL_H;
    let mut backgrounds = String::new();
    let mut texts = String::new();

    for y in 0..rows {
        let top = BAR + PAD / 2.0 + f32::from(y) * CELL_H;
        let baseline = top + CELL_H * 0.75;
        // (first column, columns covered, text, look) for the run being built.
        let mut run: Option<(u16, u16, String, Look)> = None;
        // (first column, columns covered, colour) for the background.
        let mut fill: Option<(u16, u16, String)> = None;
        let mut x = 0;
        while x < cols {
            let cell = &buffer[(x, y)];
            let symbol = cell.symbol();
            let span = (Span::raw(symbol).width().max(1) as u16).min(cols - x);
            let look = look(cell.fg, cell.bg, cell.modifier);

            // One rect per stretch of a colour, so there are no seams.
            match (&mut fill, &look.bg) {
                (Some((_, covered, color)), Some(bg)) if color == bg => *covered += span,
                (_, bg) => {
                    if let Some(done) = fill.take() {
                        push_rect(&mut backgrounds, done, top);
                    }
                    fill = bg.clone().map(|bg| (x, span, bg));
                }
            }

            // Wide characters stand alone, so nothing after them drifts.
            let joins = span == 1 && run.as_ref().is_some_and(|(_, _, _, l)| *l == look);
            if joins {
                let (_, covered, text, _) = run.as_mut().expect("joins an open run");
                *covered += 1;
                text.push_str(symbol);
            } else {
                if let Some(done) = run.take() {
                    push_text(&mut texts, done, baseline);
                }
                run = Some((x, span, symbol.to_owned(), look));
                if span > 1 {
                    push_text(&mut texts, run.take().expect("just set"), baseline);
                }
            }
            x += span;
        }
        if let Some(done) = run {
            push_text(&mut texts, done, baseline);
        }
        if let Some(done) = fill {
            push_rect(&mut backgrounds, done, top);
        }
    }

    let dots: String = ["#ff5f57", "#febc2e", "#28c840"]
        .iter()
        .enumerate()
        .map(|(i, color)| {
            format!(
                r#"<circle cx="{}" cy="{}" r="6" fill="{color}"/>"#,
                PAD + 4.0 + 20.0 * i as f32,
                BAR / 2.0 + 2.0
            )
        })
        .collect();
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width:.0} {height:.0}" width="{width:.0}" height="{height:.0}">
<style>text {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, 'DejaVu Sans Mono', monospace; font-size: {FONT_SIZE}px; white-space: pre; }}</style>
<rect width="100%" height="100%" rx="10" fill="{BG}" stroke="#30363d"/>
{dots}
<text x="{:.1}" y="{:.1}" fill="#8b949e" text-anchor="middle" style="font-size: 13px">{}</text>
{backgrounds}
{texts}
</svg>
"##,
        width / 2.0,
        BAR / 2.0 + 6.0,
        escape(title),
    )
}

fn push_rect(out: &mut String, (x, covered, color): (u16, u16, String), top: f32) {
    let left = PAD + f32::from(x) * CELL_W;
    let width = f32::from(covered) * CELL_W;
    let _ = write!(
        out,
        r#"<rect x="{left:.1}" y="{top:.1}" width="{width:.1}" height="{CELL_H}" fill="{color}"/>"#
    );
}

fn push_text(out: &mut String, (x, covered, text, look): (u16, u16, String, Look), baseline: f32) {
    if text.trim().is_empty() {
        return;
    }
    let left = PAD + f32::from(x) * CELL_W;
    let length = f32::from(covered) * CELL_W;
    let mut style = String::new();
    if look.modifier.contains(Modifier::BOLD) {
        style.push_str("font-weight:bold;");
    }
    if look.modifier.contains(Modifier::ITALIC) {
        style.push_str("font-style:italic;");
    }
    if look.modifier.contains(Modifier::UNDERLINED) {
        style.push_str("text-decoration:underline;");
    }
    if look.modifier.contains(Modifier::DIM) {
        style.push_str("opacity:.6;");
    }
    let _ = write!(
        out,
        r#"<text x="{left:.1}" y="{baseline:.1}" fill="{}" textLength="{length:.1}" lengthAdjust="spacingAndGlyphs" xml:space="preserve""#,
        look.fg
    );
    if !style.is_empty() {
        let _ = write!(out, r#" style="{style}""#);
    }
    let _ = write!(out, ">{}</text>", escape(&text));
}

fn look(fg: Color, bg: Color, modifier: Modifier) -> Look {
    let (mut fg, mut bg) = (color(fg), color(bg));
    if modifier.contains(Modifier::REVERSED) {
        let fg_or_default = fg.unwrap_or_else(|| FG.to_owned());
        fg = Some(bg.unwrap_or_else(|| BG.to_owned()));
        bg = Some(fg_or_default);
    }
    Look { fg: fg.unwrap_or_else(|| FG.to_owned()), bg, modifier }
}

/// `None` for the terminal's default.
fn color(color: Color) -> Option<String> {
    let ansi = |i: usize| Some(ANSI[i].to_owned());
    match color {
        Color::Reset => None,
        Color::Black => ansi(0),
        Color::Red => ansi(1),
        Color::Green => ansi(2),
        Color::Yellow => ansi(3),
        Color::Blue => ansi(4),
        Color::Magenta => ansi(5),
        Color::Cyan => ansi(6),
        Color::Gray => ansi(7),
        Color::DarkGray => ansi(8),
        Color::LightRed => ansi(9),
        Color::LightGreen => ansi(10),
        Color::LightYellow => ansi(11),
        Color::LightBlue => ansi(12),
        Color::LightMagenta => ansi(13),
        Color::LightCyan => ansi(14),
        Color::White => ansi(15),
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Indexed(i) => Some(indexed(i)),
    }
}

/// The xterm 256-colour table.
fn indexed(i: u8) -> String {
    match i {
        0..16 => ANSI[usize::from(i)].to_owned(),
        16..232 => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            let i = i - 16;
            format!("#{:02x}{:02x}{:02x}", level(i / 36), level(i / 6 % 6), level(i % 6))
        }
        _ => {
            let v = 8 + 10 * (i - 232);
            format!("#{v:02x}{v:02x}{v:02x}")
        }
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    use super::*;

    #[test]
    fn runs_keep_their_columns() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        buffer.set_string(0, 0, "ab", Style::new().cyan());
        buffer.set_string(2, 0, "🎉", Style::new());
        buffer.set_string(4, 0, "<c>", Style::new().bold().reversed());
        let svg = render(&buffer, "t");

        assert!(svg.contains(r##"x="16.0" y="53.5" fill="#39c5cf" textLength="16.8""##), "{svg}");
        assert!(svg.contains(
            r#"textLength="16.8" lengthAdjust="spacingAndGlyphs" xml:space="preserve">🎉"#
        ));
        // Reversed: default colours swapped, the text escaped, a background drawn.
        assert!(svg.contains(r##"x="49.6" y="53.5" fill="#0d1117""##), "{svg}");
        assert!(svg.contains("&lt;c&gt;</text>"));
        assert!(
            svg.contains(r##"<rect x="49.6" y="40.0" width="25.2" height="18" fill="#c9d1d9"/>"##)
        );
    }

    #[test]
    fn indexed_colours() {
        assert_eq!(indexed(1), ANSI[1]);
        assert_eq!(indexed(196), "#ff0000");
        assert_eq!(indexed(244), "#808080");
    }
}
