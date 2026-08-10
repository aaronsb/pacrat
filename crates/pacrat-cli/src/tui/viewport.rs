//! The scrolling region — the one component every screen is built from.
//!
//! ADR-001's Surfaces section and mockup §2 both make the same demand, and
//! it is the reason this module exists before any screen does: *every* region
//! is a viewport from day one. With 200+ tracked packages and multi-hundred
//! line PKGBUILD diffs, a pane that assumes its content fits is a pane that
//! will silently hide the line that mattered. Writing the scroll math once,
//! here, is what keeps that promise cheap enough to keep everywhere — a
//! screen author gets j/k, half-page, page, g/G, a scrollbar and a position
//! indicator by putting their lines in a `Region`.
//!
//! The math is deliberately separated from the drawing: `Viewport` is pure
//! arithmetic over (offset, len, height) and is unit-tested, because a TUI
//! cannot be tested by looking at it in CI.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;

use super::theme;

/// One scroll gesture. The vocabulary is vim's because the mockup's is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    LineUp,
    LineDown,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// A gesture resolved against a geometry: how far, which way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Up(usize),
    Down(usize),
    Top,
    Bottom,
}

/// Where a region is looking, and how much there is to look at.
///
/// `len` and `height` are set at render time by [`Viewport::fit`] — the
/// height is not knowable until the layout has run, and the content length
/// changes under us when a screen reloads. Everything else clamps against
/// whatever those were last told to be.
///
/// **Two offsets, because "where the reader is" and "where that lands" are
/// different facts.** `desired` is the last position a *gesture* asked for;
/// `offset` is that position clamped to the content there actually is. A
/// reload replaces a long list with a one-line "refreshing…" and then puts
/// the list back, and with a single offset that round trip silently drags
/// the reader to the top — clamping is not reversible, so the position is
/// gone by the time the real lines arrive. Keeping the request separate
/// makes the clamp a *view* of the request rather than a destruction of it,
/// so content that shrinks and grows again returns the reader where they
/// were. Any gesture overwrites `desired`, so this never fights the user:
/// scrolling while the content is short means the short content is what
/// they were scrolling.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    desired: usize,
    offset: usize,
    len: usize,
    height: usize,
}

impl Viewport {
    /// Adopt this frame's geometry, and re-derive the offset from what the
    /// reader last asked for.
    ///
    /// Clamping here rather than at each gesture is what makes a shrinking
    /// terminal — or a reload that returns fewer lines — safe: the region
    /// slides back into view instead of showing blank space below the end.
    pub fn fit(&mut self, len: usize, height: usize) {
        self.len = len;
        self.height = height;
        self.offset = self.desired.min(self.max_offset());
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    /// The furthest the top line can go: the last full screen of content.
    pub fn max_offset(&self) -> usize {
        self.len.saturating_sub(self.height)
    }

    pub fn overflows(&self) -> bool {
        self.len > self.height
    }

    /// Lines moved by `ctrl-d`/`ctrl-u`. At least one, so a two-row pane
    /// still moves.
    fn half_page(&self) -> usize {
        (self.height / 2).max(1)
    }

    /// Lines moved by `ctrl-f`/`ctrl-b`: a screenful less one line of
    /// overlap, so the reader keeps a foothold across the jump.
    fn page(&self) -> usize {
        self.height.saturating_sub(1).max(1)
    }

    /// What a gesture asks for, in rows, independent of what it is applied
    /// to. The same numbers move an offset and move a cursor — a screen where
    /// `ctrl-d` scrolls half a pane in one region and jumps some other
    /// distance in the list beside it is a screen that has two half-pages.
    pub fn amount(&self, scroll: Scroll) -> Move {
        match scroll {
            Scroll::LineUp => Move::Up(1),
            Scroll::LineDown => Move::Down(1),
            Scroll::HalfPageUp => Move::Up(self.half_page()),
            Scroll::HalfPageDown => Move::Down(self.half_page()),
            Scroll::PageUp => Move::Up(self.page()),
            Scroll::PageDown => Move::Down(self.page()),
            Scroll::Top => Move::Top,
            Scroll::Bottom => Move::Bottom,
        }
    }

    /// Move. Gestures are relative to what is on screen now, so they start
    /// from `offset`; where they land becomes the new request.
    pub fn apply(&mut self, scroll: Scroll) {
        let target = match self.amount(scroll) {
            Move::Up(n) => self.offset.saturating_sub(n),
            Move::Down(n) => self.offset.saturating_add(n),
            Move::Top => 0,
            Move::Bottom => self.max_offset(),
        };
        self.desired = target.min(self.max_offset());
        self.offset = self.desired;
    }

    /// Slide the window the least it can to put `cursor` on screen.
    ///
    /// The *least* is the point. A cursor region that recentred on every
    /// move would repaint the whole pane for a one-row step, and the reader
    /// would lose the rows they were using as landmarks; scrolling only when
    /// the cursor would otherwise leave keeps `j` at the bottom edge behaving
    /// like `j` in a pager.
    ///
    /// It writes `desired` rather than only `offset`, so a list that shrinks
    /// and grows again — the reload round trip — brings the cursor's window
    /// back rather than the pre-reload one.
    pub fn follow(&mut self, cursor: usize) {
        if self.height == 0 || self.len == 0 {
            return;
        }
        let cursor = cursor.min(self.len - 1);
        if cursor < self.offset {
            self.desired = cursor;
        } else if cursor >= self.offset + self.height {
            self.desired = cursor + 1 - self.height;
        }
        self.offset = self.desired.min(self.max_offset());
    }

    /// The `12/214` indicator, or `None` when there is nothing useful to
    /// say: everything already fits, or the region has been squeezed to no
    /// body at all. The second case is why the height is checked — a
    /// collapsed region reported `0/5`, which reads as a bug rather than as
    /// "there is no room to show you any of this".
    ///
    /// The first number is the *last line on screen*, not the first: read as
    /// "you have seen this much of that much", it goes to `len` exactly when
    /// there is nothing further down, which is the question a reader asks a
    /// position indicator.
    pub fn progress(&self) -> Option<(usize, usize)> {
        (self.overflows() && self.height > 0)
            .then(|| ((self.offset + self.height).min(self.len), self.len))
    }
}

/// A titled band of lines that scrolls: title rule, body, scrollbar.
///
/// Two optional shapes on top of that, both of which the mockup's list
/// screens need and neither of which a screen should have to build for
/// itself: a **cursor**, so `j`/`k` pick a row rather than only slide the
/// window, and a **pinned header**, so a table's column names stay put while
/// its rows scroll under them. A region with a cursor also owns a two-column
/// gutter for the `▸` marker, header rows included, so every line in the
/// region stays in the same columns whether or not it is selected.
pub struct Region {
    title: String,
    /// Header rows first, then body rows. One vector because they are one
    /// table, and the split is a count rather than a second field so a screen
    /// cannot hand back a header and rows that disagree about their widths.
    lines: Vec<Line<'static>>,
    /// How many leading lines are pinned. Never scrolled, never selectable.
    frozen: usize,
    /// The selected row, as an index into the *scrollable* part. `None` for
    /// a region that is prose rather than a list.
    cursor: Option<usize>,
    /// This region's share of its screen's vertical space, counting **body
    /// rows only** — the title rule is structural and [`Panes`] reserves it.
    height: Constraint,
    view: Viewport,
}

impl Region {
    pub fn new(title: impl Into<String>, height: Constraint, lines: Vec<Line<'static>>) -> Self {
        Self {
            title: title.into(),
            lines,
            frozen: 0,
            cursor: None,
            height,
            view: Viewport::default(),
        }
    }

    /// A region whose rows are chosen rather than only read: `j`/`k` move a
    /// cursor, `enter` acts on it. `header` is pinned above the rows.
    pub fn table(
        title: impl Into<String>,
        height: Constraint,
        header: Vec<Line<'static>>,
        rows: Vec<Line<'static>>,
    ) -> Self {
        let frozen = header.len();
        let mut lines = header;
        lines.extend(rows);
        Self {
            title: title.into(),
            lines,
            frozen,
            cursor: Some(0),
            height,
            view: Viewport::default(),
        }
    }

    /// Replace the content, keeping where the reader was looking.
    ///
    /// The promise survives an intermediate state: a reload that shows a
    /// one-line "refreshing…" and then puts a hundred lines back returns the
    /// reader to where they were, because [`Viewport`] remembers the
    /// position they asked for separately from the one the content allowed.
    pub fn set_lines(&mut self, lines: Vec<Line<'static>>) {
        self.frozen = 0;
        self.lines = lines;
    }

    /// Replace a table's rows, leaving its header and its cursor alone.
    ///
    /// The cursor survives by *index*, not by identity, and that is the
    /// honest thing rather than a shortcut: a reload can return a different
    /// set of packages, and a cursor that chased a name would silently jump
    /// the reader to a row somewhere else in the list. Clamped, so a list
    /// that shrank leaves the cursor on the last real row.
    pub fn set_rows(&mut self, rows: Vec<Line<'static>>) {
        self.lines.truncate(self.frozen);
        self.lines.extend(rows);
        if let Some(cursor) = self.cursor {
            self.cursor = Some(cursor.min(self.rows().saturating_sub(1)));
        }
    }

    /// How many selectable rows this region holds.
    pub fn rows(&self) -> usize {
        self.lines.len() - self.frozen
    }

    /// The selected row, or `None` for a region with no cursor or no rows.
    pub fn cursor(&self) -> Option<usize> {
        self.cursor.filter(|_| self.rows() > 0)
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    /// Re-declare how much vertical space to ask for. A screen whose content
    /// size is only known after a load (the overview's attention list) sizes
    /// itself here rather than guessing at construction.
    pub fn set_height(&mut self, height: Constraint) {
        self.height = height;
    }

    /// One gesture. On a list it moves the cursor and the window follows; on
    /// prose it moves the window.
    ///
    /// Both readings use [`Viewport::amount`], so `ctrl-d` is the same
    /// half-page in either — a screen whose list and whose diff pane
    /// disagreed about how far a half-page is would be a screen with two
    /// half-pages.
    pub fn scroll(&mut self, scroll: Scroll) {
        let Some(cursor) = self.cursor else {
            self.view.apply(scroll);
            return;
        };
        let last = self.rows().saturating_sub(1);
        self.cursor = Some(match self.view.amount(scroll) {
            Move::Up(n) => cursor.saturating_sub(n),
            Move::Down(n) => cursor.saturating_add(n).min(last),
            Move::Top => 0,
            Move::Bottom => last,
        });
    }

    /// Draw the region.
    ///
    /// `outer` is the whole app frame, not this region's slot: the title
    /// rule is drawn across the full width so its ends land *on* the frame's
    /// side borders, which is what turns two stacked boxes into the mockup's
    /// `├─ attention ────┤` seam.
    pub fn render(&mut self, frame: &mut Frame, outer: Rect, slot: Rect, focused: bool) {
        if slot.height == 0 {
            return;
        }

        let style = Style::new().fg(if focused { theme::ACCENT } else { theme::DIM });
        // LEFT and RIGHT are in the set only for their *corners*: a corner
        // glyph is drawn where two borders meet, and this block is one row
        // tall, so the sides contribute the two tees and no verticals.
        let mut rule = Block::new()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_set(SEAM)
            .border_style(style)
            .title_top(Line::styled(format!("─ {} ", self.title), style));

        let body = Rect {
            x: slot.x,
            y: slot.y + 1,
            width: slot.width,
            height: slot.height - 1,
        };
        // A pinned header is paid for out of the body before the viewport is
        // told how tall it is: a header that scrolled would not be pinned,
        // and one the viewport counted as content would make `12/214` a
        // number about a different set of rows than the reader is counting.
        let frozen = self.frozen.min(body.height as usize);
        self.view.fit(self.rows(), body.height as usize - frozen);
        if let Some(cursor) = self.cursor {
            self.view.follow(cursor);
        }
        if let Some((seen, total)) = self.view.progress() {
            rule =
                rule.title_top(Line::styled(format!("─ {seen}/{total} ─"), style).right_aligned());
        }
        frame.render_widget(
            rule,
            Rect {
                x: outer.x,
                y: slot.y,
                width: outer.width,
                height: 1,
            },
        );
        if body.height == 0 {
            return;
        }

        // The scrollbar takes a column of the body, never of the frame: the
        // frame's right border belongs to the app, not to whichever region
        // happens to be long today.
        let (text, bar) = match self.view.overflows() && body.width > 1 {
            true => {
                let [text, bar] =
                    Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).areas(body);
                (text, Some(bar))
            }
            false => (body, None),
        };

        // No wrapping, on purpose: the viewport's arithmetic counts content
        // lines, and a wrapped line is a different number of rows than the
        // line it came from. Over-wide content is clipped for now; a
        // horizontal viewport is the same component's next job.
        //
        // Sanitizing untrusted text is *borrowed* here, not done. These
        // lines carry package names, store paths and grader output, and what
        // keeps an `ESC[8m` in one of them from hiding a row is ratatui's
        // own buffer: `Buffer::set_stringn` drops graphemes containing
        // control characters and graphemes of zero width
        // (ratatui-core-0.1.2, src/buffer/buffer.rs:351-353). That is an
        // implementation detail of a dependency, not a contract pacrat is
        // owed — so anything that ever writes to the backend directly, or
        // measures these strings before they reach the buffer, has to run
        // them through `out::visible` first, the way the CLI verbs do.
        //
        // Only the rows on screen are built, rather than the whole vector
        // scrolled by the paragraph. A review pane holds a thousand-line
        // diff and this runs every frame — cloning all of it to show
        // twenty of it is the one cost in here that grows with the data.
        frame.render_widget(Paragraph::new(Text::from(self.visible(text.height))), text);
        if let Some(bar) = bar {
            // `new(max_offset)`, not `new(len)`: the argument is the number
            // of scroll *positions*, not the number of lines. With `len` the
            // thumb reaches the bottom of its track only when the last line
            // scrolls off the top — i.e. never — so a fully scrolled region
            // shows a thumb hovering short of the end.
            let mut state =
                ScrollbarState::new(self.view.max_offset()).position(self.view.offset());
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None)
                    .thumb_style(Style::new().fg(if focused { theme::ACCENT } else { theme::DIM }))
                    .track_style(Style::new().fg(theme::DIM)),
                bar,
                &mut state,
            );
        }
    }
}

impl Region {
    /// The rows this frame actually draws: the pinned header, then the
    /// window, each with the cursor gutter a list region owns.
    fn visible(&self, height: u16) -> Vec<Line<'static>> {
        let height = height as usize;
        let frozen = self.frozen.min(height);
        let offset = self.view.offset();
        let rows = height - frozen;

        let mut out = Vec::with_capacity(height);
        out.extend(
            self.lines[..frozen]
                .iter()
                .map(|line| self.gutter(line, "")),
        );
        for (index, line) in self.lines[self.frozen..]
            .iter()
            .skip(offset)
            .take(rows)
            .enumerate()
        {
            let selected = self.cursor == Some(offset + index);
            let mut line = self.gutter(line, if selected { "▸ " } else { "" });
            if selected {
                // Reversed rather than a background colour: pacrat draws in
                // somebody else's palette, and every colour dark enough to
                // read `theme::DIM` against is a colour some terminal has
                // already spent on something. Reversing whatever the row
                // already is cannot collide with a scheme it never saw.
                line = line.patch_style(Style::new().add_modifier(Modifier::REVERSED));
            }
            out.push(line);
        }
        out
    }

    /// Prepend the two-column cursor gutter, for a region that has one.
    fn gutter(&self, line: &Line<'static>, marker: &str) -> Line<'static> {
        if self.cursor.is_none() {
            return line.clone();
        }
        let mut spans = Vec::with_capacity(line.spans.len() + 1);
        spans.push(match marker.is_empty() {
            true => Span::raw("  "),
            false => theme::accent(marker.to_string()),
        });
        spans.extend(line.spans.iter().cloned());
        Line::from(spans)
    }
}

/// The seam: a top border whose ends are tees, so a region's rule joins the
/// enclosing frame instead of colliding with it.
const SEAM: symbols::border::Set = symbols::border::Set {
    top_left: "├",
    top_right: "┤",
    ..symbols::border::PLAIN
};

/// A screen: its regions, and which one has focus.
///
/// Focus is the screen's, not the app's, so leaving a screen and coming back
/// returns the reader to the region they were reading.
pub struct Panes {
    regions: Vec<Region>,
    focus: usize,
}

impl Panes {
    pub fn new(regions: Vec<Region>) -> Self {
        Self { regions, focus: 0 }
    }

    pub fn region_mut(&mut self, index: usize) -> Option<&mut Region> {
        self.regions.get_mut(index)
    }

    pub fn region(&self, index: usize) -> Option<&Region> {
        self.regions.get(index)
    }

    /// Put focus on a region by index, for a screen that has just given the
    /// reader something new to look at. Out-of-range is ignored rather than
    /// clamped: a screen asking for a region it does not have is a bug in the
    /// screen, and moving focus somewhere arbitrary would hide it.
    pub fn focus_on(&mut self, index: usize) {
        if index < self.regions.len() {
            self.focus = index;
        }
    }

    /// `(focused, total)` for the status line — 1-based, for reading.
    pub fn focus_position(&self) -> (usize, usize) {
        (self.focus + 1, self.regions.len())
    }

    pub fn cycle(&mut self, forward: bool) {
        if self.regions.is_empty() {
            return;
        }
        let n = self.regions.len();
        self.focus = match forward {
            true => (self.focus + 1) % n,
            false => (self.focus + n - 1) % n,
        };
    }

    pub fn scroll(&mut self, scroll: Scroll) {
        if let Some(region) = self.regions.get_mut(self.focus) {
            region.scroll(scroll);
        }
    }

    pub fn render(&mut self, frame: &mut Frame, outer: Rect, inner: Rect) {
        if self.regions.is_empty() {
            return;
        }
        let slots = self.slots(inner);
        for (index, region) in self.regions.iter_mut().enumerate() {
            region.render(frame, outer, slots[index], index == self.focus);
        }
    }

    /// Divide the screen, giving every region its title rule before anyone
    /// gets a body row.
    ///
    /// The reserve is the whole point. Handing the declared constraints
    /// straight to the solver means that when the terminal is short *some
    /// region gets zero rows and vanishes* — not shrinks, vanishes, with no
    /// rule and no hint that it was ever there. Which one disappears is
    /// decided by the solver's priority order rather than by anything about
    /// the screen: `Min` outranks `Length`, so the first version of this
    /// code answered a squeezed overview by deleting the header and the
    /// attention list, the two regions a cramped screen most needs.
    ///
    /// ratatui has no single constraint meaning "one row, then a share of
    /// what is left", so the floor is taken out first and only the surplus
    /// is solved for. What a short terminal costs is then rows, everywhere,
    /// and never a region.
    fn slots(&self, inner: Rect) -> Vec<Rect> {
        let rules = self.regions.len() as u16;
        let row = |y: u16, height: u16| Rect {
            x: inner.x,
            y,
            width: inner.width,
            height,
        };
        // Below one row each there is nothing left to be fair with: the
        // terminal is shorter than this screen's list of titles.
        if inner.height <= rules {
            return (0..self.regions.len())
                .map(|index| {
                    let index = index as u16;
                    row(
                        inner.y + index.min(inner.height),
                        u16::from(index < inner.height),
                    )
                })
                .collect();
        }
        let constraints: Vec<Constraint> =
            self.regions.iter().map(|region| region.height).collect();
        let bodies = Layout::vertical(constraints).split(row(inner.y, inner.height - rules));
        let mut y = inner.y;
        bodies
            .iter()
            .map(|body| {
                let slot = row(y, body.height + 1);
                y += slot.height;
                slot
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 100 lines in a 10-row pane.
    fn tall() -> Viewport {
        let mut v = Viewport::default();
        v.fit(100, 10);
        v
    }

    #[test]
    fn a_fitting_region_neither_scrolls_nor_advertises_a_position() {
        let mut v = Viewport::default();
        v.fit(4, 10);
        assert!(!v.overflows());
        assert_eq!(v.progress(), None);
        assert_eq!(v.max_offset(), 0);
        for scroll in [Scroll::LineDown, Scroll::PageDown, Scroll::Bottom] {
            v.apply(scroll);
            assert_eq!(
                v.offset(),
                0,
                "{scroll:?} moved a region with nothing below"
            );
        }
    }

    #[test]
    fn the_gestures_move_by_the_documented_amounts() {
        let mut v = tall();
        v.apply(Scroll::LineDown);
        assert_eq!(v.offset(), 1);
        v.apply(Scroll::HalfPageDown);
        assert_eq!(v.offset(), 6, "half page is height/2");
        v.apply(Scroll::PageDown);
        assert_eq!(v.offset(), 15, "a page keeps one line of overlap");
        v.apply(Scroll::PageUp);
        assert_eq!(v.offset(), 6);
        v.apply(Scroll::HalfPageUp);
        assert_eq!(v.offset(), 1);
        v.apply(Scroll::LineUp);
        assert_eq!(v.offset(), 0);
    }

    #[test]
    fn scrolling_stops_at_both_ends() {
        let mut v = tall();
        for _ in 0..50 {
            v.apply(Scroll::PageDown);
        }
        assert_eq!(v.offset(), 90, "the last screenful is the end of the road");
        assert_eq!(v.progress(), Some((100, 100)));
        for _ in 0..50 {
            v.apply(Scroll::PageUp);
        }
        assert_eq!(v.offset(), 0);
        v.apply(Scroll::Bottom);
        assert_eq!(v.offset(), v.max_offset());
        v.apply(Scroll::Top);
        assert_eq!(v.offset(), 0);
    }

    /// The reload case: the offset must come back into range on its own, or
    /// a shorter list leaves the reader staring past its end.
    #[test]
    fn content_that_shrinks_pulls_the_offset_back() {
        let mut v = tall();
        v.apply(Scroll::Bottom);
        assert_eq!(v.offset(), 90);
        v.fit(12, 10);
        assert_eq!(v.offset(), 2);
        v.fit(3, 10);
        assert_eq!(v.offset(), 0);
    }

    /// A terminal squeezed to nothing must not divide by zero or wedge the
    /// offset somewhere it can never come back from.
    #[test]
    fn a_zero_height_pane_is_survivable() {
        let mut v = Viewport::default();
        v.fit(100, 0);
        assert_eq!(v.max_offset(), 100);
        v.apply(Scroll::HalfPageDown);
        assert_eq!(
            v.offset(),
            1,
            "one line, because half of nothing is not zero"
        );
        v.apply(Scroll::PageDown);
        assert_eq!(v.offset(), 2);
        // …and it says nothing about position, because `0/100` in a region
        // with no body reads as a bug rather than as "there is no room".
        assert_eq!(v.progress(), None);
        v.fit(100, 200);
        assert_eq!(v.offset(), 0);
    }

    /// The reload path, and the reason the request and the clamp are stored
    /// separately: `r` swaps a hundred lines for one line of "refreshing…"
    /// and then swaps them back, and the reader must not be dragged to the
    /// top by the round trip.
    #[test]
    fn a_transient_shrink_does_not_cost_the_reader_their_place() {
        let mut v = tall();
        v.apply(Scroll::Bottom);
        assert_eq!(v.offset(), 90);
        v.fit(1, 10); // "refreshing…"
        assert_eq!(v.offset(), 0, "there is nowhere else to be at one line");
        v.fit(100, 10); // the real lines come back
        assert_eq!(v.offset(), 90, "and the reader is back where they were");
    }

    /// The other half of that bargain: a gesture is the reader speaking, so
    /// it replaces the remembered position instead of being overridden by
    /// it. Scrolling while the content is short means the short content is
    /// what they meant to scroll.
    #[test]
    fn a_gesture_while_shrunk_replaces_the_remembered_place() {
        let mut v = tall();
        v.apply(Scroll::Bottom);
        v.fit(12, 10);
        v.apply(Scroll::Top);
        v.fit(100, 10);
        assert_eq!(v.offset(), 0, "the last thing the reader asked for wins");
    }

    #[test]
    fn the_indicator_counts_the_last_visible_line() {
        let mut v = tall();
        assert_eq!(v.progress(), Some((10, 100)), "ten lines seen of a hundred");
        v.apply(Scroll::HalfPageDown);
        assert_eq!(v.progress(), Some((15, 100)));
        v.apply(Scroll::Bottom);
        assert_eq!(v.progress(), Some((100, 100)), "the end reads as the end");
    }

    /// A list scrolls only when the cursor would otherwise leave, so `j` at
    /// the bottom edge behaves like `j` in a pager rather than recentring.
    #[test]
    fn the_window_follows_the_cursor_by_the_least_it_can() {
        let mut v = tall(); // 100 lines, 10 rows
        v.follow(5);
        assert_eq!(v.offset(), 0, "a cursor already on screen moves nothing");
        v.follow(9);
        assert_eq!(v.offset(), 0, "the last visible row is still visible");
        v.follow(10);
        assert_eq!(v.offset(), 1, "one row past the edge scrolls one row");
        v.follow(0);
        assert_eq!(v.offset(), 0, "and back the same way");
        v.follow(99);
        assert_eq!(v.offset(), 90);
    }

    /// The cursor is an index, and a list that shrank under it must not leave
    /// it pointing past the end.
    #[test]
    fn following_a_cursor_past_the_end_lands_on_the_last_row() {
        let mut v = Viewport::default();
        v.fit(3, 10);
        v.follow(99);
        assert_eq!(v.offset(), 0, "there is nowhere to scroll a short list");
        // A pane squeezed to nothing has no row to put the cursor on, and
        // must not divide by zero looking for one.
        let mut none = Viewport::default();
        none.fit(100, 0);
        none.follow(50);
        assert_eq!(none.offset(), 0);
    }

    fn table(rows: usize) -> Region {
        Region::table(
            "t",
            Constraint::Fill(1),
            vec![Line::raw("package  state")],
            (0..rows).map(|i| Line::raw(format!("row{i}"))).collect(),
        )
    }

    /// `j`/`k` on a list pick a row; the same keys on prose slide the window.
    /// One vocabulary, two readings, and the distances agree.
    #[test]
    fn a_gesture_moves_the_cursor_on_a_list_and_the_window_on_prose() {
        let mut list = table(100);
        assert_eq!(list.cursor(), Some(0));
        list.scroll(Scroll::LineDown);
        assert_eq!(list.cursor(), Some(1));
        list.scroll(Scroll::Bottom);
        assert_eq!(list.cursor(), Some(99), "the last row, not the last window");
        list.scroll(Scroll::LineDown);
        assert_eq!(list.cursor(), Some(99), "and it stops there");
        list.scroll(Scroll::Top);
        assert_eq!(list.cursor(), Some(0));

        let mut prose = Region::new("p", Constraint::Fill(1), vec![Line::raw("x"); 100]);
        assert_eq!(prose.cursor(), None);
        prose.scroll(Scroll::LineDown);
        assert_eq!(prose.cursor(), None, "prose never grows a cursor");
    }

    /// The header is not a row: it cannot be selected, and it is not in the
    /// count the position indicator reports.
    #[test]
    fn a_pinned_header_is_not_one_of_the_rows() {
        let list = table(50);
        assert_eq!(list.rows(), 50);
        assert_eq!(list.cursor(), Some(0), "the cursor starts on a real row");
        // An empty table has a header and nothing to select.
        let empty = Region::table(
            "t",
            Constraint::Fill(1),
            vec![Line::raw("package")],
            Vec::new(),
        );
        assert_eq!(empty.rows(), 0);
        assert_eq!(empty.cursor(), None, "there is no row to be on");
    }

    /// The reload path for a list: the header and the reader's row survive
    /// new data, and a list that shrank pulls the cursor back onto a real row
    /// rather than leaving it pointing past the end.
    #[test]
    fn replacing_rows_keeps_the_header_and_clamps_the_cursor() {
        let mut list = table(50);
        list.scroll(Scroll::Bottom);
        assert_eq!(list.cursor(), Some(49));
        list.set_rows((0..5).map(|i| Line::raw(format!("r{i}"))).collect());
        assert_eq!(list.rows(), 5);
        assert_eq!(list.cursor(), Some(4), "clamped onto the last real row");
        list.set_rows(Vec::new());
        assert_eq!(list.cursor(), None);
        // And the header is still exactly one line, not five copies of one.
        assert_eq!(list.lines.len(), 1);
    }

    /// Only the rows on screen are built. The guard is against a review pane
    /// cloning a thousand-line diff sixty times a second.
    #[test]
    fn rendering_builds_a_window_and_not_the_whole_list() {
        let mut list = table(1000);
        list.view.fit(1000, 20);
        assert_eq!(list.visible(21).len(), 21, "one header plus twenty rows");
        // Prose is windowed the same way, with no gutter and no header.
        let mut prose = Region::new("p", Constraint::Fill(1), vec![Line::raw("x"); 1000]);
        prose.view.fit(1000, 20);
        assert_eq!(prose.visible(20).len(), 20);
        // A body with no room asks for nothing rather than panicking on the
        // slice.
        assert!(prose.visible(0).is_empty());
    }

    #[test]
    fn focus_cycles_both_ways_and_wraps() {
        let region = || Region::new("r", Constraint::Min(1), Vec::new());
        let mut panes = Panes::new(vec![region(), region(), region()]);
        assert_eq!(panes.focus_position(), (1, 3));
        panes.cycle(true);
        assert_eq!(panes.focus_position(), (2, 3));
        panes.cycle(false);
        panes.cycle(false);
        assert_eq!(
            panes.focus_position(),
            (3, 3),
            "backwards from the first wraps"
        );
        panes.cycle(true);
        assert_eq!(panes.focus_position(), (1, 3));
    }

    /// A screen whose regions have not been built yet must not panic when
    /// the reader presses tab at it.
    #[test]
    fn an_empty_screen_absorbs_focus_and_scroll() {
        let mut panes = Panes::new(Vec::new());
        panes.cycle(true);
        panes.scroll(Scroll::Bottom);
        assert_eq!(panes.focus_position(), (1, 0));
    }

    fn four_regions() -> Panes {
        Panes::new(vec![
            Region::new("header", Constraint::Length(4), Vec::new()),
            Region::new("attention", Constraint::Length(3), Vec::new()),
            Region::new("list", Constraint::Fill(4), Vec::new()),
            Region::new("list", Constraint::Fill(5), Vec::new()),
        ])
    }

    fn inner(height: u16) -> Rect {
        Rect {
            x: 1,
            y: 1,
            width: 80,
            height,
        }
    }

    /// The floor: whatever the terminal does, every region keeps the row its
    /// title rule is drawn on, and the slots tile the area exactly with no
    /// gap and no overlap.
    #[test]
    fn no_region_is_ever_squeezed_out_of_existence() {
        let panes = four_regions();
        for height in 4..40u16 {
            let slots = panes.slots(inner(height));
            assert_eq!(slots.len(), 4);
            for (index, slot) in slots.iter().enumerate() {
                assert!(
                    slot.height >= 1,
                    "region {index} vanished at inner height {height}"
                );
            }
            assert_eq!(
                slots.iter().map(|s| s.height).sum::<u16>(),
                height,
                "the slots do not add up at inner height {height}"
            );
            for pair in slots.windows(2) {
                assert_eq!(
                    pair[0].y + pair[0].height,
                    pair[1].y,
                    "a gap or an overlap at inner height {height}"
                );
            }
        }
    }

    /// The declared constraints still govern once every rule is paid for: a
    /// tall screen gives the header exactly its four lines and lets the two
    /// lists divide what is left 4:5.
    #[test]
    fn the_surplus_is_shared_by_the_declared_weights() {
        let slots = four_regions().slots(inner(40));
        let bodies: Vec<u16> = slots.iter().map(|slot| slot.height - 1).collect();
        assert_eq!(bodies[0], 4, "a Length region gets exactly what it asks");
        assert_eq!(bodies[1], 3);
        let (short, long) = (bodies[2], bodies[3]);
        assert_eq!(short + long, 40 - 4 - 4 - 3, "the rest goes to the lists");
        assert!(long > short, "the heavier weight took the bigger share");
    }

    /// Past the floor there is nothing left to be fair with, and the only
    /// requirement is that it does not panic or draw outside the area.
    #[test]
    fn a_screen_shorter_than_its_own_titles_stays_inside_the_area() {
        for height in 0..=4u16 {
            let slots = four_regions().slots(inner(height));
            assert_eq!(slots.len(), 4);
            assert_eq!(slots.iter().map(|s| s.height).sum::<u16>(), height);
            for slot in &slots {
                assert!(
                    slot.y + slot.height <= inner(height).y + height,
                    "a slot ran past the bottom of the area at height {height}"
                );
            }
        }
    }
}
