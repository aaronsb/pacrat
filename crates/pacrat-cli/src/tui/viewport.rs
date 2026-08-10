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
use ratatui::style::Style;
use ratatui::symbols;
use ratatui::text::{Line, Text};
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

    /// Move. Gestures are relative to what is on screen now, so they start
    /// from `offset`; where they land becomes the new request.
    pub fn apply(&mut self, scroll: Scroll) {
        let target = match scroll {
            Scroll::LineUp => self.offset.saturating_sub(1),
            Scroll::LineDown => self.offset.saturating_add(1),
            Scroll::HalfPageUp => self.offset.saturating_sub(self.half_page()),
            Scroll::HalfPageDown => self.offset.saturating_add(self.half_page()),
            Scroll::PageUp => self.offset.saturating_sub(self.page()),
            Scroll::PageDown => self.offset.saturating_add(self.page()),
            Scroll::Top => 0,
            Scroll::Bottom => self.max_offset(),
        };
        self.desired = target.min(self.max_offset());
        self.offset = self.desired;
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
pub struct Region {
    title: String,
    lines: Vec<Line<'static>>,
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
        self.lines = lines;
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

    pub fn scroll(&mut self, scroll: Scroll) {
        self.view.apply(scroll);
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
        self.view.fit(self.lines.len(), body.height as usize);
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
        frame.render_widget(
            Paragraph::new(Text::from(self.lines.clone())).scroll((self.view.offset() as u16, 0)),
            text,
        );
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
