//! The input of TerminalSend: text and named keys. The terminal types it,
//! and the permission check reads it, with this one parser.

/// One piece of TerminalSend input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key<'a> {
    /// Text as typed. A `<...>` that is not a key name is text.
    Text(&'a str),
    /// A key that ends the line: `<Enter>`, `<C-m>`, `<C-j>`.
    LineEnd(&'a str),
    /// `<lt>`: a literal `<`.
    Lt,
    /// Another key, which can move in or change the line: `<Tab>`, `<Esc>`,
    /// `<BS>`, the arrows, the other `<C-x>`.
    Edit(&'a str),
}

impl Key<'_> {
    /// The bytes that the terminal gets.
    pub fn bytes(self) -> Vec<u8> {
        match self {
            Key::Text(text) => text.as_bytes().to_vec(),
            Key::Lt => b"<".to_vec(),
            Key::LineEnd(name) | Key::Edit(name) => named_bytes(name).unwrap_or_default(),
        }
    }
}

/// Split `input` into text and keys.
pub fn keys(input: &str) -> Vec<Key<'_>> {
    let mut keys = Vec::new();
    let mut rest = input;

    while let Some(start) = rest.find('<') {
        let name = rest[start + 1..].find('>').map(|end| &rest[start + 1..start + 1 + end]);

        match name.filter(|name| named_bytes(name).is_some()) {
            Some(name) => {
                if start > 0 {
                    keys.push(Key::Text(&rest[..start]));
                }
                keys.push(match name {
                    "lt" => Key::Lt,
                    "Enter" | "C-m" | "C-M" | "C-j" | "C-J" => Key::LineEnd(name),
                    _ => Key::Edit(name),
                });
                rest = &rest[start + name.len() + 2..];
            }
            None => {
                // Not a key: the "<" is text, and the search goes on after it.
                keys.push(Key::Text(&rest[..=start]));
                rest = &rest[start + 1..];
            }
        }
    }

    if !rest.is_empty() {
        keys.push(Key::Text(rest));
    }

    keys
}

fn named_bytes(name: &str) -> Option<Vec<u8>> {
    let bytes: &[u8] = match name {
        "Enter" => b"\r",
        "Tab" => b"\t",
        "Esc" => b"\x1b",
        "BS" => b"\x7f",
        "Up" => b"\x1b[A",
        "Down" => b"\x1b[B",
        "Right" => b"\x1b[C",
        "Left" => b"\x1b[D",
        "lt" => b"<",
        _ => {
            let letter = name.strip_prefix("C-")?;
            let [c] = letter.as_bytes() else {
                return None;
            };
            return c
                .is_ascii_alphabetic()
                .then(|| vec![c.to_ascii_lowercase() - b'a' + 1]);
        }
    };

    Some(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_names_are_text_and_known_names_are_keys() {
        assert_eq!(
            keys("x <z\nrm>y<Enter>"),
            [Key::Text("x <"), Key::Text("z\nrm>y"), Key::LineEnd("Enter")]
        );
        assert_eq!(keys("a<lt>b<C-a>"), [Key::Text("a"), Key::Lt, Key::Text("b"), Key::Edit("C-a")]);
        assert_eq!(keys("<C-m>"), [Key::LineEnd("C-m")]);
    }
}
