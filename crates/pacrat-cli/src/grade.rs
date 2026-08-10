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
//! Two other rules shape the code:
//!
//! - **The tree is the thing that was reviewed.** Graders run against
//!   `<store>/aur/packages/<name>/`, and a grading is cached under the
//!   commit that tree is at. Grading a *different* commit is refused rather
//!   than approximated, because a grading filed under commit C that actually
//!   read the tree at commit R is worse than no grading: it would let C
//!   through the gate later, unread.
//! - **Grader output is attacker text.** It has just been fed a PKGBUILD
//!   whose author would like to say things to the reviewer's terminal, so
//!   every string that comes back — titles, spans, the grader's own name,
//!   stderr — goes through [`crate::out::visible`] before it is printed.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pacrat_core::config::{Grader, MANUAL};
use pacrat_core::grading::{commit_matches, GradeReport, Subject, CONTRACT, PACRAT_SCALE};
use pacrat_core::{Thresholds, Verdict};

use crate::ctx::{self, Ctx};
use crate::out::{shell_quote, truncate, visible};
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

// ------------------------------------------------------------------ verbs

pub fn run(
    ctx: &Ctx,
    package: &str,
    commit: Option<&str>,
    grade: Option<u8>,
    note: Option<&str>,
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

    // An abbreviation of the reviewed commit *is* the reviewed commit; the
    // cache is keyed by the full hash so a later run finds this grading.
    let commit = match commit {
        None => entry.reviewed.clone(),
        Some(c) => {
            let c = check_commit(c)?;
            if commit_matches(c, &entry.reviewed) {
                entry.reviewed.clone()
            } else {
                c.to_string()
            }
        }
    };

    match grade {
        Some(g) => record_manual(ctx, package, &commit, g, note),
        None => {
            if note.is_some() {
                return Err(
                    "--note records the reason for a --grade; it has no meaning \
                            on its own"
                        .into(),
                );
            }
            run_graders(ctx, package, &commit, &entry.reviewed)
        }
    }
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
    // The same shape a program's grading gets: one cache, one reader.
    let dir = cache_dir(package)?;
    let path = ok_path(&dir, commit, MANUAL);
    write_cache(&path, &report.to_json())?;
    let _ = fs::remove_file(failed_path(&dir, commit, MANUAL));

    println!("package   {package}");
    println!("commit    {commit}");
    println!("grader    {MANUAL}");
    println!("recorded  {}", path.display());
    let (safe_note, _) = visible(note);
    println!("note      {safe_note}");
    println!();
    hold_if_held(verdict_line(&ctx.config.thresholds, Some(grade)));
    Ok(())
}

fn run_graders(ctx: &Ctx, package: &str, commit: &str, reviewed: &str) -> Result<(), String> {
    let tree = ctx.store.join("aur").join("packages").join(package);
    if !tree.is_dir() {
        return Err(format!(
            "{package} has no store tree at {} — the ledger and the store disagree; \
             `pacrat vendor {package} --force` re-installs it",
            tree.display()
        ));
    }
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

    if ctx.config.graders.is_empty() {
        return Err(no_graders_message(package));
    }

    println!("package   {package}");
    println!("commit    {commit}");
    println!("tree      {}", tree.display());

    let dir = cache_dir(package)?;
    let tree_arg = tree.to_string_lossy().into_owned();
    let mut outcomes: Vec<(String, Outcome)> = Vec::new();

    for grader in &ctx.config.graders {
        println!();
        println!("grader    {}", grader.name);
        let outcome = grade_with(grader, &dir, package, commit, &tree_arg);
        report_outcome(&dir, commit, &grader.name, &outcome);
        outcomes.push((grader.name.clone(), outcome));
    }

    // Gradings nobody asked for this run — a manual one, or a grader that
    // has since been removed from the config. They are gradings of this
    // exact subject, so they count.
    for (name, report) in other_cached(&dir, commit, &ctx.config.graders, package) {
        println!();
        println!("grader    {name}");
        println!("cache     {}", ok_path(&dir, commit, &name).display());
        let outcome = Outcome::Graded { report, took: None };
        report_outcome(&dir, commit, &name, &outcome);
        outcomes.push((name, outcome));
    }

    println!();
    hold_if_held(summarize(ctx, &outcomes));
    Ok(())
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

fn grade_with(grader: &Grader, dir: &Path, package: &str, commit: &str, tree: &str) -> Outcome {
    let cache = ok_path(dir, commit, &grader.name);
    match read_cache(&cache, package, commit) {
        Ok(Some(report)) => {
            println!("cache     {}", cache.display());
            return Outcome::Graded { report, took: None };
        }
        Ok(None) => {}
        // A cache entry we cannot trust is not a reason to hold — it is a
        // reason to ask again — but the user should know it was there.
        Err(e) => println!("warning   ignoring {}: {e}", cache.display()),
    }

    let argv = grader.argv(package, tree, commit);
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
    match ran.code {
        Some(0) => {}
        Some(code) => return fail(format!("exited {code}{}", stderr_tail(&ran.stderr))),
        None => return fail(format!("killed by a signal{}", stderr_tail(&ran.stderr))),
    }

    let text = String::from_utf8_lossy(&ran.stdout);
    let report = match GradeReport::from_json(&text) {
        Ok(r) => r,
        Err(e) => return fail(visible(&e).0),
    };
    if let Err(e) = report.is_about(package, commit) {
        return fail(visible(&e).0);
    }

    // Only a grading that survived every check is written to the cache; the
    // cache must never hold something a later read would have to re-judge.
    //
    // Keyed by *our* commit, never the grader's own spelling of it:
    // `is_about` accepts an abbreviation, and filing this under `3f9c21ab`
    // when every read asks for the full hash would be a cache that never
    // hits — the grader would be re-run, and paid for, on every invocation.
    if let Err(e) = write_cache(&cache, &text) {
        println!("warning   {e}");
    }
    let _ = fs::remove_file(failed_path(dir, commit, &grader.name));
    Outcome::Graded {
        report,
        took: Some(elapsed),
    }
}

fn report_outcome(dir: &Path, commit: &str, name: &str, outcome: &Outcome) {
    match outcome {
        Outcome::Graded { report, took } => {
            let freshness = match took {
                None => "cached".to_string(),
                Some(d) => format!("fresh {:.1}s", d.as_secs_f64()),
            };
            let mut line = format!(
                "grade {} of {}-{} · {freshness}",
                report.grade, report.scale.min, report.scale.max
            );
            // Only worth saying when the grader used its own scale.
            if report.scale != PACRAT_SCALE {
                line.push_str(&format!(" · pacrat {} of 0-4", report.pacrat_grade()));
            }
            println!("result    {line}");

            // The grader's own idea of its name, if it disagrees with ours.
            let (reported, _) = visible(&report.grader);
            if reported != name {
                println!("reports   as {:?}", truncate(&reported, 40));
            }
            if let Some(note) = report.meta.get("note").and_then(|v| v.as_str()) {
                println!("note      {}", truncate(&visible(note).0, 90));
            }

            for f in report.top_findings(FINDINGS_SHOWN) {
                let (title, _) = visible(&f.title);
                let span = match &f.span {
                    Some(s) => format!("{} · ", truncate(&visible(s).0, 30)),
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
        Outcome::Failed { reason, elapsed } => {
            println!(
                "result    {} · {} · {:.1}s",
                Verdict::Ungraded,
                truncate(reason, 120),
                elapsed.as_secs_f64()
            );
            let path = failed_path(dir, commit, name);
            match record_failure(&path, name, commit, reason) {
                // Kept so a later run — or the jobs view — can say why this
                // grader contributed nothing, without re-running it.
                Ok(()) => println!("recorded  {}", path.display()),
                Err(e) => println!("warning   {e}"),
            }
        }
    }
}

fn summarize(ctx: &Ctx, outcomes: &[(String, Outcome)]) -> Verdict {
    let grades: Vec<u8> = outcomes
        .iter()
        .filter_map(|(_, o)| match o {
            Outcome::Graded { report, .. } => Some(report.pacrat_grade()),
            Outcome::Failed { .. } => None,
        })
        .collect();
    let failed: Vec<&str> = outcomes
        .iter()
        .filter(|(_, o)| matches!(o, Outcome::Failed { .. }))
        .map(|(n, _)| n.as_str())
        .collect();

    // Worst-wins across the graders that answered (ADR-001 open question 5).
    let verdict = verdict_line(&ctx.config.thresholds, grades.iter().copied().max());

    if !failed.is_empty() {
        if grades.is_empty() {
            println!(
                "reason    no grader answered ({} failed: {}) — ungraded holds; it is \
                 never read as proceed",
                failed.len(),
                failed.join(", ")
            );
        } else {
            // Worst-wins over what answered is only sound if the reader
            // knows what did not: the missing grader could have been worse.
            println!(
                "warning   {} of {} graders produced no grading ({}) — this verdict rests \
                 on the {} that answered",
                failed.len(),
                outcomes.len(),
                failed.join(", "),
                grades.len()
            );
        }
    }
    verdict
}

/// The line the whole command exists to print. The thresholds are named on
/// it because the same grade is a different verdict on another host: the
/// number came from the grader, the reading of it is this host's config.
fn verdict_line(t: &Thresholds, grade: Option<u8>) -> Verdict {
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
    for (is_err, pipe) in [
        (false, child.stdout.take().map(PipeRead::Out)),
        (true, child.stderr.take().map(PipeRead::Err)),
    ] {
        let Some(mut pipe) = pipe else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
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
    format!(" — {}", truncate(&visible(first).0, 100))
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

/// A cached grading, if there is a trustworthy one. `Ok(None)` is a miss;
/// `Err` is a file that exists and cannot be believed.
fn read_cache(path: &Path, package: &str, commit: &str) -> Result<Option<GradeReport>, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    // Re-validated on the way out, not trusted because we wrote it: the file
    // is in a user-writable directory and may be years older than this
    // binary's idea of the contract.
    let report = GradeReport::from_json(&text)?;
    report.is_about(package, commit)?;
    Ok(Some(report))
}

fn write_cache(path: &Path, json: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    fs::write(path, json).map_err(|e| format!("{}: {e}", path.display()))
}

/// Why a grader produced nothing. Deliberately *not* a `GradeReport`: the
/// distinct top-level key means no reader can mistake this file for a
/// grading, whatever it does with the rest of the fields.
fn record_failure(path: &Path, grader: &str, commit: &str, reason: &str) -> Result<(), String> {
    let record = serde_json::json!({
        "pacrat": "grader-failure/v1",
        "grader": grader,
        "commit": commit,
        "reason": reason,
        "at": now_secs(),
    });
    write_cache(path, &format!("{record:#}\n"))
}

/// Cached gradings for this subject from graders that are not in the config
/// — the built-in `manual` one, or a grader since removed. Unreadable files
/// are skipped: this is a scan, and a stray file here must not fail the run.
fn other_cached(
    dir: &Path,
    commit: &str,
    configured: &[Grader],
    package: &str,
) -> Vec<(String, GradeReport)> {
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
            if name.is_empty() || configured.iter().any(|g| g.name == name) {
                return None;
            }
            let report = read_cache(&e.path(), package, commit).ok().flatten()?;
            Some((name.to_string(), report))
        })
        .collect();
    // `manual` first, then alphabetical: a human's reading leads.
    found.sort_by(|a, b| (a.0 != MANUAL, &a.0).cmp(&(b.0 != MANUAL, &b.0)));
    found
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

    #[test]
    fn commits_must_look_like_commits() {
        assert!(check_commit("3f9c21ab").is_ok());
        assert!(check_commit(&"a".repeat(40)).is_ok());
        for bad in [
            "",
            "abc", // too short to name one tree
            "../../etc/passwd",
            "3f9c21ab/../x",
            "HEAD~1",
            "3f9c21ab.manual", // would collide with a cache filename
            &"a".repeat(65),
        ] {
            assert!(check_commit(bad).is_err(), "{bad:?} should be rejected");
        }
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

    fn report_json(package: &str, commit: &str, grade: u8) -> String {
        format!(
            r#"{{"contract":"pacrat-grade/v1","grader":"stub",
                 "subject":{{"package":"{package}","commit":"{commit}"}},"grade":{grade}}}"#
        )
    }

    #[test]
    fn the_cache_re_validates_what_it_reads() {
        let f = Fixture::new("cache");
        let path = ok_path(&f.dir, "3f9c21ab", "stub");

        // A miss is not an error.
        assert_eq!(read_cache(&path, "mdcat", "3f9c21ab").unwrap(), None);

        write_cache(&path, &report_json("mdcat", "3f9c21ab", 2)).unwrap();
        let hit = read_cache(&path, "mdcat", "3f9c21ab").unwrap().unwrap();
        assert_eq!(hit.grade, 2);

        // Same file, different subject: not a hit, and not silently ignored.
        assert!(read_cache(&path, "pacseek", "3f9c21ab").is_err());
        assert!(read_cache(&path, "mdcat", "deadbeefcafe").is_err());

        // A file that is no longer a valid grading is an error, not a grade.
        write_cache(&path, "{\"contract\":\"pacrat-grade/v0\"}").unwrap();
        assert!(read_cache(&path, "mdcat", "3f9c21ab").is_err());
    }

    #[test]
    fn a_failure_record_can_never_be_read_as_a_grading() {
        let f = Fixture::new("failrec");
        let path = failed_path(&f.dir, "3f9c21ab", "stub");
        record_failure(&path, "stub", "3f9c21ab", "timed out after 2s").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("grader-failure/v1"));
        assert!(text.contains("timed out after 2s"));
        assert!(
            GradeReport::from_json(&text).is_err(),
            "a failure record parsed as a grading: {text}"
        );
    }

    #[test]
    fn unconfigured_gradings_are_found_but_failures_and_strangers_are_not() {
        let f = Fixture::new("others");
        let commit = "3f9c21ab";
        write_cache(
            &ok_path(&f.dir, commit, MANUAL),
            &report_json("mdcat", commit, 1),
        )
        .unwrap();
        write_cache(
            &ok_path(&f.dir, commit, "retired"),
            &report_json("mdcat", commit, 3),
        )
        .unwrap();
        write_cache(
            &ok_path(&f.dir, commit, "configured"),
            &report_json("mdcat", commit, 0),
        )
        .unwrap();
        // Another commit's grading, a failure record, and a corrupt file.
        write_cache(
            &ok_path(&f.dir, "deadbeefcafe", "manual"),
            &report_json("mdcat", "deadbeefcafe", 4),
        )
        .unwrap();
        record_failure(&failed_path(&f.dir, commit, "flaky"), "flaky", commit, "x").unwrap();
        write_cache(&ok_path(&f.dir, commit, "corrupt"), "{").unwrap();

        let configured = vec![Grader {
            name: "configured".into(),
            cmd: vec!["x".into()],
            timeout_s: 300,
        }];
        let found = other_cached(&f.dir, commit, &configured, "mdcat");
        let names: Vec<&str> = found.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, [MANUAL, "retired"], "found {names:?}");
        assert_eq!(found[0].1.grade, 1);

        // A directory that does not exist is an empty scan, not a failure.
        assert!(other_cached(Path::new("/nonexistent"), commit, &[], "mdcat").is_empty());
    }

    /// The end of the loop: run, cache, hit. The grader here abbreviates the
    /// commit in its report — allowed — so this also pins that the cache is
    /// keyed by pacrat's hash and not the grader's spelling of it. Keyed the
    /// other way the file would be written where no read looks for it, and
    /// an expensive grader would be paid for on every single run.
    #[test]
    fn a_grading_is_cached_and_the_second_run_does_not_invoke() {
        let f = Fixture::new("cachehit");
        let full = "3f9c21ab55de0011223344556677889900aabbcc";
        let counter = f.dir.join("runs");
        let cmd = f.grader_argv(
            "abbrev",
            &format!(
                "echo ran >> {}\n\
                 echo '{{\"contract\":\"pacrat-grade/v1\",\"grader\":\"abbrev\",\
                 \"subject\":{{\"package\":\"mdcat\",\"commit\":\"3f9c21ab\"}},\"grade\":2}}'\n",
                counter.display()
            ),
        );
        let grader = Grader {
            name: "abbrev".into(),
            cmd,
            timeout_s: 30,
        };

        let first = grade_with(&grader, &f.dir, "mdcat", full, "/tree");
        assert!(
            matches!(first, Outcome::Graded { took: Some(_), .. }),
            "the first run should have invoked the grader"
        );
        let second = grade_with(&grader, &f.dir, "mdcat", full, "/tree");
        match second {
            Outcome::Graded { report, took } => {
                assert_eq!(took, None, "the second run should have been cached");
                assert_eq!(report.grade, 2);
            }
            Outcome::Failed { reason, .. } => panic!("second run failed: {reason}"),
        }

        // The only proof that matters: the program ran once, not twice.
        assert_eq!(fs::read_to_string(&counter).unwrap(), "ran\n");
        assert!(ok_path(&f.dir, full, "abbrev").exists());
    }

    /// A grader that fails leaves a breadcrumb and no grading — and is asked
    /// again next time, because a failure is not an answer to cache.
    #[test]
    fn a_failure_is_recorded_but_never_cached_as_a_grading() {
        let f = Fixture::new("failtwice");
        let commit = "3f9c21ab55de0011223344556677889900aabbcc";
        let counter = f.dir.join("runs");
        let cmd = f.grader_argv(
            "broken",
            &format!("echo ran >> {}\necho 'not json'\n", counter.display()),
        );
        let grader = Grader {
            name: "broken".into(),
            cmd,
            timeout_s: 30,
        };

        for _ in 0..2 {
            let outcome = grade_with(&grader, &f.dir, "mdcat", commit, "/tree");
            let Outcome::Failed { reason, .. } = outcome else {
                panic!("garbage on stdout must not produce a grading");
            };
            report_outcome(
                &f.dir,
                commit,
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
        assert!(failed_path(&f.dir, commit, "broken").exists());
        assert!(
            !ok_path(&f.dir, commit, "broken").exists(),
            "a failure was cached as a grading"
        );
    }
}
