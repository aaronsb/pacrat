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
