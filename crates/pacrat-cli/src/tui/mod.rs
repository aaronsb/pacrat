//! The TUI shell: seven screens on number keys, one frame, every region a
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
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use pacrat_core::config::Ui;
use pacrat_core::decisions::REASON_MAX;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::{Frame, Terminal};

use crate::ctx::Ctx;
use crate::out;
use crate::status;
use childlock::Hold;
use joblog::{Handle, Log};
use keymap::{action_for, Action, Local};
use prompt::{Prompt, Typed};
use screens::about::About;
use screens::browse::{Browse, Rung};
use screens::config::Config;
use screens::hosts::Hosts;
use screens::jobs::Jobs;
use screens::overview::Overview;
use screens::updates::Updates;
use viewport::{Panes, Scroll};

mod childlock;
mod joblog;
mod keymap;
mod prompt;
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
    // From here until the terminal goes back, every `say!` in the program is
    // collected instead of printed. It is not silenced: the lines land in
    // the jobs screen, verbatim and in order, which is the only place they
    // can honestly go while ratatui owns the screen they would otherwise be
    // written onto. See `out::capture_start`.
    out::capture_start();
    let mut app = App::new();
    let outcome = turn(terminal, ctx, &mut app);
    // Whatever was said and not yet collected is not dropped on the way out:
    // it is a record of commands that really ran.
    for line in out::capture_stop() {
        eprintln!("{line}");
    }
    outcome
}

fn turn(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ctx: &Ctx,
    app: &mut App,
) -> Result<(), String> {
    loop {
        app.tick();
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
        //
        // Requested work goes first, and for the same reason one step
        // further on: `on_key` pushes the job into the log as *running*, so
        // the frame above is the one that shows it running. Work performed
        // before that frame would only ever be seen finished.
        if app.pending.is_some() {
            app.perform(ctx);
            // A full repaint afterwards, and a *best-effort* one.
            //
            // Why repaint: capture takes pacrat's own chatter, but a verb
            // reached from here could still hold a `println!` this module
            // has never read, and one stray line written into ratatui's
            // buffer survives every subsequent diffed frame.
            //
            // Why best-effort: `clear` asks the terminal where the cursor is
            // and waits for it to answer, so it can time out on a terminal
            // that is slow or is not really answering — and it did, which is
            // how this comment came to be written. Ending the session over a
            // failed *defensive* repaint would be the tail wagging the dog:
            // the worst a skipped clear can leave is a stale cell, which the
            // next resize or `r` fixes, while the failure it used to cause
            // was the whole TUI exiting after a successful review.
            if let Err(e) = terminal.clear() {
                app.log
                    .note(format!("redraw after the last operation: {e}"));
            }
            continue;
        }
        if app.needs_load() {
            app.load(ctx);
            continue;
        }
        let ready = event::poll(TICK).map_err(|e| format!("reading input: {e}"))?;
        // Whether or not a key arrived, an input cycle has passed. This is
        // the only place a screen may be told it has gone stale — anywhere
        // earlier and the load path would loop back to the draw without ever
        // reaching the poll above, which is a spin at full CPU with a
        // keyboard that does nothing.
        app.settled();
        if ready {
            if let Event::Key(key) = event::read().map_err(|e| format!("reading input: {e}"))? {
                // Presses everywhere; repeats only where a repeat is the
                // point.
                //
                // Terminals that report the full set (kitty's protocol,
                // Windows consoles) send `Repeat` for auto-repeat, and
                // acting on those would act twice — or forty times — on one
                // keystroke held a moment too long. So they are dropped,
                // with exactly one exception: auto-repeat *is* the
                // childlock's designed input, and on a terminal that reports
                // it as `Repeat` the bar would otherwise take a single
                // press and then sit there while the reader holds a key that
                // has stopped meaning anything. A lock with no key on some
                // terminals is worse than no lock.
                let wanted = match key.kind {
                    KeyEventKind::Press => true,
                    KeyEventKind::Repeat => app.wants_repeats(),
                    KeyEventKind::Release => false,
                };
                if wanted {
                    // Is there already more input behind this keystroke?
                    // Only the childlock cares, and for it this is half the
                    // answer to "was this typed or pasted": a person holding
                    // a key has produced exactly one event and the next is
                    // 30ms away, while a paste always has thousands queued.
                    // A poll that errors is read as "no queue", which is the
                    // direction that costs a reader a retry rather than the
                    // direction that lets a flood through.
                    let flooded = event::poll(Duration::ZERO).unwrap_or(false);
                    app.on_key(key, flooded);
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
    About,
}

impl Tab {
    pub const ALL: [Tab; 7] = [
        Tab::Overview,
        Tab::Browse,
        Tab::Updates,
        Tab::Hosts,
        Tab::Jobs,
        Tab::Config,
        Tab::About,
    ];

    pub fn digit(self) -> char {
        match self {
            Tab::Overview => '1',
            Tab::Browse => '2',
            Tab::Updates => '3',
            Tab::Hosts => '4',
            Tab::Jobs => '5',
            Tab::Config => '6',
            Tab::About => '7',
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
            Tab::About => "about",
        }
    }

    /// Mockup §2: each screen is one question. The help overlay leads with
    /// the current screen's, so a reader who opened `?` because they are
    /// lost is told what they are looking at before they are told the keys.
    pub fn question(self) -> &'static str {
        match self {
            Tab::Overview => "what needs my attention?",
            Tab::Browse => "what exists, and what's our relationship to it?",
            Tab::Updates => "what changed upstream, and what did the graders say?",
            Tab::Hosts => "does each machine match the manifest?",
            Tab::Jobs => "what is pacrat running right now, exactly?",
            Tab::Config => "which gates are auto, and what's connected?",
            Tab::About => "who cooked this, and out of what?",
        }
    }
}

/// Which screen is in front, and the state that outlives switching away
/// from one.
struct App {
    tab: Tab,
    overview: Overview,
    browse: Browse,
    updates: Updates,
    hosts: Hosts,
    jobs: Jobs,
    config: Config,
    about: About,
    /// What this session has run, and everything it said while running it.
    log: Log,
    overlay: Overlay,
    /// Work to do after the next frame is drawn, and the log entry already
    /// pushed for it. Two-phase because the frame that shows a job running
    /// has to be drawn before the job blocks the thread that would draw it.
    pending: Option<(Handle, Work)>,
    reload_pending: bool,
    quit: bool,
}

/// Something that takes long enough to be worth showing first.
enum Work {
    /// updates `enter` — clone, stage, diff, read the gradings on file.
    Review,
    /// browse `enter` — the three sources `pacrat info` merges.
    Detail,
    /// The override, once the friction has been paid: a write to the
    /// store's decision ledger, through the CLI's own writer.
    Override(String),
}

/// What is in front of the screen, if anything.
///
/// Modal, all of them, and deliberately: an overlay that let the keys
/// underneath through would be an overlay whose `q` quits the program while
/// somebody is typing a justification into it.
enum Overlay {
    None,
    Help,
    /// A line of text being typed, and what it is for.
    Asking(Prompt, Asking),
    /// The hold-to-confirm bar. See [`childlock`].
    Childlock(Hold),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asking {
    /// browse `/`.
    Search,
    /// The justification behind an override, after the hold.
    Justification,
}

impl App {
    fn new() -> Self {
        Self {
            tab: Tab::Overview,
            overview: Overview::new(),
            browse: Browse::new(),
            updates: Updates::new(),
            hosts: Hosts::new(),
            jobs: Jobs::new(),
            config: Config::new(),
            about: About::new(),
            log: Log::new(),
            overlay: Overlay::None,
            pending: None,
            reload_pending: false,
            quit: false,
        }
    }

    fn panes_mut(&mut self) -> &mut Panes {
        match self.tab {
            Tab::Overview => &mut self.overview.panes,
            Tab::Browse => &mut self.browse.panes,
            Tab::Updates => &mut self.updates.panes,
            Tab::Hosts => &mut self.hosts.panes,
            Tab::Jobs => &mut self.jobs.panes,
            Tab::Config => &mut self.config.panes,
            Tab::About => &mut self.about.panes,
        }
    }

    /// A frame passed. The only thing that cares is the childlock, whose bar
    /// has to empty when the reader lets go — and "let go" is a silence, so
    /// something has to notice the silence.
    fn tick(&mut self) {
        if let Overlay::Childlock(hold) = &mut self.overlay {
            hold.tick(Instant::now());
        }
    }

    /// An input cycle finished — a key arrived, or the poll timed out.
    ///
    /// The one place a screen is allowed to be told it has gone stale. The
    /// loop's load path goes back to the draw without polling, so a screen
    /// marked stale anywhere earlier would be reloaded forever and no key
    /// would ever be read: full CPU, a frozen keyboard, and `kill` as the
    /// only exit. Jobs is the only screen that refreshes on a clock, and
    /// this is the clock.
    fn settled(&mut self) {
        self.jobs.settled();
    }

    /// Does anything on screen want auto-repeat events?
    ///
    /// Only the childlock, and only while it is up. See the event loop for
    /// why repeats are dropped everywhere else.
    fn wants_repeats(&self) -> bool {
        matches!(self.overlay, Overlay::Childlock(_))
    }

    fn needs_load(&self) -> bool {
        if self.reload_pending {
            return true;
        }
        match self.tab {
            Tab::Overview => self.overview.needs_load(),
            Tab::Browse => self.browse.needs_load(),
            Tab::Updates => self.updates.needs_load(),
            Tab::Hosts => self.hosts.needs_load(),
            Tab::Jobs => self.jobs.needs_load(),
            Tab::Config => self.config.needs_load(),
            Tab::About => self.about.needs_load(),
        }
    }

    fn load(&mut self, ctx: &Ctx) {
        self.reload_pending = false;
        // Anything a load said belongs in the log before the jobs screen is
        // asked to render it.
        self.log.absorb();
        match self.tab {
            Tab::Overview => self.overview.load(ctx),
            Tab::Browse => self.browse.load(ctx),
            Tab::Updates => self.updates.load(ctx),
            Tab::Hosts => self.hosts.load(ctx),
            Tab::Jobs => self.jobs.load(&self.log),
            Tab::Config => self.config.load(ctx),
            // No `ctx`: the one screen with nothing to ask a store.
            Tab::About => self.about.load(),
        }
        self.log.absorb();
    }

    /// Do the work the last frame said was happening, and close its entry in
    /// the log with whatever the verb had to say about it.
    fn perform(&mut self, ctx: &Ctx) {
        let Some((handle, work)) = self.pending.take() else {
            return;
        };
        let outcome = match work {
            Work::Review => self.updates.review(ctx),
            Work::Detail => {
                self.browse.detail(ctx);
                Ok("detail".to_string())
            }
            Work::Override(reason) => self.updates.record_override(ctx, &reason),
        };
        self.log.finish(handle, outcome);
    }

    fn on_key(&mut self, key: KeyEvent, flooded: bool) {
        match &mut self.overlay {
            Overlay::Help => return self.on_help_key(key),
            Overlay::Asking(..) => return self.on_prompt_key(key),
            Overlay::Childlock(_) => return self.on_childlock_key(key, flooded),
            Overlay::None => {}
        }
        match action_for(key, self.tab) {
            Some(Action::Quit | Action::Interrupt) => self.quit = true,
            Some(Action::ToggleHelp) => self.overlay = Overlay::Help,
            Some(Action::Screen(tab)) => self.tab = tab,
            Some(Action::Focus(forward)) => self.panes_mut().cycle(forward),
            Some(Action::Scroll(scroll)) => self.scroll(scroll),
            Some(Action::Reload) => self.reload(),
            Some(Action::Local(local)) => self.on_local(local),
            None => {}
        }
    }

    fn scroll(&mut self, scroll: Scroll) {
        self.panes_mut().scroll(scroll);
        // Two screens keep a detail pane pointed at whichever row the cursor
        // is on. Neither asks the system anything to do it — the answer came
        // back with the list — so it happens on the keystroke rather than
        // through the load path.
        match self.tab {
            Tab::Browse => self.browse.moved(),
            Tab::Hosts => self.hosts.moved(),
            Tab::Updates => self.updates.moved(),
            _ => {}
        }
    }

    fn reload(&mut self) {
        match self.tab {
            Tab::Overview => self.overview.reload(),
            Tab::Browse => self.browse.reload(),
            Tab::Updates => self.updates.reload(),
            Tab::Hosts => self.hosts.reload(),
            Tab::Jobs => self.jobs.reload(),
            Tab::Config => self.config.reload(),
            Tab::About => self.about.reload(),
        }
        self.reload_pending = true;
    }

    /// A screen's own verb. Exhaustive on purpose: a key added to the table
    /// and forgotten here is a compile error rather than a key that quietly
    /// does nothing.
    fn on_local(&mut self, local: Local) {
        match local {
            Local::Search => {
                self.overlay = Overlay::Asking(
                    Prompt::new(
                        "search the official repos and the AUR",
                        "both worlds are asked; either may fail without costing the other",
                        128,
                    ),
                    Asking::Search,
                )
            }
            Local::Detail => self.queue(Work::Detail, "detail"),
            Local::Vendor => self.browse.suggest(Rung::Vendor),
            Local::Track => self.browse.suggest(Rung::Track),
            Local::Install => self.browse.suggest(Rung::Install),
            Local::Review => self.queue(Work::Review, "review"),
            Local::Adopt => self.updates.suggest_adopt(),
            Local::Reject => self.updates.suggest_reject(),
            Local::Override => self.open_childlock(),
            Local::Select => self.hosts.toggle(),
            Local::AdoptSelection => self.hosts.suggest_adopt(),
            Local::Sync => self.hosts.suggest_sync(),
            Local::Probe => self.jobs.suggest_probe(),
            Local::Retry => self.jobs.suggest_retry(),
            Local::PresetLeft => self.config.preview(false),
            Local::PresetRight => self.config.preview(true),
        }
    }

    /// Push the job as running, and let the loop do it after the next frame.
    fn queue(&mut self, work: Work, verb: &str) {
        let what = match self.tab {
            Tab::Browse => self.browse.selected().map(str::to_string),
            Tab::Updates => self.updates.selected_package(),
            _ => None,
        };
        let handle = self.log.start(match what {
            Some(package) => format!("{verb} {package}"),
            None => verb.to_string(),
        });
        self.pending = Some((handle, work));
    }

    // ------------------------------------------------------- the overlays

    fn on_help_key(&mut self, key: KeyEvent) {
        // While help covers the screen the only keys that mean anything are
        // the ones that uncover it. Scrolling a pane the reader cannot see
        // is worse than doing nothing. ctrl-c is the exception — "get me
        // out" must not need to be pressed twice, or it is not the escape
        // hatch it is there to be.
        match action_for(key, self.tab) {
            Some(Action::Quit | Action::ToggleHelp) => self.overlay = Overlay::None,
            Some(Action::Interrupt) => self.quit = true,
            _ => {}
        }
    }

    fn on_prompt_key(&mut self, key: KeyEvent) {
        // ctrl-c first and unconditionally, before the prompt sees the key:
        // a modal that can only be left by answering it is a trap, and this
        // one is reached at the end of a friction gate somebody may well
        // want to abandon.
        if action_for(key, self.tab) == Some(Action::Interrupt) {
            self.quit = true;
            return;
        }
        let Overlay::Asking(prompt, asking) = &mut self.overlay else {
            return;
        };
        let asking = *asking;
        match prompt.key(key) {
            Typed::Editing | Typed::Ignored => {}
            Typed::Cancelled => self.overlay = Overlay::None,
            Typed::Submitted => {
                if !prompt.answered() {
                    // An empty answer is not an answer, and for the
                    // justification it is the exact thing the friction
                    // exists to prevent. Neither closes the prompt.
                    return;
                }
                let value = prompt.value().to_string();
                self.overlay = Overlay::None;
                match asking {
                    Asking::Search => {
                        self.browse.search_for(&value);
                        self.reload_pending = true;
                    }
                    Asking::Justification => self.queue(Work::Override(value), "override"),
                }
            }
        }
    }

    /// `o` on updates: open the hold, or say why there is nothing to hold
    /// for.
    ///
    /// Two doors are shut here rather than at the bar. The screen decides
    /// whether there is a reviewed BLOCK to override at all — an override is
    /// a record that a human accepted specific bytes, so it is offered only
    /// for bytes this session fetched and showed them. And the hold needs an
    /// interactive terminal on *stdin*: the timing below makes a burst of
    /// input fail, and this makes a non-human input source fail before it
    /// gets to try.
    fn open_childlock(&mut self) {
        if let Err(why) = self.updates.overridable() {
            self.updates.refuse_override(&why);
            return;
        }
        if !io::stdin().is_terminal() {
            self.updates.refuse_override(
                "the override needs a terminal on stdin — holding a key is a claim \
                 about a person, and pacrat will not accept it from a pipe. The CLI \
                 door is `pacrat adopt-update --override-block --reason \"…\"`.",
            );
            return;
        }
        self.overlay = Overlay::Childlock(Hold::new());
    }

    fn on_childlock_key(&mut self, key: KeyEvent, flooded: bool) {
        if action_for(key, self.tab) == Some(Action::Interrupt) {
            self.quit = true;
            return;
        }
        if key.code == KeyCode::Esc {
            self.overlay = Overlay::None;
            return;
        }
        let Overlay::Childlock(hold) = &mut self.overlay else {
            return;
        };
        // Only the one key feeds it. Any other keystroke is a release as far
        // as this is concerned — it is not the key being held — so it is
        // ignored and the gap closes the run on the next tick.
        if key.code != KeyCode::Char(' ') {
            return;
        }
        if hold.press(Instant::now(), flooded) {
            // The friction is paid; now the part the CLI charges too.
            self.overlay = Overlay::Asking(
                Prompt::new(
                    "why is this BLOCK being accepted?",
                    "recorded permanently in the store's decision ledger, and synced \
                     to every host",
                    REASON_MAX,
                ),
                Asking::Justification,
            );
        }
    }

    // ---------------------------------------------------------- rendering

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let (focused, regions) = self.panes_mut().focus_position();
        let footer = self.footer();
        let block = Block::bordered()
            .border_style(ratatui::style::Style::new().fg(theme::DIM))
            .title_top(self.tab_bar())
            .title_bottom(footer)
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
        match &self.overlay {
            Overlay::None => {}
            Overlay::Help => render_help(frame, area, self.tab),
            Overlay::Asking(prompt, _) => render_prompt(frame, area, prompt),
            Overlay::Childlock(hold) => render_childlock(frame, area, hold),
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

    /// The mockup's footer bar: this screen's verbs, then the two that are
    /// always true. Built from the keymap rather than written out, so a
    /// screen cannot advertise a key it does not have.
    fn footer(&self) -> Line<'static> {
        let mut spans = vec![theme::dim("─ ")];
        for binding in keymap::for_screen(self.tab) {
            spans.push(theme::accent(binding.keys));
            spans.push(theme::dim(" · "));
        }
        spans.push(theme::accent("?"));
        spans.push(theme::dim(" help · "));
        spans.push(theme::accent("q"));
        spans.push(theme::dim(" quit "));
        Line::from(spans)
    }
}

// -------------------------------------------------------------- overlays

/// Centre a box of the given size in `area`, clipped to it.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// Draw a bordered overlay over whatever was underneath.
///
/// `Clear` first, always: an overlay drawn over live content without it is
/// transparent wherever its own text happens to be a space.
fn overlay(
    frame: &mut Frame,
    rect: Rect,
    title: Line<'static>,
    foot: Line<'static>,
    lines: Vec<Line<'static>>,
) {
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_style(ratatui::style::Style::new().fg(theme::ACCENT))
                .title_top(title)
                .title_bottom(foot),
        ),
        rect,
    );
}

/// The `?` overlay: the keymap, rendered from the same table that dispatches
/// it.
///
/// This screen's rows lead, because they are the ones that differ from what
/// the reader already knows — and because a screen that has *taken* a letter
/// makes the global row below it wrong where they are standing, which they
/// can only see if both are shown together.
///
/// Not a `Region`, and so not scrollable: it is chrome rather than content,
/// and it is sized to its own list. If that list ever outgrows a small
/// terminal it should become a `Region` like everything else rather than
/// grow a second scrolling mechanism.
fn render_help(frame: &mut Frame, area: Rect, tab: Tab) {
    let row = |keys: &str, what: &str| {
        Line::from(vec![
            theme::plain("  "),
            theme::accent(format!("{keys:<30}")),
            theme::plain(what.to_string()),
        ])
    };
    let mut lines = vec![
        Line::default(),
        Line::from(vec![
            theme::plain("  "),
            theme::dim(tab.question().to_string()),
        ]),
        Line::default(),
    ];
    let mine: Vec<&keymap::Binding> = keymap::for_screen(tab).collect();
    if !mine.is_empty() {
        lines.push(Line::from(vec![
            theme::plain("  "),
            theme::bold(theme::ACCENT, format!("[{}] {}", tab.digit(), tab.title())),
            theme::dim(" — these win over the keys below"),
        ]));
        lines.extend(mine.iter().map(|b| row(b.keys, b.what)));
        lines.push(Line::default());
    }
    lines.push(Line::from(vec![
        theme::plain("  "),
        theme::dim("everywhere"),
    ]));
    lines.extend(keymap::global().map(|b| row(b.keys, b.what)));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        theme::plain("  "),
        theme::dim("every screen action has a CLI twin: `pacrat --help`"),
    ]));

    let rect = centred(area, 76, lines.len() as u16 + 2);
    overlay(
        frame,
        rect,
        Line::from(vec![
            theme::dim("─ "),
            theme::accent("? help"),
            theme::dim(" · keys "),
        ]),
        Line::from(vec![theme::dim("─ esc or ? closes ")]),
        lines,
    );
}

/// A line of text being typed.
fn render_prompt(frame: &mut Frame, area: Rect, prompt: &Prompt) {
    let mut lines = vec![
        Line::default(),
        Line::from(vec![theme::plain("  "), theme::plain(prompt.label.clone())]),
    ];
    if !prompt.hint.is_empty() {
        lines.push(Line::from(vec![
            theme::plain("  "),
            theme::dim(prompt.hint.clone()),
        ]));
    }
    lines.push(Line::default());
    // The reader's own text, echoed. Not run through `out::visible` — and
    // that is not an omission: control characters never enter the buffer,
    // because `Prompt::key` refuses the keystroke rather than storing a byte
    // it would have to show as something else.
    lines.push(Line::from(vec![
        theme::plain("   "),
        theme::tinted(theme::INFO, prompt.value().to_string()),
        theme::bold(theme::ACCENT, "▍"),
    ]));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        theme::plain("  "),
        theme::dim(format!("{} characters left", prompt.remaining())),
    ]));

    let width = 76.min(area.width);
    let rect = centred(area, width, lines.len() as u16 + 2);
    overlay(
        frame,
        rect,
        Line::from(vec![
            theme::dim("─ "),
            theme::accent("input"),
            theme::dim(" "),
        ]),
        Line::from(vec![theme::dim(
            "─ enter accepts · esc cancels · ctrl-u clears ",
        )]),
        lines,
    );
}

/// The hold-to-confirm bar (ADR-001 decision 2).
///
/// The bar is the whole affordance: there is no key that overrides a BLOCK,
/// there is a key that has to be *held*, and the only way to know it is
/// working is to watch it fill. It says both ways of feeding it, because a
/// terminal with auto-repeat switched off would otherwise present a lock
/// with no key.
fn render_childlock(frame: &mut Frame, area: Rect, hold: &Hold) {
    const WIDTH: usize = 60;
    let filled = (hold.filled() * WIDTH as f64).round() as usize;
    let lines = vec![
        Line::default(),
        Line::from(vec![
            theme::plain("  "),
            theme::tinted(theme::BAD, "This candidate is held by a BLOCK verdict."),
        ]),
        Line::from(vec![
            theme::plain("  "),
            theme::dim("Overriding it is recorded permanently in the store's decision"),
        ]),
        Line::from(vec![
            theme::plain("  "),
            theme::dim("ledger, synced to every host, with your name on it."),
        ]),
        Line::default(),
        Line::from(vec![
            theme::plain("  "),
            theme::tinted(theme::WARN, "Hold "),
            theme::bold(theme::ACCENT, "space"),
            theme::tinted(theme::WARN, " for three seconds"),
            theme::dim("  (or tap it, if your terminal does not repeat)"),
        ]),
        Line::default(),
        Line::from(vec![
            theme::plain("  ["),
            theme::tinted(theme::BAD, "█".repeat(filled)),
            theme::dim("░".repeat(WIDTH - filled)),
            theme::plain("]"),
        ]),
        Line::default(),
        Line::from(vec![
            theme::plain("  "),
            theme::dim("Letting go empties it. A justification comes next."),
        ]),
    ];
    let rect = centred(area, WIDTH as u16 + 8, lines.len() as u16 + 2);
    overlay(
        frame,
        rect,
        Line::from(vec![
            theme::dim("─ "),
            theme::tinted(theme::BAD, "override a BLOCK"),
            theme::dim(" "),
        ]),
        Line::from(vec![theme::dim("─ esc cancels ")]),
        lines,
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
