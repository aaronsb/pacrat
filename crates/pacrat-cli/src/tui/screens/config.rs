//! `[6] config` — mockup §8. Which gates are auto, and what is connected.
//!
//! The pipeline is drawn because the shape *is* the explanation: detect →
//! grade → decide → build → serve, with the preset deciding how much of it
//! runs without being asked. Below it, the connection status for everything
//! pacrat talks to.
//!
//! ## Why `←`/`→` previews rather than writes
//!
//! The mockup has the preset row as a control. This one moves a highlight
//! and shows the resulting gates and the two lines of TOML that would set
//! them — it does not write the file, and that is a deliberate refusal
//! rather than unfinished work.
//!
//! `~/.config/pacrat/config.toml` is a file a person writes by hand. It has
//! comments in it, and it may have keys this pacrat has never heard of —
//! `Config` deserializes with `#[serde(default)]` and no capture for the
//! rest, so a round trip through `to_string` *deletes* both. A preset switch
//! is a two-character edit; paying for it with somebody's comments and a
//! forward-compatibility guarantee is a bad trade, and a silent one. The
//! decision ledger makes the opposite choice for the opposite reason: it is
//! pacrat's own file, and it keeps unknown fields explicitly so it can be
//! rewritten safely.
//!
//! `pacrat config set` is the verb that should own this (mockup §11); it is
//! not built, and inventing a writer here that the CLI does not have would
//! put the fleet's two surfaces out of step in the one place — configuration
//! — where that is hardest to notice.

use pacrat_core::config::Mode;
use pacrat_core::Verdict;
use ratatui::layout::Constraint;
use ratatui::text::Line;

use crate::ctx::Ctx;
use crate::out::truncate;
use crate::push;
use crate::setup;
use crate::tui::theme;
use crate::tui::viewport::{Panes, Region};

use super::{bad, field, note, refreshing};

const PIPELINE: usize = 0;
const POLICY: usize = 1;
const GRADERS: usize = 2;
const SOURCES: usize = 3;

const PRESETS: [Mode; 3] = [Mode::Manual, Mode::Semi, Mode::Auto];

pub struct Config {
    pub panes: Panes,
    loaded: bool,
    /// Which preset the highlight is on. Starts on the configured one, and
    /// moving it changes nothing but this screen.
    previewing: usize,
    /// Whether `previewing` has been put on the configured preset yet.
    ///
    /// Its own flag rather than a reuse of `loaded`, and the difference is a
    /// bug that was there: `preview` clears `loaded` to force a repaint, so
    /// a load that read "first time" off `loaded` snapped the highlight back
    /// to the configured preset on the very frame that was meant to show it
    /// moved. The arrow key appeared to do nothing at all.
    placed: bool,
}

impl Config {
    pub fn new() -> Self {
        Self {
            panes: Panes::new(vec![
                Region::new("flow", Constraint::Length(10), refreshing()),
                Region::new("policy", Constraint::Length(4), refreshing()),
                Region::new("graders", Constraint::Fill(3), refreshing()),
                Region::new("sources", Constraint::Fill(4), refreshing()),
            ]),
            loaded: false,
            previewing: 1,
            placed: false,
        }
    }

    pub fn needs_load(&self) -> bool {
        !self.loaded
    }

    pub fn reload(&mut self) {
        self.loaded = false;
        for index in [PIPELINE, POLICY, GRADERS, SOURCES] {
            if let Some(region) = self.panes.region_mut(index) {
                region.set_lines(refreshing());
            }
        }
    }

    /// `←`/`→`. Moves a highlight; writes nothing.
    pub fn preview(&mut self, forward: bool) {
        self.previewing = match forward {
            true => (self.previewing + 1).min(PRESETS.len() - 1),
            false => self.previewing.saturating_sub(1),
        };
        self.loaded = false;
    }

    pub fn load(&mut self, ctx: &Ctx) {
        self.loaded = true;
        let config = &ctx.config;
        if !self.placed {
            self.placed = true;
            self.previewing = PRESETS
                .iter()
                .position(|mode| *mode == config.update_mode)
                .unwrap_or(1);
        }

        // ---- the pipeline ------------------------------------------------
        let mut lines = vec![Line::default()];
        let mut preset_row = vec![
            theme::plain("  "),
            theme::dim(format!("{:<14}", "flow preset")),
        ];
        for (index, mode) in PRESETS.iter().enumerate() {
            let configured = *mode == config.update_mode;
            let cursor = index == self.previewing;
            preset_row.push(match (cursor, configured) {
                (true, _) => theme::bold(theme::ACCENT, format!("‹ {mode} ›   ")),
                (false, true) => theme::tinted(theme::INFO, format!("  {mode}     ")),
                (false, false) => theme::dim(format!("  {mode}     ")),
            });
        }
        lines.push(Line::from(preset_row));
        lines.push(Line::default());
        for row in BOXES {
            lines.push(Line::from(vec![theme::plain("   "), theme::dim(*row)]));
        }
        lines.push(Line::default());
        lines.push(note(format!(
            "configured: {} · previewing: {}{}",
            config.update_mode,
            PRESETS[self.previewing],
            match PRESETS[self.previewing] == config.update_mode {
                true => " (the one in force)",
                false => "",
            }
        )));
        self.set(PIPELINE, lines);
        if let Some(region) = self.panes.region_mut(PIPELINE) {
            region.set_title("flow — detect → grade → decide → build → serve");
        }

        // ---- the gates and the policy ------------------------------------
        let previewing = PRESETS[self.previewing];
        let mut policy = vec![
            Line::from(vec![
                theme::plain("  "),
                theme::dim(format!("{:<14}", "gates")),
                theme::plain(gates(previewing)),
            ]),
            Line::from(vec![
                theme::plain("  "),
                theme::dim(format!("{:<14}", "decide policy")),
                theme::tinted(theme::OK, Verdict::Proceed.label()),
                theme::plain(" adopt · "),
                theme::tinted(theme::WARN, Verdict::Warn.label()),
                theme::plain(" hold · "),
                theme::tinted(theme::BAD, Verdict::Block.label()),
                theme::plain(" hold always · "),
                theme::tinted(theme::DIM, Verdict::Ungraded.label()),
                theme::plain(" holds too"),
            ]),
            Line::from(vec![
                theme::plain("  "),
                theme::dim(format!("{:<14}", "thresholds")),
                theme::plain(format!(
                    "warn at grade {} · block at grade {}",
                    config.thresholds.warn_at, config.thresholds.block_at
                )),
            ]),
            note(format!(
                "two invariants no preset changes: {} always holds, and every external \
                 call is logged",
                Verdict::Block
            )),
        ];
        if previewing != config.update_mode {
            policy.push(Line::default());
            policy.push(note("to make the preview the setting, by hand:"));
            policy.push(Line::from(vec![
                theme::plain("    "),
                theme::tinted(theme::INFO, format!("update_mode = \"{previewing}\"")),
                theme::dim(format!("   in {}", config_path())),
            ]));
            policy.push(note(
                "pacrat will not rewrite that file: it is yours, with your comments \
                 in it, and a round trip through pacrat's parser would drop them",
            ));
        }
        // Asked for from the content rather than from a constant beside it.
        // The constant said nine for a block of eight and nobody noticed:
        // the region is `Length`, so the extra row was simply blank, and the
        // test that guarded it asserted only that the number *grew*. A
        // height derived from the lines cannot drift from them.
        let wanted = policy.len() as u16;
        self.set(POLICY, policy);
        if let Some(region) = self.panes.region_mut(POLICY) {
            region.set_height(Constraint::Length(wanted));
            region.set_title("policy — read-only, and deliberately so");
        }

        // ---- the graders --------------------------------------------------
        let mut graders = Vec::new();
        if config.graders.is_empty() {
            graders.push(field(
                "graders",
                "none configured — `manual` is always on, and ungraded holds",
            ));
        }
        for grader in &config.graders {
            graders.push(Line::from(vec![
                theme::plain("  "),
                theme::tinted(theme::INFO, format!("{:<16}", truncate(&grader.name, 16))),
                theme::dim(format!("timeout {}s", grader.timeout_s)),
                theme::dim(match grader.scale {
                    Some(scale) => format!(" · scale pinned {}-{}", scale.min, scale.max),
                    None => " · scale unpinned".into(),
                }),
            ]));
            graders.push(Line::from(vec![
                theme::plain("    "),
                theme::dim(format!("$ {}", grader.cmd.join(" "))),
            ]));
        }
        graders.push(Line::default());
        graders.push(Line::from(vec![
            theme::plain("  "),
            theme::tinted(theme::INFO, format!("{:<16}", "manual")),
            theme::dim("pacrat grade PKG --grade N --note \"…\"          always on"),
        ]));
        graders.push(note(
            "pacrat owns the verdict: graders return a number and findings, and the \
             mapping to a verdict is this host's thresholds",
        ));
        graders.push(note(
            "worst grade wins, and a human grading is one voice — it can give quorum \
             and raise, never suppress a tool's BLOCK (ADR-001 q5)",
        ));
        self.set(GRADERS, graders);

        // ---- what is connected ---------------------------------------------
        let state = setup::state(&config.repo);
        let mut sources = vec![
            Line::from(vec![
                theme::plain("  "),
                theme::dim(format!("{:<10}", "repo")),
                theme::plain(format!("[{}] {}   ", config.repo.name, config.repo.path)),
                match state.complete() {
                    true => theme::tinted(theme::OK, "✓ serving"),
                    false => theme::tinted(
                        theme::WARN,
                        format!("missing {}", state.missing().join(" + ")),
                    ),
                },
            ]),
            Line::from(vec![
                theme::plain("  "),
                theme::dim(format!("{:<10}", "guards")),
                mark("pacman.conf", state.conf_section),
                theme::plain("  "),
                mark("hook", state.hook),
                theme::plain("  "),
                mark("script", state.script),
                theme::plain("  "),
                mark("repo db", state.repo_db),
            ]),
            field(
                "serving",
                match &config.repo.server {
                    Some(url) => format!("{url} — signatures required (ADR-001 q6)"),
                    None => "local only — nothing crosses a network".to_string(),
                },
            ),
            field("store", ctx.store.display().to_string()),
            field("host", ctx.host.clone()),
            field("config", config_path()),
            field("state", state_path()),
        ];
        match push::status_line() {
            Ok(Some(line)) => sources.push(Line::from(vec![
                theme::plain("  "),
                theme::dim(format!("{:<10}", "aur ssh")),
                theme::tinted(theme::WARN, line),
            ])),
            Ok(None) => sources.push(field("aur ssh", "nothing queued to publish")),
            Err(e) => sources.push(bad(format!("publish queue: {e}"))),
        }
        if !state.complete() {
            sources.push(Line::default());
            sources.push(note(
                "`pacrat setup` prints what is missing and the commands for it",
            ));
        }
        self.set(SOURCES, sources);

        // The one thing this screen cannot answer without asking the
        // network, and it does not ask: an RPC probe on every load of a
        // configuration screen would be a round trip nobody requested.
        if let Some(region) = self.panes.region_mut(SOURCES) {
            region.set_title("sources — what pacrat is connected to");
        }
    }

    fn set(&mut self, index: usize, lines: Vec<Line<'static>>) {
        if let Some(region) = self.panes.region_mut(index) {
            region.set_lines(lines);
        }
    }
}

/// The mockup's boxes, as the glyphs it draws them with.
const BOXES: &[&str] = &[
    "┌────────┐    ┌────────┐    ┌────────┐    ┌────────┐    ┌────────┐",
    "│ detect │───▶│ grade  │───▶│ decide │───▶│ build  │───▶│ serve  │",
    "└────────┘    └────────┘    └────────┘    └────────┘    └────────┘",
];

/// What each preset means at the three gates, in `Mode`'s own words rather
/// than a second table that could disagree with it.
fn gates(mode: Mode) -> String {
    match mode {
        Mode::Manual => "grade verbose · decide manual · build manual — asks about everything",
        Mode::Semi => "grade auto · decide manual · build auto — asks when it is not clean",
        Mode::Auto => "grade auto · decide auto · build auto — never asks; anything unclean holds",
    }
    .to_string()
}

fn mark(label: &str, present: bool) -> ratatui::text::Span<'static> {
    theme::tinted(
        match present {
            true => theme::OK,
            false => theme::WARN,
        },
        format!("{} {label}", if present { "✓" } else { "✗" }),
    )
}

/// Where the two host-local paths are, as the reader would type them.
fn config_path() -> String {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(base) => format!(
            "{}/pacrat/config.toml",
            std::path::Path::new(&base).display()
        ),
        None => "~/.config/pacrat/config.toml".to_string(),
    }
}

fn state_path() -> String {
    crate::ctx::state_dir().map_or_else(|e| e, |p| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every preset says what it does at all three gates, and no two say the
    /// same thing. The screen's whole claim is that the word in the config
    /// file has consequences a reader can see before they change it.
    #[test]
    fn each_preset_describes_all_three_gates_distinctly() {
        let mut seen: Vec<String> = Vec::new();
        for mode in PRESETS {
            let text = gates(mode);
            for gate in ["grade", "decide", "build"] {
                assert!(text.contains(gate), "{mode} says nothing about {gate}");
            }
            assert!(
                !seen.contains(&text),
                "{mode} reads the same as another preset"
            );
            seen.push(text);
        }
        // And the three presets are the three `Mode`s — a fourth added to
        // core and forgotten here would be a setting with no row.
        assert_eq!(PRESETS.len(), 3);
        for mode in [Mode::Auto, Mode::Semi, Mode::Manual] {
            assert!(PRESETS.contains(&mode), "{mode} has no preset row");
        }
    }

    /// Only auto answers the decide gate by itself, and nothing answers a
    /// BLOCK — the invariant no preset changes.
    #[test]
    fn only_auto_decides_without_asking() {
        assert!(gates(Mode::Auto).contains("decide auto"));
        assert!(gates(Mode::Semi).contains("decide manual"));
        assert!(gates(Mode::Manual).contains("decide manual"));
        assert!(
            gates(Mode::Auto).contains("holds"),
            "the preset that never asks must still say what it does with an \
             unclean answer"
        );
    }

    /// The policy region asks for exactly the rows it puts in itself.
    ///
    /// Pinned against the real line count, in both states, because the
    /// hand-maintained version of this number was wrong — nine for a block
    /// of eight — and the old test could not see it: it asserted that the
    /// number grew when the preview block appeared, which nine also does.
    #[test]
    fn the_policy_region_asks_for_exactly_the_rows_it_uses() {
        for (configured, previewing) in [(Mode::Semi, Mode::Semi), (Mode::Semi, Mode::Auto)] {
            let mut screen = Config::new();
            let ctx = ctx_with(configured);
            screen.load(&ctx);
            while PRESETS[screen.previewing] != previewing {
                screen.preview(true);
                screen.load(&ctx);
            }

            let region = screen.panes.region(POLICY).unwrap();
            assert_eq!(
                region.height(),
                Constraint::Length(region.line_count() as u16),
                "previewing {previewing} on a host configured {configured}: the \
                 region asked for a different number of rows than it holds"
            );
            // And the preview really does add its block, so the two cases
            // above are two cases.
            let expected = if previewing == configured { 4 } else { 8 };
            assert_eq!(region.line_count(), expected);
        }
    }

    /// A `Ctx` with no store behind it: `load` reads the config for the
    /// policy region and only touches the filesystem for the sources region,
    /// which is allowed to come back empty.
    fn ctx_with(mode: Mode) -> Ctx {
        Ctx {
            store: std::env::temp_dir().join("pacrat-config-screen-test"),
            host: "north".into(),
            config: pacrat_core::config::Config {
                update_mode: mode,
                ..Default::default()
            },
        }
    }
}
