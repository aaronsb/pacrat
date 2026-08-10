//! The five screens that are not built yet.
//!
//! They exist so the shell's navigation is complete and honest: a tab that
//! opens onto a working, focusable, scrollable region which says what will
//! land there, which task brings it, and what to type today instead. An
//! empty pane would test the same navigation while telling the reader
//! nothing, and a screen that pretended to have data would be worse than
//! either.

use ratatui::layout::Constraint;
use ratatui::text::Line;

use crate::ctx::Ctx;
use crate::setup;
use crate::tui::theme;
use crate::tui::viewport::{Panes, Region};
use crate::tui::Tab;

/// The task that turns these five into real screens.
const SCREENS_TASK: &str = "task #20";

/// Where the config screen's live values live, when it has them.
const SETTINGS: usize = 1;

pub fn build(tab: Tab, ctx: &Ctx) -> Panes {
    let mut regions = vec![Region::new(
        format!("{} — {}", tab.title(), tab.question()),
        Constraint::Fill(3),
        lands_here(tab),
    )];
    // Cheap, read-only, and already loaded: the config screen can show the
    // real gate numbers today even though editing them is task #20's job.
    if tab == Tab::Config {
        regions.push(Region::new(
            "this host's settings — read-only until the gate editor lands",
            Constraint::Fill(2),
            settings(ctx),
        ));
    }
    Panes::new(regions)
}

/// `r` on one of these screens.
///
/// Only the lines are replaced, never the `Panes`. Rebuilding the screen
/// wholesale is the obvious way to refresh it and it quietly throws away
/// everything the reader had arranged — which region had focus, how far
/// each was scrolled — so a key that means "ask again" would also mean
/// "start over". With one region and two of them static that costs nothing
/// today; with task #20's five-region screens it would be the difference
/// between a usable refresh and one nobody presses twice.
pub fn refresh(panes: &mut Panes, tab: Tab, ctx: &Ctx) {
    // Everything else on these screens is prose about work not yet done.
    // It cannot have changed since the last frame, so re-rendering it would
    // be motion without information.
    if tab == Tab::Config {
        if let Some(region) = panes.region_mut(SETTINGS) {
            region.set_lines(settings(ctx));
        }
    }
}

fn lands_here(tab: Tab) -> Vec<Line<'static>> {
    let (bullets, twins): (&[&str], &[&str]) = match tab {
        // Unreachable in practice — the overview is a real screen — but a
        // total match is one less thing to keep in step.
        Tab::Overview => (&["already here"], &["pacrat status"]),
        Tab::Browse => (
            &[
                "custody-aware search across the official repos and the AUR",
                "a detail pane identical at every rung of the ladder:",
                "description, upstream, PKGBUILD summary, install dates, grade",
                "v vendor · t track · a grade · enter for the full PKGBUILD",
            ],
            &["pacrat search <term>", "pacrat info <package>"],
        ),
        Tab::Updates => (
            &[
                "the centrepiece: what changed upstream beside what the",
                "graders said about it — diff since the reviewed commit and",
                "the grading, on one screen, with the grader argv verbatim",
                "a adopt+build · d defer · x reject · g regrade",
                "an upstream region for tracked-but-not-vendored drift",
            ],
            &[
                "pacrat updates",
                "pacrat review <package>",
                "pacrat adopt-update <package>",
            ],
        ),
        Tab::Hosts => (
            &[
                "the n-way matrix: every package against every host, with",
                "✓ installed · ✗ missing · + unmanaged · — not in profile",
                "multi-select adopt, for burning down the unmanaged backlog",
                "profile membership edited here, not in a config file",
            ],
            &["pacrat hosts", "pacrat sync"],
        ),
        Tab::Jobs => (
            &[
                "what pacrat is running right now, argv verbatim, with the",
                "full log one keypress away — gradings, builds, repo-adds",
                "the publish queue and its hourly AUR ssh probe",
                "jobs live only as long as this process does (ADR-001 q3)",
            ],
            &["pacrat build", "pacrat push <package>"],
        ),
        Tab::Config => (
            &[
                "the pipeline drawn — detect → grade → decide → build → serve",
                "with each gate auto / verbose / manual and presets over them",
                "grader and source connection status, guards, thresholds",
                "two invariants no preset changes: BLOCK always holds, and",
                "every external call is logged",
            ],
            &["pacrat config", "pacrat setup"],
        ),
    };

    let mut lines = vec![
        Line::default(),
        Line::from(vec![
            theme::plain("   "),
            theme::dim("not built yet — arrives with "),
            theme::accent(SCREENS_TASK),
        ]),
        Line::default(),
    ];
    lines.extend(bullets.iter().map(|bullet| {
        Line::from(vec![
            theme::plain("     "),
            theme::dim("· "),
            theme::plain(bullet.to_string()),
        ])
    }));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        theme::plain("   "),
        theme::dim("today, from the CLI:"),
    ]));
    lines.extend(twins.iter().map(|twin| {
        Line::from(vec![
            theme::plain("     "),
            theme::tinted(theme::INFO, twin.to_string()),
        ])
    }));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        theme::plain("   "),
        theme::dim("every screen action has a CLI twin — that is the contract, "),
        theme::dim("mockup §11"),
    ]));
    lines
}

/// Real values, read from the config this process already parsed. Nothing
/// here queries anything; the one filesystem probe (`setup::state`) is the
/// same one the overview and `pacrat setup` share.
fn settings(ctx: &Ctx) -> Vec<Line<'static>> {
    let config = &ctx.config;
    let state = setup::state(&config.repo);
    let mut lines = vec![
        field(
            "default_ui",
            format!("{:?}", config.default_ui).to_lowercase(),
        ),
        field(
            "thresholds",
            format!(
                "warn at grade {} · block at grade {}",
                config.thresholds.warn_at, config.thresholds.block_at
            ),
        ),
        field(
            "repo",
            format!("[{}] {}", config.repo.name, config.repo.path),
        ),
        field(
            "serving",
            match &config.repo.server {
                Some(url) => format!("{url} (signatures required — ADR-001 q6)"),
                None => "local only".into(),
            },
        ),
        field(
            "guards",
            format!(
                "pacman.conf {} · hook {} · script {}",
                yesno(state.conf_section),
                yesno(state.hook),
                yesno(state.script)
            ),
        ),
        Line::default(),
    ];
    if config.graders.is_empty() {
        lines.push(field(
            "graders",
            "none configured — `manual` is always on, and ungraded holds".into(),
        ));
        return lines;
    }
    lines.push(field("graders", String::new()));
    for grader in &config.graders {
        lines.push(Line::from(vec![
            theme::plain("     "),
            theme::tinted(theme::INFO, format!("{:<16}", grader.name)),
            theme::dim(format!("timeout {}s", grader.timeout_s)),
            theme::dim(match grader.scale {
                Some(scale) => format!(" · scale pinned {}-{}", scale.min, scale.max),
                None => " · scale unpinned".into(),
            }),
        ]));
        lines.push(Line::from(vec![
            theme::plain("       "),
            theme::dim(format!("$ {}", grader.cmd.join(" "))),
        ]));
    }
    lines
}

fn field(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        theme::plain("   "),
        theme::dim(format!("{label:<12}")),
        theme::plain(value),
    ])
}

fn yesno(present: bool) -> &'static str {
    match present {
        true => "✓",
        false => "✗",
    }
}
