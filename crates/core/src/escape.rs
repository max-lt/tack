//! Text from outside, shown to the user and read by the model: control
//! characters and invisible characters become visible escapes.

/// Characters that a terminal does not show but the model reads: a log
/// line can hide text in them. The set is Default_Ignorable_Code_Point and
/// the format characters (category Cf), from Unicode 16, plus the line and
/// paragraph separators and the blank glyphs U+2800, U+3164, U+FFA0,
/// U+115F and U+1160. Ranges are sorted and do not overlap.
const INVISIBLE: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x206F),
    (0x2800, 0x2800),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0000, 0xE0FFF),
];

/// Characters that act on a terminal or that a terminal does not show.
pub fn is_hidden(c: char) -> bool {
    let code = u32::from(c);
    (c.is_control() && c != '\t')
        || INVISIBLE
            .iter()
            .any(|&(first, last)| (first..=last).contains(&code))
}

/// `text` with each hidden character as text: `\xNN` for a control
/// character, `\u{N}` for an invisible one.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());

    for c in text.chars() {
        match c {
            c if c.is_control() && c != '\t' => {
                out.push_str(&format!("\\x{:02x}", u32::from(c)));
            }
            c if is_hidden(c) => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
            c => out.push(c),
        }
    }

    out
}
