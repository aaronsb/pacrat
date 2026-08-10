//! The TUI shell: six screens on number keys, one frame, every region a
//! viewport (ADR-001 Surfaces; mockup §2 and §3).
//!
//! This module owns three things and delegates the rest: the terminal's
//! state, the event loop, and which screen is in front. The scrolling
//! component is [`viewport`], the keymap is [`keymap`], and the screens are
//! under [`screens`].
//!
//! **The terminal is borrowed, and it goes back the way it was found.** Raw
//! mode and the alternate screen are global state on a device the user owns;
//! a TUI that exits without undoing them leaves a shell that does not echo
//! and does not scroll, and the user's next move is to close the window. So
//! every way out of this module runs [`restore`], and there are four of
//! them:
//!
//! 1. `Drop` on the ordinary return,
//! 2. `Drop` again for a `?` that bails out mid-loop,
//! 3. the panic hook, for when a `Drop` will not run or will run too late to
//!    put the message somewhere readable,
//! 4. a signal handler for `TERM`, `HUP` and `INT`, because the default
//!    disposition for those is to end the process where it stands — between
//!    `enable_raw_mode` and the `Drop` that undoes it — and a `Drop` that is
//!    never reached restores nothing. It restores from inside the handler
//!    rather than raising a flag for the loop, for a reason worth reading
//!    before changing it: see `install_signal_handlers`.
//!
//! [`restore`] itself never short-circuits: each step is attempted whatever
//! the last one did.
//!
//! The one exit with nothing on it is `SIGKILL`, which is uncatchable. That
//! is why `ctrl-c` is bound as a *key* (raw mode means it is one, not a
//! signal): a reflex that appears to do nothing is what sends somebody to
//! `kill -9`.

use std::io::{self, IsTerminal, Stdout};
use std::sync::Once;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use pacrat_core::config::Ui;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::{Frame, Terminal};

use crate::ctx::Ctx;
use crate::status;
use keymap::{action_for, Action, BINDINGS};
use screens::overview::Overview;
use screens::placeholder;
use viewport::Panes;

mod keymap;
mod screens;
mod theme;
mod viewport;

/// How long the loop waits for a key before drawing again. Nothing animates
/// yet, so this only bounds how stale a resize can look; short enough to
/// feel immediate, long enough to leave the CPU alone.
const TICK: Duration = Duration::from_millis(250);

/// Bare `pacrat`: whichever face `default_ui` asks for.
///
/// The fallback is the interesting case. A TUI writing to a pipe is not a
/// degraded TUI, it is thousands of cursor-positioning escapes where the
/// caller expected text — so a *preference* for the TUI yields to the
/// evidence that nobody is watching, and says so on stderr rather than
/// silently doing something else.
pub fn run_default(ctx: &Ctx) -> Result<(), String> {
    match ctx.config.default_ui {
        Ui::Cli => status::run(ctx),
        Ui::Tui if !io::stdout().is_terminal() => {
            eprintln!(
                "pacrat: default_ui = tui, but stdout is not a terminal — showing status instead \
                 (`pacrat tui` to insist)"
            );
            status::run(ctx)
        }
        Ui::Tui => run(ctx),
    }
}

/// `pacrat tui`: the shell, asked for by name.
///
/// Unlike the preference above this refuses rather than falls back. Being
/// asked for the TUI explicitly and printing a status report instead is the
/// tool deciding it knows better; an error tells the caller exactly what
/// happened and leaves the choice with them.
pub fn run(ctx: &Ctx) -> Result<(), String> {
    if !io::stdout().is_terminal() {
        return Err(
            "`pacrat tui` needs a terminal on stdout — for a pipe or a file, \
             `pacrat status` is the same screen as text"
                .into(),
        );
    }
    let mut session = Session::open()?;
    let outcome = event_loop(&mut session.terminal, ctx);
    // Explicit, so the terminal is back before a caller prints anything.
    drop(session);
    outcome
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, ctx: &Ctx) -> Result<(), String> {
    let mut app = App::new(ctx);
    loop {
        terminal
            .draw(|frame| app.render(frame))
            .map_err(|e| format!("drawing: {e}"))?;
        // Draw first, then work. Everything a screen asks the system is a
        // blocking subprocess, and this ordering is the whole of pacrat's
        // "loading" machinery: the frame the reader is looking at while
        // pacman answers says "refreshing…" because it was drawn before the
        // question was asked. No runtime, no threads, no channels — at four
        // queries a screen, an async stack would be scaffolding around a
        // 200ms wait (ADR-001 decision 3 keeps jobs in-process for the same
        // reason).
        if app.needs_load() {
            app.load(ctx);
            continue;
        }
        if event::poll(TICK).map_err(|e| format!("reading input: {e}"))? {
            // Key *presses*. Terminals that report releases and repeats
            // (kitty's protocol, Windows) would otherwise act twice on one
            // keystroke.
            if let Event::Key(key) = event::read().map_err(|e| format!("reading input: {e}"))? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key);
                }
            }
        }
        if app.quit {
            return Ok(());
        }
    }
}

// ------------------------------------------------------------ the terminal

/// The borrowed terminal. Exists to be dropped.
struct Session {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

/// The three that mean "stop", and whose default disposition would end the
/// process where it stands, unwinding nothing. `INT` is here even though raw
/// mode means `ctrl-c` never becomes one: `kill -INT` from another terminal
/// is the same request, and covering only the signals a keyboard cannot send
/// would be a strange place to draw the line.
const STOPPING: [i32; 3] = [
    signal_hook::consts::SIGTERM,
    signal_hook::consts::SIGHUP,
    signal_hook::consts::SIGINT,
];

impl Session {
    fn open() -> Result<Self, String> {
        install_panic_hook();
        // Registered before raw mode, so there is no window in which the
        // terminal is altered and nothing is listening for the signal that
        // would leave it that way.
        install_signal_handlers()?;
        enable_raw_mode().map_err(|e| format!("entering raw mode: {e}"))?;
        // From here on every failure path has to restore, because raw mode
        // is already on.
        let mut out = io::stdout();
        if let Err(e) = execute!(out, EnterAlternateScreen, cursor::Hide) {
            restore();
            return Err(format!("entering the alternate screen: {e}"));
        }
        match Terminal::new(CrosstermBackend::new(out)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(e) => {
                restore();
                Err(format!("starting the terminal backend: {e}"))
            }
        }
    }
}

/// Restore and leave from inside the handler itself.
///
/// **Why not a flag the event loop polls.** That was the first design and it
/// is the safer-looking one: set an atomic, notice it at the top of the next
/// turn, unwind normally. It works for `kill -TERM`, and it does nothing at
/// all for the case `SIGHUP` actually names — because when the terminal goes
/// away, the loop stops turning. On a hung-up tty `read(2)` returns `Ok(0)`
/// forever, and crossterm reads a zero-length read as "nothing yet": the
/// inner loop in `try_read` neither breaks on it nor consults its timeout
/// (crossterm-0.29.0, src/event/source/unix/mio.rs:96-104), so `event::poll`
/// never returns. The process spins at 100% CPU with the flag set and nobody
/// left to read it — which is what the first version of this code did, and
/// how this comment came to be written. A guarantee that holds only while
/// the terminal is healthy is not the one a hangup handler is for.
///
/// **What it costs.** `restore` is not in the async-signal-safe subset:
/// crossterm keeps the pre-raw-mode termios behind a `parking_lot` mutex,
/// and writing the escape sequences locks stdout. Both are held for
/// microseconds, twice in a run, by the only thread this process has — and
/// stdout's lock is re-entrant on that thread regardless. Weighed against a
/// terminal left in raw mode or an orphan spinning forever, that window is
/// worth taking. `low_level::exit` is `_exit(2)`: no atexit hooks, no
/// destructors, nothing on the way out that could block.
fn install_signal_handlers() -> Result<(), String> {
    for signal in STOPPING {
        // SAFETY: the handler calls `restore` and then `_exit`. See above
        // for why that is not the signal-safe subset, and why it is still
        // the right trade.
        let registered = unsafe {
            signal_hook::low_level::register(signal, move || {
                restore();
                // 128+N: the shell's convention for "ended by this signal".
                signal_hook::low_level::exit(128 + signal);
            })
        };
        // Failing here is an exhausted process, not a routine condition, and
        // carrying on would mean running with the guarantee this function
        // exists to provide quietly missing.
        registered.map_err(|e| format!("listening for signal {signal}: {e}"))?;
    }
    Ok(())
}

impl Drop for Session {
    fn drop(&mut self) {
        restore();
    }
}

/// Put the terminal back. Best effort, and deliberately without `?`: if
/// leaving raw mode fails there is all the more reason to still try to leave
/// the alternate screen, and a restore that gave up halfway is the failure
/// mode this function exists to prevent. Safe to call twice — the panic hook
/// and `Drop` both will, during an unwinding panic.
fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
}

/// Restore before the panic is reported, then report it as usual.
///
/// `Drop` alone is not enough: it does not run under `panic = "abort"`, and
/// during an unwind the default hook prints the message *first* — into the
/// alternate screen, which is then torn down, taking the only description of
/// what went wrong with it. Restoring inside the hook puts the backtrace on
/// the shell's own screen, where it can be read and pasted.
fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}

// ------------------------------------------------------------- the screens

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Browse,
    Updates,
    Hosts,
    Jobs,
    Config,
}

impl Tab {
    pub const ALL: [Tab; 6] = [
        Tab::Overview,
        Tab::Browse,
        Tab::Updates,
        Tab::Hosts,
        Tab::Jobs,
        Tab::Config,
    ];

    pub fn digit(self) -> char {
        match self {
            Tab::Overview => '1',
            Tab::Browse => '2',
            Tab::Updates => '3',
            Tab::Hosts => '4',
            Tab::Jobs => '5',
            Tab::Config => '6',
        }
    }

    pub fn from_digit(digit: char) -> Option<Self> {
        Self::ALL.into_iter().find(|tab| tab.digit() == digit)
    }

    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "overview",
            Tab::Browse => "browse",
            Tab::Updates => "updates",
            Tab::Hosts => "hosts",
            Tab::Jobs => "jobs",
            Tab::Config => "config",
        }
    }

    /// Mockup §2: each screen is one question. The placeholders lead with
    /// theirs, so a screen with no data still says what it is for.
    pub fn question(self) -> &'static str {
        match self {
            Tab::Overview => "what needs my attention?",
            Tab::Browse => "what exists, and what's our relationship to it?",
            Tab::Updates => "what changed upstream, and what did the graders say?",
            Tab::Hosts => "does each machine match the manifest?",
            Tab::Jobs => "what is pacrat running right now, exactly?",
            Tab::Config => "which gates are auto, and what's connected?",
        }
    }
}

struct App {
    tab: Tab,
    overview: Overview,
    /// The five that are not built yet, in `Tab::ALL` order minus overview.
    placeholders: Vec<(Tab, Panes)>,
    help: bool,
    reload_pending: bool,
    quit: bool,
}

impl App {
    fn new(ctx: &Ctx) -> Self {
        Self {
            tab: Tab::Overview,
            overview: Overview::new(),
            placeholders: Tab::ALL
                .into_iter()
                .filter(|tab| *tab != Tab::Overview)
                .map(|tab| (tab, placeholder::build(tab, ctx)))
                .collect(),
            help: false,
            reload_pending: false,
            quit: false,
        }
    }

    fn panes_mut(&mut self) -> &mut Panes {
        match self.tab {
            Tab::Overview => &mut self.overview.panes,
            tab => self
                .placeholders
                .iter_mut()
                .find(|(candidate, _)| *candidate == tab)
                .map(|(_, panes)| panes)
                // Unreachable: `placeholders` is built from `Tab::ALL`.
                .expect("every tab has a screen"),
        }
    }

    fn needs_load(&self) -> bool {
        self.reload_pending || (self.tab == Tab::Overview && self.overview.needs_load())
    }

    fn load(&mut self, ctx: &Ctx) {
        self.reload_pending = false;
        match self.tab {
            Tab::Overview => self.overview.load(ctx),
            // In place, never rebuilt: replacing the `Panes` would silently
            // throw away the reader's focus and scroll positions along with
            // the stale lines.
            tab => {
                if let Some((_, panes)) = self
                    .placeholders
                    .iter_mut()
                    .find(|(candidate, _)| *candidate == tab)
                {
                    placeholder::refresh(panes, tab, ctx);
                }
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Help is modal: while it covers the screen the only keys that mean
        // anything are the ones that uncover it. Scrolling a pane the reader
        // cannot see is worse than doing nothing. ctrl-c is the exception —
        // "get me out" must not need to be pressed twice, or it is not the
        // escape hatch it is there to be.
        if self.help {
            match action_for(key) {
                Some(Action::Quit | Action::ToggleHelp) => self.help = false,
                Some(Action::Interrupt) => self.quit = true,
                _ => {}
            }
            return;
        }
        match action_for(key) {
            Some(Action::Quit | Action::Interrupt) => self.quit = true,
            Some(Action::ToggleHelp) => self.help = true,
            Some(Action::Screen(tab)) => self.tab = tab,
            Some(Action::Focus(forward)) => self.panes_mut().cycle(forward),
            Some(Action::Scroll(scroll)) => self.panes_mut().scroll(scroll),
            Some(Action::Reload) => {
                if self.tab == Tab::Overview {
                    self.overview.reload();
                }
                self.reload_pending = true;
            }
            None => {}
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let (focused, regions) = self.panes_mut().focus_position();
        let block = Block::bordered()
            .border_style(ratatui::style::Style::new().fg(theme::DIM))
            .title_top(self.tab_bar())
            .title_bottom(Line::from(vec![
                theme::dim("─ "),
                theme::accent("?"),
                theme::dim(" help · "),
                theme::accent("q"),
                theme::dim(" quit "),
            ]))
            .title_bottom(
                Line::from(vec![theme::dim(match regions > 1 {
                    true => format!("─ tab · region {focused}/{regions} ─"),
                    false => "─".into(),
                })])
                .right_aligned(),
            );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.panes_mut().render(frame, area, inner);
        if self.help {
            render_help(frame, area);
        }
    }

    /// `─ pacrat ── [1]overview [2]browse …`, the mockup's top border.
    fn tab_bar(&self) -> Line<'static> {
        let mut spans = vec![
            theme::dim("─ "),
            theme::bold(theme::ACCENT, "pacrat"),
            theme::dim(" ── "),
        ];
        for tab in Tab::ALL {
            let label = format!("[{}]{}", tab.digit(), tab.title());
            spans.push(match tab == self.tab {
                true => theme::bold(theme::ACCENT, label),
                false => theme::dim(label),
            });
            spans.push(theme::dim(" "));
        }
        Line::from(spans)
    }
}

/// The `?` overlay: the keymap, rendered from the same table that dispatches
/// it, over whatever was underneath.
///
/// Not a `Region`, and so not scrollable — it is chrome rather than content,
/// and it is sized to its own fixed list. If the list ever outgrows a small
/// terminal it should become a `Region` like everything else rather than
/// grow a second scrolling mechanism.
fn render_help(frame: &mut Frame, area: Rect) {
    let mut lines = vec![Line::default()];
    for binding in BINDINGS {
        lines.push(Line::from(vec![
            theme::plain("  "),
            theme::accent(format!("{:<30}", binding.keys)),
            theme::plain(binding.what.to_string()),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        theme::plain("  "),
        theme::dim("the about tab — the petit chef — arrives with "),
        theme::accent("task #21"),
    ]));
    lines.push(Line::from(vec![
        theme::plain("  "),
        theme::dim("every screen action has a CLI twin: `pacrat --help`"),
    ]));

    let width = 76.min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    // Clear first: an overlay drawn over live content without it is
    // transparent wherever its own text happens to be a space.
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_style(ratatui::style::Style::new().fg(theme::ACCENT))
                .title_top(Line::from(vec![
                    theme::dim("─ "),
                    theme::accent("? help"),
                    theme::dim(" · keys "),
                ]))
                .title_bottom(Line::from(vec![theme::dim("─ esc or ? closes ")])),
        ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The number keys, the tab titles and the screen list are three views
    /// of one set; a tab that lost its digit would be unreachable.
    #[test]
    fn every_tab_has_a_unique_digit_that_maps_back() {
        for tab in Tab::ALL {
            assert_eq!(Tab::from_digit(tab.digit()), Some(tab));
            assert!(!tab.title().is_empty());
            assert!(
                tab.question().ends_with('?'),
                "{} has no question",
                tab.title()
            );
        }
        let digits: Vec<char> = Tab::ALL.iter().map(|tab| tab.digit()).collect();
        let mut unique = digits.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(digits.len(), unique.len(), "two tabs share a digit");
    }
}
