//! `[4] hosts` — mockup §6. Does each machine match the manifest?
//!
//! The matrix is `pkg`'s n-way diff promoted to a UI: every package the
//! store tracks anywhere, against every host that tracks anything.
//!
//! ## What a column can honestly say
//!
//! The mockup's legend is `✓ installed · ✗ missing on host · + installed,
//! unmanaged · — not in profile`, and three of those four are claims about
//! what is *installed* on a machine. pacrat can only make them about the
//! machine it is running on. ADR-001 decision 4 settles that sync is
//! self-only — each host runs pacrat for itself, the matrix is read-only
//! awareness of the others, and pacrat grows no remote-execution surface —
//! so there is no mechanism by which this screen could know what is on
//! `slab` right now, and a `✓` in slab's column would be pacrat inventing
//! one.
//!
//! So the columns say two different kinds of thing, and the legend says
//! which is which: **this host's column is manifest against reality**, with
//! the mockup's four marks; **every other column is the manifest**, `·` for
//! listed and `—` for not. That is a deviation from the mockup and it is the
//! honest reading of the same data — the alternative is a screen that looks
//! more informative than it is, about exactly the question ("is that machine
//! actually in the state I think?") a reader would most want to trust.

use std::collections::{BTreeMap, BTreeSet};

use pacrat_core::pkg::Source;
use pacrat_core::Custody;
use ratatui::layout::Constraint;
use ratatui::text::Line;

use crate::ctx::Ctx;
use crate::custody::{self, Index};
use crate::live;
use crate::out::{list_preview, shell_quote, truncate, visible_line};
use crate::tui::select::{self, Selection};
use crate::tui::theme;
use crate::tui::viewport::{Panes, Region};

use super::{bad, command, field, note, refreshing};

const MATRIX: usize = 0;
const LEGEND: usize = 1;
const DETAIL: usize = 2;

const NAME_W: usize = 28;
/// Wide enough for a hostname column that stays readable; longer names are
/// clipped rather than allowed to shear the grid.
const HOST_W: usize = 9;

/// One package's row across the fleet.
struct Row {
    package: String,
    custody: Option<Custody>,
    /// Per host, in the same order as `hosts`: is it in that host's manifest?
    tracked: Vec<bool>,
    /// Installed here. Only ever answered for this host.
    installed_here: bool,
}

pub struct Hosts {
    pub panes: Panes,
    loaded: bool,
    hosts: Vec<String>,
    /// Which of `hosts` is this machine. Held rather than passed, because
    /// the row renderer is the only thing that needs it and it is the one
    /// column allowed to make a claim about what is installed.
    this_host: String,
    rows: Vec<Row>,
    /// The marked rows. The model, the keys and the row dressing are
    /// [`select`]'s — one selection language, spoken per screen (ADR-002) —
    /// and what a mark *means* here is "this package, this host's manifest".
    selected: Selection,
}

impl Hosts {
    pub fn new() -> Self {
        Self {
            panes: Panes::new(vec![
                Region::table(
                    "matrix",
                    Constraint::Fill(6),
                    vec![Line::default()],
                    Vec::new(),
                ),
                Region::new("legend", Constraint::Length(3), refreshing()),
                Region::new("detail", Constraint::Fill(3), refreshing()),
            ]),
            loaded: false,
            hosts: Vec::new(),
            this_host: String::new(),
            rows: Vec::new(),
            selected: Selection::new(),
        }
    }

    pub fn needs_load(&self) -> bool {
        !self.loaded
    }

    pub fn reload(&mut self) {
        self.loaded = false;
        for index in [LEGEND, DETAIL] {
            if let Some(region) = self.panes.region_mut(index) {
                region.set_lines(refreshing());
            }
        }
    }

    pub fn load(&mut self, ctx: &Ctx) {
        self.loaded = true;
        self.hosts = ctx.tracked_hosts();
        self.this_host = ctx.host.clone();
        if self.hosts.is_empty() {
            self.set(
                DETAIL,
                vec![
                    Line::default(),
                    note(format!(
                        "no host lists under {}",
                        ctx.packages_dir().display()
                    )),
                    note("`pacrat add <package>` writes the first one."),
                ],
            );
            return;
        }

        // Every host's manifest, per source, unioned into one package set.
        let mut membership: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut trouble: Vec<String> = Vec::new();
        for host in &self.hosts {
            for source in Source::ALL {
                match ctx.tracked(host, source) {
                    Ok(list) => {
                        for package in list {
                            membership.entry(package).or_default().insert(host.clone());
                        }
                    }
                    // A list that will not read is this host's problem to
                    // fix, and naming it beside the matrix is half the
                    // diagnosis. The other hosts' rows are still real.
                    Err(e) => trouble.push(truncate(&e.replace('\n', " "), 160)),
                }
            }
        }

        let installed: BTreeSet<String> = [Source::Native, Source::Aur]
            .into_iter()
            .filter_map(|source| live::installed(source).ok())
            .flatten()
            .collect();
        let index = Index::build(ctx).ok();

        // Installed here but in nobody's manifest: the backlog this screen
        // exists to burn down, so it is in the matrix rather than only in a
        // count somewhere.
        let unmanaged: Vec<String> = installed
            .iter()
            .filter(|package| !membership.contains_key(*package))
            .cloned()
            .collect();

        self.rows = membership
            .keys()
            .cloned()
            .chain(unmanaged)
            .map(|package| Row {
                tracked: self
                    .hosts
                    .iter()
                    .map(|host| {
                        membership
                            .get(&package)
                            .is_some_and(|hosts| hosts.contains(host))
                    })
                    .collect(),
                installed_here: installed.contains(&package),
                custody: index.as_ref().and_then(|i| i.custody(&package)),
                package,
            })
            .collect();
        self.rows.sort_by(|a, b| a.package.cmp(&b.package));

        // A mark whose row vanished between loads goes with it; reordering
        // alone changes nothing, because the marks are names.
        self.selected
            .keep(self.rows.iter().map(|row| row.package.as_str()));

        let header = self.header();
        if let Some(region) = self.panes.region_mut(MATRIX) {
            // The header is replaced in place, never the `Region` — the
            // rule the shell's own reload path follows, and for the reason
            // it gives: assigning a fresh `Region` throws away the reader's
            // cursor and scroll position along with the stale lines, so `r`
            // would mean "ask again *and* start over" on the one screen
            // where finding your row again costs the most. It is set here
            // rather than in `repaint` because only a load can change the
            // host columns.
            region.set_header(vec![header]);
        }
        self.repaint();

        let mut legend = vec![Line::from(vec![
            theme::plain("  "),
            theme::tinted(theme::OK, "✓"),
            theme::dim(" installed   "),
            theme::tinted(theme::BAD, "✗"),
            theme::dim(" tracked here, not installed   "),
            theme::tinted(theme::WARN, "+"),
            theme::dim(" installed, unmanaged   "),
            theme::dim("— not tracked"),
        ])];
        legend.push(note(format!(
            "the {} column is the manifest against this machine; every other column \
             is the manifest only",
            ctx.host
        )));
        legend.push(note(
            "sync is self-only (ADR-001 q4) — pacrat cannot see what another host \
             has installed, and will not pretend to",
        ));
        for e in &trouble {
            legend.push(bad(e.clone()));
        }
        self.set(LEGEND, legend);
        self.restate();
    }

    fn header(&self) -> Line<'static> {
        let mut columns = String::new();
        for host in &self.hosts {
            columns.push_str(&format!("{:<HOST_W$}", truncate(host, HOST_W - 1)));
        }
        Line::from(vec![theme::dim(format!(
            "{:<NAME_W$} {columns}{}",
            "package", "state"
        ))])
    }

    fn row_line(&self, row: &Row) -> Line<'static> {
        let mut spans = vec![theme::plain(format!(
            "{:<width$} ",
            truncate(&visible_line(&row.package).0, NAME_W - 1),
            width = NAME_W - 1
        ))];
        for (index, host) in self.hosts.iter().enumerate() {
            let (mark, colour) = cell(
                host == &self.this_host,
                row.tracked.get(index).copied().unwrap_or(false),
                row.installed_here,
            );
            spans.push(theme::tinted(colour, format!("{mark:<HOST_W$}")));
        }
        spans.push(theme::tinted(
            match row.custody {
                Some(Custody::Vendored | Custody::Maintained) => theme::ACCENT,
                Some(_) => theme::INFO,
                None => theme::DIM,
            },
            match row.custody {
                Some(_) => custody::label(row.custody).to_string(),
                None => "unmanaged".to_string(),
            },
        ));
        select::decorate(Line::from(spans), self.selected.contains(&row.package))
    }

    /// The matrix rows and title, redrawn — on load, and on every change to
    /// the marks. Replaced in place, never the `Region`: `set_rows` keeps
    /// the cursor and clamps it, so marking never costs the reader their
    /// place. The title carries the selection count (ADR-002: a count in
    /// the region title, never a list in the detail pane).
    fn repaint(&mut self) {
        let rows: Vec<Line<'static>> = self.rows.iter().map(|row| self.row_line(row)).collect();
        let title = format!(
            "matrix — {} packages across {} host{}{}",
            self.rows.len(),
            self.hosts.len(),
            if self.hosts.len() == 1 { "" } else { "s" },
            self.selected.title_suffix()
        );
        if let Some(region) = self.panes.region_mut(MATRIX) {
            region.set_rows(rows);
            region.set_title(title);
        }
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.panes.region(MATRIX)?.cursor()?)
    }

    /// `space`.
    pub fn toggle(&mut self) {
        let Some(package) = self.selected_row().map(|row| row.package.clone()) else {
            return;
        };
        self.selected.toggle(&package);
        self.repaint();
        self.restate();
    }

    /// `*` — every row in the matrix.
    pub fn select_all(&mut self) {
        self.selected
            .all(self.rows.iter().map(|row| row.package.as_str()));
        self.repaint();
        self.restate();
    }

    /// `-` — none of them.
    pub fn select_none(&mut self) {
        self.selected.none();
        self.repaint();
        self.restate();
    }

    /// `!` — the marked and the unmarked trade places.
    pub fn select_invert(&mut self) {
        self.selected
            .invert(self.rows.iter().map(|row| row.package.as_str()));
        self.repaint();
        self.restate();
    }

    /// The cursor moved: only the detail pane changes, and it costs nothing.
    pub fn moved(&mut self) {
        self.restate();
    }

    fn restate(&mut self) {
        let Some(row) = self.selected_row() else {
            self.set(DETAIL, vec![Line::default(), note("no row selected")]);
            return;
        };
        let package = row.package.clone();
        let custody = row.custody;
        let installed_here = row.installed_here;
        let on: Vec<String> = self
            .hosts
            .iter()
            .zip(&row.tracked)
            .filter(|(_, tracked)| **tracked)
            .map(|(host, _)| host.clone())
            .collect();

        let mut lines = vec![
            field("package", visible_line(&package).0),
            field(
                "state",
                match custody {
                    Some(_) => custody::label(custody).to_string(),
                    None => "unmanaged — installed here, in nobody's manifest".to_string(),
                },
            ),
            field(
                "tracked on",
                match on.is_empty() {
                    true => "no host".to_string(),
                    false => list_preview(&on, 8),
                },
            ),
            field(
                "here",
                match installed_here {
                    true => "installed".to_string(),
                    false => "not installed".to_string(),
                },
            ),
        ];
        // Never the selection's ledger: the marks are a count in the matrix
        // title, and the names are what the apply step shows (ADR-002).
        lines.push(Line::default());
        lines.push(note(
            "space marks a row · A adopts the marks · s plans this host",
        ));
        self.set(DETAIL, lines);
        if let Some(region) = self.panes.region_mut(DETAIL) {
            region.set_title(truncate(&package, 40));
        }
    }

    /// `A` — bulk adopt, as the one command that does it.
    pub fn suggest_adopt(&mut self) {
        let names: Vec<String> = match self.selected.is_empty() {
            false => self.selected.names(),
            true => self
                .selected_row()
                .map(|row| vec![row.package.clone()])
                .unwrap_or_default(),
        };
        if names.is_empty() {
            return;
        }
        let quoted: Vec<String> = names.iter().map(|n| shell_quote(n)).collect();
        let lines = command(
            "adopting records packages this host already has into its manifest. It \
             installs nothing, and committing the store is what shares it with the \
             fleet.",
            &[format!("pacrat add {}", quoted.join(" "))],
        );
        self.answer(lines, format!("adopt {} package(s)", names.len()));
    }

    /// `s` — plan this host. Self-only, and the plan is commands to read.
    pub fn suggest_sync(&mut self) {
        let lines = command(
            "sync plans this machine toward the store and prints the commands — it \
             runs none of them, and pacrat never elevates. Another host's drift is \
             closed by running pacrat there (ADR-001 q4).",
            &["pacrat sync".to_string(), "pacrat sync --prune".to_string()],
        );
        self.answer(lines, "sync this host".to_string());
    }

    fn answer(&mut self, lines: Vec<Line<'static>>, what: String) {
        self.set(DETAIL, lines);
        if let Some(region) = self.panes.region_mut(DETAIL) {
            region.set_title(what);
        }
        self.panes.focus_on(DETAIL);
    }

    fn set(&mut self, index: usize, lines: Vec<Line<'static>>) {
        if let Some(region) = self.panes.region_mut(index) {
            region.set_lines(lines);
        }
    }
}

/// One cell of the matrix: what this host's column may claim, and what
/// another's may.
///
/// Pure, and separated from the drawing, because the whole argument of this
/// screen is in the six cases — three of the mockup's four marks are claims
/// about what is *installed*, and only one column is entitled to make one.
/// A claim that big should be a table somebody can read and a test somebody
/// can break, not a `match` buried in a renderer.
fn cell(mine: bool, tracked: bool, installed_here: bool) -> (&'static str, ratatui::style::Color) {
    match (mine, tracked, installed_here) {
        // This machine: the manifest against reality, all four marks.
        (true, true, true) => ("✓", theme::OK),
        (true, true, false) => ("✗", theme::BAD),
        (true, false, true) => ("+", theme::WARN),
        (true, false, false) => ("—", theme::DIM),
        // Anyone else: the manifest, and nothing about what is installed.
        (false, true, _) => ("·", theme::INFO),
        (false, false, _) => ("—", theme::DIM),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(package: &str) -> Row {
        Row {
            package: package.into(),
            custody: None,
            tracked: vec![true],
            installed_here: false,
        }
    }

    /// ADR-002's selection visuals, at the screen level: the count is a
    /// suffix on the matrix title, the marked row carries the glyph, and
    /// the unmarked title carries nothing at all.
    #[test]
    fn marks_are_a_title_count_and_a_dressed_row() {
        let mut screen = Hosts::new();
        screen.hosts = vec!["north".into()];
        screen.this_host = "north".into();
        screen.rows = vec![row("fd"), row("ripgrep")];

        screen.repaint();
        let title = screen.panes.region(MATRIX).unwrap().title().to_string();
        assert!(!title.contains("marked"), "an empty selection was counted");

        screen.select_all();
        let title = screen.panes.region(MATRIX).unwrap().title().to_string();
        assert!(title.ends_with("· 2 marked"), "no count in: {title}");
        let first = &screen.row_line(&screen.rows[0]).spans[0];
        assert_eq!(first.content.as_ref(), "•", "the marked row has no glyph");

        screen.select_invert();
        let title = screen.panes.region(MATRIX).unwrap().title().to_string();
        assert!(!title.contains("marked"), "inverting all left a count");
    }

    /// This host's column is the only one that says anything about reality,
    /// and it says all four things.
    #[test]
    fn this_hosts_column_compares_the_manifest_against_the_machine() {
        assert_eq!(cell(true, true, true).0, "✓", "tracked and installed");
        assert_eq!(cell(true, true, false).0, "✗", "tracked, not installed");
        assert_eq!(cell(true, false, true).0, "+", "installed, unmanaged");
        assert_eq!(cell(true, false, false).0, "—", "neither");
    }

    /// Every other column is the manifest and only the manifest. The test
    /// that matters is the *negative* one: whatever this machine happens to
    /// have installed must not change a mark in somebody else's column, or
    /// the screen is quietly reporting cube's packages as slab's.
    #[test]
    fn another_hosts_column_never_claims_anything_about_what_is_installed() {
        for installed_here in [true, false] {
            assert_eq!(
                cell(false, true, installed_here).0,
                "·",
                "an installed-here flag leaked into another host's column"
            );
            assert_eq!(cell(false, false, installed_here).0, "—");
        }
        // And it never borrows the marks that mean "installed" or "missing".
        for tracked in [true, false] {
            for installed_here in [true, false] {
                let mark = cell(false, tracked, installed_here).0;
                assert!(
                    !["✓", "✗", "+"].contains(&mark),
                    "another host's column used {mark}, which is a claim about a \
                     machine pacrat cannot see"
                );
            }
        }
    }
}
