//! `[1] overview` — mockup §3. The landing screen is a triage list, not a
//! dashboard of vanity numbers: every attention line names the screen or the
//! command that resolves it.
//!
//! It renders what pacrat already knows and computes nothing of its own —
//! `setup::state` for the repo line, `ctx.load_sources` for the ledger,
//! `ctx.tracked` for the lists, `live::host_drift` for the drift. Those are
//! the same calls `pacrat status` makes, deliberately: the CLI twin and the
//! screen have to be able to disagree about *layout* and never about facts.

use pacrat_core::pkg::Source;
use pacrat_core::sources::Role;
use ratatui::layout::Constraint;
use ratatui::text::{Line, Span};

use crate::ctx::Ctx;
use crate::live;
use crate::out::{list_preview, truncate};
use crate::setup;
use crate::tui::theme;
use crate::tui::viewport::{Panes, Region};

const STORE: usize = 0;
const ATTENTION: usize = 1;
const TRACKED: usize = 2;
const DRIFT: usize = 3;

/// The most rows the attention list may claim before it starts scrolling
/// like every other region.
const ATTENTION_CAP: usize = 8;

pub struct Overview {
    pub panes: Panes,
    loaded: bool,
}

impl Overview {
    pub fn new() -> Self {
        let refreshing = || {
            vec![Line::from(vec![
                theme::plain("  "),
                theme::dim("refreshing…"),
            ])]
        };
        // Body rows; `Panes` reserves each region's title rule on top.
        //
        // The header asks for exactly its four fields, the two lists divide
        // whatever is left, and attention is re-sized at load time once its
        // length is known — it is the region the screen exists for, and it
        // should not be the one scrolling while a host list has rows to
        // spare.
        Self {
            panes: Panes::new(vec![
                Region::new("store & ledger", Constraint::Length(4), refreshing()),
                Region::new("attention", Constraint::Length(1), refreshing()),
                Region::new("tracked lists", Constraint::Fill(4), refreshing()),
                Region::new("drift", Constraint::Fill(5), refreshing()),
            ]),
            loaded: false,
        }
    }

    /// True until [`Overview::load`] has run. The app draws a frame in this
    /// state before loading, which is what makes the wait visible instead of
    /// looking like a hang: the queries shell out to pacman, and pacman is
    /// not instant on a machine with two thousand packages.
    pub fn needs_load(&self) -> bool {
        !self.loaded
    }

    /// `r`. Puts every region back to "refreshing…" so the reader can see
    /// that the key did something, then lets the next frame do the work.
    pub fn reload(&mut self) {
        self.loaded = false;
        for index in [STORE, ATTENTION, TRACKED, DRIFT] {
            if let Some(region) = self.panes.region_mut(index) {
                region.set_lines(vec![Line::from(vec![
                    theme::plain("  "),
                    theme::dim("refreshing…"),
                ])]);
            }
        }
    }

    /// Ask the system, once. Every query here can fail and none of them may
    /// take the TUI down with them — a machine without `flatpak`, a store
    /// with a hand-broken list, a `sources.toml` mid-merge-conflict are all
    /// things to *report on the screen*, because the screen is where the
    /// person who can fix them is looking.
    ///
    /// Nearly every string below arrives from outside pacrat: package names
    /// out of the store's lists, paths out of the config, error text out of
    /// `pacman`. None of it is run through `out::visible` on the way into a
    /// `Span`, and the reason is that ratatui's buffer already refuses to
    /// place control characters and zero-width graphemes — see the note in
    /// `viewport::Region::render`, which is also where the condition on that
    /// borrowed guarantee is written down.
    pub fn load(&mut self, ctx: &Ctx) {
        self.loaded = true;
        let mut attention: Vec<Line<'static>> = Vec::new();

        let setup_state = setup::state(&ctx.config.repo);
        let sources = ctx.load_sources();
        let hosts = ctx.tracked_hosts();
        let this_host_tracked = hosts.iter().any(|host| host == &ctx.host);

        // ---- store ----------------------------------------------------
        let repo = &ctx.config.repo;
        let repo_state = if setup_state.complete() {
            theme::tinted(theme::OK, "  ✓ serving")
        } else if setup_state.untouched() {
            theme::tinted(theme::WARN, "  not set up")
        } else {
            theme::tinted(
                theme::WARN,
                format!("  missing {}", setup_state.missing().join(" + ")),
            )
        };
        let ledger = match &sources {
            Ok(sources) if sources.packages.is_empty() => {
                vec![theme::dim("nothing vendored yet")]
            }
            Ok(sources) => vec![
                theme::plain("vendored "),
                theme::tinted(theme::ACCENT, sources.count(Role::Vendored).to_string()),
                theme::dim(" · "),
                theme::plain("maintained "),
                theme::tinted(theme::ACCENT, sources.count(Role::Maintained).to_string()),
            ],
            Err(e) => vec![theme::tinted(theme::BAD, truncate(e, 60))],
        };
        self.set(
            STORE,
            vec![
                field("store", vec![theme::tinted(theme::INFO, path(&ctx.store))]),
                field("host", vec![theme::tinted(theme::INFO, ctx.host.clone())]),
                field(
                    "repo",
                    vec![
                        theme::plain(format!("[{}] {}", repo.name, repo.path)),
                        repo_state,
                    ],
                ),
                field("ledger", ledger),
            ],
        );

        if !setup_state.complete() {
            let what = match setup_state.untouched() {
                true => "this host is not serving [".to_string() + &repo.name + "] yet",
                false => format!(
                    "[{}] is partly set up: no {}",
                    repo.name,
                    setup_state.missing().join(", no ")
                ),
            };
            attention.push(line(
                theme::tinted(theme::WARN, "▲"),
                what,
                "[6] config · `pacrat setup`",
            ));
        }
        if let Err(e) = &sources {
            attention.push(line(
                theme::tinted(theme::BAD, "■"),
                format!("the ledger will not parse: {e}"),
                "aur/sources.toml",
            ));
        }

        // ---- tracked lists --------------------------------------------
        let mut tracked = vec![Line::from(vec![
            theme::plain("   "),
            theme::dim(format!("{:<12}", "host")),
            theme::dim(
                Source::ALL
                    .iter()
                    .map(|s| format!("{:>9}", s.name()))
                    .collect::<String>(),
            ),
        ])];
        for host in &hosts {
            let mine = *host == ctx.host;
            let counts: Result<String, String> = Source::ALL
                .iter()
                .map(|source| Ok(format!("{:>9}", ctx.tracked(host, *source)?.len())))
                .collect();
            let name = theme::tinted(
                if mine { theme::INFO } else { theme::DIM },
                format!("{host:<12}"),
            );
            tracked.push(Line::from(match counts {
                Ok(counts) => vec![
                    theme::tinted(theme::ACCENT, if mine { " * " } else { "   " }),
                    name,
                    theme::plain(counts),
                ],
                // A list that will not read is this host's problem to fix,
                // and naming the host beside it is half the diagnosis.
                Err(e) => vec![
                    theme::plain("   "),
                    name,
                    theme::tinted(theme::BAD, truncate(&e.replace('\n', " "), 48)),
                ],
            }));
        }
        if hosts.is_empty() {
            tracked.push(Line::from(vec![
                theme::plain("   "),
                theme::dim(format!("no hosts under {}", path(&ctx.packages_dir()))),
            ]));
            attention.push(line(
                theme::dim("○"),
                "no host is tracked in this store yet".into(),
                "`pacrat add <pkg>`",
            ));
        } else if !this_host_tracked {
            attention.push(line(
                theme::dim("○"),
                format!("{} has no tracked lists yet", ctx.host),
                "`pacrat add <pkg>`",
            ));
        }
        self.set(TRACKED, tracked);

        // ---- drift ------------------------------------------------------
        let drift = match this_host_tracked {
            true => Some(live::host_drift(ctx)),
            false => None,
        };
        let mut rows: Vec<Line<'static>> = Vec::new();
        match &drift {
            None => rows.push(Line::from(vec![
                theme::plain("   "),
                theme::dim("nothing to compare — this host has no tracked lists"),
            ])),
            Some(Err(e)) => rows.push(Line::from(vec![
                theme::plain("   "),
                theme::tinted(theme::BAD, truncate(&e.replace('\n', " "), 70)),
            ])),
            Some(Ok(sources)) => {
                let (mut missing, mut extra) = (0usize, 0usize);
                for sd in sources {
                    let d = &sd.drift;
                    missing += d.missing.len();
                    extra += d.extra.len();
                    if d.in_sync() {
                        rows.push(Line::from(vec![
                            theme::plain("   "),
                            theme::dim(format!("{:<9}", sd.source.name())),
                            theme::tinted(theme::OK, "✓ in sync"),
                            theme::dim(format!(" ({} packages)", sd.tracked.len())),
                        ]));
                        continue;
                    }
                    rows.push(Line::from(vec![
                        theme::plain("   "),
                        theme::dim(format!("{:<9}", sd.source.name())),
                        theme::tinted(theme::WARN, format!("{} missing", d.missing.len())),
                        theme::dim(" · "),
                        theme::tinted(theme::WARN, format!("{} extra", d.extra.len())),
                        theme::dim(format!("   [{}]", live::query_argv(sd.source))),
                    ]));
                    for (label, names) in [("missing", &d.missing), ("extra", &d.extra)] {
                        if !names.is_empty() {
                            rows.push(Line::from(vec![
                                theme::plain("            "),
                                theme::dim(format!("{label:<8} ")),
                                theme::plain(list_preview(names, 12)),
                            ]));
                        }
                    }
                }
                if missing + extra > 0 {
                    attention.push(line(
                        theme::tinted(theme::WARN, "▲"),
                        format!("{} missing · {} extra on {}", missing, extra, ctx.host),
                        "`pacrat sync`",
                    ));
                }
                rows.push(Line::default());
                rows.push(Line::from(vec![
                    theme::plain("   "),
                    theme::dim(
                        "`pacrat sync` turns that into commands — it prints them, runs none",
                    ),
                ]));
            }
        }
        self.set(DRIFT, rows);
        if let Some(region) = self.panes.region_mut(DRIFT) {
            region.set_title(format!("drift on {} — tracked vs installed", ctx.host));
        }

        // ---- attention ---------------------------------------------------
        if attention.is_empty() {
            attention.push(line(
                theme::tinted(theme::OK, "✓"),
                "nothing needs attention on this host".into(),
                "",
            ));
        }
        // The screens that would answer these are not built yet; saying so
        // is better than an attention list that quietly omits the two rows
        // the mockup leads with.
        attention.push(Line::default());
        attention.push(Line::from(vec![
            theme::plain("   "),
            theme::dim("pending updates and running jobs arrive with [3] and [5] — "),
            theme::dim("`pacrat updates`"),
        ]));
        // Ask for exactly the rows this holds, so a short list wastes none
        // and a long one is not the region that scrolls. Capped, because
        // "every host is drifting" must not push the lists off the screen —
        // past the cap it scrolls like anything else and says so.
        //
        // `Min`, not `Length`, and that is the whole ordering policy of this
        // screen. The two are identical while there is room; they differ
        // only when the terminal cannot seat everything, and there `Min`
        // outranks `Length` in the solver — so the rows that do exist go to
        // the triage list first and the header yields. At an inner height of
        // six this is the difference between a screen showing "▲ this host
        // is not serving [dotfiles-aur] yet" and one showing a store path
        // the reader already knows. It does not grow past its content on a
        // tall terminal, because the two `Fill` lists take the surplus.
        let wanted = attention.len().clamp(1, ATTENTION_CAP) as u16;
        self.set(ATTENTION, attention);
        if let Some(region) = self.panes.region_mut(ATTENTION) {
            region.set_height(Constraint::Min(wanted));
        }
    }

    fn set(&mut self, index: usize, lines: Vec<Line<'static>>) {
        if let Some(region) = self.panes.region_mut(index) {
            region.set_lines(lines);
        }
    }
}

/// `  label   value` — the store region's two-column shape.
fn field(label: &str, mut value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![theme::plain("  "), theme::dim(format!("{label:<8}"))];
    spans.append(&mut value);
    Line::from(spans)
}

/// One attention row: a severity mark, what is true, and where to go about
/// it. The pointer column is what makes this a triage list.
fn line(mark: Span<'static>, what: String, pointer: &str) -> Line<'static> {
    Line::from(vec![
        theme::plain("   "),
        mark,
        theme::plain(" "),
        theme::plain(format!("{:<54}", truncate(&what, 54))),
        theme::dim(match pointer.is_empty() {
            true => String::new(),
            false => format!("→ {pointer}"),
        }),
    ])
}

fn path(p: &std::path::Path) -> String {
    p.display().to_string()
}
