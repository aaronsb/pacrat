//! `[3] updates` — mockup §5, the centrepiece. What changed upstream, and
//! what the graders said about it.
//!
//! "Diff and grading share the screen because that's the actual decision."
//! The two panes below the list are one question asked from both sides, and
//! this screen exists to put them where the eye can hold both at once.
//!
//! ## Two steps, because a grading is about bytes
//!
//! Loading probes every ledger row with `git ls-remote`, which is the
//! `pacrat updates` data path exactly: reviewed commit against upstream
//! HEAD, one round trip each, and a row that could not be asked is
//! *unreachable* rather than current.
//!
//! What loading cannot do is fill the grade column, and the reason is a rule
//! rather than a cost. A grading is of a tree digest, never of a commit id —
//! `git commit --amend` renames a tree without touching it, and a lookup
//! keyed on the name would report a freshly-renamed BLOCK as ungraded. So
//! the digest has to exist before a verdict can, and the digest is only
//! knowable by fetching the candidate. That is `enter`: clone, stage, diff,
//! and read the gradings on file *for those bytes*, which is what
//! `pacrat review` does and in the same order.
//!
//! The column reads `—` until then, and that is not a placeholder standing
//! in for a number: nothing has graded those bytes as far as this screen
//! knows, and UNGRADED holds.
//!
//! ## The override
//!
//! ADR-001 decision 2 puts one door past a BLOCK in each surface. The CLI's
//! is `--override-block --reason "…"`; this is the other one, and the
//! friction is [`crate::tui::childlock`] — a key held for three seconds
//! against a real clock — followed by the same justification the CLI
//! demands. It is offered only for a candidate this session actually
//! reviewed, which is the TUI's form of the CLI's rule that an override
//! needs `--commit`: a record saying a human accepted these bytes has to be
//! about bytes they were shown.
//!
//! The recording goes through `decisions::record_override` — the CLI's own
//! writer, not a copy of it. One ledger, one shape.

use pacrat_core::sources::Role;
use pacrat_core::version::is_newer;
use pacrat_core::Verdict;
use ratatui::layout::Constraint;
use ratatui::text::Line;

use crate::ctx::Ctx;
use crate::custody;
use crate::out::{shell_quote, short_hash, truncate, visible_line};
use crate::tui::theme;
use crate::tui::viewport::{Panes, Region};
use crate::{aur, decisions, grade, pacman, review, updates};

use super::{bad, command, field, note, refreshing, verdict_span, wrap};

const PENDING: usize = 0;
const UPSTREAM: usize = 1;
const DIFF: usize = 2;
const GRADING: usize = 3;

const NAME_W: usize = 24;
/// `malformed → unreachable`, the widest the pair can be — `updates`' own
/// constant, for the same reason: padding the two commits as one cell is
/// what keeps the arrows in a column.
const DRIFT_W: usize = 23;
const ROLE_W: usize = 10;
/// The verdict cell. Wide enough for the longest thing that goes in it —
/// `! unreachable` — so the columns to its right start in one place whatever
/// state a row is in.
const VERDICT_W: usize = 14;
const VER_W: usize = 16;

/// What `ls-remote` said about one ledger row. The four states are
/// [`crate::updates`]', decided by its predicates rather than by new ones —
/// two answers to "has this drifted" would eventually hide a pending update
/// behind a stale refusal.
enum Probe {
    Current,
    Pending(String),
    Rejected(String),
    Unreachable(String),
}

struct Row {
    package: String,
    reviewed: String,
    role: Role,
    aur: Option<String>,
    probe: Probe,
    rejected_note: Option<String>,
    version: Option<String>,
}

impl Row {
    fn candidate(&self) -> Option<&str> {
        match &self.probe {
            Probe::Pending(c) | Probe::Rejected(c) => Some(c),
            _ => None,
        }
    }
}

/// A tracked-but-not-vendored package the AUR has moved past.
struct Upstream {
    package: String,
    installed: String,
    available: String,
}

/// A candidate this session fetched, diffed and looked up gradings for.
///
/// The commit and digest are kept because an override has to name them: the
/// ledger records what was accepted, and "these bytes" is the only thing an
/// entry can honestly claim a human agreed to.
#[derive(Debug)]
pub struct Reviewed {
    pub package: String,
    pub commit: String,
    pub digest: String,
    pub verdict: Verdict,
    pub grade: Option<u8>,
}

pub struct Updates {
    pub panes: Panes,
    loaded: bool,
    rows: Vec<Row>,
    upstream: Vec<Upstream>,
    reviewed: Option<Reviewed>,
    trouble: Vec<String>,
}

impl Updates {
    pub fn new() -> Self {
        Self {
            panes: Panes::new(vec![
                Region::table("pending", Constraint::Fill(4), vec![header()], Vec::new()),
                Region::new(
                    "upstream · tracked, not yet vendored",
                    Constraint::Fill(2),
                    refreshing(),
                ),
                Region::new("diff", Constraint::Fill(4), refreshing()),
                Region::new("grading", Constraint::Fill(3), refreshing()),
            ]),
            loaded: false,
            rows: Vec::new(),
            upstream: Vec::new(),
            reviewed: None,
            trouble: Vec::new(),
        }
    }

    pub fn needs_load(&self) -> bool {
        !self.loaded
    }

    pub fn reload(&mut self) {
        self.loaded = false;
        // The review is dropped with the probe that produced it. A diff left
        // on screen beside a freshly-probed list would be a diff of a
        // candidate the list may no longer be about.
        self.reviewed = None;
        for index in [UPSTREAM, DIFF, GRADING] {
            if let Some(region) = self.panes.region_mut(index) {
                region.set_lines(refreshing());
            }
        }
    }

    /// The `pacrat updates` detect step: every ledger row against its
    /// upstream, plus the tracked-but-not-vendored half.
    pub fn load(&mut self, ctx: &Ctx) {
        self.loaded = true;
        self.trouble.clear();

        let sources = match ctx.load_sources() {
            Ok(sources) => sources,
            Err(e) => {
                self.set(DIFF, vec![bad(format!("the ledger will not parse: {e}"))]);
                return;
            }
        };

        let mut rows: Vec<Row> = sources
            .packages
            .iter()
            .map(|(package, entry)| {
                let probe = match updates::ls_remote(&entry.upstream) {
                    Err(e) => Probe::Unreachable(e),
                    Ok(c) if !updates::drifted(&entry.reviewed, &c) => Probe::Current,
                    Ok(c) if updates::refused(entry, &c) => Probe::Rejected(c),
                    Ok(c) => Probe::Pending(c),
                };
                Row {
                    package: package.clone(),
                    reviewed: entry.reviewed.clone(),
                    role: entry.role,
                    aur: updates::aur_repo(&entry.upstream).map(str::to_string),
                    probe,
                    rejected_note: entry.rejected_note.clone(),
                    version: None,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.package.cmp(&b.package));

        // Both halves want a version for a name, so they ask together —
        // splitting it would double the round trips and give one outage two
        // different-sounding warnings.
        let tracked = tracked_aur(ctx).unwrap_or_default();
        let candidates: Vec<String> = tracked
            .into_iter()
            .filter(|p| !sources.packages.contains_key(p))
            .collect();
        let wanted: Vec<String> = rows
            .iter()
            .filter(|r| matches!(r.probe, Probe::Pending(_)))
            .filter_map(|r| r.aur.clone())
            .chain(candidates.iter().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        let versions = match wanted.is_empty() {
            true => Default::default(),
            false => match aur::info_many(&wanted) {
                Ok(hits) => hits
                    .into_iter()
                    .map(|p| (p.name, p.version))
                    .collect::<std::collections::BTreeMap<_, _>>(),
                Err(e) => {
                    self.trouble.push(format!(
                        "the AUR RPC could not be asked ({e}) — pending rows show \
                         commits without versions, and tracked-but-not-vendored \
                         packages went unexamined"
                    ));
                    Default::default()
                }
            },
        };
        for row in &mut rows {
            row.version = row
                .aur
                .as_ref()
                .and_then(|name| versions.get(name))
                .cloned();
        }
        self.rows = rows;
        self.upstream = upstream_half(&candidates, &versions, &mut self.trouble);
        self.paint();
    }

    fn paint(&mut self) {
        let (mut pending, mut held, mut unreachable, mut rejected) = (0, 0, 0, 0);
        for row in &self.rows {
            match row.probe {
                Probe::Pending(_) => pending += 1,
                Probe::Rejected(_) => rejected += 1,
                Probe::Unreachable(_) => unreachable += 1,
                Probe::Current => {}
            }
        }
        if self
            .reviewed
            .as_ref()
            .is_some_and(|r| r.verdict == Verdict::Block)
        {
            held += 1;
        }

        let lines: Vec<Line<'static>> = self.rows.iter().map(|row| self.row_line(row)).collect();
        if let Some(region) = self.panes.region_mut(PENDING) {
            region.set_rows(lines);
        }
        let mut title = format!(
            "pending — {pending} pending · {} current · {rejected} rejected",
            self.rows.len() - pending - rejected - unreachable
        );
        if unreachable > 0 {
            title.push_str(&format!(" · {unreachable} unreachable"));
        }
        if held > 0 {
            title.push_str(&format!(" · {held} held"));
        }
        if let Some(region) = self.panes.region_mut(PENDING) {
            region.set_title(title);
        }

        let mut up: Vec<Line<'static>> = Vec::new();
        for e in &self.trouble {
            up.push(bad(format!("warning: {e}")));
        }
        if self.upstream.is_empty() {
            up.push(note(
                "nothing tracked outside the ledger has moved — every AUR package \
                 some host tracks is either curated or current",
            ));
        } else {
            up.push(note(
                "these update outside curation: no reviewed commit, no grading, no hold",
            ));
            for u in &self.upstream {
                up.push(Line::from(vec![
                    theme::plain(format!(
                        "  {:<NAME_W$} ",
                        truncate(&visible_line(&u.package).0, NAME_W)
                    )),
                    theme::dim(format!(
                        "{:<24}",
                        truncate(
                            &format!(
                                "{} → {}",
                                visible_line(&u.installed).0,
                                visible_line(&u.available).0
                            ),
                            24
                        )
                    )),
                    theme::tinted(
                        theme::INFO,
                        format!("pacrat vendor {}", shell_quote(&u.package)),
                    ),
                ]));
            }
        }
        self.set(UPSTREAM, up);

        if self.reviewed.is_none() {
            self.set(
                DIFF,
                vec![
                    Line::default(),
                    note("enter — fetch the selected candidate and diff it against the"),
                    note("store's reviewed tree. Viewing runs no graders; the gradings"),
                    note("shown are the ones already on file for those bytes."),
                ],
            );
            self.set(
                GRADING,
                vec![
                    Line::default(),
                    note("gradings are keyed by tree digest, not by commit — there is"),
                    note("no verdict to show until the candidate has been fetched."),
                    note("Until then every row is UNGRADED, which holds."),
                ],
            );
        }
    }

    fn row_line(&self, row: &Row) -> Line<'static> {
        let (cell, state) = match &row.probe {
            Probe::Current => ("current".to_string(), theme::dim("current")),
            Probe::Pending(c) => (
                drift_cell(&row.reviewed, short_hash(c)),
                match self.verdict_of(&row.package) {
                    Some(v) => verdict_span(v),
                    None => theme::dim("—"),
                },
            ),
            Probe::Rejected(c) => (
                drift_cell(&row.reviewed, short_hash(c)),
                theme::tinted(theme::DIM, "✗ rejected"),
            ),
            Probe::Unreachable(_) => (
                drift_cell(&row.reviewed, "unreachable"),
                theme::tinted(theme::BAD, "! unreachable"),
            ),
        };
        // The verdict cell is padded here rather than by taking its width
        // out of the version column's. It is a coloured span of its own, so
        // it cannot be `{:<width$}`'d with the text around it — and the
        // arithmetic version of that ("give version whatever is left") put a
        // one-character `—` flush against an eight-character version and
        // read as `—2.10.1-1`. Padding the cell to its own reserved width
        // makes every column start in the same place whatever is in it.
        let drawn = state.content.chars().count();
        Line::from(vec![
            theme::plain(format!(
                "{:<NAME_W$} ",
                truncate(&visible_line(&row.package).0, NAME_W)
            )),
            theme::dim(format!("{cell:<DRIFT_W$} ")),
            state,
            theme::plain(" ".repeat(VERDICT_W.saturating_sub(drawn))),
            theme::dim(format!(
                "{:>VER_W$}  {:<ROLE_W$}",
                row.version
                    .as_deref()
                    .map_or("—".to_string(), |v| truncate(&visible_line(v).0, VER_W)),
                custody::label(Some(row.role.into())),
            )),
        ])
    }

    /// The verdict this screen is entitled to show for a row: only the one
    /// it fetched, and only while the row still names that candidate.
    fn verdict_of(&self, package: &str) -> Option<Verdict> {
        let reviewed = self.reviewed.as_ref()?;
        (reviewed.package == package).then_some(reviewed.verdict)
    }

    fn selected(&self) -> Option<&Row> {
        self.rows.get(self.panes.region(PENDING)?.cursor()?)
    }

    /// The package the cursor is on, for the job log's entry.
    pub fn selected_package(&self) -> Option<String> {
        self.selected().map(|row| row.package.clone())
    }

    /// The cursor moved. Nothing is asked of the system: the two states
    /// that carry an explanation — a row that could not be probed and a
    /// candidate somebody already refused — came back with it, and the whole
    /// point of recording a refusal is that a reader can see the answer
    /// without asking the question again.
    ///
    /// A review already on screen is left alone. It is about a package, and
    /// scrolling past a row is not a request to throw it away.
    pub fn moved(&mut self) {
        if self.reviewed.is_some() {
            return;
        }
        let Some(row) = self.selected() else { return };
        let package = row.package.clone();
        let lines = match &row.probe {
            Probe::Unreachable(why) => {
                let mut lines = vec![
                    Line::default(),
                    Line::from(vec![
                        theme::plain("  "),
                        theme::tinted(theme::BAD, "could not be probed"),
                    ]),
                    Line::default(),
                ];
                lines.extend(wrap(&visible_line(why).0, 76).into_iter().map(note));
                lines.push(Line::default());
                lines.push(note(
                    "unreachable does not mean up to date — press `r` to probe again",
                ));
                lines
            }
            Probe::Rejected(commit) => {
                let mut lines = vec![
                    Line::default(),
                    Line::from(vec![
                        theme::plain("  "),
                        theme::dim("a human already refused "),
                        theme::plain(short_hash(commit).to_string()),
                    ]),
                    Line::default(),
                ];
                lines.extend(
                    wrap(
                        &row.rejected_note
                            .as_deref()
                            .map_or("(no reason recorded)".to_string(), |n| visible_line(n).0),
                        76,
                    )
                    .into_iter()
                    .map(note),
                );
                lines.push(Line::default());
                lines.push(note(
                    "the refusal applies to this commit only — once upstream moves, \
                     the new candidate will be raised again",
                ));
                lines
            }
            _ => return,
        };
        self.set(DIFF, lines);
        if let Some(region) = self.panes.region_mut(DIFF) {
            region.set_title(truncate(&package, 40));
        }
    }

    /// Why `o` had nothing to open.
    ///
    /// It lands in the grading region rather than an overlay because that is
    /// where the verdict it is talking about already is — a modal saying
    /// "the verdict is WARN, not BLOCK" over the pane that says WARN would
    /// be telling the reader something they are looking at.
    pub fn refuse_override(&mut self, why: &str) {
        let mut lines = vec![Line::default()];
        lines.push(Line::from(vec![
            theme::plain("  "),
            theme::tinted(theme::WARN, "no override offered"),
        ]));
        lines.push(Line::default());
        for chunk in wrap(why, 76) {
            lines.push(note(chunk));
        }
        self.set(GRADING, lines);
        self.panes.focus_on(GRADING);
    }

    /// What the reader has looked at, if the cursor is still on it.
    pub fn reviewed(&self) -> Option<&Reviewed> {
        let reviewed = self.reviewed.as_ref()?;
        (self.selected()?.package == reviewed.package).then_some(reviewed)
    }

    /// `enter` — the review step: fetch the candidate, stage both trees,
    /// diff them, and read whatever is on file for the bytes that came back.
    ///
    /// This is the one screen action that shells out, and it does the same
    /// four things `pacrat review` does in the same order. Its chatter — the
    /// `run  git clone …` lines git prints before each call — is captured
    /// into the jobs log by the caller, which is where the always-visible
    /// rule is honoured behind an alternate screen.
    pub fn review(&mut self, ctx: &Ctx) -> Result<String, String> {
        let Some(row) = self.selected() else {
            return Err("no row selected".into());
        };
        let package = row.package.clone();
        let Some(candidate) = row.candidate().map(str::to_string) else {
            let why = match row.probe {
                Probe::Current => "is at its reviewed commit — there is no candidate to look at",
                Probe::Unreachable(_) => {
                    "could not be probed, so there is no candidate to fetch. \
                     Press `r` to retry"
                }
                _ => "has no candidate",
            };
            self.set(
                DIFF,
                vec![Line::default(), note(format!("{package} {why}"))],
            );
            return Err(format!("{package} {why}"));
        };
        self.reviewed = None;

        let curated = review::curated(ctx, &package)?;
        let cand = review::fetch(&package, &curated.entry, &curated.tree)?;
        let outcome = self.render_review(ctx, &package, &curated, &cand, &candidate);
        // The trees are scratch, and they are only worth keeping when
        // something went wrong with reading them.
        review::finish(&cand, outcome.is_ok());
        outcome
    }

    fn render_review(
        &mut self,
        ctx: &Ctx,
        package: &str,
        curated: &review::Curated,
        cand: &review::Candidate,
        expected: &str,
    ) -> Result<String, String> {
        let changes = review::changes_between(curated, cand)?;
        let mut lines = vec![
            field("reviewed", short_hash(&curated.entry.reviewed).to_string()),
            field("candidate", short_hash(&cand.commit).to_string()),
            field("digest", cand.digest.clone()),
        ];
        // Upstream moving between the probe and the fetch is not an error,
        // but it does mean the row the reader chose and the bytes they are
        // about to be shown are two different candidates — and an override
        // recorded against the wrong one is exactly the failure the record
        // exists to prevent.
        if !pacrat_core::grading::commit_matches(expected, &cand.commit) {
            lines.push(bad(format!(
                "upstream moved since the list was built: the row said {} and HEAD \
                 is now {}",
                short_hash(expected),
                short_hash(&cand.commit)
            )));
        }
        for (label, names) in [
            ("changed", changes.changed()),
            ("added", changes.added()),
            ("removed", changes.removed()),
        ] {
            if !names.is_empty() {
                lines.push(field(label, crate::out::list_preview(names, 12)));
            }
        }
        // `*.install` runs as root at install time, which is the classic
        // place to hide a payload — so an unchanged one is still named.
        let installs: Vec<String> = changes
            .unchanged()
            .iter()
            .filter(|f| f.ends_with(".install"))
            .cloned()
            .collect();
        if !installs.is_empty() {
            lines.push(field(
                "install",
                format!(
                    "{} — unchanged, and still run as root at install time",
                    crate::out::list_preview(&installs, 8)
                ),
            ));
        }
        lines.push(Line::default());

        let (diff, hidden, cut) = review::diff_lines(cand)?;
        if diff.is_empty() {
            lines.push(note(
                "no diff — the candidate's tree is byte-identical to the store's \
                 (the commit moved, the contents did not)",
            ));
        }
        lines.extend(diff.iter().map(|line| diff_line(line)));
        lines.push(Line::default());
        lines.push(note(format!("end diff  {}", changes.summary_line())));
        if cut {
            lines.push(bad(
                "the diff hit pacrat's 1 MB limit and is incomplete — run `pacrat \
                 review` in a shell, which keeps both trees for inspection",
            ));
        }
        if hidden > 0 {
            lines.push(bad(format!(
                "{hidden} control character{} in the diff replaced with ␛-style \
                 stand-ins — control characters can hide text from a reviewer",
                if hidden == 1 { "" } else { "s" }
            )));
        }
        self.set(DIFF, lines);
        if let Some(region) = self.panes.region_mut(DIFF) {
            region.set_title(format!(
                "{package} · diff since reviewed {}",
                short_hash(&curated.entry.reviewed)
            ));
        }

        // ---- the gradings, for these bytes -----------------------------
        let gradings = grade::cached(ctx, package, &cand.commit, &cand.digest)?;
        let grade_value = grade::cached_grade(&gradings);
        let verdict = grade::verdict_line(&ctx.config.thresholds, grade_value);

        let mut g = vec![Line::from(vec![
            theme::plain("  "),
            theme::dim(format!("{:<10}", "verdict")),
            verdict_span(verdict),
            theme::dim(format!(
                "  grade {} of {}-{}",
                grade_value.map_or("—".to_string(), |g| g.to_string()),
                pacrat_core::grading::PACRAT_SCALE.min,
                pacrat_core::grading::PACRAT_SCALE.max
            )),
            theme::dim("  (pacrat's, from its own thresholds)"),
        ])];
        if gradings.is_empty() {
            g.push(note("no gradings on file for these bytes"));
            g.push(note(
                "`pacrat grade` only grades the store tree, so this candidate can be \
                 graded after adoption — or beforehand with `pacrat update`, the only \
                 verb that grades a staged candidate",
            ));
        }
        for c in &gradings {
            g.push(Line::default());
            g.push(field("grader", truncate(&visible_line(&c.grader).0, 40)));
            let mut grade_cell = format!(
                "{} of {}-{}",
                c.report.grade, c.report.scale.min, c.report.scale.max
            );
            // Only worth saying when the grader used its own scale — the
            // same condition `grade::print_grading` applies, because a
            // reader comparing the two surfaces must see the same numbers.
            if c.report.scale != pacrat_core::grading::PACRAT_SCALE {
                grade_cell.push_str(&format!(" · pacrat {} of 0-4", c.report.pacrat_grade()));
            }
            g.push(field("grade", grade_cell));
            // `manual` puts its whole reason here and has no findings at
            // all, so a pane that rendered only findings would show a BLOCK
            // by a human with nothing about why — which is the one grading
            // whose reason is certain to exist.
            if let Some(n) = c.report.meta.get("note").and_then(|v| v.as_str()) {
                g.push(field("note", truncate(&visible_line(n).0, 90)));
            }
            for finding in c.report.top_findings(6) {
                let span = match &finding.span {
                    Some(s) => format!("{} · ", truncate(&visible_line(s).0, 30)),
                    None => String::new(),
                };
                g.push(Line::from(vec![
                    theme::plain("    "),
                    theme::tinted(
                        match finding.level {
                            0 => theme::OK,
                            1..=2 => theme::WARN,
                            _ => theme::BAD,
                        },
                        format!("{} ", mark(finding.level)),
                    ),
                    theme::dim(span),
                    theme::plain(truncate(&visible_line(&finding.title).0, 84)),
                ]));
            }
            if let Some(filed_as) = &c.filed_as {
                g.push(field(
                    "bytes",
                    format!(
                        "graded as {} — the same bytes under a different commit id",
                        short_hash(filed_as)
                    ),
                ));
            }
            if c.standing == pacrat_core::grading::Standing::Retired {
                g.push(field(
                    "standing",
                    format!(
                        "{} is not in this host's config — its grade can make this \
                         verdict worse, but cannot supply one on its own",
                        truncate(&visible_line(&c.grader).0, 40)
                    ),
                ));
            }
        }
        // A configured grader with nothing on file is not the same as no
        // grader, and why it produced nothing is often the whole story.
        for grader in &ctx.config.graders {
            if gradings.iter().any(|c| c.grader == grader.name) {
                continue;
            }
            g.push(Line::default());
            g.push(field("grader", truncate(&grader.name, 40)));
            g.push(field(
                "result",
                match grade::cached_failure(package, &cand.commit, &cand.digest, &grader.name) {
                    Some(reason) => format!("no grading — the grader failed: {reason}"),
                    None => "no grading on file for this candidate".to_string(),
                },
            ));
            g.push(Line::from(vec![
                theme::plain("    "),
                theme::dim(format!("$ {}", grader.cmd.join(" "))),
            ]));
        }
        g.push(Line::default());
        if verdict == Verdict::Block {
            g.push(Line::from(vec![
                theme::plain("  "),
                theme::tinted(theme::BAD, "BLOCK holds. "),
                theme::dim("`o` opens the override — hold the key, then give a reason."),
            ]));
        }
        self.set(GRADING, g);
        if let Some(region) = self.panes.region_mut(GRADING) {
            region.set_title(format!(
                "grading · {} of these bytes · verdict {verdict} (pacrat)",
                gradings.len()
            ));
        }

        self.reviewed = Some(Reviewed {
            package: package.to_string(),
            commit: cand.commit.clone(),
            digest: cand.digest.clone(),
            verdict,
            grade: grade_value,
        });
        self.paint();
        self.panes.focus_on(DIFF);
        Ok(format!(
            "{package} @ {} · {} · verdict {verdict}",
            short_hash(&cand.commit),
            changes.summary_line()
        ))
    }

    /// `a` and `x`: the commands whose CLI twins act.
    pub fn suggest_adopt(&mut self) {
        let Some(row) = self.selected() else { return };
        let package = shell_quote(&row.package);
        let lines = match (row.candidate(), self.reviewed()) {
            (None, _) => command(
                "nothing to adopt — this row is at its reviewed commit, or could not \
                 be probed at all.",
                &["pacrat updates".to_string()],
            ),
            (Some(candidate), _) => command(
                "adopting installs the candidate's tree into the store and advances \
                 the ledger, after showing the diff and asking. `--commit` is \
                 required: if upstream has moved past the commit you read, the adopt \
                 is refused rather than silently taking the newer one.",
                &[
                    format!("pacrat adopt-update {package} --commit {candidate}"),
                    format!("pacrat build {package}"),
                ],
            ),
        };
        self.answer(lines, "adopt");
    }

    pub fn suggest_reject(&mut self) {
        let Some(row) = self.selected() else { return };
        let package = shell_quote(&row.package);
        let lines = command(
            "a rejection applies to one commit: `pacrat updates` stops raising this \
             candidate, and once upstream moves on, the new candidate is raised \
             again.",
            &[format!("pacrat reject {package} --note \"…\"")],
        );
        self.answer(lines, "reject");
    }

    fn answer(&mut self, lines: Vec<Line<'static>>, what: &str) {
        let package = self
            .selected()
            .map(|row| row.package.clone())
            .unwrap_or_default();
        self.set(DIFF, lines);
        if let Some(region) = self.panes.region_mut(DIFF) {
            region.set_title(format!("{package} — the {what} command"));
        }
        self.panes.focus_on(DIFF);
    }

    /// Whether `o` has anything to offer, and what to say if it does not.
    ///
    /// The affordance appears only after the friction (ADR-001 decision 2),
    /// and only for a candidate this session actually fetched: an entry in
    /// the ledger says a human accepted specific bytes, and the TUI has no
    /// business writing one about bytes it only heard a commit id for.
    pub fn overridable(&self) -> Result<&Reviewed, String> {
        let reviewed = self.reviewed().ok_or(
            "nothing reviewed — press enter on the row first. An override records \
             that you accepted these bytes, so it is offered only for bytes you \
             have been shown.",
        )?;
        match reviewed.verdict {
            Verdict::Block => Ok(reviewed),
            other => Err(format!(
                "the verdict is {other}, not {}. There is nothing to override, and \
                 nothing was recorded.",
                Verdict::Block
            )),
        }
    }

    /// Write the override to the store's decision ledger.
    ///
    /// `decisions::record_override` is the CLI's writer, called here
    /// unchanged. ADR-001 settles that both surfaces record to one ledger;
    /// a second path that wrote the same file its own way would be two
    /// shapes and one filename, which is worse than two files.
    ///
    /// The adoption is deliberately *not* done here. The record is the part
    /// that must survive — an adoption with no record is the audit trail the
    /// decision exists to keep — and installing the tree is `adopt-update`'s
    /// job, printed for the reader with the commit already filled in.
    pub fn record_override(&mut self, ctx: &Ctx, reason: &str) -> Result<String, String> {
        let reviewed = self.overridable()?;
        let (package, commit, digest, grade) = (
            reviewed.package.clone(),
            reviewed.commit.clone(),
            reviewed.digest.clone(),
            reviewed.grade,
        );
        decisions::record_override(ctx, &package, &commit, &digest, grade, reason)?;

        let mut lines = vec![
            Line::from(vec![
                theme::plain("  "),
                theme::tinted(theme::BAD, "OVERRIDE  "),
                theme::plain(format!("{package} @ {} — recorded", short_hash(&commit))),
            ]),
            field("reason", truncate(&visible_line(reason).0, 200)),
            // The bytes, beside the commit that names them. A grading is of
            // a tree and so is an acceptance: the digest is what makes the
            // entry checkable later, when the commit id may have been
            // rewritten by an amend upstream.
            field("digest", digest),
            field("ledger", review::store_rel(ctx, &ctx.decisions_path())),
            Line::default(),
            note("permanent, and synced to every host. `pacrat decisions` lists it."),
            Line::default(),
        ];
        lines.extend(command(
            "the override is recorded; the adoption has not happened yet. Run this \
             command to install the tree — it re-uses the same reason, so the ledger \
             keeps a single decision.",
            &[format!(
                "pacrat adopt-update {} --commit {commit} --override-block --reason {}",
                shell_quote(&package),
                shell_quote(reason)
            )],
        ));
        self.set(GRADING, lines);
        if let Some(region) = self.panes.region_mut(GRADING) {
            region.set_title(format!("{package} — override recorded"));
        }
        self.panes.focus_on(GRADING);
        Ok(format!(
            "override-block recorded for {package} @ {}",
            short_hash(&commit)
        ))
    }

    fn set(&mut self, index: usize, lines: Vec<Line<'static>>) {
        if let Some(region) = self.panes.region_mut(index) {
            region.set_lines(lines);
        }
    }
}

/// Every AUR package name any host tracks.
fn tracked_aur(ctx: &Ctx) -> Result<std::collections::BTreeSet<String>, String> {
    let mut tracked = std::collections::BTreeSet::new();
    for host in ctx.tracked_hosts() {
        tracked.extend(ctx.tracked(&host, pacrat_core::pkg::Source::Aur)?);
    }
    Ok(tracked)
}

fn upstream_half(
    candidates: &[String],
    available: &std::collections::BTreeMap<String, String>,
    trouble: &mut Vec<String>,
) -> Vec<Upstream> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let installed = match pacman::installed_versions() {
        Ok(map) => map,
        Err(e) => {
            trouble.push(format!(
                "tracked-but-not-vendored AUR packages went unexamined ({e})"
            ));
            return Vec::new();
        }
    };
    candidates
        .iter()
        .filter_map(|package| {
            // No local install means no local version, and that is another
            // host's drift to answer for — inventing a number here would be
            // worse than leaving the row out.
            let have = installed.get(package)?;
            let theirs = available.get(package)?;
            is_newer(have, theirs).then(|| Upstream {
                package: package.clone(),
                installed: have.clone(),
                available: theirs.clone(),
            })
        })
        .collect()
}

fn header() -> Line<'static> {
    Line::from(vec![theme::dim(format!(
        "{:<NAME_W$} {:<DRIFT_W$} {:<VERDICT_W$}{:>VER_W$}  {}",
        "package", "reviewed → candidate", "verdict", "version", "role"
    ))])
}

/// The reviewed cell: a hash abbreviated, anything else named.
///
/// `updates`' rule, and its reason — `short_hash` would clip a doubled
/// forty-character hash down to eight that match the candidate's exactly,
/// and the row would read as pacrat crying wolf over two identical-looking
/// commits.
fn drift_cell(reviewed: &str, candidate: &str) -> String {
    let reviewed = reviewed.trim();
    let shown = match pacrat_core::sources::valid_commit(reviewed) {
        true => short_hash(reviewed),
        false => "malformed",
    };
    format!("{shown} → {candidate}")
}

fn mark(level: u8) -> &'static str {
    match level {
        0 => "✓",
        1..=2 => "▲",
        _ => "■",
    }
}

/// A diff line, coloured by what it does. The text is already neutered —
/// `review::diff_lines` does it, and does not offer a version that has not.
fn diff_line(line: &str) -> Line<'static> {
    let colour = match line.chars().next() {
        Some('+') => theme::OK,
        Some('-') => theme::BAD,
        Some('@') => theme::INFO,
        _ => theme::DIM,
    };
    Line::from(vec![
        theme::plain("  "),
        theme::tinted(colour, truncate(line, 200)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A hash is abbreviated, and anything that could not be one is named.
    ///
    /// The doubled hash is the case this exists for: `short_hash` would clip
    /// it to eight characters that match the candidate's exactly, and the
    /// row would read as pacrat crying wolf over two identical-looking
    /// commits. Same rule as `updates::reviewed_cell`, and the two must not
    /// drift — a screen that called a value comparable while the CLI called
    /// it malformed would be two answers about one ledger.
    #[test]
    fn the_drift_cell_abbreviates_hashes_and_names_everything_else() {
        let full = "5a4705a4aaa2e7f10a7dd6c302256dd373516e56";
        assert_eq!(drift_cell(full, "cbf58f74"), "5a4705a4 → cbf58f74");
        assert_eq!(drift_cell(" 5a4705a4\n", "cbf58f74"), "5a4705a4 → cbf58f74");
        assert_eq!(
            drift_cell(&full.repeat(2), "unreachable"),
            "malformed → unreachable"
        );
        assert_eq!(drift_cell("", "x"), "malformed → x");
        assert_eq!(drift_cell("v2.10.1", "x"), "malformed → x");
        // The column is sized for the widest the cell can be.
        assert_eq!(DRIFT_W, "malformed → unreachable".chars().count());
        assert!("reviewed → candidate".chars().count() <= DRIFT_W);
        assert_eq!(ROLE_W, custody::label(Some(Role::Maintained.into())).len());
    }

    fn versions(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// The upstream half is only ever about packages that are *newer* out
    /// there than what is on this machine, and only about ones this machine
    /// actually has — a host that does not have it cannot say what version
    /// it is behind, and another host's drift is that host's to answer for.
    #[test]
    fn the_upstream_half_needs_a_local_version_to_compare_against() {
        let installed = versions(&[("marktext-bin", "0.19.1"), ("yay", "12.4.2")]);
        let available = versions(&[
            ("marktext-bin", "0.19.2"),
            ("yay", "12.4.2"),
            ("never-installed", "9.9"),
        ]);
        let candidates: Vec<String> = ["marktext-bin", "yay", "never-installed"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let rows = rows_of(&candidates, &available, &installed);
        assert_eq!(rows.len(), 1, "only the one that moved");
        assert_eq!(rows[0].package, "marktext-bin");
        assert_eq!(rows[0].installed, "0.19.1");
        assert_eq!(rows[0].available, "0.19.2");
    }

    /// A package the RPC could not price is left out rather than guessed at,
    /// and a fully vendored fleet has no upstream half at all.
    #[test]
    fn an_unpriced_or_curated_package_is_not_an_upstream_row() {
        let installed = versions(&[("mdcat", "2.10.1")]);
        assert!(rows_of(&["mdcat".to_string()], &BTreeMap::new(), &installed).is_empty());
        assert!(rows_of(&[], &versions(&[("mdcat", "2.11.0")]), &installed).is_empty());
    }

    /// The pure half of `upstream_half`, so the set arithmetic can be tested
    /// without a `pacman -Q` on the machine running the tests.
    fn rows_of(
        candidates: &[String],
        available: &BTreeMap<String, String>,
        installed: &BTreeMap<String, String>,
    ) -> Vec<Upstream> {
        candidates
            .iter()
            .filter_map(|package| {
                let have = installed.get(package)?;
                let theirs = available.get(package)?;
                is_newer(have, theirs).then(|| Upstream {
                    package: package.clone(),
                    installed: have.clone(),
                    available: theirs.clone(),
                })
            })
            .collect()
    }

    /// Everything that can go in the verdict cell fits the width reserved
    /// for it, so the version and role columns start in one place down the
    /// whole table.
    ///
    /// The bug this pins: the cell used to be unpadded and the *version*
    /// column took whatever width was left, so a one-character `—` beside an
    /// eight-character version rendered as `—2.10.1-1`.
    #[test]
    fn every_verdict_cell_fits_the_width_reserved_for_it() {
        let mut widest = 0;
        for text in ["current", "✗ rejected", "! unreachable", "—"] {
            widest = widest.max(text.chars().count());
            assert!(
                text.chars().count() <= VERDICT_W,
                "{text:?} is wider than the {VERDICT_W}-column verdict cell"
            );
        }
        for verdict in [
            Verdict::Proceed,
            Verdict::Warn,
            Verdict::Block,
            Verdict::Ungraded,
        ] {
            let drawn = verdict_span(verdict).content.chars().count();
            widest = widest.max(drawn);
            assert!(
                drawn <= VERDICT_W,
                "{verdict} draws {drawn} columns, wider than the cell reserved for it"
            );
        }
        // And the reservation is not wildly generous either — a column of
        // mostly blank is a column the eye has to cross for nothing.
        assert!(
            VERDICT_W <= widest + 2,
            "the verdict cell is {VERDICT_W} wide for {widest} columns of content"
        );
    }

    /// A row only shows a verdict for the candidate this session fetched.
    /// The rule it enforces is the one the whole screen turns on: a grading
    /// is of bytes, and a verdict beside a package whose bytes nobody has
    /// seen would be a number with nothing behind it.
    #[test]
    fn a_verdict_belongs_to_the_package_that_was_reviewed_and_no_other() {
        let mut screen = Updates::new();
        screen.reviewed = Some(Reviewed {
            package: "mdcat".into(),
            commit: "5a4705a4".into(),
            digest: "d".repeat(64),
            verdict: Verdict::Block,
            grade: Some(4),
        });
        assert_eq!(screen.verdict_of("mdcat"), Some(Verdict::Block));
        assert_eq!(screen.verdict_of("coolercontrol"), None);
        assert_eq!(Updates::new().verdict_of("mdcat"), None);
    }

    /// A screen with one pending row, the cursor on it.
    fn one_row(package: &str, candidate: &str) -> Updates {
        let mut screen = Updates::new();
        screen.rows = vec![Row {
            package: package.into(),
            reviewed: "3f9c21ab".into(),
            role: Role::Vendored,
            aur: None,
            probe: Probe::Pending(candidate.into()),
            rejected_note: None,
            version: None,
        }];
        screen
            .panes
            .region_mut(PENDING)
            .unwrap()
            .set_rows(vec![Line::raw(package.to_string())]);
        screen
    }

    fn reviewed_as(package: &str, verdict: Verdict) -> Reviewed {
        Reviewed {
            package: package.into(),
            commit: "5a4705a4".into(),
            digest: "d".repeat(64),
            verdict,
            grade: Some(4),
        }
    }

    /// The override is offered for a reviewed BLOCK and for nothing else,
    /// and every refusal says so in words rather than by the affordance
    /// quietly not being there.
    #[test]
    fn the_override_is_offered_only_for_a_reviewed_block() {
        let mut screen = one_row("mdcat", "5a4705a4");
        for (verdict, offered) in [
            (Verdict::Proceed, false),
            (Verdict::Warn, false),
            (Verdict::Ungraded, false),
            (Verdict::Block, true),
        ] {
            screen.reviewed = Some(reviewed_as("mdcat", verdict));
            assert_eq!(
                screen.overridable().is_ok(),
                offered,
                "{verdict} was {}offered an override",
                if offered { "not " } else { "" }
            );
            if !offered {
                let err = screen.overridable().unwrap_err();
                assert!(
                    err.contains("nothing was recorded"),
                    "a refusal must say the ledger is untouched: {err}"
                );
            }
        }
    }

    /// The TUI's form of the CLI's rule that an override needs `--commit`.
    ///
    /// An entry in the decision ledger says a human accepted specific bytes.
    /// A BLOCK the reader never fetched is a commit id they were told about
    /// and a tree they have not seen, so there is nothing here honest enough
    /// to record — and scrolling the cursor off the reviewed row is the same
    /// situation arrived at from the other direction.
    #[test]
    fn an_override_needs_bytes_the_reader_was_actually_shown() {
        let mut screen = one_row("mdcat", "5a4705a4");
        let err = screen.overridable().unwrap_err();
        assert!(err.contains("nothing reviewed"), "{err}");

        // Reviewed, and a BLOCK — but the cursor has moved to another
        // package, so the row in front of the reader is not the one the
        // review was about.
        screen.reviewed = Some(reviewed_as("coolercontrol", Verdict::Block));
        assert!(
            screen.overridable().is_err(),
            "an override was offered for a package the cursor is not on"
        );

        screen.reviewed = Some(reviewed_as("mdcat", Verdict::Block));
        assert!(screen.overridable().is_ok());
    }
}
