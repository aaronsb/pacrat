//! Row selection: one language, spoken per screen (ADR-002).
//!
//! Every screen that selects uses this model and these controls; what a
//! selection *means* stays screen-local. On hosts it is "these packages,
//! this host's manifest"; a future updates selection would mean "these
//! candidates". The contract a screen inherits by using this module:
//!
//! * **State is a name-keyed set.** A reload can reorder rows, and a
//!   selection that silently became a different set of packages is the one
//!   bug a bulk screen must not have. Indices never enter this type.
//! * **Controls are the same everywhere:** `space` toggles the cursor row,
//!   `*` marks all, `-` marks none, `!` inverts. A screen that selects
//!   claims those keys in the keymap and no others.
//! * **A marked row shows both signals:** the background tint
//!   ([`theme::SELECT`]) and a marker glyph in column 0. The tint is the
//!   affordance; the glyph is the one that survives a monochrome terminal —
//!   the same rule the verdict marks follow.
//! * **The detail pane is never the selection's ledger.** The count lives
//!   in the region title ([`Selection::title_suffix`]); the names are what
//!   the apply step shows anyway.

use std::collections::BTreeSet;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::tui::theme;

/// The marker a selected row carries in column 0.
const MARK: &str = "•";

/// The rows a reader has marked, by name.
#[derive(Debug, Default)]
pub struct Selection {
    names: BTreeSet<String>,
}

impl Selection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// The marked names, sorted — a `BTreeSet` walks in order, so the list
    /// a caller shows or acts on is stable however the marks were made.
    pub fn names(&self) -> Vec<String> {
        self.names.iter().cloned().collect()
    }

    /// `space` — mark the row, or unmark it.
    pub fn toggle(&mut self, name: &str) {
        if !self.names.remove(name) {
            self.names.insert(name.to_string());
        }
    }

    /// `*` — mark every row on the screen.
    pub fn all<'a>(&mut self, rows: impl IntoIterator<Item = &'a str>) {
        self.names.extend(rows.into_iter().map(str::to_string));
    }

    /// `-` — mark nothing.
    pub fn none(&mut self) {
        self.names.clear();
    }

    /// `!` — the marked rows and the unmarked ones trade places.
    ///
    /// Built from the rows on screen rather than edited in place, which is
    /// what makes it also the cleanup: a name that no longer has a row —
    /// deleted from the store between reloads, say — cannot be in either
    /// half of the trade, so it drops out here instead of haunting the
    /// count.
    pub fn invert<'a>(&mut self, rows: impl IntoIterator<Item = &'a str>) {
        self.names = rows
            .into_iter()
            .filter(|name| !self.names.contains(*name))
            .map(str::to_string)
            .collect();
    }

    /// A reload happened: keep only the marks that still name a row.
    ///
    /// The set survives reordering untouched — it is keyed by name — but a
    /// row that vanished must take its mark with it, or the title's count
    /// promises packages an apply cannot find.
    pub fn keep<'a>(&mut self, rows: impl IntoIterator<Item = &'a str>) {
        let live: BTreeSet<&str> = rows.into_iter().collect();
        self.names.retain(|name| live.contains(name.as_str()));
    }

    /// What the region title appends: ` · N marked`, or nothing at all.
    ///
    /// The count lives in the title rather than the detail pane because a
    /// count is what the apply step needs a reader to have read, and a list
    /// of names is what apply shows anyway.
    pub fn title_suffix(&self) -> String {
        match self.names.len() {
            0 => String::new(),
            n => format!(" · {n} marked"),
        }
    }
}

/// Dress one row: the marker glyph in column 0, and the tint behind a
/// marked row.
///
/// The glyph column is always present — one space wide on an unmarked row —
/// so marking never shifts the columns to its right. On a marked row every
/// span gets [`theme::SELECT`] behind it; ink that was [`theme::DIM`] is
/// lifted to the default ink first, because dim grey into a grey tint is a
/// cell the reader can no longer read, and a mark must never make a row
/// *less* legible.
pub fn decorate(row: Line<'static>, selected: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(row.spans.len() + 1);
    if !selected {
        spans.push(theme::plain(" "));
        spans.extend(row.spans);
        return Line::from(spans);
    }
    spans.push(Span::styled(
        MARK,
        Style::new().fg(theme::ACCENT).bg(theme::SELECT),
    ));
    for mut span in row.spans {
        if span.style.fg == Some(theme::DIM) {
            span.style.fg = None;
        }
        span.style.bg = Some(theme::SELECT);
        spans.push(span);
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(selection: &Selection) -> Vec<String> {
        selection.names()
    }

    #[test]
    fn toggle_marks_a_row_and_toggling_again_unmarks_it() {
        let mut s = Selection::new();
        assert!(s.is_empty());
        s.toggle("mdcat");
        assert!(s.contains("mdcat"));
        assert_eq!(marked(&s), ["mdcat"]);
        s.toggle("mdcat");
        assert!(!s.contains("mdcat"));
        assert!(s.is_empty());
    }

    #[test]
    fn all_none_and_invert_are_the_whole_screen_operations() {
        let rows = ["a", "b", "c"];
        let mut s = Selection::new();
        s.toggle("b");

        s.all(rows);
        assert_eq!(marked(&s), ["a", "b", "c"]);

        s.none();
        assert!(s.is_empty());

        s.toggle("a");
        s.invert(rows);
        assert_eq!(marked(&s), ["b", "c"]);
        s.invert(rows);
        assert_eq!(marked(&s), ["a"]);
    }

    /// The reason the state is a name-keyed set: a reload can reorder the
    /// rows, and the marks must still mean the same packages — never "the
    /// third row", whatever the third row has become.
    #[test]
    fn a_reload_that_reorders_rows_still_marks_the_same_packages() {
        let mut s = Selection::new();
        s.toggle("ripgrep");
        s.toggle("fd");

        // The list comes back in a different order, with a newcomer.
        let reordered = ["zoxide", "fd", "ripgrep"];
        s.keep(reordered);
        assert_eq!(marked(&s), ["fd", "ripgrep"], "the marks moved packages");
    }

    /// A row that vanished takes its mark with it — through `keep` on a
    /// reload, and through `invert`, which is built from the rows on screen.
    #[test]
    fn a_vanished_row_cannot_stay_marked() {
        let mut s = Selection::new();
        s.toggle("gone");
        s.toggle("here");

        s.keep(["here"]);
        assert_eq!(marked(&s), ["here"]);

        s.toggle("gone-again");
        s.invert(["here", "other"]);
        assert_eq!(marked(&s), ["other"], "invert kept a name with no row");
    }

    #[test]
    fn the_title_suffix_counts_and_is_silent_at_zero() {
        let mut s = Selection::new();
        assert_eq!(s.title_suffix(), "");
        s.toggle("a");
        assert_eq!(s.title_suffix(), " · 1 marked");
        s.toggle("b");
        assert_eq!(s.title_suffix(), " · 2 marked");
    }

    /// Every character a line is actually made of, in order.
    fn text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn row() -> Line<'static> {
        Line::from(vec![
            theme::plain("mdcat  "),
            theme::tinted(theme::OK, "✓"),
            theme::dim("—"),
        ])
    }

    /// The glyph column exists on every row, so marking one never shifts
    /// the columns beside it — and an unmarked row carries no tint at all.
    #[test]
    fn an_unmarked_row_gets_a_blank_gutter_and_no_tint() {
        let line = decorate(row(), false);
        assert_eq!(text(&line), " mdcat  ✓—");
        assert!(
            line.spans.iter().all(|s| s.style.bg.is_none()),
            "an unmarked row was tinted"
        );
    }

    /// Both signals, together: the tint is the affordance, the glyph is the
    /// one that survives a monochrome terminal.
    #[test]
    fn a_marked_row_carries_the_glyph_and_the_tint_on_every_span() {
        let line = decorate(row(), true);
        assert_eq!(text(&line), "•mdcat  ✓—");
        assert!(
            line.spans.iter().all(|s| s.style.bg == Some(theme::SELECT)),
            "a span escaped the tint"
        );
        // The coloured ink survives: the tint sits behind it, not over it.
        assert!(line
            .spans
            .iter()
            .any(|s| s.style.fg == Some(theme::OK) && s.content == "✓"));
    }

    /// Dim ink is lifted on a marked row: dim grey into a grey tint is a
    /// cell the reader can no longer read, and a mark must never make a row
    /// less legible.
    #[test]
    fn dim_ink_is_lifted_out_of_the_tint() {
        let line = decorate(row(), true);
        assert!(
            line.spans.iter().all(|s| s.style.fg != Some(theme::DIM)),
            "dim ink was left to vanish into the tint"
        );
    }
}
