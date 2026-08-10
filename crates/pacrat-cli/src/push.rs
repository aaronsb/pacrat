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
        if !probe.open {
            return block(package, &curated, &probe.answer).map(|()| Outcome::Held);
        }
    }

    let outcome = publish(package, &curated, &remote, yes)?;
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

/// What the AUR said when asked whether it is accepting anything.
struct AurProbe {
    open: bool,
    answer: String,
}

impl AurProbe {
    fn report(&self) {
        if self.open {
            println!("probe     write access ok — {}", self.answer);
        } else {
            println!("probe     blocked — {}", self.answer);
        }
    }
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
            open: false,
            answer: format!("ssh could not be run: {e}"),
        },
        Ok(ran) if ran.timed_out => AurProbe {
            open: false,
            answer: format!("no answer within {}s", PROBE_TIMEOUT.as_secs()),
        },
        Ok(ran) => AurProbe {
            open: ran.status.success(),
            answer: said(&ran),
        },
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
fn block(package: &str, curated: &review::Curated, answer: &str) -> Result<(), String> {
    let digest = fstree::digest(&curated.tree)?;
    let mut queue = load_queue()?;
    queue.block(package, &curated.entry.reviewed, &digest, now(), answer);
    save_queue(&queue)?;

    println!();
    println!(
        "queued    {package} @ {}",
        short_hash(&curated.entry.reviewed)
    );
    println!("answer    {}", visible_line(answer).0);
    println!("queue     {}", queue_path()?.display());
    println!();
    println!(
        "not published — the remote is not accepting publishes. The errand is on \
         file; `pacrat push` (no arguments) probes again and works the queue"
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
        if !probe.open {
            queue.probed(now(), &probe.answer);
            save_queue(&queue)?;
            show_queue(&queue);
            println!();
            println!(
                "still blocked — {} publish{} waiting",
                queue.len(),
                if queue.len() == 1 { "" } else { "es" }
            );
            std::process::exit(HELD);
        }
    }

    let packages: Vec<String> = queue.pushes.keys().cloned().collect();
    for package in &packages {
        println!();
        println!("── {package} ──");
        match eligible(ctx, package)? {
            Eligibility::Gone(why) => {
                let mut queue = load_queue()?;
                queue.pushes.remove(package);
                save_queue(&queue)?;
                println!("dropped   {why}");
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
                // Not published: what was queued is not what is in the store
                // now, and "publish the newer thing instead" is a decision
                // nobody made. Re-queued against the new bytes so the next
                // run — or an explicit `pacrat push <package>` — sees them.
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
    }

    let left = load_queue()?;
    if left.is_empty() {
        println!();
        println!("publish queue empty");
        return Ok(());
    }
    show_queue(&left);
    println!();
    println!(
        "{} publish{} still queued",
        left.len(),
        if left.len() == 1 { "" } else { "es" }
    );
    std::process::exit(HELD);
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
    Gone(String),
}

/// Is this queued errand still the errand it was?
///
/// The digest is the question, not the commit. A queue entry says "publish
/// *these bytes*", and between queueing and draining the store can be synced,
/// edited, or advanced through the review gate — all of which change what
/// would go out while the ledger may say nothing at all. ADR-001 settled this
/// for gradings ("a grading is about bytes, not about a commit") and it is the
/// same question here, asked days later instead of seconds.
fn eligible(ctx: &Ctx, package: &str) -> Result<Eligibility, String> {
    let queue = load_queue()?;
    let Some(entry) = queue.pushes.get(package).cloned() else {
        return Ok(Eligibility::Gone("no longer in the queue".into()));
    };
    let curated = match review::curated(ctx, package) {
        Ok(c) => c,
        Err(e) => return Ok(Eligibility::Gone(e)),
    };
    if curated.entry.role != Role::Maintained {
        return Ok(Eligibility::Gone(format!(
            "{package} is {} in the ledger now — push is for maintained packages",
            role_word(curated.entry.role)
        )));
    }

    let digest = fstree::digest(&curated.tree)?;
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
) -> Result<Outcome, String> {
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
) -> Result<Outcome, String> {
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
        return Err(format!(
            "the store tree for {package} has no PKGBUILD — there is nothing to publish"
        ));
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

/// A package version as it will be published.
#[derive(Debug, PartialEq, Eq)]
struct Version {
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
        let pick = |key: &str, default: &str| {
            srcinfo_field(srcinfo, key)
                .or_else(|| pkgbuild::field(pkgbuild, key))
                .map(|v| truncate(&visible_line(&v).0, 60))
                .unwrap_or_else(|| default.to_string())
        };
        Self {
            pkgver: pick("pkgver", "0"),
            pkgrel: pick("pkgrel", "1"),
        }
    }

    fn full(&self) -> String {
        format!("{}-{}", self.pkgver, self.pkgrel)
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
/// Only asked of two files at the same `pkgver`, which is the version an
/// upstream tag names. A `pkgrel` bump at the same `pkgver` is ordinary
/// maintenance — a patch, a dependency fix — and must still not come with a
/// different tarball, so the alarm deliberately ignores `pkgrel`.
fn alarm(published: &Published, version: &Version, store_pkgbuild: &str) -> Result<(), String> {
    let (Some(prev), Some(prev_pkgbuild)) = (&published.version, &published.pkgbuild) else {
        return Ok(());
    };
    if prev.pkgver != version.pkgver {
        return Ok(());
    }

    let changes = pkgbuild::changed_sums(prev_pkgbuild, store_pkgbuild);
    if changes.is_empty() {
        if prev.pkgrel == version.pkgrel {
            // Not an alarm, and not nothing: pacman compares versions, so a
            // changed tree at an unchanged version reaches nobody's machine.
            println!(
                "warning   republishing {} with neither pkgver nor pkgrel bumped — \
                 pacman will not see this as an update, so no host will pick it up",
                version.full()
            );
        }
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
    Err(format!(
        "not published — {} is already published and the bytes behind one of its \
         sources have changed. An immutable tag whose tarball changed is an incident, \
         never a silent re-sum: someone or something rewrote it. Investigate before \
         republishing — if the change is legitimate (a re-cut release), publish it \
         under a new pkgver",
        version.full()
    ))
}

/// Stage everything and report what moved, as file names.
fn stage_all(clone: &Path) -> Result<Vec<String>, String> {
    Git::new([
        OsStr::new("-C"),
        clone.as_os_str(),
        OsStr::new("add"),
        OsStr::new("--all"),
        OsStr::new("--"),
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
