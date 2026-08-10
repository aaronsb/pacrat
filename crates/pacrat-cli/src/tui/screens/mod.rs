//! The six screens, and the handful of shapes they share.
//!
//! Each screen is the same three methods over a [`Panes`](super::viewport::Panes)
//! — `needs_load`, `load`, `reload` — because the shell drives all of them
//! the same way: draw a frame, then do the work the frame said was
//! happening. [`overview`] is the worked example; the rest follow it.
//!
//! ## What a screen may do
//!
//! Read anything; run almost nothing. Every screen here reads live state
//! through the same functions the CLI verbs read it through — `ctx`,
//! `custody::Index`, `live`, `grade`, `push::load_queue`, `setup::state` —
//! so the two surfaces can disagree about layout and never about facts.
//!
//! What they mostly do *not* do is act, and the reason is the house rule
//! about external commands rather than timidity. Every command pacrat runs
//! is printed before it runs; a verb driven from behind the alternate screen
//! prints into a buffer ratatui owns. Two of pacrat's verbs also stop to ask
//! a question on stdin, which in raw mode is not a question anybody can
//! answer. So the screens hand the reader the exact command — which is the
//! mockup's own answer for the sudo cases (§6: "sync north → pacman -S …,
//! *or print the command*"), applied consistently.
//!
//! The exceptions are the two things that run no external command at all and
//! ask nobody anything:
//!
//! * [`updates`]' **review** step, which does shell out — and which routes
//!   the verbs' chatter into the jobs log through [`crate::out`]'s capture,
//!   so the argv lines land somewhere a human reads them rather than
//!   nowhere.
//! * [`updates`]' **override**, which is a write to the store's decision
//!   ledger through `decisions::record_override` — the CLI's own function,
//!   because ADR-001 decision 2 says one ledger, one shape.

use pacrat_core::Verdict;
use ratatui::text::{Line, Span};

use crate::out::truncate;
use crate::tui::theme;

pub mod browse;
pub mod config;
pub mod hosts;
pub mod jobs;
pub mod overview;
pub mod updates;

/// What a region says while the queries behind it are still running. The
/// shell draws this frame *before* asking, which is the whole of pacrat's
/// loading machinery.
pub fn refreshing() -> Vec<Line<'static>> {
    vec![Line::from(vec![
        theme::plain("  "),
        theme::dim("refreshing…"),
    ])]
}

/// A verdict as the mockup draws it: a glyph that survives a monochrome
/// terminal, and a colour for the one that does not have to.
pub fn verdict_span(verdict: Verdict) -> Span<'static> {
    let (mark, colour) = match verdict {
        Verdict::Proceed => ("●", theme::OK),
        Verdict::Warn => ("▲", theme::WARN),
        Verdict::Block => ("■", theme::BAD),
        Verdict::Ungraded => ("○", theme::DIM),
    };
    theme::tinted(colour, format!("{mark} {verdict}"))
}

/// `  label   value` — the two-column shape every detail pane uses.
pub fn field(label: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        theme::plain("  "),
        theme::dim(format!("{label:<10}")),
        theme::plain(value.into()),
    ])
}

pub fn note(text: impl Into<String>) -> Line<'static> {
    Line::from(vec![theme::plain("  "), theme::dim(text.into())])
}

pub fn bad(text: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        theme::plain("  "),
        theme::tinted(theme::BAD, truncate(&text.into().replace('\n', " "), 200)),
    ])
}

/// The block a screen shows instead of running something: why it is the
/// reader's to run, and the line to run.
///
/// One helper because the answer has to look the same everywhere. A screen
/// that phrased this its own way each time would read as six different
/// policies rather than one.
pub fn command(why: &str, argv: &[String]) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default(), note(why.to_string()), Line::default()];
    lines.extend(argv.iter().map(|line| {
        Line::from(vec![
            theme::plain("    "),
            theme::tinted(theme::INFO, line.clone()),
        ])
    }));
    lines.push(Line::default());
    lines.push(note(
        "pacrat prints every command it runs before running it — behind this \
         screen there is nowhere for that line to land, so the command is yours",
    ));
    lines
}
