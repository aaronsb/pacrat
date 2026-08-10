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
//!
//! ## The drift is the interface
//!
//! ADR-002's amendment reads the two mismatch cells as the reconciliation
//! surface, each with a resolving act in both directions: `✗` (tracked
//! here, not installed) is either "install me" (`pacrat sync`, manifest
//! wins) or "I removed this on purpose" (`pacrat untrack`, reality wins);
//! `+` (installed here, not in this host's manifest) is either "adopt me"
//! (`pacrat add`) or an uninstall `sync --prune` already plans. Only a
//! human can say which, so the detail pane spells out the choice on
//! exactly those rows — and `A` / `x` apply the manifest-editing half over
//! the marks, through the CLI verbs' own writers, after a confirm that
//! names every package. Both are manifest-only edits: the machine's
//! package state stays pacman's, run by the operator.

use std::collections::{BTreeMap, BTreeSet};

use pacrat_core::pkg::Source;
use pacrat_core::Custody;
use ratatui::layout::Constraint;
use ratatui::text::Line;

use crate::ctx::Ctx;
use crate::custody::{self, Index};
use crate::out::{list_preview, shell_quote, truncate, visible_line};
use crate::tui::select::{self, Selection};
use crate::tui::theme;
use crate::tui::viewport::{Panes, Region};
use crate::{add, live, untrack};

use super::{bad, command, field, note, refreshing, wrap};

/// The two reconciling directions a hosts selection can be applied in
/// (ADR-002 amendment). Both are manifest-only edits — the only kind of
/// write pacrat makes: `Add` accepts installed-but-untracked reality into
/// the manifest, `Untrack` accepts a removal that already happened. The
/// machine's package state is pacman's in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconcile {
    Add,
    Untrack,
}

impl Reconcile {
    /// The verb, as the confirm overlay and the job log speak it.
    pub fn verb(self) -> &'static str {
        match self {
            Reconcile::Add => "add",
            Reconcile::Untrack => "untrack",
        }
    }
}

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

    /// Which of the columns is this machine's, if it has one.
    fn mine(&self) -> Option<usize> {
        self.hosts.iter().position(|host| host == &self.this_host)
    }

    fn restate(&mut self) {
        let Some(row) = self.selected_row() else {
            self.set(DETAIL, vec![Line::default(), note("no row selected")]);
            return;
        };
        let package = row.package.clone();
        let custody = row.custody;
        let installed_here = row.installed_here;
        let tracked_here = self
            .mine()
            .and_then(|mine| row.tracked.get(mine))
            .copied()
            .unwrap_or(false);
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
        // The two mismatch cells are the reconciliation surface, and the
        // drift is genuinely ambiguous — only a human can say which way it
        // resolves — so exactly these rows get the choice in words. The
        // quoted name is shell-ready: the line is an invitation to paste.
        if tracked_here && !installed_here {
            lines.push(Line::default());
            lines.push(note("✗ tracked here but not installed — two readings:"));
            lines.push(note(format!(
                "  removed by hand on purpose? `pacrat untrack {}` accepts that (x here)",
                shell_quote(&package)
            )));
            lines.push(note("  still wanted? `pacrat sync` prints the install"));
        } else if !tracked_here && installed_here {
            lines.push(Line::default());
            lines.push(note(
                "+ installed here, not in this host's manifest — two readings:",
            ));
            lines.push(note(format!(
                "  belongs here? `pacrat add {}` records it (A here)",
                shell_quote(&package)
            )));
            lines.push(note(
                "  should go? `pacrat sync --prune` already plans the uninstall",
            ));
        }
        // Never the selection's ledger: the marks are a count in the matrix
        // title, and the names are what the apply step shows (ADR-002).
        lines.push(Line::default());
        lines.push(note(
            "space marks a row · A adds · x untracks · s plans this host",
        ));
        self.set(DETAIL, lines);
        if let Some(region) = self.panes.region_mut(DETAIL) {
            region.set_title(truncate(&package, 40));
        }
    }

    /// What `A` or `x` would apply to: the marks, or the cursor row when
    /// nothing is marked.
    ///
    /// The refusals happen here, *before* the confirm, with the data the
    /// matrix already holds — asking a reader to confirm an apply the
    /// writer is certain to refuse would spend their attention on a
    /// question with no right answer. The writer still re-checks: this is
    /// a courtesy, not the gate.
    pub fn apply_candidates(&self, direction: Reconcile) -> Result<Vec<String>, String> {
        let names: Vec<String> = match self.selected.is_empty() {
            false => self.selected.names(),
            true => self
                .selected_row()
                .map(|row| vec![row.package.clone()])
                .unwrap_or_default(),
        };
        if names.is_empty() {
            return Err("no row selected".into());
        }
        let mine = self.mine();
        let offends = |name: &String| {
            let row = self.rows.iter().find(|row| &row.package == name);
            match direction {
                // Add records reality: a row not installed here is not a
                // reality this host can record.
                Reconcile::Add => !row.is_some_and(|row| row.installed_here),
                // Untrack edits this host's manifest: a row it does not
                // name is nothing to edit.
                Reconcile::Untrack => !row
                    .and_then(|row| mine.and_then(|m| row.tracked.get(m)))
                    .copied()
                    .unwrap_or(false),
            }
        };
        let offenders: Vec<String> = names.iter().filter(|n| offends(n)).cloned().collect();
        if !offenders.is_empty() {
            return Err(match direction {
                Reconcile::Add => format!(
                    "not installed on this host: {} — adopt records reality, it \
                     does not install (that's `pacrat sync`)",
                    list_preview(&offenders, 8)
                ),
                Reconcile::Untrack => format!(
                    "not tracked on this host: {} — untrack edits this host's \
                     manifest, and those rows are not in it",
                    list_preview(&offenders, 8)
                ),
            });
        }
        Ok(names)
    }

    /// Why `A` or `x` had nothing to apply. It lands in the detail pane,
    /// where the row it is talking about already is.
    pub fn refuse_apply(&mut self, why: &str) {
        let mut lines = vec![
            Line::default(),
            Line::from(vec![
                theme::plain("  "),
                theme::tinted(theme::WARN, "nothing applied"),
            ]),
            Line::default(),
        ];
        lines.extend(wrap(why, 76).into_iter().map(note));
        self.answer(lines, "nothing applied".to_string());
    }

    /// The apply, once the confirm is answered: the manifest edit through
    /// the CLI verb's own writer, called unchanged — the `record_override`
    /// precedent, one writer, two surfaces.
    ///
    /// The marks are cleared and the matrix reloaded before the report is
    /// painted: the columns must tell the new manifest's truth, and a mark
    /// surviving its own apply would count packages a next apply cannot
    /// find. The report then overwrites the detail pane, because "what
    /// changed and what happens next" is the answer the reader confirmed
    /// their way into.
    pub fn apply(
        &mut self,
        ctx: &Ctx,
        direction: Reconcile,
        names: &[String],
    ) -> Result<String, String> {
        let host = ctx.host.clone();
        let changes = match direction {
            Reconcile::Add => add::adopt(ctx, &host, names),
            Reconcile::Untrack => untrack::untrack(ctx, &host, names),
        };
        let changes = match changes {
            Ok(changes) => changes,
            Err(e) => {
                self.answer(
                    vec![Line::default(), bad(e.clone())],
                    "nothing applied".to_string(),
                );
                return Err(e);
            }
        };
        self.selected.none();
        self.load(ctx);

        let mut lines = vec![Line::default()];
        if changes.is_empty() {
            // Only the add direction can land here: untrack refuses
            // strangers, so its empty answer never reaches an apply.
            lines.push(note(format!(
                "already tracked on {host} — the manifest is unchanged"
            )));
        }
        let mut total = 0;
        for c in &changes {
            total += c.packages.len();
            lines.push(field(
                c.source.name(),
                format!(
                    "{} {} ({} → {} packages)",
                    match direction {
                        Reconcile::Add => "tracked",
                        Reconcile::Untrack => "untracked",
                    },
                    list_preview(&c.packages, 12),
                    c.before,
                    c.after
                ),
            ));
        }
        lines.push(Line::default());
        lines.push(note("commit the store to sync this to other hosts"));
        if direction == Reconcile::Untrack {
            lines.push(note(
                "nothing was uninstalled — anything still installed here appears \
                 in `pacrat sync --prune`'s plan",
            ));
        }
        let what = format!("{}ed {total} package(s)", direction.verb());
        self.answer(lines, what.clone());
        Ok(format!("{what} on {host}"))
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

    /// A screen with this host's column and a mixed set of drift states,
    /// the cursor on the first row.
    fn screen() -> Hosts {
        let mut screen = Hosts::new();
        screen.hosts = vec!["north".into(), "slab".into()];
        screen.this_host = "north".into();
        screen.rows = vec![
            // ✓ tracked here and installed.
            Row {
                package: "fd".into(),
                custody: None,
                tracked: vec![true, false],
                installed_here: true,
            },
            // ✗ tracked here, not installed — the untrack surface.
            Row {
                package: "cowsay".into(),
                custody: None,
                tracked: vec![true, true],
                installed_here: false,
            },
            // + installed here, in this host's manifest nowhere — the add
            // surface (slab tracking it changes nothing about that).
            Row {
                package: "zoxide".into(),
                custody: None,
                tracked: vec![false, true],
                installed_here: true,
            },
        ];
        screen
            .panes
            .region_mut(MATRIX)
            .unwrap()
            .set_rows(vec![Line::default(); 3]);
        screen
    }

    /// The apply acts on the marks, or on the cursor row when nothing is
    /// marked — the same fallback both directions share.
    #[test]
    fn apply_candidates_are_the_marks_or_the_cursor_row() {
        let mut screen = screen();
        // Nothing marked: the cursor row (fd, installed here) answers.
        assert_eq!(
            screen.apply_candidates(Reconcile::Add).unwrap(),
            vec!["fd".to_string()]
        );
        // Marked rows outrank the cursor.
        screen.selected.toggle("zoxide");
        assert_eq!(
            screen.apply_candidates(Reconcile::Add).unwrap(),
            vec!["zoxide".to_string()]
        );
    }

    /// The refusals, before the confirm: add records reality, so a row not
    /// installed here refuses; untrack edits this host's manifest, so a row
    /// it does not name refuses. Both in plain words, naming the offender.
    #[test]
    fn an_apply_the_writer_would_refuse_is_refused_before_the_confirm() {
        let mut screen = screen();
        screen.selected.toggle("cowsay"); // ✗: not installed here
        let err = screen.apply_candidates(Reconcile::Add).unwrap_err();
        assert!(err.contains("not installed on this host: cowsay"), "{err}");
        // The same mark is exactly what untrack is for.
        assert_eq!(
            screen.apply_candidates(Reconcile::Untrack).unwrap(),
            vec!["cowsay".to_string()]
        );

        screen.selected.toggle("cowsay");
        screen.selected.toggle("zoxide"); // +: not tracked here
        let err = screen.apply_candidates(Reconcile::Untrack).unwrap_err();
        assert!(err.contains("not tracked on this host: zoxide"), "{err}");
        // And that mark is exactly what add is for.
        assert_eq!(
            screen.apply_candidates(Reconcile::Add).unwrap(),
            vec!["zoxide".to_string()]
        );
    }

    /// One stranger refuses the whole apply — a mixed selection must not
    /// quietly shrink to the rows that happen to work.
    #[test]
    fn one_offender_refuses_the_whole_selection() {
        let mut screen = screen();
        screen.selected.toggle("fd");
        screen.selected.toggle("cowsay");
        assert!(screen.apply_candidates(Reconcile::Add).is_err());
        screen.selected.toggle("cowsay");
        assert_eq!(
            screen.apply_candidates(Reconcile::Add).unwrap(),
            vec!["fd".to_string()]
        );
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
