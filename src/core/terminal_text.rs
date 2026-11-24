//! Model text cannot execute terminal control sequences, including split OSC data.

#[derive(Default)]
enum Escape {
    #[default]
    Text,
    Start,
    Csi,
    Osc,
    OscEnd,
}

#[derive(Default)]
pub struct TerminalText {
    state: Escape,
}

impl TerminalText {
    pub fn push(&mut self, text: &str) -> String {
        let mut output = String::new();
        for character in text.chars() {
            match self.state {
                Escape::Text => match character {
                    '\x1b' => self.state = Escape::Start,
                    '\n' => output.push('\n'),
                    '\t' => output.push('\t'),
                    c if c.is_control()
                        || matches!(c,'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}') =>
                        {}
                    c => output.push(c),
                },
                Escape::Start => {
                    self.state = match character {
                        '[' => Escape::Csi,
                        ']' | 'P' | '_' | '^' => Escape::Osc,
                        _ => Escape::Text,
                    }
                }
                Escape::Csi => {
                    if ('\u{40}'..='\u{7e}').contains(&character) {
                        self.state = Escape::Text;
                    }
                }
                Escape::Osc => match character {
                    '\x07' => self.state = Escape::Text,
                    '\x1b' => self.state = Escape::OscEnd,
                    _ => {}
                },
                Escape::OscEnd => {
                    self.state = if character == '\\' {
                        Escape::Text
                    } else {
                        Escape::Osc
                    }
                }
            }
        }
        output
    }
}

pub fn clean(text: &str) -> String {
    TerminalText::default().push(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_split_clipboard_and_colour_controls_but_keeps_thai() {
        let mut cleaner = TerminalText::default();
        assert_eq!(cleaner.push("hello\x1b]52;c;"), "hello");
        assert_eq!(cleaner.push("hidden\x07 สวัสดี\x1b["), " สวัสดี");
        assert_eq!(cleaner.push("31mworld\x1b[0m\n"), "world\n");
        assert_eq!(clean("\u{202e}text\r\n\t"), "text\n\t");
    }
}
