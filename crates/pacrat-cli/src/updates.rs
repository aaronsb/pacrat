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
//! Both halves reach the network, and neither is allowed to take the run down
//! — the rule `search` follows for its two worlds. A remote that will not
//! answer costs its own row and no other; a dead AUR RPC costs the version
//! column and the upstream half, while the ledger's commit-level answer
//! survives intact, because that one comes from git.
//!
//! What a failure *does* cost is the right to say "nothing pending". Degraded
//! output still prints; it just cannot exit 0. See the lattice below.
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
//!
//! The lattice, in order — the first rule that applies wins:
//!
//! | Condition                                        | Exit |
//! |--------------------------------------------------|------|
//! | anything pending, from either half                | 10   |
//! | a ledger row could not be probed                  | 1    |
//! | the upstream half was asked and could not answer  | 1    |
//! | otherwise                                         | 0    |
//!
//! Two things about that order are load-bearing, and both are about what a
//! *zero* claims. Exit 0 says "I checked, and there is nothing" — so a single
//! unreachable ledger row disqualifies it, even when every other row came
//! back current. A partial probe cannot support a total claim, and the two
//! halves answer different questions: the upstream half finding nothing says
//! nothing whatsoever about the ledger rows that went unasked.
//!
//! Pending outranks broken because either way there is real work, and 10 is
//! the code that gets a human looking. A run that is both partly broken and
//! has something pending exits 10; the unreachable rows are in the table
//! above it, where the human who came for the 10 will see them.
//!
//! ## Refusals
//!
//! A candidate a human has rejected (`pacrat reject`) is drift with an
//! answer already on it. It is listed, in its own region, and it is **not
//! pending**: it neither exits 10 nor counts toward the pending total. A
//! timer that re-raised a decided question every hour would teach its reader
//! to ignore the one row that mattered. The refusal names a commit, so
//! upstream moving past it is a new candidate, unrejected, and pending
//! again — see `sources::SourceEntry::rejected`.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::time::Duration;

use clap::ValueEnum;
use pacrat_core::pkg::Source;
use pacrat_core::sources::{Role, SourceEntry};
use pacrat_core::version::is_newer;
use serde::Serialize;

use crate::aur::{self, AurPkg};
use crate::ctx::Ctx;
use crate::custody;
use crate::out::{list_preview, shell_quote, short_hash, truncate, visible, visible_line};
use crate::pacman;
use crate::proc;
use crate::say;

/// How many upstream rows the CLI prints before counting the rest. The
/// pending list is the actionable one and is never capped; the upstream
/// region is awareness, and a host tracking three hundred AUR packages
/// should not bury it (`out::list_preview`'s rule, one row per line).
const UPSTREAM_CAP: usize = 20;

/// Wall-clock bound on one `git ls-remote`. Sized against the AUR RPC's
/// `--max-time 10` — the same order of patience for the same kind of wait —
/// with room for a slow forge's TLS handshake on top.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);

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

/// What `git ls-remote` had to say about one ledger entry.
#[derive(Debug, PartialEq, Eq)]
enum Probe {
    /// Upstream HEAD is the reviewed commit; nothing to do.
    Current,
    /// Upstream has moved: the candidate commit awaiting review.
    Pending(String),
    /// Upstream has moved to a commit a human already refused. Drift with an
    /// answer on it — reported, never counted as pending.
    Rejected(String),
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
    /// Why the human refused, when they said. Only ever shown on a
    /// `Rejected` row.
    rejected_note: Option<String>,
    // gradings join here (task #13): the verdict column reads from the
    // grading cache, keyed by (grader, package, candidate commit).
}

/// The ledger half's rows, sorted into what the reader is being told.
struct Ledger<'a> {
    pending: &'a [&'a Row],
    rejected: &'a [&'a Row],
    unreachable: &'a [&'a Row],
    /// Rows found at their reviewed commit.
    current: usize,
}

impl Ledger<'_> {
    /// Is there anything at all to show from this half?
    fn quiet(&self) -> bool {
        self.pending.is_empty() && self.rejected.is_empty() && self.unreachable.is_empty()
    }
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
    /// version to compare. Named rather than guessed at.
    unchecked: Vec<String>,
}

pub fn run(ctx: &Ctx, fmt: Format) -> Result<(), String> {
    // In JSON mode stdout belongs to the object alone, so every human line
    // this run produces goes to stderr instead. One switch, set before
    // anything prints, for the same reason `update` sets it: the modules
    // underneath do not know a JSON caller exists, and should not have to.
    // ADR-001's "external calls are always visible" has no quiet mode —
    // `--format json` changes where a line lands, never whether it exists.
    crate::out::chatter_to_stderr(fmt == Format::Json);
    let sources = ctx.load_sources()?;

    let mut rows: Vec<Row> = sources
        .packages
        .iter()
        .map(|(package, entry)| probe(package, entry))
        .collect();
    // The ledger is a BTreeMap, so this is already the order — stated anyway,
    // because a stable table is a property of the output, not an accident of
    // which container the ledger happens to use.
    rows.sort_by(|a, b| a.package.cmp(&b.package));

    // Both halves want the same thing from the AUR — a version for a name —
    // so they ask together. Splitting it in two would double the round trips
    // and give one outage two different-sounding warnings.
    let candidates = upstream_candidates(&tracked_aur(ctx)?, &sources.packages);
    let (versions, rpc_error) = fetch_versions(&rpc_names(&rows, &candidates));
    if let Some(e) = &rpc_error {
        say!(
            "warning: the AUR RPC could not be asked ({e}) — pending rows show \
             commits without versions, and tracked-but-not-vendored packages \
             went unexamined"
        );
    }
    let upstream = upstream_half(&candidates, &versions, rpc_error.as_deref());
    if let Some(e) = &upstream.error {
        say!("warning: tracked-but-not-vendored AUR packages went unexamined ({e})");
    }

    let of = |want: fn(&Probe) -> bool| -> Vec<&Row> {
        rows.iter().filter(|r| want(&r.probe)).collect()
    };
    let pending = of(|p| matches!(p, Probe::Pending(_)));
    let rejected = of(|p| matches!(p, Probe::Rejected(_)));
    let unreachable = of(|p| matches!(p, Probe::Unreachable(_)));

    let current = rows.len() - pending.len() - rejected.len() - unreachable.len();

    // Decided before the report is written, not after: the headline "nothing
    // pending" is exactly the claim a broken run has not earned, and it
    // should not be printed and then contradicted by the exit code.
    let verdict = verdict(Outcome {
        pending: pending.len(),
        upstream: upstream.rows.len(),
        unreachable: unreachable.len(),
        upstream_failed: upstream.answered == Some(false),
    });
    let complete = !matches!(verdict, Verdict::Broken(_));

    let report = Ledger {
        pending: &pending,
        rejected: &rejected,
        unreachable: &unreachable,
        current,
    };
    match fmt {
        Format::Text => report_text(&report, &versions, &upstream, complete),
        Format::Json => report_json(&report, &versions, &upstream)?,
    }

    match verdict {
        Verdict::Clean => Ok(()),
        Verdict::Pending => std::process::exit(crate::HELD),
        Verdict::Broken(why) => Err(why),
    }
}

/// What one run learned, stripped of everything the exit code does not turn on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Outcome {
    pending: usize,
    upstream: usize,
    /// Ledger rows whose remote could not be probed at all.
    unreachable: usize,
    /// The upstream half had something to ask about and could not ask it.
    upstream_failed: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Clean,
    Pending,
    Broken(String),
}

/// Apply the lattice documented in the module header.
///
/// Pure, so that "exit 0 means pacrat checked and found nothing" is a
/// property with a test behind it rather than a claim in a doc comment.
fn verdict(o: Outcome) -> Verdict {
    if o.pending > 0 || o.upstream > 0 {
        // Real work exists either way, and 10 is the code that fetches a
        // human. Any unreachable rows are in the table they will be reading.
        return Verdict::Pending;
    }
    if o.unreachable > 0 {
        return Verdict::Broken(format!(
            "{} ledger upstream{} could not be probed — with rows unasked, \
             \"nothing pending\" is not a claim this run can make",
            o.unreachable,
            plural(o.unreachable)
        ));
    }
    if o.upstream_failed {
        return Verdict::Broken(
            "the tracked-but-not-vendored check could not run — packages that \
             update outside curation went unexamined"
                .into(),
        );
    }
    Verdict::Clean
}

/// `git ls-remote -- <upstream> HEAD` for one ledger entry.
fn probe(package: &str, entry: &SourceEntry) -> Row {
    let probe = match ls_remote(&entry.upstream) {
        Err(e) => Probe::Unreachable(e),
        Ok(candidate) if !drifted(&entry.reviewed, &candidate) => Probe::Current,
        Ok(candidate) if refused(entry, &candidate) => Probe::Rejected(candidate),
        Ok(candidate) => Probe::Pending(candidate),
    };
    Row {
        package: package.to_string(),
        reviewed: entry.reviewed.clone(),
        role: entry.role,
        aur: aur_repo(&entry.upstream).map(str::to_string),
        probe,
        rejected_note: entry.rejected_note.clone(),
    }
}

/// Has a human already refused this exact candidate?
///
/// Expressed through [`drifted`] rather than a second comparison of its own:
/// `rejected` is a commit in the same hand-editable ledger `reviewed` lives
/// in, and two predicates that disagree about whether a short hash matches
/// would eventually hide a pending update behind a stale refusal. A
/// `rejected` value that names no commit is therefore drift — which is to
/// say the row stays pending, the safe direction.
///
/// Shared with `review`: the verb that records a refusal and the verb that
/// honours it have to mean the same thing by it.
pub fn refused(entry: &SourceEntry, candidate: &str) -> bool {
    entry
        .rejected
        .as_deref()
        .is_some_and(|commit| !drifted(commit, candidate))
}

/// Ask a remote for its HEAD, argv first.
///
/// Three things bound what a hostile or merely broken upstream can do here,
/// because the ledger is a synced file and this verb runs from a timer:
///
/// * `--` guards the URL the way `vendor` guards its clone — an upstream that
///   begins with a hyphen is a git option otherwise.
/// * The env closes the interactive doors. A private-repo upstream would
///   otherwise ask for credentials in the middle of the table, and a timer
///   unit has no one to answer. (ssh's own passphrase and host-key prompts
///   are not reachable through these; `GIT_SSH_COMMAND` is deliberately left
///   alone rather than clobbering a user's ssh setup for their maintained
///   repos, so the timeout is what covers that case.)
/// * The timeout makes "never answers" a reportable outcome. A TCP connect to
///   a host that drops packets does not return on its own, and one such
///   upstream would otherwise hang the whole run.
///
/// The argv line goes wherever this run's chatter goes (`crate::out`).
/// Shared with `review` and `update`, whose verbs ask the same question of
/// the same remotes and must ask it the same guarded way.
pub fn ls_remote(upstream: &str) -> Result<String, String> {
    // Quoted so the line can be pasted, and neutered so it cannot paint:
    // the URL comes out of a ledger every host in the fleet can write to.
    say!(
        "run       git ls-remote -- {} HEAD",
        visible_line(&shell_quote(upstream)).0
    );
    let mut cmd = Command::new("git");
    cmd.args(["ls-remote", "--", upstream, "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/true");

    let ran = proc::run_with_timeout(cmd, REMOTE_TIMEOUT).map_err(|e| format!("git: {e}"))?;
    if ran.timed_out {
        return Err(format!("timed out after {}s", REMOTE_TIMEOUT.as_secs()));
    }
    if !ran.status.success() {
        let reason = git_error(&String::from_utf8_lossy(&ran.stderr));
        return Err(if reason.is_empty() {
            format!("git ls-remote exited {}", ran.status)
        } else {
            reason
        });
    }
    parse_ls_remote(&String::from_utf8_lossy(&ran.stdout))
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

/// Could this string name a commit?
///
/// One predicate, because two would eventually disagree: `drifted` deciding a
/// value is comparable while `reviewed_cell` calls the same value malformed
/// would put "malformed → 5a4705a4  current" on a row, which is nonsense the
/// reader has no way to resolve. Between git's own floor for an abbreviation
/// and the length of a full object id, hex only.
fn names_a_commit(s: &str) -> bool {
    /// git's own floor for an abbreviated object id.
    const MIN_ABBREV: usize = 7;
    /// A full SHA-1. Longer is not a hash that was abbreviated; it is a
    /// mangled field.
    const FULL: usize = 40;

    let n = s.chars().count();
    (MIN_ABBREV..=FULL).contains(&n) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// The commit `git ls-remote <url> HEAD` reports for HEAD.
///
/// The ref is matched by name rather than the hash being picked out by shape.
/// git writes warnings and redirect notices to this stream in some
/// configurations, and "the first thing that looks like a hash" would be
/// right only for as long as those happen to sort after the answer. Asking
/// for the row labelled HEAD is right by construction.
fn parse_ls_remote(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let object = fields.next()?;
        // `ref: refs/heads/master  HEAD` — a symref line names HEAD too, but
        // its first field is a path, not an object.
        (fields.next() == Some("HEAD") && names_a_commit(object)).then_some(object)
    })
}

/// Has upstream moved past what was reviewed?
///
/// Prefix, not equality: a ledger written by hand carries short hashes
/// (`3f9c21ab`) while `ls-remote` always answers with forty characters, and a
/// plain `!=` would report every such package as drifted forever.
///
/// The prefix only runs one way, and only for a `reviewed` that could name a
/// commit at all. Everything else — too short to identify anything, longer
/// than the hash it is supposed to abbreviate, empty, not hex — is a ledger
/// pacrat cannot read, and an unreadable ledger is reported as drift. This
/// verb's whole job is to notice; the failure it must not have is calling
/// something current on a comparison that did not really happen.
pub fn drifted(reviewed: &str, candidate: &str) -> bool {
    let reviewed = reviewed.trim().to_ascii_lowercase();
    if !names_a_commit(&reviewed) {
        return true;
    }
    !candidate.trim().to_ascii_lowercase().starts_with(&reviewed)
}

/// The reviewed commit as a table cell.
///
/// A value that cannot be an object id is named rather than abbreviated.
/// `short_hash` would clip a doubled forty-character hash down to eight that
/// match the candidate's exactly, and the row would read as pacrat crying
/// wolf over two identical-looking commits.
fn reviewed_cell(reviewed: &str) -> String {
    let reviewed = reviewed.trim();
    if names_a_commit(reviewed) {
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
fn fetch_versions(wanted: &[String]) -> (BTreeMap<String, String>, Option<String>) {
    if wanted.is_empty() {
        return (BTreeMap::new(), None);
    }
    for url in aur::info_urls(wanted) {
        say!("run       {}", aur::argv(&url));
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
fn upstream_half(
    candidates: &[String],
    available: &BTreeMap<String, String>,
    rpc_error: Option<&str>,
) -> UpstreamHalf {
    let nothing = |answered, error: Option<&str>| UpstreamHalf {
        rows: Vec::new(),
        answered,
        error: error.map(str::to_string),
        unchecked: Vec::new(),
    };

    if candidates.is_empty() {
        return nothing(None, None);
    }
    if rpc_error.is_some() {
        // No `error` text: the caller shares this RPC with the version column
        // and has already said so once. Twice would read as two outages.
        return nothing(Some(false), None);
    }

    // Traced here rather than beside the RPC: a call announced and then
    // skipped because the RPC died first would be a printed argv that never
    // ran, which is the opposite of what the visibility rule is for.
    say!("run       {}", pacman::query_versions_argv());
    let installed = match pacman::installed_versions() {
        Ok(map) => map,
        Err(e) => return nothing(Some(false), Some(&e)),
    };

    let mut half = UpstreamHalf {
        rows: Vec::new(),
        answered: Some(true),
        error: None,
        unchecked: Vec::new(),
    };
    for package in candidates {
        // No local install means no local version, and that is another host's
        // drift to answer for — inventing a number here would be worse than
        // naming the gap.
        let Some(have) = installed.get(package) else {
            half.unchecked.push(package.clone());
            continue;
        };
        let Some(theirs) = available.get(package) else {
            continue;
        };
        if is_newer(have, theirs) {
            half.rows.push(Upstream {
                package: package.clone(),
                installed: have.clone(),
                available: theirs.clone(),
            });
        }
    }
    half
}

/// Every AUR package name any host tracks.
fn tracked_aur(ctx: &Ctx) -> Result<BTreeSet<String>, String> {
    let mut tracked = BTreeSet::new();
    for host in ctx.tracked_hosts() {
        tracked.extend(ctx.tracked(&host, Source::Aur)?);
    }
    Ok(tracked)
}

/// The names worth one RPC: the pkgbases behind pending ledger rows, plus
/// every tracked-but-not-vendored package. Deduped — the two sets are
/// disjoint by construction today, but a pkgbase that matches a tracked name
/// would otherwise be asked about twice.
fn rpc_names(rows: &[Row], candidates: &[String]) -> Vec<String> {
    let mut names: BTreeSet<String> = rows
        .iter()
        .filter(|r| matches!(r.probe, Probe::Pending(_)))
        .filter_map(|r| r.aur.clone())
        .collect();
    names.extend(candidates.iter().cloned());
    names.into_iter().collect()
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
    ledger: &Ledger,
    versions: &BTreeMap<String, String>,
    upstream: &UpstreamHalf,
    complete: bool,
) {
    say!();
    if ledger.quiet() && upstream.rows.is_empty() {
        say!(
            "{} — {} curated {}",
            // The unqualified headline is a claim about everything. A run
            // that could not ask every question has not earned it, and the
            // exit code is about to say so.
            if complete {
                "nothing pending"
            } else {
                "nothing pending in what could be checked"
            },
            ledger.current,
            if ledger.current == 1 {
                "package at its reviewed commit"
            } else {
                "packages at their reviewed commits"
            }
        );
    } else {
        if !ledger.quiet() {
            render_ledger(ledger, versions);
        }
        if !upstream.rows.is_empty() {
            render_upstream(&upstream.rows);
        }
    }
    if !upstream.unchecked.is_empty() {
        say!("{}", unchecked_note(&upstream.unchecked));
    }
}

/// The ledger half: what has moved, what was refused, and what could not be
/// asked.
fn render_ledger(ledger: &Ledger, versions: &BTreeMap<String, String>) {
    let name_w = column(
        ledger
            .pending
            .iter()
            .chain(ledger.unreachable)
            .map(|r| r.package.as_str()),
        "package",
    );
    let drift_table = !ledger.pending.is_empty() || !ledger.unreachable.is_empty();
    if drift_table {
        // gradings join here (task #13): a `grade` column goes between role
        // and version, and the header grows with it.
        say!(
            "{:<name_w$}  {:<DRIFT_W$}  {:<ROLE_W$}  version",
            "package",
            "reviewed → candidate",
            "role"
        );
    }
    for row in ledger.pending {
        let Probe::Pending(candidate) = &row.probe else {
            continue;
        };
        let version = row
            .aur
            .as_ref()
            .and_then(|name| versions.get(name))
            .map_or("—", String::as_str);
        say!(
            "{:<name_w$}  {:<DRIFT_W$}  {:<ROLE_W$}  {version}",
            truncate(&row.package, name_w),
            drift_cell(&row.reviewed, short_hash(candidate)),
            custody::label(Some(row.role.into())),
        );
    }
    for row in ledger.unreachable {
        let Probe::Unreachable(why) = &row.probe else {
            continue;
        };
        say!(
            "{:<name_w$}  {:<DRIFT_W$}  {:<ROLE_W$}  —",
            truncate(&row.package, name_w),
            drift_cell(&row.reviewed, "unreachable"),
            custody::label(Some(row.role.into())),
        );
        say!("{:<name_w$}  {why}", "");
    }
    if !ledger.rejected.is_empty() {
        if drift_table {
            say!();
        }
        render_rejected(ledger.rejected);
    }
    say!();
    say!("{}", counts(ledger));
}

/// Drift a human has already answered. Its own region, below the pending
/// table: these rows are information, not work, and mixing them into the
/// list of things to do is how a decided question gets asked again.
fn render_rejected(rows: &[&Row]) {
    say!("rejected · a human refused this candidate");
    let name_w = column(rows.iter().map(|r| r.package.as_str()), "package");
    say!(
        "{:<name_w$}  {:<DRIFT_W$}  why",
        "package",
        "reviewed → candidate"
    );
    for row in rows {
        let Probe::Rejected(candidate) = &row.probe else {
            continue;
        };
        // The note came out of a synced file another host wrote, so it is
        // neutered and folded like any other text pacrat did not author.
        let why = match &row.rejected_note {
            Some(note) => truncate(&visible_line(note).0, 60),
            None => "—".to_string(),
        };
        say!(
            "{:<name_w$}  {:<DRIFT_W$}  {why}",
            truncate(&row.package, name_w),
            drift_cell(&row.reviewed, short_hash(candidate)),
        );
    }
}

/// The tallies under the table. Every row of the ledger half is in exactly
/// one of them, which is the point: pending plus rejected plus unreachable
/// plus current is the ledger.
fn counts(ledger: &Ledger) -> String {
    let mut line = format!(
        "{} pending · {} current",
        ledger.pending.len(),
        ledger.current
    );
    for (n, word) in [
        (ledger.rejected.len(), "rejected"),
        (ledger.unreachable.len(), "unreachable"),
    ] {
        if n > 0 {
            line.push_str(&format!(" · {n} {word}"));
        }
    }
    line
}

/// The upstream half: packages that would update outside curation.
fn render_upstream(rows: &[Upstream]) {
    say!();
    say!("upstream · tracked, not yet vendored");
    let name_w = column(rows.iter().map(|u| u.package.as_str()), "package");
    // "installed → aur" is a single cell, so its width is the widest pair
    // plus the arrow — the versions are unbounded in principle and clipped at
    // a width that fits an epoch and a long git-describe.
    let ver_w = rows
        .iter()
        .map(|u| u.installed.chars().count() + u.available.chars().count() + 3)
        .chain(std::iter::once("installed → aur".chars().count()))
        .max()
        .unwrap_or(16)
        .min(48);
    say!(
        "{:<name_w$}  {:<ver_w$}  bring it through curation",
        "package",
        "installed → aur"
    );
    for u in rows.iter().take(UPSTREAM_CAP) {
        say!(
            "{:<name_w$}  {:<ver_w$}  pacrat vendor {}",
            truncate(&u.package, name_w),
            truncate(&format!("{} → {}", u.installed, u.available), ver_w),
            // The name comes out of a host's tracked list, which is a text
            // file a human edits — the suggestion is meant to be pasted, so
            // it is quoted like every other argv pacrat prints.
            shell_quote(&u.package),
        );
    }
    if rows.len() > UPSTREAM_CAP {
        say!("… and {} more", rows.len() - UPSTREAM_CAP);
    }
}

fn unchecked_note(unchecked: &[String]) -> String {
    format!(
        "note   {} tracked AUR package{} not installed here — no local version \
         to compare: {}",
        unchecked.len(),
        plural(unchecked.len()),
        list_preview(unchecked, 8)
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

/// The machine-readable answer.
///
/// Everything the exit code turns on is in here, because a consumer that has
/// to parse stderr to learn why it got a 1 does not have a machine format.
/// In particular `ledger_checked` plus `unreachable` reconstruct the one
/// distinction that matters most: an empty `pending` means "nothing is
/// pending" only when the ledger half actually answered for every row.
#[derive(Serialize)]
struct Report<'a> {
    pending: Vec<PendingJson<'a>>,
    /// Candidates a human refused. Reported rather than dropped: a machine
    /// reading this has to be able to tell "nothing moved" from "something
    /// moved and we said no", which are different states of the world.
    rejected: Vec<RejectedJson<'a>>,
    upstream: Vec<UpstreamJson<'a>>,
    /// Ledger rows whose remote could not be probed. The difference between
    /// "no update" and "could not ask" is the whole reason this verb degrades
    /// instead of failing.
    unreachable: Vec<UnreachableJson<'a>>,
    /// Tracked AUR packages with no local install to compare against.
    unchecked: &'a [String],
    /// Ledger rows found at their reviewed commit.
    current: usize,
    /// Every ledger row was probed successfully. False means `pending` is a
    /// floor, not a total.
    ledger_checked: bool,
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
struct RejectedJson<'a> {
    package: &'a str,
    reviewed: &'a str,
    candidate: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
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
    ledger: &Ledger,
    versions: &BTreeMap<String, String>,
    upstream: &UpstreamHalf,
) -> Result<(), String> {
    let report = Report {
        unchecked: &upstream.unchecked,
        current: ledger.current,
        ledger_checked: ledger.unreachable.is_empty(),
        rejected: ledger
            .rejected
            .iter()
            .filter_map(|row| {
                let Probe::Rejected(candidate) = &row.probe else {
                    return None;
                };
                Some(RejectedJson {
                    package: &row.package,
                    reviewed: &row.reviewed,
                    candidate,
                    note: row.rejected_note.as_deref(),
                })
            })
            .collect(),
        pending: ledger
            .pending
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
            .rows
            .iter()
            .map(|u| UpstreamJson {
                package: &u.package,
                installed: &u.installed,
                available: &u.available,
            })
            .collect(),
        unreachable: ledger
            .unreachable
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
    // The answer itself, on stdout, always: this is the object.
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
    fn only_the_row_labelled_head_is_read() {
        let head = "5a4705a4aaa2e7f10a7dd6c302256dd373516e56";
        let other = "1111111111111111111111111111111111111111";
        // A ref that is not HEAD is not the answer, wherever it sits — the
        // old shape-matching version returned whichever came first.
        assert_eq!(
            parse_ls_remote(&format!("{other}\trefs/heads/master\n{head}\tHEAD\n")),
            Some(head)
        );
        assert_eq!(
            parse_ls_remote(&format!("{other}\trefs/tags/v1\n{other}\trefs/heads/x\n")),
            None
        );
        // A ref whose name merely ends in HEAD is a different ref.
        assert_eq!(
            parse_ls_remote(&format!("{other}\trefs/remotes/origin/HEAD\n")),
            None
        );
        // A bare hash with no ref column names nothing.
        assert_eq!(parse_ls_remote(&format!("{head}\n")), None);
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

    fn entry(name: &str) -> SourceEntry {
        SourceEntry {
            upstream: format!("https://aur.archlinux.org/{name}.git"),
            reviewed: "3f9c21ab".into(),
            role: Role::Vendored,
            note: None,
            rejected: None,
            rejected_note: None,
        }
    }

    fn ledger(names: &[&str]) -> BTreeMap<String, SourceEntry> {
        names.iter().map(|n| ((*n).to_string(), entry(n))).collect()
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

    fn outcome(pending: usize, upstream: usize, unreachable: usize, failed: bool) -> Outcome {
        Outcome {
            pending,
            upstream,
            unreachable,
            upstream_failed: failed,
        }
    }

    fn code(o: Outcome) -> i32 {
        match verdict(o) {
            Verdict::Clean => 0,
            Verdict::Pending => crate::HELD,
            Verdict::Broken(_) => 1,
        }
    }

    #[test]
    fn exit_zero_requires_that_everything_was_actually_checked() {
        // The clean run: a ledger fully probed, nothing to report.
        assert_eq!(code(outcome(0, 0, 0, false)), 0);
        // A ledger row that could not be probed disqualifies "nothing
        // pending" — a partial probe cannot support a total claim, however
        // many other rows came back current.
        assert_eq!(code(outcome(0, 0, 1, false)), 1);
        // Nor can the upstream half rescue it. The halves answer different
        // questions, and one finding nothing says nothing about the other.
        assert_eq!(code(outcome(0, 0, 2, false)), 1);
        // The upstream half asked and could not answer: also not a zero.
        assert_eq!(code(outcome(0, 0, 0, true)), 1);
    }

    #[test]
    fn pending_outranks_broken_because_either_way_there_is_work() {
        assert_eq!(code(outcome(1, 0, 0, false)), crate::HELD);
        assert_eq!(code(outcome(0, 1, 0, false)), crate::HELD);
        // Partly broken and partly pending: 10 is the code that fetches a
        // human, and the unreachable rows are in the table they will read.
        assert_eq!(code(outcome(1, 0, 3, false)), crate::HELD);
        assert_eq!(code(outcome(0, 1, 3, true)), crate::HELD);
    }

    #[test]
    fn a_broken_run_says_which_half_broke() {
        let Verdict::Broken(ledger) = verdict(outcome(0, 0, 2, false)) else {
            panic!("unreachable rows must be a failure");
        };
        assert!(ledger.contains("2 ledger upstreams"), "{ledger}");
        let Verdict::Broken(up) = verdict(outcome(0, 0, 0, true)) else {
            panic!("an unanswerable upstream half must be a failure");
        };
        assert!(up.contains("outside curation"), "{up}");
        // With both halves down, the ledger's failure is the one named: it
        // owns the reviewed-commit claim, which is what a zero would assert.
        let Verdict::Broken(both) = verdict(outcome(0, 0, 1, true)) else {
            panic!("both halves down must be a failure");
        };
        assert_eq!(both, verdict_message(outcome(0, 0, 1, false)));
    }

    fn verdict_message(o: Outcome) -> String {
        match verdict(o) {
            Verdict::Broken(why) => why,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn one_predicate_decides_what_names_a_commit() {
        let full = "5a4705a4aaa2e7f10a7dd6c302256dd373516e56";
        // The comparison and the cell must never disagree about a value:
        // "comparable, but shown as malformed" is a row nobody can resolve.
        for value in [
            full,
            "5a4705a4",
            "5a4705a",
            "5a4705",
            "",
            "  ",
            "v2.10.1",
            &full.repeat(2),
            "zzzzzzzz",
        ] {
            let names_one = names_a_commit(value.trim());
            assert_eq!(
                reviewed_cell(value) != "malformed",
                names_one,
                "the cell disagrees for {value:?}"
            );
            // A value that names a commit is current against itself; one
            // that does not is drift no matter what upstream answers.
            assert_eq!(
                !drifted(value, value),
                names_one,
                "the comparison disagrees for {value:?}"
            );
        }
    }

    fn row(package: &str, aur: Option<&str>, probe: Probe) -> Row {
        Row {
            package: package.into(),
            reviewed: "3f9c21ab".into(),
            role: Role::Vendored,
            aur: aur.map(str::to_string),
            probe,
            rejected_note: None,
        }
    }

    #[test]
    fn one_rpc_batch_covers_both_halves_without_asking_twice() {
        let rows = vec![
            row("mdcat", Some("mdcat"), Probe::Pending("aa".into())),
            // Current, so its version is never shown and never fetched.
            row("pacseek", Some("pacseek"), Probe::Current),
            // A non-AUR forge has no name the RPC would recognise.
            row("inhouse", None, Probe::Pending("bb".into())),
        ];
        let candidates = vec!["archi".to_string(), "mdcat".to_string()];
        // Sorted, deduped, and `mdcat` — wanted by both halves — asked once.
        assert_eq!(rpc_names(&rows, &candidates), ["archi", "mdcat"]);
        assert!(rpc_names(&rows, &[]).contains(&"mdcat".to_string()));
        assert!(rpc_names(&[], &candidates).len() == 2);
        assert!(rpc_names(&[], &[]).is_empty());
    }

    /// A refusal is about one commit. It silences that candidate and nothing
    /// else — the moment upstream moves, the question is new and unanswered.
    #[test]
    fn a_refusal_covers_the_commit_it_names_until_upstream_moves() {
        let full = "7d02e4c1aaa2e7f10a7dd6c302256dd373516e56";
        let moved = "0000000000000000000000000000000000000000";
        let refused_at = |commit: &str| {
            let mut e = entry("mdcat");
            e.rejected = Some(commit.into());
            e
        };
        assert!(refused(&refused_at(full), full));
        // The ledger is hand-editable, so a short hash names the same commit.
        assert!(refused(&refused_at("7d02e4c1"), full));
        assert!(!refused(&refused_at(full), moved));
        assert!(!refused(&entry("mdcat"), full));
        // A `rejected` value that names no commit must not silence anything:
        // an unreadable refusal leaves the row pending, the safe direction.
        for junk in ["", "0", "zzzzzzzz", "HEAD"] {
            assert!(!refused(&refused_at(junk), full), "{junk:?} silenced a row");
        }
    }

    /// The whole point of recording a refusal: a rejected candidate is drift
    /// that has been answered, so it is not work, so it is not a 10.
    #[test]
    fn a_rejected_candidate_is_not_pending() {
        assert_eq!(code(outcome(0, 0, 0, false)), 0);
        let rejected = [row("mdcat", None, Probe::Rejected("7d02e4c1".into()))];
        let refs: Vec<&Row> = rejected.iter().collect();
        let ledger = Ledger {
            pending: &[],
            rejected: &refs,
            unreachable: &[],
            current: 2,
        };
        // It is not silence, though: the row is in the report and in the
        // tallies, where a reader can see the decision that was made.
        assert!(!ledger.quiet());
        assert_eq!(counts(&ledger), "0 pending · 2 current · 1 rejected");
    }

    #[test]
    fn the_tallies_account_for_every_row() {
        let pending = [row("a", None, Probe::Pending("aa".into()))];
        let unreachable = [row("b", None, Probe::Unreachable("no".into()))];
        let (p, u): (Vec<&Row>, Vec<&Row>) =
            (pending.iter().collect(), unreachable.iter().collect());
        assert_eq!(
            counts(&Ledger {
                pending: &p,
                rejected: &[],
                unreachable: &u,
                current: 3,
            }),
            "1 pending · 3 current · 1 unreachable"
        );
        // The quiet ledger says the two numbers that exist and no more.
        assert_eq!(
            counts(&Ledger {
                pending: &[],
                rejected: &[],
                unreachable: &[],
                current: 4,
            }),
            "0 pending · 4 current"
        );
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
