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
}
