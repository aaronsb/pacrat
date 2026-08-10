//! `[5] jobs` — mockup §7. What pacrat is running right now, exactly.
//!
//! "Every external call is shown verbatim." That is the promise, and behind
//! an alternate screen it is a promise about *this* buffer: `git.rs` prints
//! `run  git clone -- …` before each call it makes, [`crate::out`]'s capture
//! collects those lines instead of letting them into ratatui's own buffer,
//! and the log region below renders them in order. Nothing pacrat does
//! through another tool is invisible.
//!
//! ## What is honestly missing
//!
//! The mockup's *running* region has two background gradings and a build in
//! it. ADR-001 decision 3 settles that jobs run in-process only — no daemon,
//! no IPC, background work lives exactly as long as a pacrat process does —
//! so a TUI that is not currently in the middle of something has nothing
//! running, and this screen says so rather than inventing a spinner. The
//! headless half is `pacrat update` on a timer, and the screen names it.
//!
//! The **publish queue** is real host state and is read from the same file
//! `pacrat push` writes (`$XDG_STATE_HOME/pacrat/pushes/queue.toml`), which
//! is the one part of this screen that outlives the process — a queued
//! publish is an errand, and the whole reason the queue exists is that the
//! June 2026 AUR outage turned refused publishes into lost commands.

use ratatui::layout::Constraint;
use ratatui::text::Line;

use crate::out::{epoch_stamp, short_hash, truncate, visible_line};
use crate::push;
use crate::tui::joblog::{elapsed, Log, Outcome};
use crate::tui::theme;
use crate::tui::viewport::{Panes, Region};

use super::{bad, command, note, refreshing};

const RUNNING: usize = 0;
const LOG: usize = 1;
const QUEUE: usize = 2;

pub struct Jobs {
    pub panes: Panes,
    /// An answer the reader asked for, held in the queue region.
    ///
    /// This screen reloads every frame, so anything written into a region
    /// is gone by the next one unless something holds it. `r` puts the live
    /// queue back.
    pinned: Option<(String, Vec<Line<'static>>)>,
    /// Rebuilt on the next frame. Set once per input cycle rather than
    /// every turn of the loop: see [`Jobs::needs_load`].
    stale: bool,
}

impl Jobs {
    pub fn new() -> Self {
        Self {
            panes: Panes::new(vec![
                Region::new("this session", Constraint::Fill(3), refreshing()),
                Region::new(
                    "log · every external call, verbatim",
                    Constraint::Fill(5),
                    refreshing(),
                ),
                Region::new("publish queue", Constraint::Fill(3), refreshing()),
            ]),
            pinned: None,
            stale: true,
        }
    }

    /// Unlike the other screens this one is about *now*, so it rebuilds on a
    /// clock rather than only when asked — an elapsed column that moved only
    /// when somebody pressed `r` would be a worse lie than no column.
    ///
    /// It is a flag and not `true`, and that distinction is load-bearing.
    /// The shell's loop reloads a stale screen and goes straight back to
    /// drawing *without polling for input*, so a screen that is always stale
    /// never lets a key be read: the process spins at full CPU redrawing
    /// itself and the only way out is `kill`. The flag is set by
    /// [`Jobs::settled`], which the loop calls once per input cycle, so this
    /// screen refreshes at the tick rate and the keyboard still works.
    pub fn needs_load(&self) -> bool {
        self.stale
    }

    /// An input cycle finished: rebuild on the next frame.
    pub fn settled(&mut self) {
        self.stale = true;
    }

    pub fn reload(&mut self) {
        self.pinned = None;
        self.stale = true;
    }

    pub fn load(&mut self, log: &Log) {
        self.stale = false;
        // ---- this session ------------------------------------------------
        let mut lines: Vec<Line<'static>> = Vec::new();
        if log.is_empty() {
            lines.push(Line::default());
            lines.push(note("nothing has run in this session yet."));
        }
        for job in log.jobs() {
            let (mark, colour, tail) = match &job.outcome {
                Outcome::Running => ("●", theme::INFO, elapsed(job.started.elapsed())),
                Outcome::Done(note) => (
                    "✓",
                    theme::OK,
                    format!(
                        "{}  {}",
                        job.took.map(elapsed).unwrap_or_default(),
                        truncate(&visible_line(note).0, 60)
                    ),
                ),
                Outcome::Failed(why) => (
                    "✗",
                    theme::BAD,
                    format!(
                        "{}  {}",
                        job.took.map(elapsed).unwrap_or_default(),
                        truncate(&visible_line(why).0, 60)
                    ),
                ),
            };
            lines.push(Line::from(vec![
                theme::plain("  "),
                theme::tinted(colour, format!("{mark} ")),
                theme::plain(format!("{:<40}", truncate(&job.what, 40))),
                theme::dim(tail),
            ]));
        }
        lines.push(Line::default());
        lines.push(note(
            "jobs run in-process only (ADR-001 q3): nothing here outlives this \
             process, and there is no daemon to ask.",
        ));
        lines.push(note(
            "headless warmth is `pacrat update --mode auto` on a systemd timer.",
        ));
        self.set(RUNNING, lines);
        if let Some(region) = self.panes.region_mut(RUNNING) {
            region.set_title(match log.running() {
                0 => "this session".to_string(),
                n => format!("this session — {n} running"),
            });
        }

        // ---- the log -----------------------------------------------------
        let mut log_lines: Vec<Line<'static>> = Vec::new();
        if log.lines().is_empty() {
            log_lines.push(Line::default());
            log_lines.push(note(
                "empty — every command pacrat runs is printed before it runs, and",
            ));
            log_lines.push(note(
                "nothing has run yet. Reviewing a candidate on [3] fills this.",
            ));
        }
        // Already pacrat's own text, but the argv lines in it carry an
        // upstream URL out of a ledger every host can write to — neutered on
        // the way in by `Git::line`, and clipped here so one long line
        // cannot push the column off the pane.
        log_lines.extend(log.lines().iter().map(|line| {
            Line::from(vec![
                theme::plain("  "),
                match line.starts_with("run ") {
                    true => theme::tinted(theme::INFO, truncate(line, 200)),
                    false => theme::dim(truncate(line, 200)),
                },
            ])
        }));
        self.set(LOG, log_lines);
        if let Some(region) = self.panes.region_mut(LOG) {
            region.set_title(format!(
                "log · every external call, verbatim — {} line{}",
                log.lines().len(),
                if log.lines().len() == 1 { "" } else { "s" }
            ));
        }

        // ---- the publish queue -------------------------------------------
        self.load_queue();
    }

    fn load_queue(&mut self) {
        if let Some((title, lines)) = &self.pinned {
            let (title, lines) = (title.clone(), lines.clone());
            self.set(QUEUE, lines);
            if let Some(region) = self.panes.region_mut(QUEUE) {
                region.set_title(title);
            }
            return;
        }
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut title = "publish queue".to_string();
        match push::load_queue() {
            Err(e) => lines.push(bad(format!("the publish queue will not read: {e}"))),
            Ok(queue) if queue.is_empty() => {
                lines.push(Line::default());
                lines.push(note("nothing waiting to publish."));
                lines.push(note(
                    "`pacrat push <package>` queues an errand when the AUR write path \
                     will not take it; `pacrat push` with no arguments works the queue.",
                ));
            }
            Ok(queue) => {
                title = format!("publish queue · {} blocked — aur ssh", queue.len());
                for (package, entry) in &queue.pushes {
                    lines.push(Line::from(vec![
                        theme::plain("  "),
                        theme::tinted(theme::WARN, "» "),
                        theme::plain(format!("{:<24}", truncate(&visible_line(package).0, 24))),
                        theme::dim(format!(
                            "{} · queued {} · probed {}",
                            short_hash(&entry.commit),
                            epoch_stamp(entry.queued_at),
                            epoch_stamp(entry.last_probe)
                        )),
                    ]));
                    // A remote's own words, on pacrat's report.
                    lines.push(Line::from(vec![
                        theme::plain("      "),
                        theme::dim(format!(
                            "answer  {}",
                            truncate(&visible_line(&entry.last_answer).0, 100)
                        )),
                    ]));
                    if let Some(n) = &entry.note {
                        lines.push(Line::from(vec![
                            theme::plain("      "),
                            theme::dim(format!("note    {}", truncate(&visible_line(n).0, 100))),
                        ]));
                    }
                }
                lines.push(Line::default());
                lines.push(note("p probes the write path · R works the queue"));
            }
        }
        self.set(QUEUE, lines);
        if let Some(region) = self.panes.region_mut(QUEUE) {
            region.set_title(title);
        }
    }

    /// `p` — probe the AUR write path.
    ///
    /// Shown rather than run, and this one is not a matter of chatter: a
    /// probe is an ssh connection to `aur.archlinux.org` using the reader's
    /// key, and a key with a passphrase asks for it on a terminal this
    /// screen is sitting on top of. `pacrat push` owns that conversation.
    pub fn suggest_probe(&mut self) {
        let lines = command(
            "probing opens an ssh connection to the AUR with your key. If that key \
             has a passphrase, something has to be able to ask you for it — and \
             behind this screen, nothing can.",
            &["pacrat push".to_string()],
        );
        self.answer(lines, "probe the aur write path");
    }

    /// `R` — work the queue.
    pub fn suggest_retry(&mut self) {
        let lines = command(
            "working the queue probes once, then publishes every queued package whose \
             store tree is still the tree that was queued — a digest comparison, not a \
             commit one, because a tree can be edited without any commit moving.",
            &["pacrat push --retry".to_string()],
        );
        self.answer(lines, "work the publish queue");
    }

    fn answer(&mut self, lines: Vec<Line<'static>>, what: &str) {
        self.pinned = Some((format!("publish queue — {what} · r restores"), lines));
        self.load_queue();
        self.panes.focus_on(QUEUE);
    }

    fn set(&mut self, index: usize, lines: Vec<Line<'static>>) {
        if let Some(region) = self.panes.region_mut(index) {
            region.set_lines(lines);
        }
    }
}
