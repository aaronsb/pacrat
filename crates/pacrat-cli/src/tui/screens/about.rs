//! `[7] about` — who cooked this, and out of what?
//!
//! The simplest screen in the set, deliberately: it reads no store, runs no
//! command, and asks the system nothing. The art is compiled in
//! ([`crate::art`], whose source of truth is `art/petit-chef.html`) and the
//! facts beside it were baked in by cargo. The only environment it consults
//! is NO_COLOR and COLORTERM, because colour depth is the one thing the art
//! cannot decide for itself.

use ratatui::layout::Constraint;
use ratatui::text::Line;

use crate::art;
use crate::tui::theme;
use crate::tui::viewport::{Panes, Region};

use super::{field, note};

/// Left margin for the art: centred on an 80-column terminal. The load
/// cannot see the real width — regions get their geometry at render time —
/// and a fixed margin that is roughly right beats a guess re-derived every
/// frame. Narrower terminals clip the tail first, which he can spare.
const MARGIN: usize = 20;

pub struct About {
    pub panes: Panes,
    loaded: bool,
}

impl About {
    pub fn new() -> Self {
        Self {
            panes: Panes::new(vec![Region::new(
                "the petit chef",
                Constraint::Fill(1),
                Vec::new(),
            )]),
            loaded: false,
        }
    }

    pub fn needs_load(&self) -> bool {
        !self.loaded
    }

    pub fn reload(&mut self) {
        self.loaded = false;
    }

    /// No `Ctx` parameter, alone among the screens: there is nothing here a
    /// store could answer.
    pub fn load(&mut self) {
        self.loaded = true;
        let mut lines = vec![Line::default()];
        if std::env::var_os("NO_COLOR").is_some() {
            // Half-block art is made of nothing but colour, so under
            // NO_COLOR there is nothing honest to draw. Say so instead.
            lines.push(note("NO_COLOR is set, so the pixel art is not drawn."));
        } else {
            let depth = art::depth(std::env::var("COLORTERM").ok().as_deref());
            for art_line in art::tui_lines(depth) {
                let mut spans = vec![theme::plain(" ".repeat(MARGIN))];
                spans.extend(art_line.spans);
                lines.push(Line::from(spans));
            }
        }
        lines.push(Line::default());
        lines.push(field("name", "pacrat · petit chef"));
        lines.push(field("version", env!("CARGO_PKG_VERSION")));
        lines.push(field("about", env!("CARGO_PKG_DESCRIPTION")));
        lines.push(field("source", env!("CARGO_PKG_REPOSITORY")));
        lines.push(Line::default());
        lines.push(note(
            "every screen action has a CLI twin — `pacrat --help` lists them",
        ));
        if let Some(region) = self.panes.region_mut(0) {
            region.set_lines(lines);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load fills the region and settles: no store, no ctx, no reason to
    /// load twice. The line count is art plus the info block, or the one
    /// fallback line under NO_COLOR — asserted loosely, because the exact
    /// art dimensions are the art module's tests to pin.
    #[test]
    fn the_screen_loads_from_nothing_and_settles() {
        let mut screen = About::new();
        assert!(screen.needs_load());
        screen.load();
        assert!(!screen.needs_load());
        let region = screen.panes.region(0).unwrap();
        assert!(region.line_count() > 6, "the info block is missing");
        screen.reload();
        assert!(screen.needs_load());
    }
}
