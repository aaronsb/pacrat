//! `pacrat updates` — the detect step of ADR-001's update loop, on its own.
//!
//! Two questions, one command, because they are the same worry seen from
//! either side of the custody line (mockup §5):
//!
//! 1. **Pending.** For every package in the ledger, has upstream moved past
//!    the commit a human reviewed? That is one `git ls-remote` per entry,
//!    compared against `reviewed`.
//! 2. **Upstream.** For every AUR package some host merely *tracks*, is there
//!    a newer version out there? Those update outside curation entirely — the
//!    answer is not "adopt it", it is "vendor it first".
//!
//! ## Degradation
//!
//! Both halves reach the network, and either may fail without taking the run
//! down — the rule `search` follows for its two worlds. A package whose
//! remote will not answer is reported as unreachable and the others still
//! report; a dead AUR RPC costs the version column and nothing else. Only
//! when *nothing* could be asked does the verb fail, because at that point a
//! "nothing pending" would be a claim pacrat cannot make.
//!
//! ## Exit codes
//!
//! 0 nothing pending · 10 updates pending · 1 the check could not run.
//!
//! Ten is the point of the verb. ADR-001 gives it to the headless loop as
//! "ran fine, deliberately did not act", and a timer that runs `pacrat
//! updates` wants exactly that distinction: 0 means go back to sleep, 10
//! means a human has something to look at, 1 means the check itself is
//! broken and the silence is not evidence of calm.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use clap::ValueEnum;
use pacrat_core::pkg::Source;
use pacrat_core::sources::{Role, SourceEntry};
use pacrat_core::version::is_newer;
use serde::Serialize;

use crate::aur::{self, AurPkg};
use crate::ctx::Ctx;
use crate::custody;
use crate::out::{shell_quote, short_hash, truncate, visible};
use crate::pacman;

/// How many upstream rows the CLI prints before counting the rest. The
/// pending list is the actionable one and is never capped; the upstream
/// region is awareness, and a host tracking three hundred AUR packages
/// should not bury it (`out::list_preview`'s rule, one row per line).
const UPSTREAM_CAP: usize = 20;

/// Widest package name a column will show before it is clipped.
const NAME_CAP: usize = 32;

/// The `reviewed → candidate` cell, sized for its widest possible contents:
/// `malformed → unreachable`. Padding the pair as one cell rather than each
/// side separately is what keeps the arrows in a line when a short hash and a
/// word share the column.
const DRIFT_W: usize = 23;

/// `maintained`, the longest custody label.
const ROLE_W: usize = 10;

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Columns for a human
    Text,
    /// One JSON object for a machine
    Json,
}

impl Format {
    /// Say something that is not the answer: an argv, a warning, a count.
    ///
    /// In JSON mode stdout belongs to the object alone, so this goes to
    /// stderr rather than being dropped. ADR-001's "external calls are always
    /// visible" has no quiet mode — `--format json` changes where the trace
    /// lands, never whether it exists.
    fn trace(self, line: &str) {
        match self {
            Format::Text => println!("{line}"),
            Format::Json => eprintln!("{line}"),
        }
    }
}

/// What `git ls-remote` had to say about one ledger entry.
#[derive(Debug, PartialEq, Eq)]
enum Probe {
    /// Upstream HEAD is the reviewed commit; nothing to do.
    Current,
    /// Upstream has moved: the candidate commit awaiting review.
    Pending(String),
    /// Could not ask. Not the same as "no update" and never counted as one.
    Unreachable(String),
}

/// One ledger package, probed.
struct Row {
    package: String,
    reviewed: String,
    role: Role,
    /// The AUR repository behind `upstream`, when it is an AUR one — the name
    /// to ask the RPC about, which is the pkgbase and not always the package.
    aur: Option<String>,
    probe: Probe,
    // gradings join here (task #13): the verdict column reads from the
    // grading cache, keyed by (grader, package, candidate commit).
}

/// One tracked-but-not-vendored AUR package that has moved.
struct Upstream {
    package: String,
    installed: String,
    available: String,
}

/// The upstream half's whole outcome.
struct UpstreamHalf {
    rows: Vec<Upstream>,
    /// Did this half learn anything? `None` when there was nothing to ask
    /// about — which is neither evidence of calm nor of failure, and the
    /// distinction is what keeps a fully-vendored fleet from exiting 1.
    answered: Option<bool>,
    error: Option<String>,
    /// Tracked on some host but not installed here, so there is no local
    /// version to compare. Counted rather than guessed at.
    unchecked: usize,
}

pub fn run(ctx: &Ctx, fmt: Format) -> Result<(), String> {
    let sources = ctx.load_sources()?;

    let mut rows: Vec<Row> = sources
        .packages
        .iter()
        .map(|(package, entry)| probe(package, entry, fmt))
        .collect();

    // Versions are a display nicety on top of the commit hashes, so they are
    // fetched only for the rows that will print, and only for the ones the
    // AUR can be asked about at all.
    let wanted: Vec<String> = rows
        .iter()
        .filter(|r| matches!(r.probe, Probe::Pending(_)))
        .filter_map(|r| r.aur.clone())
        .collect();
    let (versions, version_error) = fetch_versions(&wanted, fmt);

    let upstream = upstream_section(ctx, &sources.packages, fmt);

    // The ledger is a BTreeMap, so this is already the order — stated anyway,
    // because a stable table is a property of the output, not an accident of
    // which container the ledger happens to use.
    rows.sort_by(|a, b| a.package.cmp(&b.package));
    let (pending, unreachable): (Vec<&Row>, Vec<&Row>) = rows
        .iter()
        .filter(|r| r.probe != Probe::Current)
        .partition(|r| matches!(r.probe, Probe::Pending(_)));

    let current = rows.len() - pending.len() - unreachable.len();

    match fmt {
        Format::Text => report_text(&pending, &unreachable, &versions, current, &upstream),
        Format::Json => report_json(&pending, &unreachable, &versions, &upstream.rows)?,
    }

    if let Some(e) = &version_error {
        fmt.trace(&format!(
            "warning: the AUR RPC could not be asked for versions ({e}) — \
             the pending rows show commits only"
        ));
    }
    if let Some(e) = &upstream.error {
        fmt.trace(&format!(
            "warning: tracked-but-not-vendored AUR packages could not be checked ({e})"
        ));
    }

    // Everything-failed. One half down is degradation and the table above
    // says so; both halves down means the run learned nothing, and exiting 0
    // there would tell a timer all is well on no evidence at all.
    let ledger_answered = (!rows.is_empty()).then_some(rows.len() > unreachable.len());
    let halves = [ledger_answered, upstream.answered];
    if halves.contains(&Some(false)) && !halves.contains(&Some(true)) {
        return Err("could not reach any upstream — nothing was checked".into());
    }

    if pending.is_empty() && upstream.rows.is_empty() {
        return Ok(());
    }
    std::process::exit(crate::HELD)
}

/// `git ls-remote -- <upstream> HEAD` for one ledger entry.
fn probe(package: &str, entry: &SourceEntry, fmt: Format) -> Row {
    let probe = match ls_remote(&entry.upstream, fmt) {
        Err(e) => Probe::Unreachable(e),
        Ok(candidate) if drifted(&entry.reviewed, &candidate) => Probe::Pending(candidate),
        Ok(_) => Probe::Current,
    };
    Row {
        package: package.to_string(),
        reviewed: entry.reviewed.clone(),
        role: entry.role,
        aur: aur_repo(&entry.upstream).map(str::to_string),
        probe,
    }
}

/// Ask a remote for its HEAD, argv first.
///
/// `--` guards the URL the way `vendor` guards its clone: an upstream that
/// begins with a hyphen is a git option otherwise, and the ledger is a file
/// that syncs between hosts.
fn ls_remote(upstream: &str, fmt: Format) -> Result<String, String> {
    fmt.trace(&format!(
        "run       git ls-remote -- {} HEAD",
        shell_quote(upstream)
    ));
    let out = Command::new("git")
        .args(["ls-remote", "--", upstream, "HEAD"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        let raw = String::from_utf8_lossy(&out.stderr);
        let reason = git_error(&raw);
        return Err(if reason.is_empty() {
            format!("git ls-remote exited {}", out.status)
        } else {
            reason
        });
    }
    parse_ls_remote(&String::from_utf8_lossy(&out.stdout))
        .map(str::to_string)
        .ok_or_else(|| "upstream has no HEAD (an empty repository?)".to_string())
}

/// Boil git's stderr down to one line fit for a table cell.
///
/// Three things happen here, each for its own reason. The text is a *remote's*
/// text, so it goes through the same neutering a PKGBUILD does — a server that
/// can name itself can otherwise paint the terminal. It collapses to one line,
/// so it cannot forge extra rows. And git's advice prose ("Please make sure you
/// have the correct access rights…") is dropped in favour of its `fatal:` /
/// `error:` lines, which are the part that says what actually went wrong.
fn git_error(stderr: &str) -> String {
    /// Long enough for a URL and a reason; short enough not to reflow a table.
    const CAP: usize = 120;

    let (safe, _) = visible(stderr.trim());
    let lines: Vec<&str> = safe
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let diagnostic: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.starts_with("fatal:") || l.starts_with("error:"))
        .collect();
    let kept = if diagnostic.is_empty() {
        lines
    } else {
        diagnostic
    };
    truncate(&kept.join("; "), CAP)
}

/// The commit in `git ls-remote`'s `<sha>\tHEAD` output.
///
/// Anything that is not a plausible object id is skipped rather than trusted:
/// git puts warnings and redirect notices on stdout in some configurations,
/// and a hash is the one thing this function is allowed to return.
fn parse_ls_remote(text: &str) -> Option<&str> {
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .find(|field| field.len() >= 7 && field.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Has upstream moved past what was reviewed?
///
/// Prefix, not equality: a ledger written by hand carries short hashes
/// (`3f9c21ab`) while `ls-remote` always answers with forty characters, and a
/// plain `!=` would report every such package as drifted forever.
///
/// The prefix only runs one way, and only for a `reviewed` long enough to name
/// a commit. Everything else — a value too short to identify anything, a
/// value *longer* than the hash it is supposed to abbreviate, an empty one —
/// is a ledger pacrat cannot read, and an unreadable ledger is reported as
/// drift. This verb's whole job is to notice; the failure it must not have is
/// calling something current on a comparison that did not really happen.
fn drifted(reviewed: &str, candidate: &str) -> bool {
    /// git's own floor for an abbreviated object id.
    const MIN_ABBREV: usize = 7;

    let reviewed = reviewed.trim().to_ascii_lowercase();
    let candidate = candidate.trim().to_ascii_lowercase();
    if reviewed.len() < MIN_ABBREV {
        return true;
    }
    !candidate.starts_with(&reviewed)
}

/// The reviewed commit as a table cell.
///
/// A value that cannot be an object id is named rather than abbreviated.
/// `short_hash` would clip a doubled forty-character hash down to eight that
/// match the candidate's exactly, and the row would read as pacrat crying
/// wolf over two identical-looking commits.
fn reviewed_cell(reviewed: &str) -> String {
    let reviewed = reviewed.trim();
    let plausible = (7..=40).contains(&reviewed.chars().count())
        && reviewed.chars().all(|c| c.is_ascii_hexdigit());
    if plausible {
        short_hash(reviewed).to_string()
    } else {
        "malformed".into()
    }
}

/// The `reviewed → candidate` cell, composed so it pads as one unit.
fn drift_cell(reviewed: &str, candidate: &str) -> String {
    format!("{} → {candidate}", reviewed_cell(reviewed))
}

/// The AUR repository a git URL points at, or None for any other forge.
///
/// pacrat has no forge-specific behavior (sources.rs) — this is not an
/// exception, it is how the verb knows which upstreams the AUR RPC can price.
/// A GitHub-hosted PKGBUILD is perfectly valid and simply shows its hashes.
fn aur_repo(upstream: &str) -> Option<&str> {
    // https://aur.archlinux.org/x.git · ssh://aur@aur.archlinux.org/x.git ·
    // aur@aur.archlinux.org:x.git — the three forms the AUR hands out.
    let rest = upstream
        .split_once("://")
        .map_or(upstream, |(_, rest)| rest)
        .trim_start_matches("aur@");
    let rest = rest.strip_prefix("aur.archlinux.org")?;
    let name = rest.trim_start_matches([':', '/']);
    let name = name.strip_suffix(".git").unwrap_or(name);
    let name = name.trim_end_matches('/');
    (!name.is_empty() && !name.contains('/')).then_some(name)
}

/// Human-readable versions for the pending rows, by AUR repository name.
/// Failure degrades the version column; it is never the run's failure.
fn fetch_versions(wanted: &[String], fmt: Format) -> (BTreeMap<String, String>, Option<String>) {
    if wanted.is_empty() {
        return (BTreeMap::new(), None);
    }
    for url in aur::info_urls(wanted) {
        fmt.trace(&format!("run       {}", aur::argv(&url)));
    }
    match aur::info_many(wanted) {
        Ok(hits) => (by_name(hits), None),
        Err(e) => (BTreeMap::new(), Some(e)),
    }
}

fn by_name(hits: Vec<AurPkg>) -> BTreeMap<String, String> {
    hits.into_iter().map(|p| (p.name, p.version)).collect()
}

/// The tracked-but-not-vendored half: AUR packages some host's manifest lists
/// that the ledger has never taken custody of, and that the AUR has since
/// moved past what is installed here.
fn upstream_section(
    ctx: &Ctx,
    ledger: &BTreeMap<String, SourceEntry>,
    fmt: Format,
) -> UpstreamHalf {
    let nothing = |answered, error| UpstreamHalf {
        rows: Vec::new(),
        answered,
        error,
        unchecked: 0,
    };

    let mut tracked: BTreeSet<String> = BTreeSet::new();
    for host in ctx.tracked_hosts() {
        tracked.extend(ctx.tracked(&host, Source::Aur));
    }
    let candidates = upstream_candidates(&tracked, ledger);
    if candidates.is_empty() {
        return nothing(None, None);
    }

    for url in aur::info_urls(&candidates) {
        fmt.trace(&format!("run       {}", aur::argv(&url)));
    }
    let available = match aur::info_many(&candidates) {
        Ok(hits) => by_name(hits),
        Err(e) => return nothing(Some(false), Some(e)),
    };
    // Traced here rather than with the URLs above: a call announced and then
    // skipped because the RPC died first would be a printed argv that never
    // ran, which is the opposite of what the visibility rule is for.
    fmt.trace(&format!("run       {}", pacman::query_versions_argv()));
    let installed = match pacman::installed_versions() {
        Ok(map) => map,
        Err(e) => return nothing(Some(false), Some(e)),
    };

    let mut half = UpstreamHalf {
        rows: Vec::new(),
        answered: Some(true),
        error: None,
        unchecked: 0,
    };
    for package in candidates {
        // No local install means no local version, and that is another host's
        // drift to answer for — inventing a number here would be worse than
        // counting the gap.
        let Some(have) = installed.get(&package) else {
            half.unchecked += 1;
            continue;
        };
        let Some(theirs) = available.get(&package) else {
            continue;
        };
        if is_newer(have, theirs) {
            half.rows.push(Upstream {
                package,
                installed: have.clone(),
                available: theirs.clone(),
            });
        }
    }
    half
}

/// Tracked AUR names the ledger does not carry, sorted.
fn upstream_candidates(
    tracked: &BTreeSet<String>,
    ledger: &BTreeMap<String, SourceEntry>,
) -> Vec<String> {
    tracked
        .iter()
        .filter(|p| !ledger.contains_key(*p))
        .cloned()
        .collect()
}

fn report_text(
    pending: &[&Row],
    unreachable: &[&Row],
    versions: &BTreeMap<String, String>,
    current: usize,
    upstream: &UpstreamHalf,
) {
    println!();
    if pending.is_empty() && unreachable.is_empty() && upstream.rows.is_empty() {
        println!(
            "nothing pending — {current} curated {}",
            if current == 1 {
                "package at its reviewed commit".to_string()
            } else {
                "packages at their reviewed commits".to_string()
            }
        );
        if upstream.unchecked > 0 {
            println!("{}", unchecked_note(upstream.unchecked));
        }
        return;
    }

    if !pending.is_empty() || !unreachable.is_empty() {
        let name_w = column(
            pending
                .iter()
                .chain(unreachable)
                .map(|r| r.package.as_str()),
            "package",
        );
        // gradings join here (task #13): a `grade` column goes between role
        // and version, and the header grows with it.
        println!(
            "{:<name_w$}  {:<DRIFT_W$}  {:<ROLE_W$}  version",
            "package", "reviewed → candidate", "role"
        );
        for row in pending {
            let Probe::Pending(candidate) = &row.probe else {
                continue;
            };
            let version = row
                .aur
                .as_ref()
                .and_then(|name| versions.get(name))
                .map_or("—", String::as_str);
            println!(
                "{:<name_w$}  {:<DRIFT_W$}  {:<ROLE_W$}  {version}",
                truncate(&row.package, name_w),
                drift_cell(&row.reviewed, short_hash(candidate)),
                custody::label(Some(row.role.into())),
            );
        }
        for row in unreachable {
            let Probe::Unreachable(why) = &row.probe else {
                continue;
            };
            println!(
                "{:<name_w$}  {:<DRIFT_W$}  {:<ROLE_W$}  —",
                truncate(&row.package, name_w),
                drift_cell(&row.reviewed, "unreachable"),
                custody::label(Some(row.role.into())),
            );
            println!("{:<name_w$}  {why}", "");
        }
        println!();
        println!(
            "{} pending · {current} current{}",
            pending.len(),
            if unreachable.is_empty() {
                String::new()
            } else {
                format!(" · {} unreachable", unreachable.len())
            }
        );
    }

    if !upstream.rows.is_empty() {
        println!();
        println!("upstream · tracked, not yet vendored");
        let name_w = column(upstream.rows.iter().map(|u| u.package.as_str()), "package");
        // "installed → aur" is a single cell, so its width is the widest
        // pair plus the arrow — the versions are unbounded in principle and
        // clipped at a width that fits an epoch and a long git-describe.
        let ver_w = upstream
            .rows
            .iter()
            .map(|u| u.installed.chars().count() + u.available.chars().count() + 3)
            .chain(std::iter::once("installed → aur".chars().count()))
            .max()
            .unwrap_or(16)
            .min(48);
        println!(
            "{:<name_w$}  {:<ver_w$}  bring it through curation",
            "package", "installed → aur"
        );
        for u in upstream.rows.iter().take(UPSTREAM_CAP) {
            println!(
                "{:<name_w$}  {:<ver_w$}  pacrat vendor {}",
                truncate(&u.package, name_w),
                truncate(&format!("{} → {}", u.installed, u.available), ver_w),
                u.package,
            );
        }
        if upstream.rows.len() > UPSTREAM_CAP {
            println!("… and {} more", upstream.rows.len() - UPSTREAM_CAP);
        }
    }
    if upstream.unchecked > 0 {
        println!("{}", unchecked_note(upstream.unchecked));
    }
}

fn unchecked_note(unchecked: usize) -> String {
    format!(
        "note   {unchecked} tracked AUR package{} not installed here — no local \
         version to compare",
        plural(unchecked)
    )
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// A name column wide enough for its contents, within reason.
fn column<'a>(values: impl Iterator<Item = &'a str>, header: &str) -> usize {
    values
        .map(|v| v.chars().count())
        .chain(std::iter::once(header.len()))
        .max()
        .unwrap_or(header.len())
        .clamp(header.len(), NAME_CAP)
}

#[derive(Serialize)]
struct Report<'a> {
    pending: Vec<PendingJson<'a>>,
    upstream: Vec<UpstreamJson<'a>>,
    /// Not in the brief, but the difference between "no update" and "could
    /// not ask" is the whole reason this verb degrades instead of failing,
    /// and a machine reader needs it as much as a human does.
    unreachable: Vec<UnreachableJson<'a>>,
}

#[derive(Serialize)]
struct PendingJson<'a> {
    package: &'a str,
    reviewed: &'a str,
    candidate: &'a str,
    role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
}

#[derive(Serialize)]
struct UpstreamJson<'a> {
    package: &'a str,
    installed: &'a str,
    available: &'a str,
}

#[derive(Serialize)]
struct UnreachableJson<'a> {
    package: &'a str,
    reviewed: &'a str,
    error: &'a str,
}

fn report_json(
    pending: &[&Row],
    unreachable: &[&Row],
    versions: &BTreeMap<String, String>,
    upstream: &[Upstream],
) -> Result<(), String> {
    let report = Report {
        pending: pending
            .iter()
            .filter_map(|row| {
                let Probe::Pending(candidate) = &row.probe else {
                    return None;
                };
                Some(PendingJson {
                    package: &row.package,
                    reviewed: &row.reviewed,
                    candidate,
                    role: row.role,
                    version: row
                        .aur
                        .as_ref()
                        .and_then(|name| versions.get(name))
                        .map(String::as_str),
                })
            })
            .collect(),
        upstream: upstream
            .iter()
            .map(|u| UpstreamJson {
                package: &u.package,
                installed: &u.installed,
                available: &u.available,
            })
            .collect(),
        unreachable: unreachable
            .iter()
            .filter_map(|row| {
                let Probe::Unreachable(error) = &row.probe else {
                    return None;
                };
                Some(UnreachableJson {
                    package: &row.package,
                    reviewed: &row.reviewed,
                    error,
                })
            })
            .collect(),
    };
    let text = serde_json::to_string_pretty(&report).map_err(|e| format!("json: {e}"))?;
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_remote_output_yields_the_head_commit() {
        // Captured from `git ls-remote -- https://aur.archlinux.org/mdcat.git HEAD`.
        assert_eq!(
            parse_ls_remote("5a4705a4aaa2e7f10a7dd6c302256dd373516e56\tHEAD\n"),
            Some("5a4705a4aaa2e7f10a7dd6c302256dd373516e56")
        );
        // Some remotes answer HEAD with the branch it points at as well.
        assert_eq!(
            parse_ls_remote(
                "ref: refs/heads/master\tHEAD\n\
                 5a4705a4aaa2e7f10a7dd6c302256dd373516e56\tHEAD\n"
            ),
            Some("5a4705a4aaa2e7f10a7dd6c302256dd373516e56")
        );
    }

    #[test]
    fn ls_remote_output_that_is_not_a_commit_is_not_one() {
        // An empty repository: the command succeeds and says nothing.
        assert_eq!(parse_ls_remote(""), None);
        assert_eq!(parse_ls_remote("\n\n"), None);
        assert_eq!(
            parse_ls_remote("warning: redirecting to https://x/\n"),
            None
        );
        // Too short to be an object id, and not hex.
        assert_eq!(parse_ls_remote("abc123\tHEAD\n"), None);
        assert_eq!(parse_ls_remote("zzzzzzzzzz\tHEAD\n"), None);
    }

    #[test]
    fn git_stderr_becomes_one_diagnostic_line() {
        // Captured from `git ls-remote -- file:///nonexistent/x.git HEAD`.
        let stderr = "fatal: '/nonexistent/x.git' does not appear to be a git repository\n\
                      fatal: Could not read from remote repository.\n\
                      \n\
                      Please make sure you have the correct access rights\n\
                      and the repository exists.\n";
        assert_eq!(
            git_error(stderr),
            "fatal: '/nonexistent/x.git' does not appear to be a git repository; \
             fatal: Could not read from remote repository."
        );
    }

    #[test]
    fn git_stderr_that_hides_or_sprawls_cannot_reshape_the_table() {
        // A remote naming itself in escape codes does not get to paint, and
        // one that answers in newlines does not get to forge rows.
        assert_eq!(git_error("fatal: \x1b[2Kx\n"), "fatal: ␛[2Kx");
        assert!(!git_error("fatal: a\nfatal: b\n").contains('\n'));
        // No fatal/error line: keep what there is rather than say nothing.
        assert_eq!(git_error("something odd\n"), "something odd");
        assert_eq!(git_error("   \n\n"), "");
        // A wall of text is clipped, and the clip is marked.
        let long = format!("fatal: {}\n", "x".repeat(500));
        assert_eq!(git_error(&long).chars().count(), 120);
        assert!(git_error(&long).ends_with('…'));
    }

    #[test]
    fn a_full_hash_matching_a_short_reviewed_one_is_not_drift() {
        let full = "5a4705a4aaa2e7f10a7dd6c302256dd373516e56";
        assert!(!drifted(full, full));
        assert!(!drifted("5a4705a4", full));
        assert!(!drifted("5A4705A4", full));
        assert!(!drifted(" 5a4705a4\n", full));
    }

    #[test]
    fn a_different_hash_is_drift_and_so_is_no_hash_at_all() {
        let full = "5a4705a4aaa2e7f10a7dd6c302256dd373516e56";
        assert!(drifted("0000000000000000000000000000000000000000", full));
        assert!(drifted("5a4705a5", full));
        // Nothing was ever reviewed, so everything upstream is unreviewed.
        assert!(drifted("", full));
        assert!(drifted("  ", full));
    }

    #[test]
    fn a_reviewed_value_that_cannot_name_a_commit_is_drift_not_calm() {
        let full = "5a4705a4aaa2e7f10a7dd6c302256dd373516e56";
        // Too short to identify anything: `5` prefixes a sixteenth of all
        // commits, and matching by luck would silently report "current".
        assert!(drifted("5", full));
        assert!(drifted("5a4705", full));
        // Seven is git's own floor, and is accepted.
        assert!(!drifted("5a4705a", full));
        // Longer than the hash it claims to abbreviate — a mangled ledger,
        // and the one shape a symmetric prefix test would call current.
        assert!(drifted(&full.repeat(2), full));
    }

    #[test]
    fn the_reviewed_cell_abbreviates_hashes_and_names_everything_else() {
        let full = "5a4705a4aaa2e7f10a7dd6c302256dd373516e56";
        assert_eq!(reviewed_cell(full), "5a4705a4");
        assert_eq!(reviewed_cell("5a4705a4"), "5a4705a4");
        assert_eq!(reviewed_cell(" 5a4705a4\n"), "5a4705a4");
        // The doubled hash: eight characters of it look exactly like the
        // candidate's, so abbreviating would make the row read as a lie.
        assert_eq!(reviewed_cell(&full.repeat(2)), "malformed");
        assert_eq!(reviewed_cell(""), "malformed");
        assert_eq!(reviewed_cell("v2.10.1"), "malformed");
    }

    #[test]
    fn the_drift_cell_never_outgrows_its_column() {
        let full = "5a4705a4aaa2e7f10a7dd6c302256dd373516e56";
        assert_eq!(drift_cell(full, "cbf58f74"), "5a4705a4 → cbf58f74");
        // The widest the cell can get, and what DRIFT_W is sized for.
        assert_eq!(
            drift_cell(&full.repeat(2), "unreachable"),
            "malformed → unreachable"
        );
        assert_eq!(DRIFT_W, "malformed → unreachable".chars().count());
        assert_eq!(ROLE_W, custody::label(Some(Role::Maintained.into())).len());
        // The header has to fit the column it labels.
        assert!("reviewed → candidate".chars().count() <= DRIFT_W);
    }

    #[test]
    fn aur_upstreams_are_recognised_in_every_form_the_aur_hands_out() {
        assert_eq!(
            aur_repo("https://aur.archlinux.org/mdcat.git"),
            Some("mdcat")
        );
        assert_eq!(aur_repo("https://aur.archlinux.org/mdcat"), Some("mdcat"));
        assert_eq!(
            aur_repo("ssh://aur@aur.archlinux.org/playtimed.git"),
            Some("playtimed")
        );
        assert_eq!(
            aur_repo("aur@aur.archlinux.org:playtimed.git"),
            Some("playtimed")
        );
    }

    #[test]
    fn other_forges_have_no_aur_name() {
        assert_eq!(aur_repo("https://github.com/swsnr/mdcat.git"), None);
        assert_eq!(aur_repo("file:///srv/pkgbuilds/mdcat"), None);
        assert_eq!(aur_repo("/srv/pkgbuilds/mdcat"), None);
        assert_eq!(aur_repo("https://aur.archlinux.org/"), None);
        // A lookalike host is not the AUR.
        assert_eq!(aur_repo("https://aur.archlinux.org.evil.test/x.git"), None);
        // Nested paths are not AUR repositories.
        assert_eq!(aur_repo("https://aur.archlinux.org/a/b.git"), None);
    }

    fn ledger(names: &[&str]) -> BTreeMap<String, SourceEntry> {
        names
            .iter()
            .map(|n| {
                (
                    (*n).to_string(),
                    SourceEntry {
                        upstream: format!("https://aur.archlinux.org/{n}.git"),
                        reviewed: "3f9c21ab".into(),
                        role: Role::Vendored,
                        note: None,
                    },
                )
            })
            .collect()
    }

    fn tracked(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn upstream_is_tracked_minus_the_ledger() {
        let candidates = upstream_candidates(
            &tracked(&["marktext-bin", "mdcat", "yay"]),
            &ledger(&["mdcat"]),
        );
        assert_eq!(candidates, ["marktext-bin", "yay"]);
    }

    #[test]
    fn a_fully_vendored_fleet_has_no_upstream_section() {
        assert!(upstream_candidates(&tracked(&["mdcat"]), &ledger(&["mdcat"])).is_empty());
        assert!(upstream_candidates(&tracked(&[]), &ledger(&["mdcat"])).is_empty());
        // A ledger entry no host tracks is not an upstream row either — it is
        // curated, which is the whole point.
        assert!(upstream_candidates(&tracked(&[]), &ledger(&["mdcat", "yay"])).is_empty());
    }

    #[test]
    fn the_name_column_fits_its_contents_within_a_ceiling() {
        assert_eq!(column(["a", "bb"].into_iter(), "package"), "package".len());
        assert_eq!(column(["marktext-bin"].into_iter(), "package"), 12);
        let long = "a".repeat(80);
        assert_eq!(column([long.as_str()].into_iter(), "package"), NAME_CAP);
        assert_eq!(column([].into_iter(), "package"), "package".len());
    }
}
