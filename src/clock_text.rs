use anyhow::Result;
use std::collections::BTreeMap;

/// Render four seven-segment positions. Letters are case-insensitive approximations:
/// B/D/N/Q/R/T use lowercase shapes; I looks like 1, O like 0, S like 5.
/// Unsupported letters and non-ASCII input are rejected, never transliterated.
/// Short text is centered (odd spare space goes right); long text scrolls through
/// four blank positions at each end. Each frame explicitly clears unused A..G
/// segments, leaving auxiliary clock indicators to the caller's ownership policy.
pub(crate) fn frames(text: &str, brightness: u8) -> Result<Vec<BTreeMap<String, u8>>> {
    anyhow::ensure!(brightness <= 100, "brightness must be between 0 and 100");
    anyhow::ensure!(text.is_ascii(), "clock text supports ASCII only");
    anyhow::ensure!(
        (1..=32).contains(&text.len()),
        "clock text must contain 1 to 32 characters"
    );
    anyhow::ensure!(
        text.bytes().any(|ch| ch != b' '),
        "clock text must not be blank"
    );
    let glyphs: Vec<&str> = text.bytes().map(glyph).collect::<Result<_>>()?;
    let mut padded = Vec::new();
    if glyphs.len() <= 4 {
        padded.resize((4 - glyphs.len()) / 2, "");
        padded.extend(glyphs);
        padded.resize(4, "");
    } else {
        padded.resize(4, "");
        padded.extend(glyphs);
        padded.extend([""; 4]);
    }
    Ok(padded
        .windows(4)
        .map(|window| {
            let mut frame = BTreeMap::new();
            for (position, glyph) in window.iter().enumerate() {
                for part in 'A'..='G' {
                    let value = if glyph.contains(part) { brightness } else { 0 };
                    frame.insert(format!("{part}{}", position + 1), value);
                }
            }
            frame
        })
        .collect())
}

fn glyph(ch: u8) -> Result<&'static str> {
    Ok(match ch.to_ascii_uppercase() {
        b' ' => "",
        b'-' => "G",
        b'_' => "D",
        b'0' | b'O' => "ABCDEF",
        b'1' | b'I' => "BC",
        b'2' => "ABDEG",
        b'3' => "ABCDG",
        b'4' => "BCFG",
        b'5' | b'S' => "ACDFG",
        b'6' => "ACDEFG",
        b'7' => "ABC",
        b'8' => "ABCDEFG",
        b'9' => "ABCDFG",
        b'A' => "ABCEFG",
        b'B' => "CDEFG",
        b'C' => "ADEF",
        b'D' => "BCDEG",
        b'E' => "ADEFG",
        b'F' => "AEFG",
        b'G' => "ACDEF",
        b'H' => "BCEFG",
        b'J' => "BCDE",
        b'L' => "DEF",
        b'N' => "CEG",
        b'P' => "ABEFG",
        b'Q' => "ABCFG",
        b'R' => "EG",
        b'T' => "DEFG",
        b'U' => "BCDEF",
        b'Y' => "BCDFG",
        _ => anyhow::bail!("unsupported clock character: {:?}", char::from(ch)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(frame: &BTreeMap<String, u8>) -> Vec<&str> {
        frame
            .iter()
            .filter(|(_, value)| **value != 0)
            .map(|(key, _)| key.as_str())
            .collect()
    }

    #[test]
    fn sp_matches_confirmed_midi_frame_and_clears_unused_segments() {
        let rendered = frames("SP", 70).unwrap();
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].len(), 28);
        assert_eq!(
            lit(&rendered[0]),
            ["A2", "A3", "B3", "C2", "D2", "E3", "F2", "F3", "G2", "G3"]
        );
        assert!(rendered[0]
            .values()
            .all(|value| *value == 0 || *value == 70));
    }

    #[test]
    fn centers_short_text_with_extra_blank_on_right() {
        assert_eq!(lit(&frames("1", 100).unwrap()[0]), ["B2", "C2"]);
        assert_eq!(
            lit(&frames("111", 100).unwrap()[0]),
            ["B1", "B2", "B3", "C1", "C2", "C3"]
        );
        assert_eq!(frames("1111", 100).unwrap().len(), 1);
    }

    #[test]
    fn scrolling_enters_from_right_and_leaves_to_left() {
        let rendered = frames("11111", 100).unwrap();
        assert_eq!(rendered.len(), 10);
        assert!(lit(&rendered[0]).is_empty());
        assert_eq!(lit(&rendered[1]), ["B4", "C4"]);
        assert_eq!(
            lit(&rendered[4]),
            ["B1", "B2", "B3", "B4", "C1", "C2", "C3", "C4"]
        );
        assert_eq!(lit(&rendered[8]), ["B1", "C1"]);
        assert!(lit(&rendered[9]).is_empty());
        assert!(rendered.iter().all(|frame| frame.len() == 28));
    }

    #[test]
    fn validates_input_before_rendering() {
        for text in [
            "",
            "   ",
            "Привет",
            "é",
            "hi\n",
            "@",
            "W",
            "M",
            "K",
            "V",
            "X",
            "Z",
        ] {
            assert!(frames(text, 50).is_err(), "accepted {text:?}");
        }
        assert!(frames("SP", 101).is_err());
        assert!(frames(&"1".repeat(33), 50).is_err());
        assert_eq!(frames(&"1".repeat(32), 50).unwrap().len(), 37);
        assert!(frames("SP", 0).unwrap()[0]
            .values()
            .all(|value| *value == 0));
    }

    #[test]
    fn supports_digits_readable_letters_and_simple_separators() {
        assert_eq!(frames("sp", 50).unwrap(), frames("SP", 50).unwrap());
        assert_eq!(lit(&frames("-", 50).unwrap()[0]), ["G2"]);
        assert_eq!(lit(&frames("_", 50).unwrap()[0]), ["D2"]);
        for ch in "0123456789AbCdEFGHIJLnOPqrStUY-_".chars() {
            assert!(frames(&ch.to_string(), 50).is_ok());
        }
        assert!(frames("S P", 50).is_ok());
    }
}
