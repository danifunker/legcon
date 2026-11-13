//! Character sets old machines use for names and text, decoded and encoded both ways.
//!
//! The DOS code pages (CP437, CP850) are not in `encoding_rs` and arrive with their tables in
//! phase 1; until then a DOS machine uses Windows-1252, which agrees on ASCII.

/// A machine's character set; chosen per machine, guessed by the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Charset {
    /// ISO-8859-1: every byte is the code point of the same number (most 1990s Unix).
    #[default]
    Latin1,
    /// Windows-1252 (Windows 9x / NT without Unicode).
    Windows1252,
    MacRoman,
    ShiftJis,
    Utf8,
}

impl Charset {
    pub const ALL: [Charset; 5] = [
        Charset::Latin1,
        Charset::Windows1252,
        Charset::MacRoman,
        Charset::ShiftJis,
        Charset::Utf8,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Charset::Latin1 => "ISO-8859-1",
            Charset::Windows1252 => "Windows-1252",
            Charset::MacRoman => "MacRoman",
            Charset::ShiftJis => "Shift-JIS",
            Charset::Utf8 => "UTF-8",
        }
    }

    pub fn from_label(label: &str) -> Option<Charset> {
        Charset::ALL
            .into_iter()
            .find(|c| c.label().eq_ignore_ascii_case(label))
    }

    fn encoding(self) -> Option<&'static encoding_rs::Encoding> {
        match self {
            Charset::Latin1 => None,
            Charset::Windows1252 => Some(encoding_rs::WINDOWS_1252),
            Charset::MacRoman => Some(encoding_rs::MACINTOSH),
            Charset::ShiftJis => Some(encoding_rs::SHIFT_JIS),
            Charset::Utf8 => Some(encoding_rs::UTF_8),
        }
    }

    /// Bytes from the machine as text; bytes with no mapping become U+FFFD.
    pub fn decode(self, bytes: &[u8]) -> String {
        match self.encoding() {
            None => bytes.iter().map(|&b| b as char).collect(),
            Some(enc) => enc.decode_without_bom_handling(bytes).0.into_owned(),
        }
    }

    /// Text as bytes for the machine; `None` when a character has no mapping in this set.
    pub fn encode(self, text: &str) -> Option<Vec<u8>> {
        match self.encoding() {
            None => text
                .chars()
                .map(|c| u8::try_from(u32::from(c)).ok())
                .collect(),
            Some(enc) => {
                let (bytes, _, unmappable) = enc.encode(text);
                (!unmappable).then(|| bytes.into_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Charset;

    #[test]
    fn latin1_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255).collect();
        let text = Charset::Latin1.decode(&all);
        assert_eq!(Charset::Latin1.encode(&text).unwrap(), all);
    }

    #[test]
    fn macroman_and_unmappable() {
        // 0x8A is a-umlaut in MacRoman.
        assert_eq!(Charset::MacRoman.decode(&[0x8A]), "\u{e4}");
        assert_eq!(Charset::MacRoman.encode("\u{e4}").unwrap(), vec![0x8A]);
        assert_eq!(Charset::Latin1.encode("\u{20ac}"), None);
        assert_eq!(Charset::from_label("macroman"), Some(Charset::MacRoman));
    }
}
