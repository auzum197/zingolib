//! Large digits for the balance: a 5x7 pixel font drawn with half blocks, four rows tall.
//! Terminal cells are about twice as tall as wide, so each half block is a square pixel, and
//! each pixel row can take its own colour.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Eight pixel rows per glyph, `#` lit. The eighth row is below the baseline, for the comma.
#[rustfmt::skip]
fn glyph(c: char) -> Option<[&'static str; 8]> {
    Some(match c {
        '0' => [".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###.", "....."],
        '1' => ["..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###.", "....."],
        '2' => [".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####", "....."],
        '3' => [".###.", "#...#", "....#", "..##.", "....#", "#...#", ".###.", "....."],
        '4' => ["...#.", "..##.", ".#.#.", "#..#.", "#####", "...#.", "...#.", "....."],
        '5' => ["#####", "#....", "####.", "....#", "....#", "#...#", ".###.", "....."],
        '6' => ["..##.", ".#...", "#....", "####.", "#...#", "#...#", ".###.", "....."],
        '7' => ["#####", "....#", "...#.", "..#..", ".#...", ".#...", ".#...", "....."],
        '8' => [".###.", "#...#", "#...#", ".###.", "#...#", "#...#", ".###.", "....."],
        '9' => [".###.", "#...#", "#...#", ".####", "....#", "...#.", ".##..", "....."],
        '.' => [".", ".", ".", ".", ".", ".", "#", "."],
        ',' => [".", ".", ".", ".", ".", ".", "#", "#"],
        _ => return None,
    })
}

/// Renders `text` as four lines, one column between glyphs. `shade` colours the pixel rows
/// from the top of a digit to the baseline, and the comma's tail takes the baseline colour.
/// `None` if any character has no glyph, so the caller can fall back to plain text.
pub fn render(text: &str, shade: &[Color; 7]) -> Option<[Line<'static>; 4]> {
    let glyphs = text.chars().map(glyph).collect::<Option<Vec<_>>>()?;
    let colour = |row: usize| shade[row.min(6)];
    Some(std::array::from_fn(|r| {
        let (top, bottom) = (colour(2 * r), colour(2 * r + 1));
        let mut cells = Vec::new();
        for (i, g) in glyphs.iter().enumerate() {
            if i > 0 {
                cells.push((' ', Style::new()));
            }
            for (t, b) in g[2 * r].bytes().zip(g[2 * r + 1].bytes()) {
                cells.push(match (t == b'#', b == b'#') {
                    (true, true) if top == bottom => ('█', Style::new().fg(top)),
                    (true, true) => ('▀', Style::new().fg(top).bg(bottom)),
                    (true, false) => ('▀', Style::new().fg(top)),
                    (false, true) => ('▄', Style::new().fg(bottom)),
                    (false, false) => (' ', Style::new()),
                });
            }
        }
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (ch, style) in cells {
            match spans.last_mut() {
                Some(span) if span.style == style => span.content.to_mut().push(ch),
                _ => spans.push(Span::styled(ch.to_string(), style)),
            }
        }
        Line::from(spans)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAT: [Color; 7] = [Color::Reset; 7];

    fn shape(text: &str) -> [String; 4] {
        render(text, &FLAT).unwrap().map(|line| line.to_string())
    }

    #[test]
    fn every_supported_character_renders_evenly() {
        let rows = render("0123456789.,", &FLAT).unwrap();
        let width = rows[0].width();
        assert!(rows.iter().all(|r| r.width() == width));
        // ten digits of five, two marks of one, eleven gaps
        assert_eq!(width, 10 * 5 + 2 + 11);
    }

    #[test]
    fn digits_are_distinct() {
        let shapes: Vec<[String; 4]> = ('0'..='9').map(|d| shape(&d.to_string())).collect();
        for (i, a) in shapes.iter().enumerate() {
            for b in &shapes[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn eight_and_the_decimal_point_look_right() {
        assert_eq!(shape("8"), ["▄▀▀▀▄", "▀▄▄▄▀", "█   █", " ▀▀▀ "]);
        assert_eq!(shape("1.5")[3], " ▀▀▀  ▀  ▀▀▀ ");
    }

    #[test]
    fn a_gradient_colours_each_pixel_row() {
        let shade = std::array::from_fn(|i| Color::Indexed(i as u8));
        let rows = render("8", &shade).unwrap();
        // the sides of the lower bowl span pixel rows 4 and 5
        let side = &rows[2].spans[0];
        assert_eq!(side.content, "▀");
        assert_eq!(
            side.style,
            Style::new().fg(Color::Indexed(4)).bg(Color::Indexed(5))
        );
    }

    #[test]
    fn unknown_characters_fall_back() {
        assert!(render("1.5 ZEC", &FLAT).is_none());
        assert!(render("", &FLAT).is_some());
    }
}
