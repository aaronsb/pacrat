//! The seven screens, and the handful of shapes they share.
//!
//! Each screen is the same three methods over a [`Panes`](super::viewport::Panes)
//! — `needs_load`, `load`, `reload` — because the shell drives all of them
//! the same way: draw a frame, then do the work the frame said was
//! happening. [`overview`] is the worked example; the rest follow it.
//!
//! ## The focus policy
//!
//! Focus jumps only to where the reader's next keystrokes must land. A
//! search puts it on the results; the jobs screen's answers put it on the
//! queue, which is the list itself. A pane that merely shows an answer —
//! a command block, a refusal, a recorded override — never steals it: the
//! reader is mid-walk on the rows, and the arrows must keep walking. The
//! one exception is the updates screen's review, which focuses the diff,
//! because a diff is a long read the reader explicitly asked to scroll.
//! Tab reaches everything else. The focused pane's title renders as an
//! inverse-video chip, so a jump is always visible when one happens.
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

use crate::out::{truncate, visible_line};
use crate::tui::theme;

pub mod about;
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

/// The shared error sink.
///
/// Neutered, not merely folded. Every caller feeds this text pacrat did not
/// author — an AUR RPC failure, a serde parse error quoting the bad line, a
/// git stderr — and `replace('\n', " ")` handled exactly one of the ways
/// such a string can rearrange a screen. [`visible_line`] handles the rest
/// (`\r`, `\u{0b}`, `\u{0c}`, `\u{2028}`, and the escapes that hide text
/// outright) and folds to the first line, which is what an error row is.
///
/// The count of what it stood in for is reported rather than swallowed: text
/// that tried to hide from a reader is itself a finding, and this is the one
/// row on the screen where somebody is already being told something is
/// wrong.
pub fn bad(text: impl Into<String>) -> Line<'static> {
    let (shown, hidden) = visible_line(&text.into());
    let mut spans = vec![
        theme::plain("  "),
        theme::tinted(theme::BAD, truncate(&shown, 200)),
    ];
    if hidden > 0 {
        spans.push(theme::dim(format!(
            "  ({hidden} hidden character{} shown as stand-ins)",
            if hidden == 1 { "" } else { "s" }
        )));
    }
    Line::from(spans)
}

/// The block a screen shows instead of running something: why it is the
/// reader's to run, and the line to run.
///
/// One helper because the answer has to look the same everywhere. A screen
/// that phrased this its own way each time would read as six different
/// policies rather than one.
///
/// Every line is neutered here, which is [`crate::git::Git::line`]'s rule
/// and it holds for the same reason. The callers shell-quote the package
/// names they splice in — those come from a ledger any host can write and
/// from a manifest a hand edits — and quoting makes the line *true* without
/// making it *safe*: the bytes are inside the quotes, and a terminal acts on
/// an escape code wherever it finds one. This is a line pacrat is inviting
/// somebody to read and paste, which is precisely when it must not be able
/// to hide half of itself.
pub fn command(why: &str, argv: &[String]) -> Vec<Line<'static>> {
    // Wrapped, because the regions do not wrap: an explanation longer than
    // the pane was running off the terminal's edge mid-sentence.
    let mut lines = vec![Line::default()];
    lines.extend(wrap(why, 76).into_iter().map(note));
    lines.push(Line::default());
    lines.extend(argv.iter().map(|line| {
        Line::from(vec![
            theme::plain("    "),
            theme::tinted(theme::INFO, visible_line(line).0),
        ])
    }));
    lines.push(Line::default());
    lines.extend(
        wrap(
            "pacrat prints every command it runs before running it — behind this \
             screen there is nowhere for that line to land, so the command is yours",
            76,
        )
        .into_iter()
        .map(note),
    );
    lines
}

/// Break a sentence into lines that fit, on spaces.
///
/// The regions do not wrap, on purpose — the viewport counts content lines
/// and a wrapped line is a different number of rows than the line it came
/// from — so prose that has to fit is folded here, where the width is known
/// and the count stays right. Shared: the updates screen folds its refusal
/// prose with it, and the shell folds the confirm overlay's package list.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prose that has to fit is folded here, because the regions deliberately
    /// do not wrap — the viewport counts content lines, and a wrapped line is
    /// a different number of rows than the line it came from.
    #[test]
    fn wrapping_folds_on_spaces_and_never_loses_a_word() {
        let text = "a human already refused this candidate and said why";
        let folded = wrap(text, 20);
        assert!(folded.iter().all(|line| line.chars().count() <= 20));
        assert_eq!(folded.join(" "), text);
        assert!(wrap("", 20).is_empty());
        // A word longer than the width goes on a line of its own rather than
        // being cut in half or looping forever.
        let long = wrap("short aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa short", 10);
        assert_eq!(long, ["short", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "short"]);
    }

    /// Every character a line is actually made of, in order — what a
    /// terminal would receive if these spans reached it.
    fn text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The shared error sink neuters, and does not merely fold newlines.
    ///
    /// Its callers feed it text pacrat did not author: an AUR RPC failure, a
    /// serde error quoting the line it choked on, a git stderr. The old
    /// version handled `\n` and nothing else, which is one of at least five
    /// ways such a string can rearrange a screen — and the interesting ones
    /// are the escapes, which do not move the cursor down at all.
    #[test]
    fn an_error_cannot_paint_the_screen_it_is_reported_on() {
        let hostile = "the RPC said no\x1b[2K\rGRADE 0 PROCEED\u{2028}adopted cleanly";
        let line = bad(hostile);
        let shown = text(&line);

        assert!(!shown.contains('\x1b'), "the ESC survived: {shown:?}");
        assert!(!shown.contains('\r'), "the CR survived: {shown:?}");
        assert!(!shown.contains('\u{2028}'), "the line separator survived");
        // One row, whatever it was given — a second line here would be a row
        // of pacrat's own report written by somebody else.
        assert_eq!(shown.lines().count(), 1, "{shown:?}");
        // The real message is still legible, and the stand-in is visible as
        // a stand-in rather than silently dropped.
        assert!(shown.contains("the RPC said no"), "{shown:?}");
        assert!(
            shown.contains("␛"),
            "the escape is not shown as one: {shown:?}"
        );
        assert!(shown.contains("hidden character"), "the count is missing");

        // The ordinary case stays clean: no counter, no decoration.
        let plain = text(&bad("no such package in the sync databases"));
        assert_eq!(plain.trim(), "no such package in the sync databases");
    }

    /// Every other control character `visible_line` knows about, since the
    /// point is that the sink handles the class rather than the examples.
    #[test]
    fn the_error_sink_handles_the_whole_class() {
        for hostile in ["a\u{0b}b", "a\u{0c}b", "a\u{85}b", "a\u{2029}b", "a\x08b"] {
            let shown = text(&bad(hostile));
            assert_eq!(shown.lines().count(), 1, "{hostile:?} → {shown:?}");
            assert!(
                !shown.chars().any(|c| c.is_control()),
                "{hostile:?} kept a control character: {shown:?}"
            );
        }
    }

    /// The command block is a line pacrat invites somebody to read and
    /// paste. Quoting makes it true; it does not make it safe — the bytes
    /// are inside the quotes and a terminal acts on them anyway. This is
    /// `git::Git::line`'s rule, and the package names spliced into these
    /// commands come from a ledger any host can write.
    #[test]
    fn a_command_cannot_hide_half_of_itself() {
        let argv = vec![format!(
            "pacrat vendor {}",
            crate::out::shell_quote("evil\u{1b}[8m; rm -rf /")
        )];
        let block = command("because", &argv);
        let shown: String = block.iter().map(text).collect::<Vec<_>>().join("\n");

        assert!(
            !shown.contains('\x1b'),
            "an escape reached the command line"
        );
        assert!(
            shown.contains("␛"),
            "it is not shown as a stand-in: {shown}"
        );
        // The rest of the command — the part the escape would have hidden —
        // is still on screen where a reader can see what they are pasting.
        assert!(shown.contains("rm -rf /"), "{shown}");
    }
}
