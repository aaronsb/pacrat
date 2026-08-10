//! `pacrat update` — ADR-001's whole loop in one command: detect → grade →
//! decide → build → served.
//!
//! Every step already exists as a verb a human can run. This module runs
//! them in order without one, which makes it the timer entry point the ADR
//! asks for — the thing that keeps gradings warm and the store current
//! before anybody looks. It is deliberately *thin*: drift detection is
//! [`crate::updates`], staging and adoption are [`crate::review`], grading
//! is [`crate::grade`], building is [`crate::build`]. What is genuinely new
//! here is one small pure function — [`decide`] — and the discipline around
//! it.
//!
//! ## The gate
//!
//! ADR-001's invariants are not preferences this loop weighs against
//! convenience:
//!
//! - **BLOCK always holds, and there is no override in this verb.** The
//!   override exists (decision 2), it is friction-gated, and it lives in
//!   `pacrat adopt-update --override-block --reason "…"` where a human
//!   authors a justification that lands in the store's decision ledger. A
//!   loop that could talk itself past a BLOCK would not be a gate, and a
//!   loop that could do it *unattended* would be a scheduled bypass. The
//!   output says so rather than leaving the reader to wonder whether they
//!   missed a flag.
//! - **Ungraded holds.** Failure is never Proceed — a grader that crashed,
//!   timed out, or answered garbage tells us nothing about the bytes, and
//!   "nothing" is not permission. Only `--mode manual`, where a person is
//!   sitting there, may *ask* about it, and the default answer is no.
//! - **Every external call is visible.** Argv lines for the probes, the
//!   clones and the graders come from the modules that make those calls, so
//!   they are the same lines the individual verbs print.
//!
//! ## Asking
//!
//! A prompt in this loop is answered `no` unless a human at a terminal types
//! `y`. In particular a *pipe* is a no, which is a deliberate departure from
//! `adopt-update`, where `echo y |` works. The difference is what is being
//! answered: `adopt-update` is aimed by a person at one package they just
//! read about, while one `y` on this verb's stdin would answer every
//! question in a run about packages the loop has not printed yet — including
//! questions raised by a candidate that arrived this morning. Friction that
//! a pipe can satisfy is not friction.
//!
//! ## Exit codes
//!
//! 0 nothing pending, or everything pending was adopted and built · 10 a
//! hold · 1 the loop could not do its job.
//!
//! An **unreachable** upstream is a hold, not a failure: the other rows were
//! still checked, and the one that was not is exactly the kind of
//! outstanding item 10 exists to name. It becomes a failure only when *every*
//! upstream was unreachable, because then the run learned nothing at all. A
//! package that was fetched and then failed a check — a tree with no
//! PKGBUILD, a grader that edited what it was reading — is a hold rather than
//! part of that count: the run reached it and learned something specific.
//! (This is a considered difference from `pacrat updates`, which answers a
//! narrower question — "is anything pending?" — and cannot answer it at all
//! with rows unprobed, so it exits 1 there. This verb reports what it did,
//! and it did do something.)

use std::io::IsTerminal;

use pacrat_core::config::Mode;
use pacrat_core::sources::SourceEntry;
use pacrat_core::Verdict;
use serde::Serialize;

use crate::build;
use crate::ctx::Ctx;
use crate::fstree;
use crate::grade;
use crate::out::{self, list_preview, short_hash, truncate, visible_line};
use crate::review;
use crate::say;
use crate::updates::{self, Format};
use crate::vendor;
use crate::HELD;

/// Widest package column before it is clipped — `updates`' number, because
/// these two verbs print tables a reader compares side by side.
const NAME_CAP: usize = 32;

/// `--mode` on the command line. The model is core's
/// [`pacrat_core::config::Mode`] — this is the clap face of it, the way
/// `vendor::RoleArg` is the clap face of a role, so that core keeps no
/// opinion about argument parsing.
#[derive(Copy, Clone, clap::ValueEnum)]
pub enum ModeArg {
    /// Never ask: adopt a clean verdict, hold everything else
    Auto,
    /// Ask when the answer is not clean
    Semi,
    /// Ask about every adoption, and before the build
    Manual,
}

impl From<ModeArg> for Mode {
    fn from(mode: ModeArg) -> Self {
        match mode {
            ModeArg::Auto => Mode::Auto,
            ModeArg::Semi => Mode::Semi,
            ModeArg::Manual => Mode::Manual,
        }
    }
}

pub fn run(ctx: &Ctx, mode: Option<Mode>, fmt: Format) -> Result<(), String> {
    // Before anything prints: in JSON mode stdout carries one object and
    // nothing else, so every human line this run produces — including
    // makepkg's, several modules down — goes to stderr instead.
    out::chatter_to_stderr(fmt == Format::Json);

    let mode = mode.unwrap_or(ctx.config.update_mode);
    let sources = ctx.load_sources()?;

    say!("mode      {mode}");
    say!("store     {}", ctx.sources_path().display());
    say!(
        "gate      BLOCK always holds and this verb has no override; ungraded holds{}",
        match mode {
            Mode::Manual => " unless you say otherwise",
            _ => "",
        }
    );
    if asks(mode) && !stdin_is_a_human() {
        say!(
            "stdin     not a terminal — every question in this run answers no, so \
             anything that would be asked about is held"
        );
    }

    // ---- detect ----------------------------------------------------------
    let mut rows: Vec<Row> = Vec::new();
    for (package, entry) in &sources.packages {
        rows.push(detect(package, entry));
    }

    // ---- grade and decide, package by package ----------------------------
    for row in &mut rows {
        if row.outcome != Outcome::Pending {
            continue;
        }
        say!();
        say!("──────── {}", truncate(&visible_line(&row.package).0, 60));
        consider(ctx, mode, row);
    }

    // ---- build -----------------------------------------------------------
    let adopted: Vec<String> = rows
        .iter()
        .filter(|r| r.outcome == Outcome::Adopted)
        .map(|r| r.package.clone())
        .collect();
    let build = build_stage(ctx, mode, &adopted);

    // ---- report ----------------------------------------------------------
    let summary = Summary {
        rows: &rows,
        adopted: &adopted,
        build: &build,
    };
    let exit = exit_of(&summary);
    match fmt {
        Format::Text => report_text(&summary, &exit),
        Format::Json => report_json(ctx, mode, &summary, &exit)?,
    }

    match exit {
        Exit::Clean => Ok(()),
        Exit::Held => std::process::exit(HELD),
        Exit::Broken(why) => Err(why),
    }
}

// ------------------------------------------------------------------ detect

/// One ledger package, from the probe through to what became of it.
struct Row {
    package: String,
    reviewed: String,
    /// The commit this run is deciding about — the probe's answer, replaced
    /// by the clone's HEAD once there is one, because the clone is the thing
    /// that was actually graded.
    candidate: Option<String>,
    verdict: Option<Verdict>,
    grade: Option<u8>,
    outcome: Outcome,
}

/// What became of one package in this run.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// Upstream is at the reviewed commit. Not a row anybody needs to read.
    Current,
    /// Detected as drifted and not yet decided about. A transient state
    /// inside one run — every pending row is replaced by what became of it
    /// before the report — and named rather than borrowed from `Current`,
    /// because a placeholder that reads as "nothing to do" is the one wrong
    /// answer that would look right.
    Pending,
    /// The store now holds the candidate.
    Adopted,
    /// Graded WARN, and this mode does not adopt a WARN unasked.
    HeldWarn,
    /// The invariant. No mode and no flag moves this one.
    HeldBlock,
    /// Nothing with standing graded these bytes.
    HeldUngraded,
    /// A question was asked and the answer was no — including the silent no
    /// of a stdin that is not a terminal.
    Declined,
    /// A human refused this exact candidate before (`pacrat reject`). Drift
    /// with an answer already on it: reported, never re-raised as work.
    RejectedSkip,
    /// The probe could not ask.
    Unreachable(String),
    /// It could ask, and something after that went wrong — a clone that
    /// failed, a tree with no PKGBUILD, a grade cache that will not open.
    Failed(String),
    /// Upstream went back to the reviewed commit between the probe and the
    /// clone. Rare, and named rather than reported as an adoption of nothing.
    Moot,
}

impl Outcome {
    /// The word in the summary's first column.
    fn word(&self) -> &'static str {
        match self {
            Outcome::Current => "current",
            Outcome::Pending => "pending",
            Outcome::Adopted => "adopted",
            Outcome::HeldWarn | Outcome::HeldBlock | Outcome::HeldUngraded => "held",
            Outcome::Declined => "held",
            Outcome::RejectedSkip => "skipped",
            Outcome::Unreachable(_) => "unreachable",
            Outcome::Failed(_) => "failed",
            Outcome::Moot => "current",
        }
    }

    /// The machine-readable name. Finer than [`Outcome::word`] on purpose:
    /// a consumer deciding whether to page someone needs to tell a BLOCK
    /// from a WARN nobody was around to answer.
    fn tag(&self) -> &'static str {
        match self {
            Outcome::Current => "current",
            Outcome::Pending => "pending",
            Outcome::Adopted => "adopted",
            Outcome::HeldWarn => "held-warn",
            Outcome::HeldBlock => "held-block",
            Outcome::HeldUngraded => "held-ungraded",
            Outcome::Declined => "declined",
            Outcome::RejectedSkip => "rejected-skip",
            Outcome::Unreachable(_) => "unreachable",
            Outcome::Failed(_) => "failed",
            Outcome::Moot => "moot",
        }
    }

    /// Did this run deliberately not act on this package?
    ///
    /// Unreachable counts: the package was not adopted, and the reason was
    /// that pacrat could not look. A refusal does not, for the reason
    /// `updates` does not count one as pending — a decided question re-raised
    /// every hour teaches its reader to ignore the row that mattered.
    fn is_hold(&self) -> bool {
        matches!(
            self,
            // `Pending` cannot survive a run today — every pending row is
            // decided before the report. It is counted as a hold anyway, so
            // that a future early return which forgets to set an outcome
            // fails closed. In the one module whose thesis is that an absent
            // answer is never permission, the placeholder for "no answer
            // yet" must not exit 0.
            Outcome::Pending
                | Outcome::HeldWarn
                | Outcome::HeldBlock
                | Outcome::HeldUngraded
                | Outcome::Declined
                | Outcome::Unreachable(_)
                | Outcome::Failed(_)
        )
    }

    /// Could the run reach this package's upstream at all?
    ///
    /// Narrower than "did it end well", and the narrowness is the point: the
    /// only rule that reads this is the one deciding whether a run learned
    /// *nothing*, and a package that was fetched and then failed a check
    /// taught its reader something rather specific. Only an upstream that
    /// never answered is unlearned-from.
    fn reached(&self) -> bool {
        !matches!(self, Outcome::Unreachable(_))
    }
}

/// Ask the remote where it is, and sort the answer.
///
/// The three predicates are `updates`', not new ones: the verb that reports
/// drift and the verb that acts on it have to mean the same thing by
/// "moved", by "refused", and by "could not ask".
fn detect(package: &str, entry: &SourceEntry) -> Row {
    let row = |candidate: Option<String>, outcome: Outcome| Row {
        package: package.to_string(),
        reviewed: entry.reviewed.clone(),
        candidate,
        verdict: None,
        grade: None,
        outcome,
    };
    match updates::ls_remote(&entry.upstream) {
        Err(e) => row(None, Outcome::Unreachable(e)),
        Ok(head) if !updates::drifted(&entry.reviewed, &head) => row(None, Outcome::Current),
        Ok(head) if updates::refused(entry, &head) => row(Some(head), Outcome::RejectedSkip),
        Ok(head) => row(Some(head), Outcome::Pending),
    }
}

// ------------------------------------------------------- grade and decide

/// Stage the candidate, grade its bytes, and act on the verdict.
fn consider(ctx: &Ctx, mode: Mode, row: &mut Row) {
    let curated = match review::curated(ctx, &row.package) {
        Ok(c) => c,
        Err(e) => return fail(row, e),
    };
    review::preamble(&row.package, &curated);

    // Fetched, never remembered — `review`'s first rule, and the reason this
    // loop grades the clone's HEAD rather than the probe's answer. The two
    // can differ, and the bytes on disk are the only thing a grader read.
    let cand = match review::fetch(&row.package, &curated.entry, &curated.tree) {
        Ok(c) => c,
        Err(e) => return fail(row, e),
    };
    let outcome = decide_and_act(ctx, mode, row, &curated, &cand);
    // Scratch is kept when something went wrong, because then it is
    // evidence; otherwise it is litter. The adopted case is the interesting
    // one: the bytes are in the store now, so the copy in /tmp is redundant.
    review::finish(&cand, !matches!(outcome, Outcome::Failed(_)));
    row.outcome = outcome;
}

fn decide_and_act(
    ctx: &Ctx,
    mode: Mode,
    row: &mut Row,
    curated: &review::Curated,
    cand: &review::Candidate,
) -> Outcome {
    review::header(curated, cand);
    row.candidate = Some(cand.commit.clone());

    // The clone is the authority, so the two questions detection answered
    // are asked again against it. Upstream can move — in either direction —
    // between a probe and a clone, and acting on the probe's answer would be
    // acting on a commit nobody fetched.
    if !updates::drifted(&curated.entry.reviewed, &cand.commit) {
        say!("current   upstream is back at the reviewed commit — nothing to adopt");
        return Outcome::Moot;
    }
    if updates::refused(&curated.entry, &cand.commit) {
        say!("skipped   this candidate was refused before");
        return Outcome::RejectedSkip;
    }

    // What adopting would do, before anything is decided about it — the bar
    // `review` set for a prompt, and the same summary it prints.
    match review::changes_between(curated, cand) {
        Ok(changes) => changes.render(),
        Err(e) => return Outcome::Failed(e),
    }

    say!();
    let graded = match grade::grade_tree(
        ctx,
        &row.package,
        &cand.commit,
        &cand.tree,
        &cand.digest,
        false,
    ) {
        Ok(g) => g,
        Err(e) => return Outcome::Failed(e),
    };

    // The bytes that were read must be the bytes that would be adopted.
    //
    // A grader is handed a path, not a copy, and nothing about running a
    // program stops it writing through the path it was given. A grader that
    // appends to the PKGBUILD it was asked to judge would otherwise have its
    // own line adopted, built and served on the strength of a verdict about
    // the file before it edited it — the digest that made the grading, the
    // digest the cache recorded, and the digest of the tree about to be
    // installed would all be *stated* to be the same one while the tree on
    // disk had moved. Re-hashing is one walk of a handful of files, and it
    // is the only thing standing between "graded" and "graded, then edited
    // by the grader".
    //
    // Checked before the verdict is recorded: a verdict about bytes that are
    // gone is not a verdict about this candidate, and reporting one would
    // invite exactly the reading this refuses.
    match fstree::digest(&cand.tree) {
        Err(e) => return Outcome::Failed(format!("re-reading the graded tree: {e}")),
        Ok(after) if after != cand.digest => {
            say!("digest    {after}");
            return Outcome::Failed(
                "the candidate tree changed while it was being graded — the bytes that \
                 were read are not the bytes that would be adopted"
                    .into(),
            );
        }
        Ok(_) => {}
    }

    row.verdict = Some(graded.verdict);
    row.grade = graded.grade;
    if graded.no_graders {
        say!(
            "reason    this host has no graders configured and nothing is on file for \
             these bytes — see `pacrat grade --help`"
        );
    }

    match decide(mode, graded.verdict) {
        Decision::Hold => {
            say!("{}", hold_line(mode, graded.verdict, &row.package));
            held(graded.verdict)
        }
        Decision::Ask => {
            say!();
            match ask(
                &format!("adopt {} at {}?", row.package, short_hash(&cand.commit)),
                "--mode auto adopts a clean verdict without asking",
            ) {
                Err(e) => Outcome::Failed(e),
                Ok(false) => {
                    say!("not adopted");
                    Outcome::Declined
                }
                Ok(true) => adopt(ctx, row, curated, cand),
            }
        }
        Decision::Adopt => {
            say!();
            say!(
                "adopting  {} — {} in {mode} mode",
                short_hash(&cand.commit),
                graded.verdict
            );
            adopt(ctx, row, curated, cand)
        }
    }
}

fn adopt(ctx: &Ctx, row: &Row, curated: &review::Curated, cand: &review::Candidate) -> Outcome {
    match review::install(ctx, &row.package, curated, cand) {
        Ok(()) => Outcome::Adopted,
        Err(e) => Outcome::Failed(e),
    }
}

fn fail(row: &mut Row, why: String) {
    say!("failed    {}", truncate(&visible_line(&why).0, 200));
    row.outcome = Outcome::Failed(why);
}

/// The verdict a hold is recorded as.
fn held(verdict: Verdict) -> Outcome {
    match verdict {
        Verdict::Block => Outcome::HeldBlock,
        Verdict::Ungraded => Outcome::HeldUngraded,
        // A PROCEED never reaches here — `decide` has no mode that holds one
        // — and if one ever did, calling it a WARN is the safe misnomer.
        _ => Outcome::HeldWarn,
    }
}

/// What a verdict means in a mode. **The whole gate, and nothing else.**
///
/// Pure and tiny on purpose: this is the function that decides whether a
/// machine installs code nobody read, so it should be readable in one sitting
/// and testable without a filesystem, a network or a grader.
///
/// |          | auto | semi | manual |
/// |----------|------|------|--------|
/// | PROCEED  | adopt| adopt| ask    |
/// | WARN     | hold | ask  | ask    |
/// | BLOCK    | hold | hold | hold   |
/// | UNGRADED | hold | hold | ask    |
///
/// Two rows are invariants rather than policy: BLOCK never moves, and
/// UNGRADED is only ever *asked* about — never adopted unasked — because a
/// grader that produced nothing has said nothing.
///
/// The PROCEED row is where this departs from a literal reading of ADR-001's
/// one-line preset sketch ("semi-auto: grade auto, decide manual"). Asking a
/// human to confirm a clean verdict on every package is how a person learns
/// to type `y` without reading, which costs exactly the attention the WARN
/// row needs. So semi spends its prompts where a prompt carries information,
/// and `manual` remains for a person who wants to see everything.
fn decide(mode: Mode, verdict: Verdict) -> Decision {
    match (verdict, mode) {
        (Verdict::Block, _) => Decision::Hold,
        (Verdict::Ungraded, Mode::Manual) => Decision::Ask,
        (Verdict::Ungraded, _) => Decision::Hold,
        (Verdict::Warn, Mode::Auto) => Decision::Hold,
        (Verdict::Warn, _) => Decision::Ask,
        (Verdict::Proceed, Mode::Manual) => Decision::Ask,
        (Verdict::Proceed, _) => Decision::Adopt,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Adopt,
    Ask,
    Hold,
}

/// Why this hold happened, and what the reader can do about it.
fn hold_line(mode: Mode, verdict: Verdict, package: &str) -> String {
    match verdict {
        Verdict::Block => format!(
            "held      {verdict} — this verb has no override. `pacrat review {package}` \
             is the diff; accepting the risk on the record is `pacrat adopt-update \
             {package} --commit <c> --override-block --reason \"…\"`"
        ),
        Verdict::Ungraded => format!(
            "held      {verdict} — nothing graded these bytes, and failure is never \
             read as proceed. `pacrat review {package}`"
        ),
        _ => format!(
            "held      {verdict} — {mode} mode does not adopt a {verdict} unasked. \
             `pacrat review {package}`, then `--mode semi` or `pacrat adopt-update`"
        ),
    }
}

/// Does this mode ever stop and ask?
fn asks(mode: Mode) -> bool {
    mode != Mode::Auto
}

/// Is there a person on the other end of stdin?
///
/// The loop's one question about its caller. See the module note on asking:
/// a pipe is not a person, and one `y` must not answer a whole run.
fn stdin_is_a_human() -> bool {
    std::io::stdin().is_terminal()
}

fn ask(question: &str, hint: &str) -> Result<bool, String> {
    if !stdin_is_a_human() {
        say!("asking    {question} — no terminal to ask, so the answer is no");
        return Ok(false);
    }
    vendor::ask(question, hint)
}

// ------------------------------------------------------------------- build

/// What happened at the build stage.
enum Build {
    /// Nothing was adopted, so there was nothing to serve.
    Nothing,
    /// A human was asked and said no. The trees are in the store and the
    /// packages are not served — a deliberate not-doing, which is a hold.
    Declined,
    Ran(Result<(), String>),
}

/// Build what was adopted.
fn build_stage(ctx: &Ctx, mode: Mode, adopted: &[String]) -> Build {
    if adopted.is_empty() {
        return Build::Nothing;
    }
    say!();
    say!("──────── build");
    say!("packages  {}", list_preview(adopted, 12));
    if mode == Mode::Manual {
        // One question for the stage, not one per package: the decisions
        // were made above, and this is the "and now put them where pacman
        // can see them" step.
        match ask(
            "build the adopted packages now?",
            "auto and semi build without asking",
        ) {
            Err(e) => return Build::Ran(Err(e)),
            Ok(false) => {
                say!(
                    "not built — the store holds the adopted trees; `pacrat build` \
                     serves them when you are ready"
                );
                return Build::Declined;
            }
            Ok(true) => {}
        }
    }
    Build::Ran(build::run(ctx, adopted))
}

// ------------------------------------------------------------------ report

/// Everything the exit code and the report turn on.
struct Summary<'a> {
    rows: &'a [Row],
    adopted: &'a [String],
    build: &'a Build,
}

impl Summary<'_> {
    fn holds(&self) -> usize {
        self.rows.iter().filter(|r| r.outcome.is_hold()).count()
    }
    fn reached(&self) -> usize {
        self.rows.iter().filter(|r| r.outcome.reached()).count()
    }
    /// Rows worth a line in the table: everything that is not a package
    /// sitting quietly at its reviewed commit.
    fn interesting(&self) -> Vec<&Row> {
        self.rows
            .iter()
            .filter(|r| !matches!(r.outcome, Outcome::Current))
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Exit {
    Clean,
    Held,
    Broken(String),
}

/// The exit lattice, in order — the first rule that applies wins. Pure, so
/// that "exit 0 means the loop finished its work" is a property with a test
/// behind it rather than a claim in a doc comment.
fn exit_of(s: &Summary) -> Exit {
    if matches!(s.build, Build::Ran(Err(_))) {
        return Exit::Broken(
            "the build stage failed — the store holds the adopted trees, but they are \
             not served; the summary above says which package and why"
                .into(),
        );
    }
    // A run that could not ask a single question learned nothing, and
    // "nothing pending" is not something it may imply by exiting 10 either.
    if !s.rows.is_empty() && s.reached() == 0 {
        return Exit::Broken(format!(
            "none of the {} ledger upstream{} could be reached — this run learned \
             nothing about any of them",
            s.rows.len(),
            if s.rows.len() == 1 { "" } else { "s" }
        ));
    }
    // A declined build is a hold for the same reason every other hold is
    // one: something the run could have done, it deliberately did not, and
    // the packages sitting adopted-but-unserved are the outstanding work.
    if s.holds() > 0 || matches!(s.build, Build::Declined) {
        return Exit::Held;
    }
    Exit::Clean
}

fn report_text(s: &Summary, exit: &Exit) {
    let rows = s.interesting();
    say!();
    say!("summary");
    if rows.is_empty() {
        say!(
            "  nothing pending — {} curated package{} at their reviewed commits",
            s.rows.len(),
            if s.rows.len() == 1 { "" } else { "s" }
        );
    }
    let name_w = rows
        .iter()
        .map(|r| r.package.chars().count())
        .max()
        .unwrap_or(7)
        .clamp(7, NAME_CAP);
    for row in &rows {
        say!(
            "  {:<11}  {:<name_w$}  {:<8}  {:<8}  {}",
            row.outcome.word(),
            truncate(&visible_line(&row.package).0, name_w),
            row.candidate.as_deref().map_or("—", short_hash),
            row.verdict.map_or("—", Verdict::label),
            truncate(&visible_line(&why(row)).0, 90),
        );
    }

    say!();
    say!(
        "reached   {} of {} · {} adopted · {} held{}",
        s.reached(),
        s.rows.len(),
        s.adopted.len(),
        s.holds(),
        // Named on the tally line as well as below it: a run that exits 10
        // over a declined build would otherwise read "0 held" and leave the
        // reader hunting for the row that earned the code.
        match s.build {
            Build::Declined => " · build declined",
            _ => "",
        }
    );
    match s.build {
        Build::Nothing => {}
        Build::Declined => say!(
            "served    nothing — the build was declined; `pacrat build` serves the \
             adopted trees when you are ready"
        ),
        Build::Ran(Ok(())) => say!("served    {}", list_preview(s.adopted, 12)),
        Build::Ran(Err(e)) => say!("served    build failed: {}", truncate(e, 90)),
    }
    if matches!(exit, Exit::Held) && s.holds() > 0 {
        say!(
            "next      `pacrat review <package>` for anything held; nothing above was \
             adopted past a gate"
        );
    }
}

/// Why a row ended the way it did, in one line.
fn why(row: &Row) -> String {
    match &row.outcome {
        Outcome::Adopted => "tree and ledger advanced".into(),
        Outcome::HeldWarn => format!("{} holds unless a human says otherwise", Verdict::Warn),
        Outcome::HeldBlock => format!("{} always holds; no override in this verb", Verdict::Block),
        Outcome::HeldUngraded => "nothing graded these bytes".into(),
        Outcome::Declined => "asked, and the answer was no".into(),
        Outcome::RejectedSkip => "a human refused this candidate".into(),
        Outcome::Unreachable(e) => format!("could not ask: {e}"),
        Outcome::Failed(e) => e.lines().next().unwrap_or_default().to_string(),
        Outcome::Current | Outcome::Moot => "at the reviewed commit".into(),
        Outcome::Pending => "detected, not yet decided".into(),
    }
}

#[derive(Serialize)]
struct Report<'a> {
    mode: &'a str,
    packages: Vec<PackageJson<'a>>,
    adopted: &'a [String],
    /// What reached the repo. Empty when the build stage did not run or did
    /// not finish — pacrat does not claim a package is served on the
    /// strength of having tried.
    served: Vec<&'a str>,
    /// Upstreams this run got an answer from. `reached < total` with an
    /// empty `packages[].candidate` is the shape of a partial run.
    reached: usize,
    total: usize,
    holds: usize,
    /// `nothing` · `declined` · `ok` · `failed`. Distinct from `served`
    /// being empty, which three of those four share.
    build: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_error: Option<String>,
    /// The code this run is about to exit with. A consumer that has to parse
    /// stderr to learn why it got a 10 does not have a machine format.
    exit: i32,
    /// Where the store is, so a fleet-wide collector can say whose run this
    /// was without being told separately.
    store: String,
    host: &'a str,
}

#[derive(Serialize)]
struct PackageJson<'a> {
    package: &'a str,
    reviewed: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grade: Option<u8>,
    outcome: &'static str,
    /// The reason, for the outcomes that have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

fn report_json(ctx: &Ctx, mode: Mode, s: &Summary, exit: &Exit) -> Result<(), String> {
    let report = Report {
        mode: mode.label(),
        packages: s
            .rows
            .iter()
            .map(|row| PackageJson {
                package: &row.package,
                reviewed: &row.reviewed,
                candidate: row.candidate.as_deref(),
                verdict: row.verdict.map(Verdict::label),
                grade: row.grade,
                outcome: row.outcome.tag(),
                detail: match &row.outcome {
                    Outcome::Unreachable(e) | Outcome::Failed(e) => Some(visible_line(e).0),
                    _ => None,
                },
            })
            .collect(),
        adopted: s.adopted,
        served: match s.build {
            Build::Ran(Ok(())) => s.adopted.iter().map(String::as_str).collect(),
            _ => Vec::new(),
        },
        build: match s.build {
            Build::Nothing => "nothing",
            Build::Declined => "declined",
            Build::Ran(Ok(())) => "ok",
            Build::Ran(Err(_)) => "failed",
        },
        reached: s.reached(),
        total: s.rows.len(),
        holds: s.holds(),
        build_error: match s.build {
            Build::Ran(Err(e)) => Some(visible_line(e).0),
            _ => None,
        },
        exit: match exit {
            Exit::Clean => 0,
            Exit::Held => HELD,
            Exit::Broken(_) => 1,
        },
        store: ctx.store.display().to_string(),
        host: &ctx.host,
    };
    let text = serde_json::to_string_pretty(&report).map_err(|e| format!("json: {e}"))?;
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: [Mode; 3] = [Mode::Auto, Mode::Semi, Mode::Manual];
    const VERDICTS: [Verdict; 4] = [
        Verdict::Proceed,
        Verdict::Warn,
        Verdict::Block,
        Verdict::Ungraded,
    ];

    /// The gate, spelled out. Written as the whole table rather than as
    /// clever assertions, because this is the one function in pacrat where
    /// a wrong cell means a machine installs code nobody read.
    #[test]
    fn the_decide_matrix_is_exactly_this() {
        use Decision::{Adopt, Ask, Hold};
        let table = [
            (Verdict::Proceed, [Adopt, Adopt, Ask]),
            (Verdict::Warn, [Hold, Ask, Ask]),
            (Verdict::Block, [Hold, Hold, Hold]),
            (Verdict::Ungraded, [Hold, Hold, Ask]),
        ];
        for (verdict, row) in table {
            for (mode, expected) in MODES.into_iter().zip(row) {
                assert_eq!(
                    decide(mode, verdict),
                    expected,
                    "{verdict} in {mode} mode should be {expected:?}"
                );
            }
        }
    }

    /// ADR-001's invariant, as a property over the whole space rather than
    /// as three cells anybody could edit one at a time.
    #[test]
    fn no_mode_adopts_a_block_and_none_adopts_an_ungraded_unasked() {
        for mode in MODES {
            assert_eq!(decide(mode, Verdict::Block), Decision::Hold);
            assert_ne!(decide(mode, Verdict::Ungraded), Decision::Adopt);
        }
    }

    /// The timer's mode never stops for anybody: a prompt in a systemd unit
    /// is a run that hangs until the timeout, which is worse than a hold.
    #[test]
    fn auto_mode_never_asks() {
        for verdict in VERDICTS {
            assert_ne!(decide(Mode::Auto, verdict), Decision::Ask, "{verdict}");
        }
        // And only a clean verdict gets through it.
        for verdict in VERDICTS {
            assert_eq!(
                decide(Mode::Auto, verdict) == Decision::Adopt,
                verdict == Verdict::Proceed,
                "{verdict}"
            );
        }
    }

    fn row(package: &str, outcome: Outcome) -> Row {
        Row {
            package: package.into(),
            reviewed: "3f9c21ab".into(),
            candidate: None,
            verdict: None,
            grade: None,
            outcome,
        }
    }

    /// The build outcomes the lattice tests need, as values a `Summary` can
    /// borrow.
    const NOTHING: Build = Build::Nothing;

    fn summary<'a>(rows: &'a [Row], adopted: &'a [String], build: &'a Build) -> Summary<'a> {
        Summary {
            rows,
            adopted,
            build,
        }
    }

    #[test]
    fn a_quiet_run_and_a_fully_adopted_one_both_exit_zero() {
        let rows = [row("a", Outcome::Current), row("b", Outcome::Current)];
        assert_eq!(exit_of(&summary(&rows, &[], &NOTHING)), Exit::Clean);
        // An empty ledger is not a broken run: there was nothing to check.
        assert_eq!(exit_of(&summary(&[], &[], &NOTHING)), Exit::Clean);

        let adopted = ["a".to_string()];
        let rows = [row("a", Outcome::Adopted), row("b", Outcome::Current)];
        assert_eq!(exit_of(&summary(&rows, &adopted, &NOTHING)), Exit::Clean);
        // A refusal is a decided question, not outstanding work.
        let rows = [row("a", Outcome::RejectedSkip)];
        assert_eq!(exit_of(&summary(&rows, &[], &NOTHING)), Exit::Clean);
    }

    #[test]
    fn every_kind_of_hold_is_a_ten() {
        for outcome in [
            Outcome::HeldWarn,
            Outcome::HeldBlock,
            Outcome::HeldUngraded,
            Outcome::Declined,
            Outcome::Unreachable("no route".into()),
            Outcome::Failed("clone failed".into()),
        ] {
            // Beside a row that *was* checked, so "nothing checkable" does
            // not claim the run first.
            let rows = [row("ok", Outcome::Current), row("x", outcome.clone())];
            assert_eq!(
                exit_of(&summary(&rows, &[], &NOTHING)),
                Exit::Held,
                "{outcome:?}"
            );
        }
    }

    /// An unreachable upstream costs its own row and no other — until it is
    /// every row, at which point the run has learned nothing and says so.
    #[test]
    fn a_run_that_could_reach_nothing_is_a_failure_not_a_hold() {
        let rows = [
            row("a", Outcome::Unreachable("no route".into())),
            row("b", Outcome::Unreachable("timed out".into())),
        ];
        let Exit::Broken(why) = exit_of(&summary(&rows, &[], &NOTHING)) else {
            panic!("a run that reached nothing must fail");
        };
        assert!(why.contains("learned nothing"), "{why}");

        // One row that answered is enough to make the rest merely holds.
        let rows = [
            row("a", Outcome::Unreachable("no route".into())),
            row("b", Outcome::Current),
        ];
        assert_eq!(exit_of(&summary(&rows, &[], &NOTHING)), Exit::Held);
    }

    /// A package that was fetched and then failed a check is a hold, even
    /// alone. The run learned something quite specific about it — a tampering
    /// grader, a tree with no PKGBUILD — and "none of the upstreams could be
    /// checked" would be a false account of a run that reached every one.
    #[test]
    fn a_failure_after_a_successful_fetch_is_a_hold_not_a_blind_run() {
        let rows = [row(
            "a",
            Outcome::Failed("the candidate tree changed".into()),
        )];
        assert_eq!(exit_of(&summary(&rows, &[], &NOTHING)), Exit::Held);
    }

    /// Adopting and then declining the build leaves packages in the store
    /// that pacman cannot see. That is outstanding work, and 0 would tell a
    /// caller the run finished it.
    #[test]
    fn a_declined_build_is_a_hold_even_with_nothing_else_outstanding() {
        let rows = [row("a", Outcome::Adopted)];
        let adopted = ["a".to_string()];
        assert_eq!(
            exit_of(&summary(&rows, &adopted, &Build::Declined)),
            Exit::Held
        );
        // The same run that went through with it is clean.
        assert_eq!(
            exit_of(&summary(&rows, &adopted, &Build::Ran(Ok(())))),
            Exit::Clean
        );
    }

    /// A build failure outranks everything: the trees are in the store and
    /// nothing is served, which is the state a caller most needs to know.
    #[test]
    fn a_failed_build_owns_the_exit_code() {
        let rows = [row("a", Outcome::Adopted), row("b", Outcome::HeldBlock)];
        let adopted = ["a".to_string()];
        let Exit::Broken(why) = exit_of(&summary(&rows, &adopted, &Build::Ran(Err("boom".into()))))
        else {
            panic!("a failed build must fail the run");
        };
        assert!(why.contains("not served"), "{why}");
    }

    #[test]
    fn the_tallies_account_for_every_row() {
        let rows = [
            row("a", Outcome::Adopted),
            row("b", Outcome::Current),
            row("c", Outcome::HeldBlock),
            row("d", Outcome::Unreachable("x".into())),
        ];
        let s = summary(&rows, &[], &NOTHING);
        assert_eq!(s.reached(), 3);
        assert_eq!(s.holds(), 2);
        // Everything but a quiet current row earns a line in the table.
        assert_eq!(s.interesting().len(), 3);
    }

    /// Two words per outcome, and they must not drift apart: the table's
    /// word is for a human, the tag is what a machine keys on.
    #[test]
    fn every_outcome_has_a_word_and_a_distinct_tag() {
        let all = [
            Outcome::Current,
            Outcome::Pending,
            Outcome::Adopted,
            Outcome::HeldWarn,
            Outcome::HeldBlock,
            Outcome::HeldUngraded,
            Outcome::Declined,
            Outcome::RejectedSkip,
            Outcome::Unreachable("x".into()),
            Outcome::Failed("x".into()),
            Outcome::Moot,
        ];
        let mut tags: Vec<&str> = all.iter().map(Outcome::tag).collect();
        tags.sort_unstable();
        let count = tags.len();
        tags.dedup();
        assert_eq!(tags.len(), count, "two outcomes share a tag");
        for outcome in &all {
            assert!(!outcome.word().is_empty());
            // A hold is never something the run did, and never a row that
            // answered cleanly.
            if outcome.is_hold() {
                assert_ne!(*outcome, Outcome::Adopted);
                assert_ne!(*outcome, Outcome::Current);
            }
        }
    }
}
