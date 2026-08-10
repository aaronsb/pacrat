//! `pacrat push` — the maintained rung's one outward-facing verb: take the
//! store's tree for a package we claim and publish it to the upstream it came
//! from. Generalised from clicue's `packaging/publish-aur.zsh`, which is where
//! the shape of this — probe, clone, compare, alarm, regenerate, push — was
//! worked out against a real AUR package.
//!
//! Everything else in pacrat reads the world and writes the store. This is the
//! only verb that writes somebody *else's* repository, which is why it is the
//! most conservative one:
//!
//! 1. **Only maintained packages.** `vendored` means pull-only. Publishing a
//!    tree we merely vendored would push a third party's package under our
//!    key, and the ledger's role is the only place that claim is recorded.
//! 2. **Ask before touching the network in a way that lasts.** The AUR write
//!    probe is a read-only question (`ssh … help`) asked before anything is
//!    cloned, because during the June 2026 incident the honest answer to
//!    "publish this" was "the AUR is not accepting publishes", and a verb that
//!    discovers that at the last step has already wasted the operator's
//!    attention. A blocked probe queues the errand (see [`pacrat_core::queue`])
//!    and exits 10.
//! 3. **The tamper alarm holds.** A checksum that changed for a source of an
//!    already-published version is an incident, not a re-sum: an immutable tag
//!    whose tarball changed means someone rewrote it. clicue exits 1 and says
//!    "investigate before republishing"; so does this, loudly, with both sums
//!    on screen.
//! 4. **The diff is shown and the human answers.** Same rule as `review`: what
//!    is about to be published is rendered — neutered — and `[y/N]` is a real
//!    question. `--yes` is the only way past it, and a non-terminal is not an
//!    answer.
//! 5. **Never a force-push.** There is no flag for it. A rejected push means
//!    the remote holds something this clone did not see, and the resolution is
//!    a human reading it, never `--force`.
//!
//! ## Deviation from clicue: no test build
//!
//! clicue's script runs `makepkg --cleanbuild` before pushing. pacrat does
//! not, deliberately:
//!
//! * **Building is `build`'s domain.** `pacrat build` already compiles this
//!   exact tree out of the store into `[dotfiles-aur]`, and that is the
//!   evidence that it builds. Running a second, differently-configured build
//!   here would make two answers to one question.
//! * **An AUR tree is built on AUR infrastructure**, by whoever installs it,
//!   with their makepkg.conf. A green build on this machine is weak evidence
//!   about theirs, and pushing was never gated on it.
//! * The one thing that genuinely has to be regenerated — `.SRCINFO`, which is
//!   what the AUR's own metadata is read from — is regenerated here via
//!   `makepkg --printsrcinfo`. That does source the PKGBUILD, so it is the one
//!   place this verb executes the tree's shell: on a tree out of our own
//!   store, at a commit the ledger records as reviewed.
//!
//! ## Exit codes
//!
//! 0 published (or the remote was already current) · 10 queued, blocked, or
//! declined · 1 the alarm, or a failure.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pacrat_core::pkg::valid_name;
use pacrat_core::pkgbuild;
use pacrat_core::queue::Queue;
use pacrat_core::sources::Role;
use pacrat_core::version::is_newer;

use crate::ctx::{self, Ctx};
use crate::fstree;
use crate::git::{self, Git};
use crate::out::{
    epoch_stamp, list_preview, shell_quote, short_hash, truncate, visible, visible_line,
};
use crate::proc;
use crate::review;
use crate::updates;
use crate::vendor;
use crate::HELD;

/// The AUR's ssh endpoint. Not configurable: it is not a preference, it is
/// where the AUR is.
const AUR_SSH_HOST: &str = "aur@aur.archlinux.org";

/// The only branch the AUR accepts. A push to anything else is refused by the
/// server, so pacrat names it rather than guessing from the clone.
const AUR_BRANCH: &str = "master";

/// Wall-clock bound on the write probe. Longer than its own
/// `ConnectTimeout=10`, so a server that connects and then thinks is bounded
/// too.
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);

/// Wall-clock bound on `makepkg --printsrcinfo`. It sources the PKGBUILD,
/// which is shell, and shell can loop.
const SRCINFO_TIMEOUT: Duration = Duration::from_secs(60);

/// Diff lines printed before the rest is left to the reader's own tools —
/// `review`'s rule, for the same reason: a diff that scrolls the beginning
/// out of the scrollback has hidden it.
const DIFF_LINES: usize = 1000;

// ------------------------------------------------------------------- entry

pub fn run(ctx: &Ctx, package: Option<&str>, retry: bool, yes: bool) -> Result<(), String> {
    match (package, retry) {
        (Some(package), false) => match one(ctx, package, yes)? {
            Outcome::Published | Outcome::Current => Ok(()),
            Outcome::Held => std::process::exit(HELD),
        },
        (None, _) => drain(ctx, yes),
        (Some(_), true) => Err(
            "--retry works the whole publish queue — run it without a package name, \
             or name the package without --retry"
                .into(),
        ),
    }
}

enum Outcome {
    /// The remote now holds the store's tree.
    Published,
    /// It already did.
    Current,
    /// Deliberately did not publish. Every hold says why in its own words
    /// first; the caller only turns that into exit 10.
    Held,
}

/// Why a publish did not happen, in the one distinction the drain needs.
///
/// A drain works several packages, and the two kinds of bad news want
/// opposite handling. A [`Failure::Fault`] is about one package — its remote
/// refused the connection, its PKGBUILD will not parse — and the other
/// errands are still perfectly runnable, so it is reported under that
/// package and the loop goes on. A [`Failure::Alarm`] is the tamper alarm,
/// and it stops everything: "investigate before republishing" is advice
/// nobody reads at the top of a wall of successful publishes, and the
/// possibility being raised is that something is wrong with the *store* or
/// with what is on the other end, which is not a per-package problem.
#[derive(Debug)]
enum Failure {
    Alarm(String),
    Fault(String),
}

impl Failure {
    fn message(&self) -> &str {
        match self {
            Failure::Alarm(m) | Failure::Fault(m) => m,
        }
    }
}

/// Every plain error inside a publish is that package's fault, not the run's.
/// The alarm is constructed deliberately and is the only thing that is not.
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure::Fault(message)
    }
}

// -------------------------------------------------------------- one package

fn one(ctx: &Ctx, package: &str, yes: bool) -> Result<Outcome, String> {
    let curated = review::curated(ctx, package)?;
    if curated.entry.role != Role::Maintained {
        return Err(format!(
            "{package} is {} in the ledger, and push is for maintained packages only — \
             vendoring is a claim about what we trust, maintaining is a claim about what \
             we are responsible for, and only the second one comes with push rights. If \
             this package really is ours: `pacrat vendor {package} --role maintained \
             --force` re-records it",
            role_word(curated.entry.role)
        ));
    }

    let remote = Remote::derive(&curated.entry.upstream, package)?;
    println!("package   {package}");
    println!("upstream  {}", visible_line(&curated.entry.upstream).0);
    println!("remote    {}", visible_line(&remote.url).0);

    if remote.aur {
        let probe = probe_aur();
        probe.report();
        if !probe.open() {
            return block(package, &curated, &probe).map(|()| Outcome::Held);
        }
    }

    let outcome = publish(package, &curated, &remote, yes).map_err(|f| f.message().to_string())?;
    if matches!(outcome, Outcome::Published | Outcome::Current) {
        // The errand is done; a queue entry for it is now a lie.
        unqueue(package)?;
    }
    Ok(outcome)
}

fn role_word(role: Role) -> &'static str {
    match role {
        Role::Vendored => "vendored",
        Role::Maintained => "maintained",
    }
}

// ------------------------------------------------------------- the remote

/// Where a publish goes, derived from the ledger's upstream.
struct Remote {
    url: String,
    /// An AUR ssh remote: the one kind with a write probe and a fixed branch.
    aur: bool,
}

impl Remote {
    /// The AUR is the common case and the only special one.
    ///
    /// An AUR upstream is almost always recorded as the https clone URL,
    /// which is read-only — publishing needs the ssh form of the *same*
    /// repository, so it is derived rather than demanded of the ledger. Any
    /// other git URL is used exactly as written: pacrat has no forge-specific
    /// behaviour beyond the AUR (ADR-001, sources.rs).
    fn derive(upstream: &str, package: &str) -> Result<Self, String> {
        match updates::aur_repo(upstream) {
            Some(repo) => {
                // The name lands inside a URL pacrat hands to git. It comes
                // out of a synced ledger, so it is checked against the
                // package grammar rather than trusted for having been parsed.
                if !valid_name(repo) {
                    return Err(format!(
                        "the ledger's upstream for {package} names the AUR repository \
                         {:?}, which is not a package name — refusing to build a push \
                         URL out of it",
                        visible_line(repo).0
                    ));
                }
                Ok(Self {
                    url: format!("ssh://{AUR_SSH_HOST}/{repo}.git"),
                    aur: true,
                })
            }
            None => Ok(Self {
                url: git::url_arg(upstream)?.to_string(),
                aur: false,
            }),
        }
    }

    /// The branch a publish lands on.
    ///
    /// Fixed for the AUR, which accepts `master` and nothing else. Elsewhere
    /// it is whatever branch the clone checked out — which git reports even
    /// for an empty repository, where it is the branch a first commit would
    /// create.
    fn branch(&self, clone: &Path) -> Result<String, String> {
        if self.aur {
            return Ok(AUR_BRANCH.to_string());
        }
        let branch = Git::new([
            OsStr::new("-C"),
            clone.as_os_str(),
            OsStr::new("symbolic-ref"),
            OsStr::new("--short"),
            OsStr::new("HEAD"),
        ])
        .text()?;
        // It becomes a ref name in a push refspec.
        if branch.is_empty()
            || branch.starts_with('-')
            || branch.contains(['~', '^', ':', '?', '*', '[', '\\', ' '])
        {
            return Err(format!(
                "the clone is on {:?}, which is not a branch name pacrat will push to",
                visible_line(&branch).0
            ));
        }
        Ok(branch)
    }
}

// --------------------------------------------------------------- the probe

/// What the AUR said when asked whether it is accepting anything, and what
/// that means.
///
/// The distinction is the point. "The AUR is not accepting publishes" is a
/// statement about a *service*, and it is only pacrat's to make when a
/// reachable server said so. An unregistered ssh key, a missing `ssh`
/// binary and a name that will not resolve are three different problems on
/// *this* machine, and telling their owner that the AUR is read-only sends
/// them to check a status page while their own setup stays broken. Every one
/// of them still queues — the errand is real either way — but the sentence
/// on screen has to match the diagnosis.
struct AurProbe {
    verdict: Verdict,
    /// The server's or ssh's own words, verbatim.
    answer: String,
}

#[derive(PartialEq, Eq)]
enum Verdict {
    /// The key works and the service answered.
    Open,
    /// A reachable server declined: maintenance, read-only, a policy.
    Refusing,
    /// The key is not one this server accepts.
    KeyRejected,
    /// Nothing was reached: DNS, routing, a firewall, a timeout.
    Unreachable,
    /// ssh itself could not be run.
    NoSsh,
}

impl AurProbe {
    fn open(&self) -> bool {
        self.verdict == Verdict::Open
    }

    /// The one line, matched to the diagnosis.
    fn report(&self) {
        let (label, gloss) = match self.verdict {
            Verdict::Open => ("write access ok", ""),
            Verdict::Refusing => (
                "blocked",
                "  (the AUR is reachable and is not accepting publishes)",
            ),
            Verdict::KeyRejected => (
                "key refused",
                "  (this is your ssh setup, not the AUR's state — the key this host \
                 offered is not registered with your AUR account, or the agent does \
                 not have it)",
            ),
            Verdict::Unreachable => (
                "unreachable",
                "  (nothing answered — DNS, the network, or a firewall; the AUR's own \
                 state is unknown from here)",
            ),
            Verdict::NoSsh => (
                "cannot ask",
                "  (ssh could not be run on this host — install openssh)",
            ),
        };
        println!("probe     {label} — {}", self.answer);
        if !gloss.is_empty() {
            println!("        {gloss}");
        }
    }

    /// How the queue and the summary line describe it.
    fn summary(&self) -> &'static str {
        match self.verdict {
            Verdict::Open => "open",
            Verdict::Refusing => "the remote is not accepting publishes",
            Verdict::KeyRejected => "the AUR refused this host's ssh key",
            Verdict::Unreachable => "the AUR could not be reached from this host",
            Verdict::NoSsh => "ssh could not be run on this host",
        }
    }
}

/// Read a probe's outcome out of what ssh did and said.
///
/// ssh's exit code is nearly useless on its own — 255 covers everything from
/// a refused key to a DNS failure — so the text is what gets classified, and
/// only after the cases that never reached a server are excluded. Anything
/// unrecognised is `Refusing`: the conservative reading, since it means a
/// server said something pacrat does not have a rule for, and the summary
/// then defers to the server's own words which are printed beside it.
fn classify(status_ok: bool, text: &str) -> Verdict {
    if status_ok {
        return Verdict::Open;
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("permission denied")
        || lower.contains("publickey")
        || lower.contains("too many authentication failures")
        || lower.contains("no supported authentication methods")
    {
        return Verdict::KeyRejected;
    }
    if lower.contains("could not resolve")
        || lower.contains("name or service not known")
        || lower.contains("connection refused")
        || lower.contains("connection timed out")
        || lower.contains("network is unreachable")
        || lower.contains("no route to host")
        || lower.contains("connection closed by")
        || lower.contains("connection reset")
    {
        return Verdict::Unreachable;
    }
    Verdict::Refusing
}

/// Ask the AUR whether it is open, without changing anything.
///
/// `ssh aur@aur.archlinux.org help` is the AUR's own read-only introspection
/// command: on a healthy server it lists the commands the key may run, which
/// is simultaneously a check that the key is registered and that the service
/// is up. During maintenance it answers with the notice instead, and that
/// notice — the server's own words — is what pacrat quotes rather than
/// paraphrasing.
///
/// The ssh flags are pacrat's here (this is not git's ssh invocation, so
/// `git`'s hands-off rule does not apply): `BatchMode` so a machine with no
/// key fails instead of prompting, `ConnectTimeout` so an unreachable host
/// fails fast, and `accept-new` so a first-ever contact with the AUR does not
/// die on an unknown host key while a *changed* key still stops everything.
fn probe_aur() -> AurProbe {
    let argv = [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=accept-new",
        AUR_SSH_HOST,
        "help",
    ];
    println!(
        "run       ssh {}",
        argv.iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut cmd = Command::new("ssh");
    cmd.args(argv);
    match proc::run_with_timeout(cmd, PROBE_TIMEOUT) {
        Err(e) => AurProbe {
            verdict: Verdict::NoSsh,
            answer: format!("ssh could not be run: {e}"),
        },
        // A timeout reached nothing it can quote, which is the definition of
        // unreachable from here — never evidence about the service itself.
        Ok(ran) if ran.timed_out => AurProbe {
            verdict: Verdict::Unreachable,
            answer: format!("no answer within {}s", PROBE_TIMEOUT.as_secs()),
        },
        Ok(ran) => {
            let answer = said(&ran);
            AurProbe {
                verdict: classify(ran.status.success(), &answer),
                answer,
            }
        }
    }
}

/// What a child said, as one line fit to quote and to store.
///
/// stdout first: the AUR writes its maintenance notice there. Neutered and
/// folded, because it is a remote's text landing in pacrat's own report — and
/// truncated, because the queue keeps it and `pacrat status` puts it on a
/// line.
fn said(ran: &proc::Ran) -> String {
    for stream in [&ran.stdout, &ran.stderr] {
        let (text, _) = visible_line(&String::from_utf8_lossy(stream));
        let text = text.trim();
        if !text.is_empty() {
            return truncate(text, 160);
        }
    }
    format!("no output ({})", ran.status)
}

// --------------------------------------------------------------- the queue

fn queue_path() -> Result<PathBuf, String> {
    Ok(ctx::state_dir()?.join("pushes").join("queue.toml"))
}

/// The queue as it is on disk; a host with none has an empty one.
pub fn load_queue() -> Result<Queue, String> {
    let path = queue_path()?;
    match fs::read_to_string(&path) {
        Ok(text) => Queue::from_toml(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(_) => Ok(Queue::default()),
    }
}

/// Write the queue atomically — the argument `Ctx::save_sources` makes about
/// the ledger, made about host state: a crash mid-write must not leave a
/// half-parsed record of what is still owed.
///
/// **There is no lock, and that is a decision rather than an omission.** Two
/// pacrats writing this file at once — a `pacrat push mdcat` in one terminal
/// while a `pacrat push` drains in another — is a read-modify-write race, and
/// the loser's change is lost. What bounds the damage is that the rename is
/// atomic (nobody ever reads a torn queue) and that the queue is a *reminder*
/// rather than a record of record: the worst case is one forgotten errand,
/// recovered by running `pacrat push <package>` again, which is the command
/// the maintainer was going to run anyway. Against that, an flock adds a
/// stale-lock failure mode to a single-user tool on a single host — a
/// mechanism whose own failures are harder to explain than the problem it
/// prevents. Revisit if pacrat ever grows a daemon (ADR-001 decision 3 says
/// it will not).
fn save_queue(queue: &Queue) -> Result<(), String> {
    let path = queue_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".queue.toml.{}.new", std::process::id()));
    fs::write(&tmp, queue.to_toml()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Record a publish the remote would not accept.
fn block(package: &str, curated: &review::Curated, probe: &AurProbe) -> Result<(), String> {
    let digest = fstree::digest(&curated.tree)?;
    let mut queue = load_queue()?;
    queue.block(
        package,
        &curated.entry.reviewed,
        &digest,
        now(),
        &probe.answer,
    );
    save_queue(&queue)?;

    println!();
    println!(
        "queued    {package} @ {}",
        short_hash(&curated.entry.reviewed)
    );
    println!("answer    {}", visible_line(&probe.answer).0);
    println!("queue     {}", queue_path()?.display());
    println!();
    // Queued whatever the diagnosis — a publish that could not happen is a
    // real errand either way — but described by what actually went wrong, so
    // that a key which needs registering is not reported as an outage the
    // maintainer can only wait out.
    println!(
        "not published — {}. The errand is on file; `pacrat push` (no arguments) \
         probes again and works the queue",
        probe.summary()
    );
    Ok(())
}

/// Drop a finished errand.
fn unqueue(package: &str) -> Result<(), String> {
    let mut queue = load_queue()?;
    if queue.pushes.remove(package).is_some() {
        save_queue(&queue)?;
        println!("queue     {package} removed from the publish queue");
    }
    Ok(())
}

/// `pacrat status`' one line about the queue, when there is one.
///
/// The answer is a remote's text and is neutered and clipped here, at the
/// point it becomes part of somebody else's report.
pub fn status_line() -> Result<Option<String>, String> {
    let queue = load_queue()?;
    if queue.is_empty() {
        return Ok(None);
    }
    let answer = queue
        .latest_answer()
        .map(|a| truncate(&visible_line(a).0, 80))
        .unwrap_or_default();
    Ok(Some(format!(
        "{} publish{} queued (aur ssh: {answer})",
        queue.len(),
        if queue.len() == 1 { "" } else { "es" }
    )))
}

/// Print the queue as rows.
fn show_queue(queue: &Queue) {
    println!();
    println!("publish queue");
    for (package, entry) in &queue.pushes {
        println!(
            "  {:<24} {} · queued {} · probed {}",
            truncate(package, 24),
            short_hash(&entry.commit),
            epoch_stamp(entry.queued_at),
            epoch_stamp(entry.last_probe)
        );
        println!(
            "      answer  {}",
            truncate(&visible_line(&entry.last_answer).0, 100)
        );
        if let Some(note) = &entry.note {
            println!("      note    {}", truncate(&visible_line(note).0, 100));
        }
    }
}

// --------------------------------------------------------------- the drain

/// Work the queue: probe once, then publish everything still eligible.
///
/// One probe for the whole queue, not one per package: they all go to the
/// same server, and asking it ten times whether it is down is noise on
/// somebody else's machine. And no probe at all when nothing in the queue is
/// bound for the AUR — the probe is a question about a specific remote, not a
/// ceremony the verb performs.
fn drain(ctx: &Ctx, yes: bool) -> Result<(), String> {
    let mut queue = load_queue()?;
    if queue.is_empty() {
        println!("publish queue empty — nothing waiting to be published");
        return Ok(());
    }
    println!("queue     {} at {}", queue.len(), queue_path()?.display());

    if bound_for_aur(ctx, &queue)? {
        let probe = probe_aur();
        probe.report();
        if !probe.open() {
            queue.probed(now(), &probe.answer);
            save_queue(&queue)?;
            show_queue(&queue);
            println!();
            println!(
                "{} — {} publish{} waiting",
                probe.summary(),
                queue.len(),
                if queue.len() == 1 { "" } else { "es" }
            );
            std::process::exit(HELD);
        }
    }

    // One package's bad day is not the queue's. The order here is stable
    // (a BTreeMap over names), so a package that fails and stops the loop
    // stops the *same* later packages on every run — a single wedged errand
    // would silently hold back every alphabetically later publish forever.
    // So each one is worked in its own right and the failures are collected.
    let packages: Vec<String> = queue.pushes.keys().cloned().collect();
    let mut faulted: Vec<String> = Vec::new();
    for package in &packages {
        println!();
        println!("── {package} ──");
        match work(ctx, package, yes) {
            Ok(()) => {}
            // The alarm is the exception, and stops the run where it stands:
            // it is a claim that something is wrong beyond this package, and
            // burying it under later successes is how it gets ignored.
            Err(Failure::Alarm(message)) => {
                println!();
                println!(
                    "stopped   the tamper alarm ends the run — the rest of the queue is \
                     untouched and will be there after you have looked"
                );
                return Err(message);
            }
            Err(Failure::Fault(message)) => {
                println!("failed    {message}");
                faulted.push(package.clone());
            }
        }
    }

    let left = load_queue()?;
    if !left.is_empty() {
        show_queue(&left);
    }
    println!();
    if !faulted.is_empty() {
        println!(
            "{} publish{} failed: {}",
            faulted.len(),
            if faulted.len() == 1 { "" } else { "es" },
            list_preview(&faulted, 12)
        );
        // A failure outranks a hold: exit 10 says "ran fine, deliberately did
        // not act", and something here did not run fine.
        return Err(format!(
            "{} of {} queued publish{} could not be attempted — see the reports above",
            faulted.len(),
            packages.len(),
            if packages.len() == 1 { "" } else { "es" }
        ));
    }
    if left.is_empty() {
        println!("publish queue empty");
        return Ok(());
    }
    println!(
        "{} publish{} still queued",
        left.len(),
        if left.len() == 1 { "" } else { "es" }
    );
    std::process::exit(HELD);
}

/// One queued errand, start to finish. Its errors belong to it.
fn work(ctx: &Ctx, package: &str, yes: bool) -> Result<(), Failure> {
    match eligible(ctx, package)? {
        Eligibility::Gone(why) => {
            let mut queue = load_queue()?;
            queue.pushes.remove(package);
            save_queue(&queue)?;
            println!("dropped   {why}");
        }
        Eligibility::Unavailable(why) => {
            // The errand stands; this host simply cannot act on it right now.
            // Dropping it here is how an unmounted store during a sync turns
            // a pending publish into one nobody remembers.
            println!("kept      {why}");
        }
        Eligibility::Moved {
            curated,
            digest,
            why,
        } => {
            let mut queue = load_queue()?;
            if let Some(entry) = queue.pushes.get_mut(package) {
                entry.commit.clone_from(&curated.entry.reviewed);
                entry.digest = digest;
                entry.note = Some(why.clone());
            }
            save_queue(&queue)?;
            // Not published: what was queued is not what is in the store now,
            // and "publish the newer thing instead" is a decision nobody made.
            // Re-queued against the new bytes so the next run — or an explicit
            // `pacrat push <package>` — sees them.
            println!("re-queued {why}");
        }
        Eligibility::Ready(curated) => {
            let remote = Remote::derive(&curated.entry.upstream, package)?;
            println!("remote    {}", visible_line(&remote.url).0);
            match publish(package, &curated, &remote, yes)? {
                Outcome::Published | Outcome::Current => {
                    let mut queue = load_queue()?;
                    queue.pushes.remove(package);
                    save_queue(&queue)?;
                }
                Outcome::Held => {}
            }
        }
    }
    Ok(())
}

/// Is any queued publish going to the AUR?
///
/// Only an AUR remote has a write probe, so only an AUR remote can make the
/// drain ask one. A queued package that has left the ledger has no remote at
/// all and is settled below, by [`eligible`], without anyone being asked
/// anything.
fn bound_for_aur(ctx: &Ctx, queue: &Queue) -> Result<bool, String> {
    let sources = ctx.load_sources()?;
    Ok(queue.pushes.keys().any(|package| {
        sources
            .packages
            .get(package)
            .is_some_and(|entry| updates::aur_repo(&entry.upstream).is_some())
    }))
}

enum Eligibility {
    Ready(review::Curated),
    Moved {
        curated: review::Curated,
        digest: String,
        why: String,
    },
    /// The errand has been *withdrawn*: forget it.
    Gone(String),
    /// The errand stands but this host cannot act on it now: keep it.
    Unavailable(String),
}

/// Is this queued errand still the errand it was?
///
/// The digest is the question, not the commit. A queue entry says "publish
/// *these bytes*", and between queueing and draining the store can be synced,
/// edited, or advanced through the review gate — all of which change what
/// would go out while the ledger may say nothing at all. ADR-001 settled this
/// for gradings ("a grading is about bytes, not about a commit") and it is the
/// same question here, asked days later instead of seconds.
///
/// **Only two answers delete a queue entry**, and both are a human's decision
/// rather than a machine's weather: the package is no longer in the ledger,
/// or its role is no longer `maintained`. Everything else — a store that is
/// not mounted, a tree that vanished mid-sync, an unreadable file — is a
/// reason this host cannot answer *right now*, and forgetting a pending
/// publish over a transient condition is the one outcome the queue exists to
/// prevent. Those keep the entry and say so.
fn eligible(ctx: &Ctx, package: &str) -> Result<Eligibility, Failure> {
    let queue = load_queue()?;
    let Some(entry) = queue.pushes.get(package).cloned() else {
        return Ok(Eligibility::Gone("no longer in the queue".into()));
    };

    // Asked of the ledger directly rather than inferred from `curated`'s
    // error, which cannot tell "withdrawn" from "the disk is not there".
    let sources = ctx.load_sources()?;
    let Some(row) = sources.packages.get(package) else {
        return Ok(Eligibility::Gone(format!(
            "{package} is no longer in the ledger — the claim that made this errand \
             ours has been withdrawn"
        )));
    };
    if row.role != Role::Maintained {
        return Ok(Eligibility::Gone(format!(
            "{package} is {} in the ledger now — push is for maintained packages",
            role_word(row.role)
        )));
    }

    let curated = match review::curated(ctx, package) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Eligibility::Unavailable(format!(
                "{e} — the errand stays queued; nothing about it has been withdrawn"
            )))
        }
    };

    let digest = match fstree::digest(&curated.tree) {
        Ok(d) => d,
        Err(e) => {
            return Ok(Eligibility::Unavailable(format!(
                "the store tree cannot be read ({e}) — the errand stays queued"
            )))
        }
    };
    if digest != entry.digest {
        let why = if curated.entry.reviewed == entry.commit {
            format!(
                "the store tree changed since it was queued (commit still {}) — \
                 re-queued against the new bytes; review them and run \
                 `pacrat push {package}`",
                short_hash(&entry.commit)
            )
        } else {
            format!(
                "the reviewed commit moved {} → {} since it was queued — re-queued \
                 against the new bytes; run `pacrat push {package}` to publish them",
                short_hash(&entry.commit),
                short_hash(&curated.entry.reviewed)
            )
        };
        return Ok(Eligibility::Moved {
            curated,
            digest,
            why,
        });
    }
    Ok(Eligibility::Ready(curated))
}

// ------------------------------------------------------------- publishing

/// Clone, mirror, compare, regenerate, show, ask, push.
///
/// The scratch clone survives a *failure* and nothing else. An outcome — a
/// publish, an already-current remote, a declined prompt — is a question that
/// got answered, and the clone is then only a copy of two trees that both
/// still exist. A failure is the case where it is evidence, and the alarm is
/// why that matters: the clone's *history* is what the remote published, so
/// `git -C <clone> show HEAD:PKGBUILD` is the copy to investigate against
/// (its working tree has already been mirrored over with the store's).
fn publish(
    package: &str,
    curated: &review::Curated,
    remote: &Remote,
    yes: bool,
) -> Result<Outcome, Failure> {
    let scratch = vendor::scratch_dir("push", package)?;
    let clone = scratch.join("remote");
    let outcome = staged(package, curated, remote, &clone, yes);
    match &outcome {
        Ok(_) => {
            let _ = fs::remove_dir_all(&scratch);
        }
        Err(_) => {
            println!();
            println!("clone     kept for inspection at {}", clone.display());
        }
    }
    outcome
}

fn staged(
    package: &str,
    curated: &review::Curated,
    remote: &Remote,
    clone: &Path,
    yes: bool,
) -> Result<Outcome, Failure> {
    Git::new([
        OsStr::new("clone"),
        OsStr::new("--"),
        OsStr::new(git::url_arg(&remote.url)?),
        clone.as_os_str(),
    ])
    .timeout(git::TRANSFER)
    .text()?;

    // Read the remote's own copy before it is overwritten: everything the
    // alarm knows about what was published, it learns here.
    let published = Published::read(clone);
    let branch = remote.branch(clone)?;

    let store_files = fstree::files(&curated.tree)?;
    if !store_files.iter().any(|f| f == "PKGBUILD") {
        return Err(Failure::Fault(format!(
            "the store tree for {package} has no PKGBUILD — there is nothing to publish"
        )));
    }
    let store_pkgbuild = read_text(&curated.tree.join("PKGBUILD"))?;
    fstree::mirror(&curated.tree, &store_files, clone)?;

    // `.SRCINFO` is the AUR's metadata, and a stale one is a package that
    // shows the wrong version in every search. Regenerated from the PKGBUILD
    // that is about to be published rather than trusted from the store.
    let srcinfo = srcinfo(clone)?;
    fs::write(clone.join(".SRCINFO"), &srcinfo)
        .map_err(|e| format!("{}: {e}", clone.join(".SRCINFO").display()))?;
    if store_files.iter().any(|f| f == ".SRCINFO")
        && read_text(&curated.tree.join(".SRCINFO"))? != srcinfo
    {
        println!(
            "note      the store's .SRCINFO differs from the one regenerated here; \
             what is published is the regenerated one"
        );
    }

    // What is being published, named before it is staged: the store's tree
    // plus the .SRCINFO just generated from it, and nothing else. Anything
    // else in the directory is a side effect of having sourced a shell script.
    let mut intended: BTreeSet<String> = store_files.iter().cloned().collect();
    intended.insert(".SRCINFO".to_string());
    let swept = sweep(clone, &intended)?;
    if !swept.is_empty() {
        println!(
            "swept     {} — left behind by makepkg reading the PKGBUILD, not published",
            list_preview(&safe_names(&swept), 12)
        );
    }

    let version = Version::read(&srcinfo, &store_pkgbuild);
    println!("version   {}", version.full());
    println!("digest    {}", fstree::digest(&curated.tree)?);
    if let Some(prev) = &published.version {
        println!("published {}", prev.full());
    } else {
        println!("published nothing yet — this is the first publish");
    }

    alarm(&published, &version, &store_pkgbuild)?;

    let changed = stage_all(clone)?;
    if changed.is_empty() {
        println!();
        println!("already current — the remote holds this tree byte for byte; nothing to push");
        return Ok(Outcome::Current);
    }
    println!("changing  {}", list_preview(&changed, 12));
    // After the nothing-to-do exit, deliberately: telling someone their
    // re-push will not reach any host is alarming and useless when the answer
    // is that there is no re-push.
    warn_if_no_bump(&published, &version);
    let diff = diff(clone)?;
    render_diff(&diff);

    println!();
    if !confirmed(package, &version, yes)? {
        return Ok(Outcome::Held);
    }

    let message = format!("{package} {}", version.full());
    Git::new([
        OsStr::new("-C"),
        clone.as_os_str(),
        OsStr::new("commit"),
        OsStr::new("-m"),
        OsStr::new(&message),
    ])
    .text()
    .map_err(|e| {
        format!(
            "{e}\n       a commit needs an author — `git config --global user.email …` \
             and `user.name` if git has never been told who you are"
        )
    })?;

    Git::new(push_argv(clone, &branch))
        .timeout(git::TRANSFER)
        .text()
        .map_err(|e| {
            format!(
                "{e}\n       a refused push usually means the remote moved since this \
                 clone — read what is there and run `pacrat push {package}` again. \
                 There is no --force, on purpose: overwriting a publish nobody here \
                 has read is how a maintainer loses somebody else's fix"
            )
        })?;

    println!();
    println!(
        "published {package} {} → {}",
        version.full(),
        visible_line(&remote.url).0
    );
    if remote.aur {
        println!(
            "          https://aur.archlinux.org/packages/{}",
            aur_name(&remote.url)
        );
    }
    Ok(Outcome::Published)
}

/// The push, as argv.
///
/// Split out to be tested rather than trusted: this is the one command in
/// pacrat that changes something outside this machine, and the two properties
/// worth pinning — an explicit `refs/heads/` destination, and no force in any
/// spelling — are properties of exactly this list.
fn push_argv(clone: &Path, branch: &str) -> Vec<std::ffi::OsString> {
    [
        OsStr::new("-C"),
        clone.as_os_str(),
        OsStr::new("push"),
        OsStr::new("origin"),
        OsStr::new(&format!("HEAD:refs/heads/{branch}")),
    ]
    .iter()
    .map(|a| a.to_os_string())
    .collect()
}

/// The repository name back out of a push URL, for the human-facing link.
fn aur_name(url: &str) -> String {
    updates::aur_repo(url)
        .map(str::to_string)
        .unwrap_or_default()
}

// ----------------------------------------------------------------- the tree

/// What the remote already holds, read before anything overwrites it.
struct Published {
    pkgbuild: Option<String>,
    version: Option<Version>,
}

impl Published {
    fn read(clone: &Path) -> Self {
        let pkgbuild = fs::read(clone.join("PKGBUILD"))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        // The published .SRCINFO is makepkg's own evaluation of the published
        // PKGBUILD, so it is the better answer where it exists; the PKGBUILD
        // text is the fallback for a repository that never had one.
        let srcinfo = fs::read(clone.join(".SRCINFO"))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        let version = match (&srcinfo, &pkgbuild) {
            (Some(srcinfo), Some(pkgbuild)) => Some(Version::read(srcinfo, pkgbuild)),
            (None, Some(pkgbuild)) => Some(Version::read("", pkgbuild)),
            _ => None,
        };
        Self { pkgbuild, version }
    }
}

/// A package version as it will be published: `[epoch:]pkgver-pkgrel`.
///
/// The epoch is carried rather than dropped because it is not decoration —
/// it is the field that outranks every other, and it exists precisely for the
/// case where a version number went *backwards*. A publish flow that read
/// `pkgver` and `pkgrel` and ignored `epoch` would print the wrong version in
/// its commit message, would call an epoch bump "no update" in the warning
/// below, and would ask the alarm to compare two releases that pacman
/// considers different packages entirely.
#[derive(Debug, PartialEq, Eq)]
struct Version {
    /// Absent when the PKGBUILD declares none, which is the common case;
    /// `Some("0")` and absent mean the same thing to pacman and are rendered
    /// the same way here.
    epoch: Option<String>,
    pkgver: String,
    pkgrel: String,
}

impl Version {
    /// `.SRCINFO` first, PKGBUILD text second.
    ///
    /// The order is the whole point: `.SRCINFO` is what makepkg computed by
    /// actually running the file, so a VCS package whose `pkgver()` is a
    /// function has a real version there and a shell fragment in the PKGBUILD.
    /// The text is the fallback for the case where makepkg said nothing.
    fn read(srcinfo: &str, pkgbuild: &str) -> Self {
        let pick = |key: &str| {
            srcinfo_field(srcinfo, key)
                .or_else(|| pkgbuild::field(pkgbuild, key))
                .map(|v| truncate(&visible_line(&v).0, 60))
        };
        Self {
            epoch: pick("epoch").filter(|e| e != "0"),
            pkgver: pick("pkgver").unwrap_or_else(|| "0".to_string()),
            pkgrel: pick("pkgrel").unwrap_or_else(|| "1".to_string()),
        }
    }

    /// The whole version, in the spelling pacman and `vercmp` use.
    fn full(&self) -> String {
        match &self.epoch {
            Some(epoch) => format!("{epoch}:{}-{}", self.pkgver, self.pkgrel),
            None => format!("{}-{}", self.pkgver, self.pkgrel),
        }
    }

    /// What the alarm means by "the same version".
    ///
    /// Epoch and pkgver, without pkgrel: a `pkgrel` bump is ordinary
    /// maintenance at the *same upstream release*, and the tarball behind
    /// that release is supposed to be the same bytes either way — which is
    /// the whole thing the alarm is watching. An epoch bump is not
    /// maintenance: it says upstream's numbering restarted, so the tarball
    /// behind `1.0` after the bump is a different artifact than the one
    /// behind `1.0` before it, and comparing their checksums would alarm on
    /// a rename.
    fn release(&self) -> (Option<&str>, &str) {
        (self.epoch.as_deref(), self.pkgver.as_str())
    }
}

/// `key = value` out of a `.SRCINFO`.
fn srcinfo_field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().to_string())
    })
}

/// Regenerate `.SRCINFO` from the PKGBUILD in the clone.
fn srcinfo(clone: &Path) -> Result<String, String> {
    let argv = ["makepkg", "--printsrcinfo"];
    println!("run       {} (in {})", argv.join(" "), clone.display());
    let mut cmd = Command::new(argv[0]);
    cmd.arg(argv[1]).current_dir(clone);
    let ran = proc::run_with_timeout(cmd, SRCINFO_TIMEOUT)
        .map_err(|e| format!("makepkg: {e} — is pacman's base-devel installed?"))?;
    if ran.timed_out {
        return Err(format!(
            "makepkg --printsrcinfo did not finish within {}s — the PKGBUILD is shell, \
             and something in it is not returning",
            SRCINFO_TIMEOUT.as_secs()
        ));
    }
    if !ran.status.success() {
        return Err(format!(
            "makepkg --printsrcinfo failed: {}",
            truncate(&visible_line(&String::from_utf8_lossy(&ran.stderr)).0, 200)
        ));
    }
    let text = String::from_utf8_lossy(&ran.stdout).into_owned();
    if text.trim().is_empty() {
        return Err("makepkg --printsrcinfo printed nothing".into());
    }
    Ok(text)
}

/// The tamper alarm.
///
/// Only asked of two files at the same *release* — the same epoch and pkgver
/// (see [`Version::release`]) — because that is the pair that names one
/// upstream artifact.
///
/// It says how many sources it was able to compare, and that line is not
/// decoration. The failure mode worth fearing here is not a false alarm, it
/// is a **miss**: a PKGBUILD written in a form [`pkgbuild`] cannot read
/// yields zero comparisons and therefore silence, which looks exactly like
/// "checked, all clear". `compared 0 sources` on the screen is the
/// difference between those two, and the only signal a reader gets that the
/// gate did not actually run.
fn alarm(published: &Published, version: &Version, store_pkgbuild: &str) -> Result<(), Failure> {
    let (Some(prev), Some(prev_pkgbuild)) = (&published.version, &published.pkgbuild) else {
        return Ok(());
    };
    if prev.release() != version.release() {
        return Ok(());
    }

    let compared = pkgbuild::comparable_sources(prev_pkgbuild, store_pkgbuild);
    let changes = pkgbuild::changed_sums(prev_pkgbuild, store_pkgbuild);
    println!(
        "compared  {compared} source{} with checksums on both sides of {}",
        if compared == 1 { "" } else { "s" },
        version.full()
    );
    if compared == 0 {
        println!(
            "warning   nothing to compare — the tamper check could not read a checksummed \
             source out of either PKGBUILD, so it is silent here rather than clear. Read \
             the diff yourself before answering"
        );
    }
    if changes.is_empty() {
        return Ok(());
    }

    println!();
    println!("ALARM     the checksum for an already-published source changed");
    for change in &changes {
        println!(
            "  source    {}",
            truncate(&visible_line(&change.source).0, 100)
        );
        println!(
            "  {:<9} published {}",
            change.algo,
            visible_line(&change.published).0
        );
        println!("            now       {}", visible_line(&change.now).0);
    }
    Err(Failure::Alarm(format!(
        "not published — {} is already published and the bytes behind one of its \
         sources have changed. An immutable tag whose tarball changed is an incident, \
         never a silent re-sum: someone or something rewrote it. Investigate before \
         republishing — if the change is legitimate, it is a different artifact and \
         needs a version that says so: a new pkgver, or a new epoch if upstream \
         re-cut the release under a number it had already used",
        version.full()
    )))
}

/// Would anyone's machine see this publish as an update?
///
/// Asked only once there is something to publish, and asked through core's
/// pacman-compatible comparison rather than by comparing fields: an epoch
/// bump *is* an update even when pkgver goes backwards, which is the entire
/// reason epoch exists, and a hand-rolled "did pkgver or pkgrel change"
/// would call that a no-op and be exactly wrong.
fn warn_if_no_bump(published: &Published, version: &Version) {
    let Some(prev) = &published.version else {
        return;
    };
    if is_newer(&prev.full(), &version.full()) {
        return;
    }
    println!(
        "warning   publishing {} over {} — pacman does not rank it higher, so no host \
         will pick it up as an update. Bump pkgrel (or pkgver, or epoch) if this is \
         meant to reach machines",
        version.full(),
        prev.full()
    );
}

/// Reduce the clone's working tree to exactly `intended`, and say what was
/// swept away.
///
/// Everything not in the intended set goes, whatever it is and however it got
/// there. In practice it got there one way: `makepkg --printsrcinfo` sources
/// the PKGBUILD, and a PKGBUILD is shell — a top-level `mkdir`, a stray
/// `curl`, a `.pkg.tar.zst` from an interrupted earlier run in a tree the
/// remote already carried. None of that is what the maintainer meant to
/// publish, and staging it because it happened to be in the directory is how
/// a build artifact ends up in somebody's AUR repository.
///
/// Its own walk rather than [`fstree::files`], because this one must be able
/// to remove what that one refuses to describe: a side effect that dropped a
/// symlink into the tree needs deleting, not an error.
fn sweep(clone: &Path, intended: &BTreeSet<String>) -> Result<Vec<String>, String> {
    let mut swept = Vec::new();
    sweep_dir(clone, clone, intended, &mut swept)?;
    swept.sort();
    Ok(swept)
}

fn sweep_dir(
    root: &Path,
    dir: &Path,
    intended: &BTreeSet<String>,
    swept: &mut Vec<String>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        // The clone's history is how the publish happens.
        if rel == ".git" {
            continue;
        }
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // Not `is_dir()`: that follows symlinks, and descending through one
        // would sweep whatever it points at.
        if kind.is_dir() {
            sweep_dir(root, &path, intended, swept)?;
            // A directory the intended set never mentions is left only if
            // something intended still lives under it.
            let _ = fs::remove_dir(&path);
        } else if !intended.contains(&rel) {
            fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            swept.push(rel);
        }
    }
    Ok(())
}

/// Stage exactly the intended set, and report what moved.
///
/// `--force` is the load-bearing flag, and it is here because of a real way
/// to lose a file: `git add` honours ignore rules, and the AUR's own
/// recommended `.gitignore` for package repositories ignores everything and
/// re-includes a handful of names. A `fix-cve.patch` in the store is then
/// silently not added — absent from the staged set, absent from the diff the
/// maintainer is shown, absent from the publish, and the command exits 0.
/// The ignore file need not even be in the tree: `core.excludesFile` puts it
/// in the user's own config, where nothing about this package can be read to
/// predict it.
///
/// So the set is decided by pacrat and stated to git, rather than discovered
/// by git and accepted by pacrat. `--force` defeats the ignore rules,
/// [`sweep`] has already removed anything outside the set, and `--all` still
/// stages the deletions of files the remote carried and the store no longer
/// has.
fn stage_all(clone: &Path) -> Result<Vec<String>, String> {
    Git::new([
        OsStr::new("-C"),
        clone.as_os_str(),
        OsStr::new("add"),
        OsStr::new("--all"),
        OsStr::new("--force"),
        OsStr::new("--"),
        OsStr::new("."),
    ])
    .text()?;
    let names = Git::new([
        OsStr::new("-C"),
        clone.as_os_str(),
        OsStr::new("diff"),
        OsStr::new("--cached"),
        OsStr::new("--name-only"),
        OsStr::new("--"),
    ])
    .text()?;
    Ok(names
        .lines()
        .map(|l| truncate(&visible_line(l).0, 60))
        .filter(|l| !l.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

/// What is about to be published, as a diff against what is published now.
fn diff(clone: &Path) -> Result<String, String> {
    let dir = clone.to_string_lossy().into_owned();
    let argv: Vec<&str> = git::NO_ATTRS
        .iter()
        .copied()
        .chain(["-C", &dir, "diff"])
        .chain(git::NO_FILTERS)
        .chain(["--cached", "--"])
        .collect();
    let ran = Git::new(argv).timeout(git::DIFF).run()?;
    let cut = ran.stdout.len() as u64 >= proc::PIPE_LIMIT;
    match ran.status.code() {
        Some(0 | 1) => {}
        // A signal with a full buffer is our own SIGPIPE: the diff was cut,
        // not lost. `review` makes the same distinction, for the same reason.
        None if cut => {}
        _ => {
            return Err(format!(
                "git diff --cached failed: {}",
                git::error(&String::from_utf8_lossy(&ran.stderr))
            ))
        }
    }
    let mut text = String::from_utf8_lossy(&ran.stdout).into_owned();
    if cut {
        text.push_str("\n[diff hit pacrat's ceiling and is not all of it]\n");
    }
    Ok(text)
}

/// Print the diff, neutered.
///
/// This is our own tree, which makes it the least hostile text pacrat renders
/// — and it is rendered by the same rules anyway. A store tree is written by
/// syncs and merges as well as by hands, and "it is ours" is a claim the
/// terminal cannot check.
fn render_diff(diff: &str) {
    let (safe, hidden) = visible(diff);
    let total = safe.lines().count();
    println!();
    for line in safe.lines().take(DIFF_LINES) {
        println!("{line}");
    }
    println!(
        "end diff  {total} line{}",
        if total == 1 { "" } else { "s" }
    );
    let over = total.saturating_sub(DIFF_LINES);
    if over > 0 {
        println!(
            "…         {over} further line{} not shown",
            if over == 1 { "" } else { "s" }
        );
    }
    if hidden > 0 {
        println!(
            "warning   {hidden} control character{} shown as ␛-style stand-ins",
            if hidden == 1 { "" } else { "s" }
        );
    }
}

/// Ask, and mean it.
///
/// Stricter than [`crate::vendor::confirm`], and only here: everywhere else a
/// piped `y` is a fine way to answer a prompt, because the consequence is
/// local and reversible. This one writes to a repository other people install
/// from. So a terminal or `--yes` — an explicit statement that nobody is
/// watching — and nothing in between.
fn confirmed(package: &str, version: &Version, yes: bool) -> Result<bool, String> {
    if yes {
        println!("publishing {package} {} on --yes", version.full());
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        println!(
            "not published — stdin is not a terminal, and a piped answer is not consent \
             for a publish. Run it from a terminal, or pass --yes"
        );
        return Ok(false);
    }
    if !vendor::confirm("publish", package, &version.full())? {
        println!("not published");
        return Ok(false);
    }
    Ok(true)
}

/// File names as fields on pacrat's report: neutered and clipped one by one,
/// because a name is written by whatever put the file there.
fn safe_names(names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|n| truncate(&visible_line(n).0, 60))
        .collect()
}

/// Read a file we are about to publish; invalid UTF-8 is shown lossily rather
/// than failing, the way `vendor` reads a tree it is reviewing.
fn read_text(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_aur_upstream_becomes_the_ssh_form_of_the_same_repository() {
        for upstream in [
            "https://aur.archlinux.org/playtimed.git",
            "ssh://aur@aur.archlinux.org/playtimed.git",
            "aur@aur.archlinux.org:playtimed.git",
        ] {
            let remote = Remote::derive(upstream, "playtimed").unwrap();
            assert_eq!(remote.url, "ssh://aur@aur.archlinux.org/playtimed.git");
            assert!(remote.aur, "{upstream}");
        }
        // The AUR repository is the pkgbase, which is not always the package.
        let remote = Remote::derive("https://aur.archlinux.org/foo-git.git", "foo").unwrap();
        assert_eq!(remote.url, "ssh://aur@aur.archlinux.org/foo-git.git");
    }

    #[test]
    fn any_other_git_url_is_published_exactly_as_written() {
        let remote = Remote::derive("git@github.com:aaronsb/x.git", "x").unwrap();
        assert_eq!(remote.url, "git@github.com:aaronsb/x.git");
        assert!(!remote.aur);
        let remote = Remote::derive("file:///srv/fixture.git", "x").unwrap();
        assert_eq!(remote.url, "file:///srv/fixture.git");
        assert!(!remote.aur);
        // A URL that could be read as an option is refused by name.
        assert!(Remote::derive("--upload-pack=x", "x").is_err());
    }

    /// The push is the one thing here that reaches outside this machine.
    #[test]
    fn the_push_is_explicit_and_can_never_be_a_force() {
        let argv = push_argv(Path::new("/tmp/clone"), "master");
        let line: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            line,
            [
                "-C",
                "/tmp/clone",
                "push",
                "origin",
                "HEAD:refs/heads/master"
            ]
        );
        assert!(!line.iter().any(|a| a.contains("force") || a == "+HEAD"));
    }

    /// The AUR being down and this host's key not being registered are two
    /// different problems, and only one of them is worth waiting out. Telling
    /// a maintainer with an unregistered key that the AUR is read-only sends
    /// them to a status page while their own setup stays broken.
    #[test]
    fn a_blocked_probe_says_which_kind_of_blocked() {
        let cases = [
            (
                "The AUR is down due to maintenance. We will be back soon.",
                Verdict::Refusing,
            ),
            ("Permission denied (publickey).", Verdict::KeyRejected),
            (
                "Received disconnect from 2a01::1 port 22: Too many authentication failures",
                Verdict::KeyRejected,
            ),
            (
                "ssh: Could not resolve hostname aur.archlinux.org: Name or service not known",
                Verdict::Unreachable,
            ),
            (
                "ssh: connect to host aur.archlinux.org port 22: Connection refused",
                Verdict::Unreachable,
            ),
            (
                "ssh: connect to host aur.archlinux.org port 22: Network is unreachable",
                Verdict::Unreachable,
            ),
            // Something a server said that pacrat has no rule for: read as
            // the server refusing, with its own words printed beside it.
            ("Repository is read-only for now", Verdict::Refusing),
        ];
        for (answer, want) in cases {
            assert!(classify(false, answer) == want, "misclassified: {answer:?}");
        }
        assert!(classify(true, "Welcome to AUR, aaronsb!") == Verdict::Open);

        // Only a reachable server's refusal may be described as the AUR not
        // accepting publishes; nothing else may claim to know its state.
        for verdict in [Verdict::KeyRejected, Verdict::Unreachable, Verdict::NoSsh] {
            let probe = AurProbe {
                verdict,
                answer: String::new(),
            };
            assert!(
                !probe.summary().contains("not accepting"),
                "claimed the AUR is read-only on the wrong evidence: {}",
                probe.summary()
            );
        }
    }

    #[test]
    fn a_version_prefers_what_makepkg_evaluated() {
        // The VCS case: the PKGBUILD holds a function, .SRCINFO holds the
        // version makepkg computed from it.
        let srcinfo = "pkgbase = x-git\n\tpkgver = 1.2.3.r4.gabcdef\n\tpkgrel = 2\n";
        let pkgbuild = "pkgver=0.0.0\npkgrel=1\n";
        let v = Version::read(srcinfo, pkgbuild);
        assert_eq!(v.full(), "1.2.3.r4.gabcdef-2");
        // No .SRCINFO: the text is the fallback.
        assert_eq!(Version::read("", pkgbuild).full(), "0.0.0-1");
        // Neither: named rather than crashed on.
        assert_eq!(Version::read("", "").full(), "0-1");
    }

    /// The epoch is carried, rendered pacman's way, and `epoch=0` is written
    /// as no epoch at all — which is what it means.
    #[test]
    fn an_epoch_is_part_of_the_version_and_of_the_release() {
        let v = Version::read("", "epoch=2\npkgver=1.0\npkgrel=3\n");
        assert_eq!(v.full(), "2:1.0-3");
        assert_eq!(v.release(), (Some("2"), "1.0"));
        assert_eq!(Version::read("", "epoch=0\npkgver=1.0\n").full(), "1.0-1");
        // .SRCINFO wins here too.
        let v = Version::read(
            "pkgbase = x\n\tepoch = 5\n\tpkgver = 9\n",
            "epoch=1\npkgver=9\n",
        );
        assert_eq!(v.full(), "5:9-1");
    }

    /// An epoch bump means upstream's numbering restarted: the tarball behind
    /// `1.0` after it is a different artifact than the one behind `1.0`
    /// before, so comparing their checksums would alarm on what is really a
    /// renumbering.
    #[test]
    fn an_epoch_bump_is_a_different_release_and_does_not_alarm() {
        let prev = "epoch=1\npkgver=1.0\npkgrel=1\nsource=('x.tar.gz')\nsha256sums=('aaaa')\n";
        let published = Published {
            pkgbuild: Some(prev.to_string()),
            version: Some(Version::read("", prev)),
        };
        let recut = "epoch=2\npkgver=1.0\npkgrel=1\nsource=('x.tar.gz')\nsha256sums=('bbbb')\n";
        assert!(alarm(&published, &Version::read("", recut), recut).is_ok());
        // Same epoch, same pkgver, changed sum: still the alarm.
        let same = "epoch=1\npkgver=1.0\npkgrel=9\nsource=('x.tar.gz')\nsha256sums=('bbbb')\n";
        assert!(alarm(&published, &Version::read("", same), same).is_err());
    }

    /// A version reaches pacrat's own report and a commit message. It is read
    /// out of files a sync could have mangled.
    #[test]
    fn a_version_cannot_forge_a_line_or_sprawl() {
        let v = Version::read("", "pkgver=1.0\x1b[2K\npublished nothing\npkgrel=1\n");
        assert!(!v.full().contains('\x1b'), "{}", v.full());
        assert_eq!(v.full().lines().count(), 1);
    }

    #[test]
    fn srcinfo_fields_are_read_by_name() {
        let text = "pkgbase = mdcat\n\tpkgdesc = cat for markdown\n\tpkgver = 2.11.0\n";
        assert_eq!(srcinfo_field(text, "pkgver").as_deref(), Some("2.11.0"));
        assert_eq!(srcinfo_field(text, "pkgbase").as_deref(), Some("mdcat"));
        assert_eq!(srcinfo_field(text, "pkgrel"), None);
    }

    /// The alarm is about a tag whose bytes changed, so it fires at the same
    /// pkgver whatever the pkgrel is doing — and stays quiet across versions.
    #[test]
    fn the_alarm_fires_on_a_rewritten_tarball_at_the_same_pkgver() {
        let prev = "pkgver=1.0\npkgrel=1\nsource=('x-1.0.tar.gz')\nsha256sums=('aaaa')\n";
        let published = Published {
            pkgbuild: Some(prev.to_string()),
            version: Some(Version::read("", prev)),
        };

        let rewritten = "pkgver=1.0\npkgrel=2\nsource=('x-1.0.tar.gz')\nsha256sums=('bbbb')\n";
        let err = alarm(&published, &Version::read("", rewritten), rewritten).unwrap_err();
        assert!(
            matches!(err, Failure::Alarm(_)),
            "the alarm must stop a drain"
        );
        let err = err.message().to_string();
        assert!(err.contains("incident"), "{err}");
        assert!(err.contains("1.0-2"), "{err}");

        // The same file again: nothing to alarm about.
        assert!(alarm(&published, &Version::read("", prev), prev).is_ok());

        // A new version with a new tarball is a release.
        let next = "pkgver=1.1\npkgrel=1\nsource=('x-1.1.tar.gz')\nsha256sums=('cccc')\n";
        assert!(alarm(&published, &Version::read("", next), next).is_ok());

        // A first publish has nothing to contradict.
        let first = Published {
            pkgbuild: None,
            version: None,
        };
        assert!(alarm(&first, &Version::read("", rewritten), rewritten).is_ok());
    }

    /// A blocked probe is quoted, never paraphrased — and a server that
    /// answers in escape codes does not get to paint the report or the queue.
    #[test]
    fn what_a_server_said_is_one_safe_line() {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "printf 'The AUR is down\\x1b[2K due to maintenance.\\n'; exit 1",
        ]);
        let ran = proc::run_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        let answer = said(&ran);
        assert!(!answer.contains('\x1b'), "{answer}");
        assert_eq!(answer.lines().count(), 1);
        assert!(answer.contains("The AUR is down"), "{answer}");

        // stderr when stdout is silent.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo 'Permission denied (publickey).' >&2; exit 255"]);
        let ran = proc::run_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        assert_eq!(said(&ran), "Permission denied (publickey).");

        // Silence still says something.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 7"]);
        let ran = proc::run_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        assert!(said(&ran).starts_with("no output"), "{}", said(&ran));
    }
}
