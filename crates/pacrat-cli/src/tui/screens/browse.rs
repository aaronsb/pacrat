//! `[2] browse` — mockup §4. What exists, and what our relationship to it is.
//!
//! The CLI twins are `pacrat search <term>` and `pacrat info <package>`, and
//! this screen is those two side by side: the table `search` prints, with
//! `info`'s answer for whichever row the cursor is on. The queries are the
//! same ones — `pacman -Ss`, the AUR RPC, `custody::Index` — because the two
//! surfaces are allowed to disagree about layout and never about facts.
//!
//! **The detail pane is identical for every custody state.** That is the
//! mockup's sentence and `info`'s module note both; an unmanaged package
//! gets the same rows as a maintained one, with an em dash where an answer
//! does not exist, so that comparing two packages is never comparing two
//! shapes.
//!
//! ## Two depths, because one of them costs a round trip
//!
//! Moving the cursor shows what the search and the custody index already
//! know: name, version, source, rung, which hosts track it, whether it is
//! installed here. That is free — it is in memory before the cursor moves.
//!
//! `enter` asks the three sources `info` asks (`pacman -Si`, `pacman -Qi`,
//! the RPC) for that one package. Doing it per cursor move would put a
//! network round trip on the `j` key, and a list that stutters is a list
//! nobody scrolls.

use pacrat_core::Custody;
use ratatui::layout::Constraint;
use ratatui::text::Line;

use crate::aur::{self, AurPkg};
use crate::ctx::Ctx;
use crate::custody::{self, Index};
use crate::out::{list_preview, short_hash, truncate, visible_line};
use crate::pacman::{self, field as pacfield, SyncHit};
use crate::tui::theme;
use crate::tui::viewport::{Panes, Region};

use super::{bad, command, field, note, refreshing};

const QUERY: usize = 0;
const RESULTS: usize = 1;
const DETAIL: usize = 2;

/// Column widths. Fixed rather than fitted to the rows: the cursor moves
/// between loads, and a table whose columns jumped every time a search
/// returned a longer name would be a table the eye has to re-find.
const NAME_W: usize = 28;
const VER_W: usize = 16;
const SRC_W: usize = 8;

/// How many hits each world contributes, matching `search`'s own caps —
/// the TUI scrolls rather than truncating, but a broad term against the sync
/// dbs really does return thousands and the RPC should not be asked to
/// serialize them.
const CAP: usize = 200;

struct Row {
    name: String,
    version: String,
    source: String,
    custody: Option<Custody>,
}

pub struct Browse {
    pub panes: Panes,
    loaded: bool,
    /// What was searched for. Empty until the reader types one.
    term: String,
    rows: Vec<Row>,
    index: Option<Index>,
    /// What a half of the search could not answer. Either world may fail
    /// without costing the other, so this is a warning beside a table rather
    /// than an error instead of one.
    trouble: Vec<String>,
    /// Set when the cursor moved, so the cheap half of the detail pane is
    /// rebuilt on the next frame without re-running any query.
    restate: bool,
}

impl Browse {
    pub fn new() -> Self {
        Self {
            panes: Panes::new(vec![
                Region::new("search", Constraint::Length(4), refreshing()),
                Region::table("results", Constraint::Fill(4), vec![header()], Vec::new()),
                Region::new("detail", Constraint::Fill(5), refreshing()),
            ]),
            loaded: false,
            term: String::new(),
            rows: Vec::new(),
            index: None,
            trouble: Vec::new(),
            restate: false,
        }
    }

    pub fn needs_load(&self) -> bool {
        !self.loaded || self.restate
    }

    pub fn reload(&mut self) {
        self.loaded = false;
        for index in [QUERY, DETAIL] {
            if let Some(region) = self.panes.region_mut(index) {
                region.set_lines(refreshing());
            }
        }
    }

    /// The term the reader typed. Searching happens on the next load, so the
    /// frame that says "searching…" is drawn before the RPC is asked.
    pub fn search_for(&mut self, term: &str) {
        self.term = term.trim().to_string();
        self.loaded = false;
    }

    pub fn selected(&self) -> Option<&str> {
        let cursor = self.panes.region(RESULTS)?.cursor()?;
        self.rows.get(cursor).map(|row| row.name.as_str())
    }

    /// The cursor moved. Cheap: nothing is asked of the system, the detail
    /// pane is only rebuilt from what the last search already returned.
    pub fn moved(&mut self) {
        self.restate = true;
    }

    pub fn load(&mut self, ctx: &Ctx) {
        let first = !self.loaded;
        self.loaded = true;
        self.restate = false;

        if first {
            // The index is this host's whole answer to "what is our
            // relationship to it": the ledger, every host's tracked lists,
            // and the live db. One build, asked per row.
            match Index::build(ctx) {
                Ok(index) => self.index = Some(index),
                Err(e) => {
                    self.set(DETAIL, vec![bad(format!("custody index: {e}"))]);
                    self.index = None;
                }
            }
            if !self.term.is_empty() {
                self.run_search();
            }
        }
        self.set_query();
        self.restate_detail();
    }

    /// Both worlds, either of which may fail without costing the other —
    /// `search`'s rule, for the same reason: half an answer that already
    /// cost a round trip should not be thrown away because the other half
    /// failed afterwards.
    fn run_search(&mut self) {
        let term = self.term.clone();
        let index = self.index.as_ref();
        let mut rows: Vec<Row> = Vec::new();
        let mut trouble: Vec<String> = Vec::new();

        match pacman::search(&term) {
            Ok(hits) => rows.extend(hits.iter().take(CAP).map(|hit| repo_row(hit, index))),
            Err(e) => trouble.push(format!("the official-repo search failed ({e})")),
        }
        match aur::search(&term) {
            Ok(hits) => {
                let mut ranked: Vec<&AurPkg> = hits.iter().collect();
                ranked.sort_by(|a, b| {
                    b.popularity
                        .unwrap_or(0.0)
                        .total_cmp(&a.popularity.unwrap_or(0.0))
                        .then_with(|| a.name.cmp(&b.name))
                });
                rows.extend(ranked.into_iter().take(CAP).map(|p| aur_row(p, index)));
            }
            Err(e) => trouble.push(format!("the AUR search failed ({e})")),
        }
        // The name that was typed goes first whatever else sorts, and the
        // sort is stable so each world keeps its own ranking underneath.
        rows.sort_by_key(|row| row.name != term);

        self.rows = rows;
        let lines: Vec<Line<'static>> = self.rows.iter().map(row_line).collect();
        if let Some(region) = self.panes.region_mut(RESULTS) {
            region.set_rows(lines);
            region.set_title(match self.rows.len() {
                0 => "results — nothing matched".to_string(),
                n => format!("results — {n} across both worlds"),
            });
        }
        self.trouble = trouble;
    }

    fn set_query(&mut self) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        if self.term.is_empty() {
            lines.push(note("nothing searched for yet — press / to type a term"));
            lines.push(note("searches both the sync databases and the AUR RPC"));
        } else {
            lines.push(Line::from(vec![
                theme::plain("  "),
                theme::dim("search  "),
                theme::tinted(theme::INFO, visible_line(&self.term).0),
            ]));
            lines.push(note(format!("  {}", pacman::search_argv(&self.term))));
            lines.push(note(format!(
                "  {}",
                aur::argv(&aur::search_url(&self.term))
            )));
        }
        for e in &self.trouble {
            lines.push(bad(format!("warning: {e}")));
        }
        self.set(QUERY, lines);
    }

    /// The cheap half of the detail pane: everything the search and the
    /// index already returned.
    fn restate_detail(&mut self) {
        let Some(row) = self.selected_row() else {
            let lines = match self.term.is_empty() {
                true => vec![
                    Line::default(),
                    note("/ searches the official repos and the AUR at once."),
                    note("Every source fills in the same columns, with an em dash"),
                    note("where it has no answer — so any two packages can be"),
                    note("compared like for like."),
                    Line::default(),
                    note("enter  everything pacrat knows about the selected package"),
                    note("v · t · i  vendor · track · install"),
                ],
                false => vec![Line::default(), note("no row selected")],
            };
            self.set(DETAIL, lines);
            return;
        };

        let name = row.name.clone();
        let version = row.version.clone();
        let source = row.source.clone();
        let rung = row.custody;
        let mut lines = vec![
            field("name", visible_line(&name).0),
            field("version", visible_line(&version).0),
            field("source", visible_line(&source).0),
        ];
        lines.extend(self.custody_lines(&name, rung));
        lines.push(Line::default());
        lines.push(note(
            "enter — ask the sync db, the local db and the AUR RPC",
        ));
        self.set(DETAIL, lines);
        if let Some(region) = self.panes.region_mut(DETAIL) {
            region.set_title(format!(
                "{} — what pacrat already knows",
                truncate(&name, 40)
            ));
        }
    }

    /// The custody rows, which are the ones this screen exists for.
    fn custody_lines(&self, package: &str, rung: Option<Custody>) -> Vec<Line<'static>> {
        let Some(index) = self.index.as_ref() else {
            return vec![field("state", "unknown — the custody index did not build")];
        };
        let mut lines = vec![Line::from(vec![
            theme::plain("  "),
            theme::dim(format!("{:<10}", "state")),
            theme::tinted(
                match rung {
                    Some(Custody::Vendored | Custody::Maintained) => theme::ACCENT,
                    Some(_) => theme::INFO,
                    None => theme::DIM,
                },
                match rung {
                    Some(_) => custody::label(rung).to_string(),
                    None => "unmanaged — nothing here knows it".to_string(),
                },
            ),
        ])];
        if let Some(entry) = index.ledger_entry(package) {
            lines.push(field("upstream", visible_line(&entry.upstream).0));
            lines.push(field("reviewed", short_hash(&entry.reviewed).to_string()));
            if let Some(rejected) = &entry.rejected {
                lines.push(field(
                    "rejected",
                    format!(
                        "{} · {}",
                        short_hash(rejected),
                        entry.rejected_note.as_deref().map_or(
                            "(no reason recorded)".to_string(),
                            |n| truncate(&visible_line(n).0, 60)
                        )
                    ),
                ));
            }
        }
        let hosts = index.hosts_tracking(package);
        lines.push(field(
            "tracked",
            match hosts.is_empty() {
                true => "no host tracks it".to_string(),
                false => list_preview(hosts, 8),
            },
        ));
        lines.push(field(
            "installed",
            match index.is_installed(package) {
                true => "yes, on this host".to_string(),
                false => "not on this host".to_string(),
            },
        ));
        lines
    }

    /// `enter`: the three sources `pacrat info` merges, for one package.
    pub fn detail(&mut self, ctx: &Ctx) {
        let Some(package) = self.selected().map(str::to_string) else {
            return;
        };
        let rung = self.selected_row().and_then(|row| row.custody);

        let sync = pacman::sync_info(&package);
        let local = pacman::local_info(&package);
        let (remote, rpc_error) = match aur::info(&package) {
            Ok(hits) => (hits.into_iter().next(), None),
            Err(e) => (None, Some(e)),
        };

        let pick = |fields: Option<&std::collections::BTreeMap<String, String>>, key: &str| {
            fields
                .and_then(|f| pacfield(f, key))
                .map(|v| visible_line(v).0)
        };
        let dash = |v: Option<String>| v.unwrap_or_else(|| "—".to_string());

        let mut lines = vec![
            field("name", visible_line(&package).0),
            field(
                "version",
                dash(
                    pick(sync.as_ref(), "Version")
                        .or_else(|| remote.as_ref().map(|p| visible_line(&p.version).0))
                        .or_else(|| pick(local.as_ref(), "Version")),
                ),
            ),
            field(
                "summary",
                dash(
                    pick(sync.as_ref(), "Description")
                        .or_else(|| {
                            remote
                                .as_ref()
                                .and_then(|p| p.description.as_ref())
                                .map(|d| visible_line(d).0)
                        })
                        .or_else(|| pick(local.as_ref(), "Description")),
                ),
            ),
            field(
                "upstream",
                dash(
                    pick(sync.as_ref(), "URL")
                        .or_else(|| {
                            remote
                                .as_ref()
                                .and_then(|p| p.url.as_ref())
                                .map(|u| visible_line(u).0)
                        })
                        .or_else(|| pick(local.as_ref(), "URL")),
                ),
            ),
        ];

        if let Some(p) = &remote {
            let mut parts = vec![format!(
                "maintainer {}",
                p.maintainer
                    .as_deref()
                    .map_or("orphaned".to_string(), |m| visible_line(m).0)
            )];
            if let Some(v) = p.votes {
                parts.push(format!("votes {v}"));
            }
            if let Some(pop) = p.popularity {
                parts.push(format!("popularity {pop:.2}"));
            }
            if let Some(t) = p.last_modified {
                parts.push(format!("updated {}", crate::out::epoch_date(t)));
            }
            lines.push(field("aur", parts.join(" · ")));
            if let Some(t) = p.out_of_date {
                lines.push(Line::from(vec![
                    theme::plain("  "),
                    theme::dim(format!("{:<10}", "")),
                    theme::tinted(
                        theme::WARN,
                        format!("FLAGGED out of date {}", crate::out::epoch_date(t)),
                    ),
                ]));
            }
        }
        if let Some(e) = &rpc_error {
            lines.push(field(
                "aur",
                format!("unknown — the RPC could not be asked ({e})"),
            ));
        }
        // Neutered like every other field read out of `pacman -Qi`. These
        // two came in through the same parser as the rows above and were
        // the only ones reaching a Span raw — a version string is not where
        // anybody expects an escape, which is the reason to be consistent
        // rather than to judge field by field.
        lines.push(field(
            "install",
            match &local {
                Some(f) => format!(
                    "{} · installed {}",
                    dash(pick(Some(f), "Version")),
                    dash(pick(Some(f), "Install Date"))
                ),
                None => "not installed on this host".to_string(),
            },
        ));

        lines.push(Line::default());
        lines.extend(self.custody_lines(&package, rung));

        // Risks a human accepted about this package. Silent when there are
        // none: an empty row about overrides nobody made would put the idea
        // in a reader's head every time they looked at a package.
        match ctx.load_decisions() {
            Ok(ledger) => {
                for d in ledger.about(&package) {
                    lines.push(field(
                        "decision",
                        format!(
                            "{} · {} · {} on {} · {}",
                            d.kind,
                            short_hash(&d.commit),
                            truncate(&visible_line(&d.at).0, 20),
                            truncate(&visible_line(&d.host).0, 24),
                            truncate(&visible_line(&d.reason).0, 60),
                        ),
                    ));
                }
            }
            Err(e) => lines.push(bad(format!("decision ledger: {e}"))),
        }

        self.set(DETAIL, lines);
        if let Some(region) = self.panes.region_mut(DETAIL) {
            region.set_title(format!(
                "{} — everything pacrat knows",
                truncate(&package, 40)
            ));
        }
        self.panes.focus_on(DETAIL);
    }

    /// `v` · `t` · `i`: the command that climbs the ladder a rung.
    pub fn suggest(&mut self, verb: Rung) {
        let Some(package) = self.selected().map(str::to_string) else {
            self.set(DETAIL, vec![Line::default(), note("no row selected")]);
            return;
        };
        let quoted = crate::out::shell_quote(&package);
        let (why, argv) = match verb {
            Rung::Vendor => (
                "vendoring clones the upstream, shows you the tree, and asks before \
                 it commits anything to the store — a review that needs your eyes \
                 and your answer.",
                vec![format!("pacrat vendor {quoted}")],
            ),
            Rung::Track => (
                "tracking records a package this host already has into the store's \
                 manifest. It installs nothing.",
                vec![format!("pacrat add {quoted}")],
            ),
            Rung::Install => (
                "pacrat never installs and never elevates. A vendored package comes \
                 from [dotfiles-aur] like anything else pacman serves.",
                vec![format!("sudo pacman -Sy {quoted}")],
            ),
        };
        let lines = command(why, &argv);
        self.set(DETAIL, lines);
        if let Some(region) = self.panes.region_mut(DETAIL) {
            region.set_title(format!("{} — the command", truncate(&package, 40)));
        }
        self.panes.focus_on(DETAIL);
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.panes.region(RESULTS)?.cursor()?)
    }

    fn set(&mut self, index: usize, lines: Vec<Line<'static>>) {
        if let Some(region) = self.panes.region_mut(index) {
            region.set_lines(lines);
        }
    }
}

/// Which rung the reader asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    Vendor,
    Track,
    Install,
}

fn header() -> Line<'static> {
    Line::from(vec![theme::dim(format!(
        "{:<NAME_W$} {:<VER_W$} {:<SRC_W$} {}",
        "name", "ver", "source", "state"
    ))])
}

fn row_line(row: &Row) -> Line<'static> {
    Line::from(vec![
        theme::plain(format!(
            "{:<NAME_W$} ",
            truncate(&visible_line(&row.name).0, NAME_W)
        )),
        theme::dim(format!(
            "{:<VER_W$} ",
            truncate(&visible_line(&row.version).0, VER_W)
        )),
        theme::dim(format!(
            "{:<SRC_W$} ",
            truncate(&visible_line(&row.source).0, SRC_W)
        )),
        theme::tinted(
            match row.custody {
                Some(Custody::Vendored | Custody::Maintained) => theme::ACCENT,
                Some(_) => theme::INFO,
                None => theme::DIM,
            },
            custody::label(row.custody).to_string(),
        ),
    ])
}

fn repo_row(hit: &SyncHit, index: Option<&Index>) -> Row {
    Row {
        custody: index.and_then(|i| i.custody(&hit.name)),
        name: hit.name.clone(),
        version: hit.version.clone(),
        source: hit.repo.clone(),
    }
}

fn aur_row(pkg: &AurPkg, index: Option<&Index>) -> Row {
    Row {
        custody: index.and_then(|i| i.custody(&pkg.name)),
        name: pkg.name.clone(),
        version: pkg.version.clone(),
        source: "aur".into(),
    }
}
