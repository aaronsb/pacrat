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
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    offset: usize,
    len: usize,
    height: usize,
}

impl Viewport {
    /// Adopt this frame's geometry, keeping the offset in range.
    ///
    /// Clamping here rather than at each gesture is what makes a shrinking
    /// terminal — or a reload that returns fewer lines — safe: the region
    /// slides back into view instead of showing blank space below the end.
    pub fn fit(&mut self, len: usize, height: usize) {
        self.len = len;
        self.height = height;
        self.offset = self.offset.min(self.max_offset());
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

    pub fn apply(&mut self, scroll: Scroll) {
        let offset = match scroll {
            Scroll::LineUp => self.offset.saturating_sub(1),
            Scroll::LineDown => self.offset.saturating_add(1),
            Scroll::HalfPageUp => self.offset.saturating_sub(self.half_page()),
            Scroll::HalfPageDown => self.offset.saturating_add(self.half_page()),
            Scroll::PageUp => self.offset.saturating_sub(self.page()),
            Scroll::PageDown => self.offset.saturating_add(self.page()),
            Scroll::Top => 0,
            Scroll::Bottom => self.max_offset(),
        };
        self.offset = offset.min(self.max_offset());
    }

    /// The `12/214` indicator, or `None` when everything already fits and
    /// the number would only be noise.
    ///
    /// The first number is the *last line on screen*, not the first: read as
    /// "you have seen this much of that much", it goes to `len` exactly when
    /// there is nothing further down, which is the question a reader asks a
    /// position indicator.
    pub fn progress(&self) -> Option<(usize, usize)> {
        self.overflows()
            .then(|| ((self.offset + self.height).min(self.len), self.len))
    }
}

/// A titled band of lines that scrolls: title rule, body, scrollbar.
pub struct Region {
    title: String,
    lines: Vec<Line<'static>>,
    /// This region's share of its screen's vertical space.
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

    /// Replace the content, keeping where the reader was looking. A reload
    /// that jumped every region back to the top would punish the `r` key.
    pub fn set_lines(&mut self, lines: Vec<Line<'static>>) {
        self.lines = lines;
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
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
        frame.render_widget(
            Paragraph::new(Text::from(self.lines.clone())).scroll((self.view.offset() as u16, 0)),
            text,
        );
        if let Some(bar) = bar {
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
        let constraints: Vec<Constraint> = self.regions.iter().map(|r| r.height).collect();
        let slots = Layout::vertical(constraints).split(inner);
        for (index, region) in self.regions.iter_mut().enumerate() {
            region.render(frame, outer, slots[index], index == self.focus);
        }
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
        v.fit(100, 200);
        assert_eq!(v.offset(), 0);
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
}
