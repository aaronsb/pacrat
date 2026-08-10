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
