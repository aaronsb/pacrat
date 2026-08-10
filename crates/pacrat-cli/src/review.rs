//! `pacrat review`, `adopt-update` and `reject` — the human half of
//! ADR-001's update loop. Detection (`updates`) says a curated package's
//! upstream has moved; grading (`grade`) says what the tools think of the
//! bytes; these three verbs are where a person looks and answers.
//!
//! The answer has three shapes and each is a verb: **review** shows the
//! candidate and decides nothing, **adopt-update** takes it into the store,
//! **reject** records that it was refused so the drift report stops asking.
//!
//! Five rules shape the module:
//!
//! 1. **The candidate is fetched, never remembered.** Every verb clones
//!    upstream itself. A scratch tree left by an earlier `review` is a
//!    statement about what upstream said then, and adopting from it would
//!    install bytes nobody re-checked. `--commit` is how a human says "the
//!    candidate I read", and a HEAD that has moved past it is refused
//!    rather than substituted.
//!
//! 2. **Everything shown is attacker text.** The diff is a rendering of a
//!    tree written by whoever controls the upstream repository, and it is
//!    being read by the one person who can still say no. Every byte of it
//!    goes through [`crate::out::visible`], file names included, so that a
//!    `curl … | sh` cannot hide behind an escape code in the very view
//!    meant to reveal it.
//!
//! 3. **BLOCK holds unless a human writes down why it should not.** ADR-001
//!    makes "BLOCK always holds" an invariant no gate *preset* relaxes, and
//!    its decision 2 settles the one door through it: `--override-block
//!    --reason "…"`, whose friction is authoring the justification and whose
//!    price is a permanent entry in the store's decision ledger. It is a
//!    door in this verb and nowhere else — `pacrat update` has no override,
//!    because a loop that can talk itself past a BLOCK is not a gate.
//!
//! 4. **Ungraded is not BLOCK.** It holds in the *automatic* loop, where
//!    nobody is reading; a human at a terminal who has just been shown the
//!    diff is the thing ungraded was holding out for. So adopt says the
//!    grading state plainly and still asks.
//!
//! 5. **The tree lands before the ledger.** If only one of the two writes
//!    survives, the store should hold bytes a human just approved while the
//!    ledger under-claims what was reviewed — never the reverse, which
//!    would have the ledger vouch for a review of bytes that are not there.
//!    Same ordering, same reason, as `vendor`.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use pacrat_core::decisions::REASON_MAX;
use pacrat_core::grading::commit_matches;
use pacrat_core::pkg::valid_name;
use pacrat_core::sources::{valid_commit, SourceEntry, NOTE_MAX};
use pacrat_core::Verdict;

use crate::ctx::Ctx;
use crate::decisions;
use crate::fstree;
use crate::git::{self, Git};
use crate::grade;
use crate::out::{list_preview, short_hash, truncate, visible, visible_line};
use crate::proc;
use crate::say;
use crate::updates;
use crate::vendor;
use crate::HELD;

/// Diff lines printed before the rest is left to the reader's own tools. A
/// review that scrolls a terminal's whole scrollback away has hidden the
/// beginning of the diff as effectively as an escape code would.
const DIFF_LINES: usize = 1000;

/// The two staged trees inside the scratch directory. They are named rather
/// than pathed because these words appear in every line of the diff — `git`
/// prints the paths it was given, and `a/reviewed/PKGBUILD` beside
/// `b/candidate/PKGBUILD` is the whole legend the reader needs.
const REVIEWED: &str = "reviewed";
const CANDIDATE: &str = "candidate";

// ------------------------------------------------------------------ verbs

/// Show one pending update: what changed, and what the graders said.
///
/// A viewer, and only a viewer. It runs no grader, writes nothing, and exits
/// 0 whatever the verdict — including BLOCK. The hold belongs to the verb
/// that would act (`adopt-update`, `build`); a reader who asked to be shown
/// something and was shown it has not been declined anything.
pub fn run(ctx: &Ctx, package: &str) -> Result<(), String> {
    let curated = curated(ctx, package)?;
    preamble(package, &curated);
    let cand = fetch(package, &curated.entry, &curated.tree)?;

    let outcome = show(ctx, package, &curated, &cand);
    // A failure leaves the trees as evidence, and so does a diff too long to
    // print: the reader was told to go look at them.
    finish(&cand, matches!(outcome, Ok(false)));
    outcome.map(|_| ())
}

/// Returns whether the scratch trees are worth keeping — true when the diff
/// was too long to print, since the reader now needs the trees themselves.
fn show(ctx: &Ctx, package: &str, curated: &Curated, cand: &Candidate) -> Result<bool, String> {
    header(curated, cand);
    let changes = changes_between(curated, cand)?;
    changes.render();

    say!();
    let keep = if cand.digest == fstree::digest(&curated.tree)? {
        // A moved HEAD whose tree is byte-for-byte the store's: a commit
        // that touched only history. Worth saying outright, because an empty
        // diff otherwise reads as "the diff failed".
        say!(
            "diff      none — the candidate's tree is byte-identical to the store's \
             (the commit moved, the contents did not)"
        );
        false
    } else {
        render_diff(&diff(&cand.scratch)?, &changes)
    };

    show_gradings(ctx, package, cand)?;
    next_steps(package, cand);
    Ok(keep)
}

/// How this adoption was asked for.
pub struct Adopt<'a> {
    /// The candidate the human says they read. Upstream having moved past it
    /// is a refusal, never a substitution.
    pub commit: Option<&'a str>,
    /// Skip the prompt (scripting).
    pub yes: bool,
    /// Adopt past a BLOCK, recording why in the store's decision ledger.
    /// ADR-001 decision 2: the friction is writing this sentence.
    pub override_block: Option<&'a str>,
}

/// Take the candidate into the store: install its tree, advance the ledger.
pub fn adopt(ctx: &Ctx, package: &str, opts: &Adopt) -> Result<(), String> {
    let want = match opts.commit {
        None => None,
        Some(c) if valid_commit(c) => Some(c),
        Some(c) => {
            return Err(format!(
                "--commit {c:?} is not a commit hash — pass the candidate `pacrat review` \
                 printed, 7-64 hex characters as `git rev-parse` prints them"
            ))
        }
    };
    if let Some(reason) = opts.override_block {
        check_override(reason, want)?;
    }

    let curated = curated(ctx, package)?;
    preamble(package, &curated);
    let cand = fetch(package, &curated.entry, &curated.tree)?;

    let outcome = execute_adopt(ctx, package, &curated, &cand, want, opts);
    finish(&cand, outcome.is_ok());
    match outcome? {
        Adoption::Adopted => Ok(()),
        // Every hold has already said why, in its own words, above.
        Adoption::Held => std::process::exit(HELD),
    }
}

enum Adoption {
    /// The store now holds the candidate — or already held it, which is the
    /// same outcome for anyone who asked for it to.
    Adopted,
    /// Deliberately did not act. Every hold says why in its own words first;
    /// the caller only turns that into exit 10.
    Held,
}

/// Refuse an override that is not one, before the network is touched.
///
/// Two requirements, both from ADR-001 decision 2, and both about what the
/// entry will have to mean to somebody reading it in six months:
///
/// * **A reason**, because the friction *is* the writing. An override with
///   no justification is a flag, and a flag is exactly the frictionless
///   thing the decision was written to avoid.
/// * **A `--commit`**, because an override says "I read this and I accept
///   it". Without one, the verb adopts whatever HEAD happens to be by the
///   time the clone finishes, and the ledger would record a human vouching
///   for bytes chosen after they stopped looking.
fn check_override(reason: &str, want: Option<&str>) -> Result<(), String> {
    if reason.trim().is_empty() {
        return Err("--reason is empty — overriding a BLOCK takes a justification".into());
    }
    let length = reason.chars().count();
    if length > REASON_MAX {
        return Err(format!(
            "--reason is {length} characters — the cap is {REASON_MAX}. The decision \
             ledger is synced to every host; put the long version in the commit message"
        ));
    }
    if want.is_none() {
        return Err(
            "--override-block needs --commit: the record says which candidate was \
             accepted, and adopting whatever HEAD is by the time the clone finishes \
             would put a human's name on bytes they never saw. `pacrat review \
             <package>` prints the commit to pass"
                .into(),
        );
    }
    Ok(())
}

fn execute_adopt(
    ctx: &Ctx,
    package: &str,
    curated: &Curated,
    cand: &Candidate,
    want: Option<&str>,
    opts: &Adopt,
) -> Result<Adoption, String> {
    header(curated, cand);

    // `--commit` is answered first, before any question about drift.
    //
    // It has to be: the human named a commit, and every answer other than
    // "that one is what HEAD is" is a refusal. Asked in the other order, an
    // upstream that *reverted* to the reviewed commit turns `--commit X`
    // into "nothing to adopt" and exit 0 — a script reading that zero
    // believes X landed, and X is the one thing that did not.
    if let Some(want) = want {
        if !commit_matches(want, &cand.commit) {
            // A hold rather than a failure: nothing is broken, there is
            // simply a commit at the other end that nobody has read — which
            // is exactly the state exit 10 exists to name.
            say!();
            say!(
                "not adopted — you asked for {} and upstream HEAD is {}. Review what \
                 is actually there (`pacrat review {package}`)",
                short_hash(want),
                short_hash(&cand.commit)
            );
            return Ok(Adoption::Held);
        }
    }

    if !updates::drifted(&curated.entry.reviewed, &cand.commit) {
        say!();
        say!(
            "nothing to adopt — upstream HEAD is the commit the ledger already \
             records as reviewed"
        );
        return Ok(Adoption::Adopted);
    }

    // What adopting would do to the store tree. A prompt that asks "adopt
    // this?" while showing nothing about it is a prompt that trains people
    // to type y — `vendor` sets the bar by dumping the PKGBUILD and every
    // `.install` before it asks, and the least this can do is name what
    // moved. The full diff stays `review`'s job, and the line below says so.
    changes_between(curated, cand)?.render();
    say!();
    say!("diff      pacrat review {package} — the line-by-line, before you answer");

    let (verdict, grade) = show_gradings(ctx, package, cand)?;
    say!();
    // The one door past a BLOCK, and it is loud on the way through: the
    // record is written below, after the prompt and before the tree lands.
    let overriding = match (verdict, opts.override_block) {
        (Verdict::Block, None) => {
            say!(
                "not adopted — BLOCK holds. The ways past it: change what the graders \
                 see, record your own reading (`pacrat grade {package} --grade N \
                 --note …`), or accept the risk on the record with `pacrat \
                 adopt-update {package} --commit {} --override-block --reason \"…\"`",
                short_hash(&cand.commit)
            );
            return Ok(Adoption::Held);
        }
        (Verdict::Block, Some(reason)) => {
            say!(
                "OVERRIDE  adopting past a {} verdict, by hand",
                Verdict::Block
            );
            say!("reason    {}", truncate(&visible_line(reason).0, 200));
            // Future tense, deliberately: nothing is written until the
            // prompt is answered, and a line that said "recorded" before a
            // human declined would be pacrat lying about its own ledger.
            say!(
                "ledger    going through with this records it in {} — permanently, \
                 and synced to every host",
                store_rel(ctx, &ctx.decisions_path())
            );
            say!();
            Some(reason)
        }
        (_, Some(_)) => {
            // Refused, not carried on with. Someone who reaches for the
            // override believes they are looking at a BLOCK, and they are
            // not: the verdict may be UNGRADED because a grader crashed, or
            // WARN, or the gradings may be of some other bytes entirely.
            // Adopting anyway would be acting on a model of the situation
            // that has just been shown to be wrong — which is the failure
            // this whole tool exists to prevent — and it would do it while
            // the human believed a record was being written. The plain verb
            // is right there.
            return Err(format!(
                "nothing to override — the verdict is {verdict}, not {}. Plain \
                 `pacrat adopt-update {package} --commit {}` handles it, and \
                 nothing was recorded in the decision ledger",
                Verdict::Block,
                short_hash(&cand.commit)
            ));
        }
        _ => None,
    };
    if verdict == Verdict::Ungraded {
        // Ungraded holds the *automatic* loop. Here there is a human, and
        // they have just been shown the diff.
        say!(
            "note      nothing has graded this candidate — ungraded holds the headless \
             loop, and is not a BLOCK; `pacrat review {package}` is where the diff is"
        );
    }

    // A refusal is a decision that was already made once. Re-adopting the
    // same commit has to be deliberate, so the interactive prompt is not
    // enough — that is the answer to a question the human may not remember
    // having answered before.
    if updates::refused(&curated.entry, &cand.commit) {
        let note = curated
            .entry
            .rejected_note
            .as_deref()
            .map_or("(no reason recorded)".to_string(), |n| {
                truncate(&visible_line(n).0, 120)
            });
        say!("rejected  {} was refused: {note}", short_hash(&cand.commit));
        if !opts.yes {
            say!("not adopted — re-adopting a candidate that was rejected takes --yes");
            return Ok(Adoption::Held);
        }
        say!("proceeding anyway on --yes");
    }

    if !opts.yes && !vendor::confirm("adopt", package, short_hash(&cand.commit))? {
        say!("not adopted");
        return Ok(Adoption::Held);
    }

    // The record before the act, and after the answer. Before, because if
    // only one of the two survives it must be the one that says a human
    // accepted a named risk — an adoption with no record is the audit trail
    // ADR-001 decision 2 exists to keep. After the prompt, because a
    // declined prompt is not a decision, and a ledger that filled up with
    // overrides nobody went through with would be a ledger nobody believes.
    if let Some(reason) = overriding {
        decisions::record_override(ctx, package, &cand.commit, grade, reason)?;
        say!(
            "recorded  {} · {package} @ {} · override-block",
            store_rel(ctx, &ctx.decisions_path()),
            short_hash(&cand.commit)
        );
    }

    // A failure from here on has to say what the ledger now claims. The
    // record is permanent and the adoption did not happen, and a reader who
    // is told only "install failed" will not think to go and look at a file
    // that now says they accepted a risk they never took.
    install(ctx, package, curated, cand).map_err(|e| match overriding {
        None => e,
        Some(_) => format!(
            "{e}\n       the decision is on the record; the adoption is not — \
             {} holds an override-block entry for {package} @ {}, and the store \
             was not changed",
            store_rel(ctx, &ctx.decisions_path()),
            short_hash(&cand.commit)
        ),
    })?;
    if overriding.is_some() {
        // Last line but one, where the eye lands: an adoption that went past
        // a BLOCK must not read like an ordinary one.
        say!();
        say!(
            "override  this adoption went past a {} verdict — `pacrat decisions` \
             lists it",
            Verdict::Block
        );
    }
    Ok(Adoption::Adopted)
}

/// Write the candidate into the store and advance the ledger.
pub fn install(
    ctx: &Ctx,
    package: &str,
    curated: &Curated,
    cand: &Candidate,
) -> Result<(), String> {
    fstree::install(&cand.tree, &cand.files, &curated.tree)
        .map_err(|e| format!("{e}\n       the store's {package} is unchanged"))?;
    say!(
        "adopted   {} file{} → {}",
        cand.files.len(),
        if cand.files.len() == 1 { "" } else { "s" },
        store_rel(ctx, &curated.tree)
    );

    // Re-read: the clone and the prompt took unbounded time and the ledger
    // is a whole-file write, so anything another process recorded meanwhile
    // has to survive ours.
    let mut sources = ctx.load_sources()?;
    let stale = format!(
        "{package} left the ledger while it was being reviewed — the tree at {} is now \
         the candidate; `pacrat vendor {package} --force` writes the entry back",
        store_rel(ctx, &curated.tree)
    );
    let entry = sources
        .packages
        .get_mut(package)
        .ok_or_else(|| stale.clone())?;
    entry.reviewed = cand.commit.clone();
    // Any adoption clears the row's refusal. Not because the new commit is
    // known to descend from the refused one — nothing here checks ancestry,
    // and upstream can move sideways — but because the refusal was recorded
    // to stop `updates` re-raising *that* candidate, and a human has now
    // answered a later question about this package with their eyes open. A
    // refusal that outlived the review it came from would be a permanent
    // silence nobody remembers switching on.
    entry.rejected = None;
    entry.rejected_note = None;

    ctx.save_sources(&sources).map_err(|e| {
        format!(
            "{e}\n       the tree at {} is the candidate but the ledger still says \
             {} — re-run `pacrat adopt-update {package} --yes` to finish",
            store_rel(ctx, &curated.tree),
            short_hash(&curated.entry.reviewed)
        )
    })?;
    say!(
        "ledger    {} · {package} @ {}",
        store_rel(ctx, &ctx.sources_path()),
        short_hash(&cand.commit)
    );

    say!();
    say!("next      commit the store, then `pacrat build {package}`");
    Ok(())
}

/// Record that a candidate was refused.
///
/// Cheaper than the other two verbs on purpose: a refusal is about a commit,
/// not about a tree, so this asks the remote for HEAD and never clones. The
/// human has already seen the tree — that is what `review` was for.
pub fn reject(ctx: &Ctx, package: &str, note: Option<&str>) -> Result<(), String> {
    let curated = curated(ctx, package)?;
    if let Some(note) = note {
        if note.trim().is_empty() {
            return Err("--note is empty".into());
        }
        // Refused here rather than at the next parse: writing it and then
        // discovering the ledger no longer loads would break the file for
        // every host, over a note.
        let length = note.chars().count();
        if length > NOTE_MAX {
            return Err(format!(
                "--note is {length} characters — the cap is {NOTE_MAX}. The ledger is \
                 synced to every host; put the long version in the commit message"
            ));
        }
    }

    preamble(package, &curated);
    let candidate = updates::ls_remote(&curated.entry.upstream)?;
    // An error (exit 1), where the same shape of nothing-to-do is exit 0 in
    // `adopt-update`. The asymmetry is real, not an oversight: adopt is
    // convergent — asked to make the store hold HEAD, and it already does,
    // so the caller got what it wanted. Reject is an instruction about a
    // specific thing, and that thing does not exist. Nothing was recorded,
    // and a zero here would tell a script a refusal is on file when none is.
    if !updates::drifted(&curated.entry.reviewed, &candidate) {
        return Err(format!(
            "nothing to reject — upstream HEAD is {}, the commit the ledger already \
             records as reviewed",
            short_hash(&candidate)
        ));
    }

    // `ls-remote` answers with an object id or nothing, and this is checked
    // anyway: the value lands in a file every host in the fleet parses, and
    // one bad string there is a ledger nobody can read.
    if !valid_commit(&candidate) {
        return Err(format!(
            "upstream answered {candidate:?} for HEAD, which is not an object id —              refusing to write it to the ledger"
        ));
    }
    let mut sources = ctx.load_sources()?;
    let entry = sources
        .packages
        .get_mut(package)
        .ok_or_else(|| format!("{package} left the ledger while its upstream was being asked"))?;
    entry.rejected = Some(candidate.clone());
    entry.rejected_note = note.map(str::to_string);
    ctx.save_sources(&sources)?;

    say!("rejected  {}", short_hash(&candidate));
    if let Some(note) = note {
        say!("note      {}", truncate(&visible_line(note).0, 120));
    }
    say!("ledger    {}", store_rel(ctx, &ctx.sources_path()));
    say!();
    say!(
        "next      commit the store. `pacrat updates` lists this as rejected rather \
         than pending until upstream moves past {}",
        short_hash(&candidate)
    );
    Ok(())
}

// ------------------------------------------------------------ the package

/// A package these verbs can work on: in the ledger, with a tree.
///
/// Shared with `push`, which needs exactly the same four-state answer — and
/// must give it in the same words, since "not vendored" and "tree but no
/// ledger entry" are the same two problems whichever verb ran into them.
pub struct Curated {
    pub entry: SourceEntry,
    pub tree: PathBuf,
}

/// Resolve the package, or say which of the four states it is in instead.
///
/// The three broken ones borrow `vendor`'s words. They are the same states
/// seen from the other side — vendoring refuses the one thing reviewing
/// requires — and describing them twice, differently, would leave a reader
/// wondering whether they are the same problem.
pub fn curated(ctx: &Ctx, package: &str) -> Result<Curated, String> {
    if !valid_name(package) {
        return Err(format!(
            "{package:?} is not a package name — expected letters, digits, \
             and @._+- (no leading hyphen or dot)"
        ));
    }
    let sources = ctx.load_sources()?;
    let tree = ctx.store.join("aur").join("packages").join(package);
    let entry = sources.packages.get(package).cloned();

    match (entry, tree.exists()) {
        (Some(entry), true) => Ok(Curated { entry, tree }),
        (entry, on_disk) => Err(match vendor::held(entry.is_some(), on_disk) {
            Some(broken) => broken.explain(package, &tree),
            None => format!(
                "{package} is not vendored — there is no reviewed commit to compare \
                 against (`pacrat vendor {package}` first)"
            ),
        }),
    }
}

// ---------------------------------------------------------- the candidate

/// The upstream tree these verbs are deciding about, staged beside a copy of
/// the store's.
pub struct Candidate {
    /// Scratch root, holding `clone/`, `reviewed/` and `candidate/`.
    pub scratch: PathBuf,
    /// The staged candidate tree: validated, and without the clone's `.git`.
    pub tree: PathBuf,
    /// Its files, as [`fstree::files`] lists them — the exact set that would
    /// be installed.
    pub files: Vec<String>,
    pub commit: String,
    /// The candidate tree's content digest: what a grading of this candidate
    /// is a grading *of*.
    pub digest: String,
}

/// Clone the upstream and stage both trees for comparison.
///
/// Both sides go through [`fstree`], which is what makes the comparison
/// honest: the candidate is validated by the same walk that would install
/// it, `.git` never reaches the diff, and the digest computed here is the
/// digest the store tree will have if this candidate is adopted — so a
/// grading of the candidate stays valid across the adoption.
pub fn fetch(package: &str, entry: &SourceEntry, store_tree: &Path) -> Result<Candidate, String> {
    let scratch = vendor::scratch_dir("review", package)?;
    match stage(&scratch, entry, store_tree) {
        Ok(candidate) => Ok(candidate),
        // Whatever was fetched before it went wrong is evidence about an
        // upstream that just misbehaved, so it stays and is named.
        Err(e) if scratch.exists() => Err(format!("{e}\n       kept at {}", scratch.display())),
        Err(e) => Err(e),
    }
}

fn stage(scratch: &Path, entry: &SourceEntry, store_tree: &Path) -> Result<Candidate, String> {
    let clone = scratch.join("clone");
    clone_upstream(&entry.upstream, &clone)?;

    let files = fstree::files(&clone)?;
    if !files.iter().any(|f| f == "PKGBUILD") {
        return Err(format!(
            "{} has no PKGBUILD at HEAD — that is not a package repository, and it \
             is what the ledger says this package's upstream is",
            safe(&entry.upstream)
        ));
    }
    let commit = Git::new([
        OsStr::new("-C"),
        clone.as_os_str(),
        OsStr::new("rev-parse"),
        OsStr::new("HEAD"),
    ])
    .text()?;
    if !valid_commit(&commit) {
        return Err(format!(
            "git rev-parse HEAD answered {commit:?}, which is not an object id"
        ));
    }

    let tree = scratch.join(CANDIDATE);
    fstree::install(&clone, &files, &tree)?;
    // The store's own tree, staged beside it so the diff has two paths in
    // one directory to name — and so the reader can keep both afterwards.
    let store_files = fstree::files(store_tree)?;
    fstree::install(store_tree, &store_files, &scratch.join(REVIEWED))?;

    let digest = fstree::digest(&tree)?;
    Ok(Candidate {
        scratch: scratch.to_path_buf(),
        tree,
        files,
        commit,
        digest,
    })
}

/// `git clone` the upstream into scratch.
///
/// Bounded and guarded like every other git call pacrat makes; [`crate::git`]
/// holds the reasons, including why `GIT_SSH_COMMAND` is left alone.
fn clone_upstream(upstream: &str, dest: &Path) -> Result<(), String> {
    // `--` so that an upstream cannot be read as a git option.
    Git::new([
        OsStr::new("clone"),
        OsStr::new("--"),
        OsStr::new(git::url_arg(upstream)?),
        dest.as_os_str(),
    ])
    .timeout(git::TRANSFER)
    .text()
    .map(|_| ())
}

/// One untrusted field on pacrat's own report: the ledger's upstream URL.
fn safe(text: &str) -> String {
    visible_line(text).0
}

/// Scratch is ours either way; when it is not removed it is because someone
/// still needs it, so it is named rather than merely left behind.
pub fn finish(cand: &Candidate, remove: bool) {
    if remove {
        let _ = fs::remove_dir_all(&cand.scratch);
    } else {
        say!();
        say!(
            "trees     {} — `{REVIEWED}` and `{CANDIDATE}` are the two sides of the \
             diff, yours to read with anything you like",
            cand.scratch.display()
        );
    }
}

// -------------------------------------------------------------- the report

/// What is known before the network is touched — printed first, so the argv
/// lines that follow are read as "this is what it is doing about that".
pub fn preamble(package: &str, curated: &Curated) {
    say!("package   {package}");
    say!(
        "role      {}",
        crate::custody::label(Some(curated.entry.role.into()))
    );
    say!("upstream  {}", safe(&curated.entry.upstream));
}

/// What the fetch found: the two commits, their versions, and the bytes.
pub fn header(curated: &Curated, cand: &Candidate) {
    say!(
        "reviewed  {}{}",
        short_hash(&curated.entry.reviewed),
        version_of(&curated.tree)
    );
    say!(
        "candidate {}{}",
        short_hash(&cand.commit),
        version_of(&cand.tree)
    );
    say!("digest    {}", cand.digest);
    if !valid_commit(curated.entry.reviewed.trim()) {
        say!(
            "warning   the ledger's reviewed commit is not an object id — the diff \
             below is against the store tree, which is the truth about what is there"
        );
    }
}

/// ` pkgver 2.11.0`, when the PKGBUILD says so plainly.
///
/// A reading aid, not an evaluation: makepkg runs shell, so a computed
/// `pkgver=$(git describe …)` is shown as the text it is rather than
/// guessed at. Absent when there is no such line — a review is about the
/// diff, and a version is a convenience on top of it.
fn version_of(tree: &Path) -> String {
    match pkgver(tree) {
        Some(v) => format!("  pkgver {v}"),
        None => String::new(),
    }
}

fn pkgver(tree: &Path) -> Option<String> {
    let bytes = fs::read(tree.join("PKGBUILD")).ok()?;
    String::from_utf8_lossy(&bytes).lines().find_map(|line| {
        let value = line.trim_start().strip_prefix("pkgver=")?;
        let value = value.trim().trim_matches(['"', '\'']);
        (!value.is_empty()).then(|| truncate(&visible_line(value).0, 40))
    })
}

/// What the two trees do and do not share.
pub struct Changes {
    changed: Vec<String>,
    added: Vec<String>,
    removed: Vec<String>,
    unchanged: Vec<String>,
}

/// What adopting this candidate would do to the store tree.
///
/// Shared by `review` and `adopt-update` so the summary a human is shown
/// before the prompt is the same summary the viewer showed them.
pub fn changes_between(curated: &Curated, cand: &Candidate) -> Result<Changes, String> {
    let reviewed_files = fstree::files(&curated.tree)?;
    Ok(changes(&reviewed_files, &cand.files, |rel| {
        // Unreadable either side counts as changed: the diff will say what
        // it can, and calling a file we could not read "unchanged" is the
        // one answer that would be a lie.
        match (
            fs::read(curated.tree.join(rel)),
            fs::read(cand.tree.join(rel)),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }))
}

/// Compare two file lists. Pure: `same` answers the question about bytes, so
/// the set arithmetic can be tested without a filesystem.
fn changes(reviewed: &[String], candidate: &[String], same: impl Fn(&str) -> bool) -> Changes {
    let before: BTreeSet<&str> = reviewed.iter().map(String::as_str).collect();
    let after: BTreeSet<&str> = candidate.iter().map(String::as_str).collect();
    let mut c = Changes {
        changed: Vec::new(),
        added: after
            .difference(&before)
            .map(|s| (*s).to_string())
            .collect(),
        removed: before
            .difference(&after)
            .map(|s| (*s).to_string())
            .collect(),
        unchanged: Vec::new(),
    };
    for rel in before.intersection(&after) {
        if same(rel) {
            c.unchanged.push((*rel).to_string());
        } else {
            c.changed.push((*rel).to_string());
        }
    }
    c
}

impl Changes {
    /// The four sets, for a caller that renders rather than prints.
    ///
    /// The TUI needs exactly what [`Changes::render`] says and cannot use
    /// `say!` to say it. Accessors rather than public fields, so the only
    /// way to build a `Changes` is still [`changes_between`] — a screen
    /// assembling its own would be a second answer to "what would adopting
    /// this do", and the value of there being one answer is the whole reason
    /// this type is shared.
    pub fn changed(&self) -> &[String] {
        &self.changed
    }

    pub fn added(&self) -> &[String] {
        &self.added
    }

    pub fn removed(&self) -> &[String] {
        &self.removed
    }

    pub fn unchanged(&self) -> &[String] {
        &self.unchanged
    }

    /// `2 changed · 0 added · …`, the line under a diff.
    pub fn summary_line(&self) -> String {
        self.summary()
    }

    pub fn render(&self) {
        say!();
        for (label, names) in [
            ("changed", &self.changed),
            ("added", &self.added),
            ("removed", &self.removed),
        ] {
            if !names.is_empty() {
                say!("{label:<9} {}", names_of(names));
            }
        }
        if !self.unchanged.is_empty() {
            say!("unchanged {}", names_of(&self.unchanged));
        }

        // `*.install` runs as root at install time, which is the classic
        // place to hide a payload — so the reviewer is told these files
        // exist even in the common case where the diff is silent about them.
        let installs: Vec<String> = self
            .unchanged
            .iter()
            .filter(|f| f.ends_with(".install"))
            .cloned()
            .collect();
        if !installs.is_empty() {
            say!(
                "install   {} — unchanged, and still run as root at install time",
                names_of(&installs)
            );
        }
    }

    fn summary(&self) -> String {
        format!(
            "{} changed · {} added · {} removed · {} unchanged",
            self.changed.len(),
            self.added.len(),
            self.removed.len(),
            self.unchanged.len()
        )
    }
}

/// File names as a list. Neutered and folded one by one: a name is a field,
/// and a newline in one would otherwise write a line of its own.
fn names_of(names: &[String]) -> String {
    let safe: Vec<String> = names
        .iter()
        .map(|n| truncate(&visible_line(n).0, 60))
        .collect();
    list_preview(&safe, 12)
}

/// The diff itself, from git.
///
/// `--no-index` is what makes this possible at all: it compares two
/// directories that are not repositories, which is exactly what a staged
/// candidate beside a staged store tree is. Hand-rolling a differ here would
/// mean writing — and being trusted about — the one view a reviewer uses to
/// decide.
fn diff(scratch: &Path) -> Result<Diff, String> {
    let dir = scratch.to_string_lossy().into_owned();
    // The one view this module exists to make trustworthy must not be
    // rewritable by config this module never read — `git::NO_ATTRS` and
    // `git::NO_FILTERS` are that door, shut, and say why.
    let argv: Vec<&str> = git::NO_ATTRS
        .iter()
        .copied()
        .chain(["-C", &dir, "diff"])
        .chain(git::NO_FILTERS)
        .chain(["--no-index", "--", REVIEWED, CANDIDATE])
        .collect();
    let ran = Git::new(argv).timeout(git::DIFF).run()?;
    // The reader is bounded, so a tree with a gigabyte of "data" in it
    // cannot make pacrat hold a gigabyte. Output at the bound is output that
    // was probably cut, and saying so is the difference between a short diff
    // and a lie.
    let cut = ran.stdout.len() as u64 >= proc::PIPE_LIMIT;
    let text = || String::from_utf8_lossy(&ran.stdout).into_owned();

    match ran.status.code() {
        // 1 is "there are differences", which is the whole point of asking.
        Some(0 | 1) => Ok(Diff { text: text(), cut }),
        // No exit code means a signal, and the signal we cause ourselves is
        // SIGPIPE: capping the read closes the pipe under a git that is
        // still writing. That is a diff we cut off, not a diff that failed,
        // and reporting it as a failure is how a reviewer ends up with no
        // diff at all for the largest change in the set — the one most
        // worth reading.
        None if cut => Ok(Diff { text: text(), cut }),
        _ => Err(format!(
            "git diff --no-index failed: {}",
            diff_failure(&ran)
        )),
    }
}

/// Why the diff failed, in one line that is never empty.
///
/// git writes nothing to stderr when it dies of a signal, and an error whose
/// text is the empty string tells the reader only that something is wrong
/// with pacrat. The status is always something we can name.
fn diff_failure(ran: &proc::Ran) -> String {
    let (stderr, _) = visible_line(&String::from_utf8_lossy(&ran.stderr));
    let stderr = truncate(stderr.trim(), 200);
    if !stderr.is_empty() {
        return stderr;
    }
    match ran.status.code() {
        Some(code) => format!("exited {code} without saying why"),
        None => format!("{} — no output to go on", ran.status),
    }
}

/// A diff, and whether pacrat is holding all of it.
struct Diff {
    text: String,
    cut: bool,
}

/// The staged candidate's diff, neutered, for a caller that draws it.
///
/// The same `git diff --no-index` [`render_diff`] prints, put through the
/// same [`visible`] on the way out — which is not optional and not the
/// caller's to remember. This text is a rendering of a file whose author
/// would like to say things to the terminal of the person deciding whether
/// to trust them, and a screen that got the raw string would be one refactor
/// away from drawing it.
///
/// Returns the neutered text, how many characters were stood in for, and
/// whether the diff hit pacrat's pipe ceiling — a reader has to be able to
/// tell a short diff from a diff that was cut off.
pub fn diff_lines(cand: &Candidate) -> Result<(Vec<String>, usize, bool), String> {
    let diff = diff(&cand.scratch)?;
    let (safe, hidden) = visible(&diff.text);
    Ok((safe.lines().map(str::to_string).collect(), hidden, diff.cut))
}

/// Print the diff. Returns true when it was too long to print in full.
///
/// Every line goes through [`visible`], because this is a rendering of a
/// file whose author would like to say things to the terminal of the person
/// deciding whether to trust them. The count of what was neutered is itself
/// a finding, so it is reported rather than swallowed.
fn render_diff(diff: &Diff, changes: &Changes) -> bool {
    let (safe, hidden) = visible(&diff.text);
    let total = safe.lines().count();
    for line in safe.lines().take(DIFF_LINES) {
        say!("{line}");
    }
    // "end diff" cannot be forged from inside the diff: every content line
    // git prints is prefixed with '+', '-' or a space, and its own headers
    // all begin with other words.
    say!("end diff  {}", changes.summary());

    let over = total.saturating_sub(DIFF_LINES);
    if over > 0 {
        say!(
            "…         {over} further line{} not shown — a diff this long is itself \
             worth a second look",
            if over == 1 { "" } else { "s" }
        );
    }
    if diff.cut {
        say!(
            "warning   the diff hit pacrat's {} MB ceiling and is not all of it — \
             read the trees themselves before deciding",
            proc::PIPE_LIMIT / (1024 * 1024)
        );
    }
    if hidden > 0 {
        say!(
            "warning   {hidden} control character{} in the diff shown as ␛-style \
             stand-ins — text that tries to hide from a reviewer",
            if hidden == 1 { "" } else { "s" }
        );
    }
    over > 0 || diff.cut
}

/// The gradings on file for this candidate's bytes, the verdict they come
/// to, and the grade behind it — which a recorded override has to name.
///
/// Read-only, deliberately: `review` does not grade, and neither does
/// `adopt-update`. Running an LLM because someone asked to look at a diff
/// would be a surprise with a bill attached, and a grading made *during* a
/// decision is one the decider never had the chance to weigh. The verb that
/// does grade a candidate is `update`, where grading is a step of its own
/// with its own gate.
fn show_gradings(
    ctx: &Ctx,
    package: &str,
    cand: &Candidate,
) -> Result<(Verdict, Option<u8>), String> {
    let gradings = grade::cached(ctx, package, &cand.commit, &cand.digest)?;

    say!();
    if gradings.is_empty() {
        say!("gradings  none on file for these bytes");
        say!(
            "reason    `pacrat grade` reads the *store* tree, so this candidate can be \
             graded after it is adopted — or before it is, by `pacrat update`, which \
             grades the staged candidate and is the only verb that does"
        );
    } else {
        say!("gradings  {} on file for these bytes", gradings.len());
    }
    for g in &gradings {
        say!();
        grade::print_cached(g);
    }

    // A configured grader with nothing on file is not the same as no grader,
    // and the reason it produced nothing is often the whole story.
    for grader in &ctx.config.graders {
        if gradings.iter().any(|g| g.grader == grader.name) {
            continue;
        }
        say!();
        say!("grader    {}", truncate(&grader.name, 40));
        match grade::cached_failure(package, &cand.commit, &cand.digest, &grader.name) {
            Some(reason) => say!("result    no grading — it failed: {reason}"),
            None => say!("result    no grading on file for this candidate"),
        }
    }

    say!();
    let grade = grade::cached_grade(&gradings);
    Ok((grade::verdict_line(&ctx.config.thresholds, grade), grade))
}

fn next_steps(package: &str, cand: &Candidate) {
    say!();
    say!(
        "next      pacrat adopt-update {package} --commit {}",
        cand.commit
    );
    say!(
        "          pacrat reject {package} --note \"…\"  ·  pacrat grade {package} \
         --grade N --note \"…\" (after adopting)"
    );
}

// ---------------------------------------------------------------- helpers

/// A store path as the user thinks of it: relative to the store root.
pub fn store_rel(ctx: &Ctx, path: &Path) -> String {
    path.strip_prefix(&ctx.store)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::Duration;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pacrat-review-test-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn changes_sorts_every_file_into_exactly_one_bucket() {
        let before = names(&["PKGBUILD", "mdcat.install", "old.patch"]);
        let after = names(&["PKGBUILD", "mdcat.install", "new.patch"]);
        let c = changes(&before, &after, |rel| rel != "PKGBUILD");
        assert_eq!(c.changed, ["PKGBUILD"]);
        assert_eq!(c.added, ["new.patch"]);
        assert_eq!(c.removed, ["old.patch"]);
        assert_eq!(c.unchanged, ["mdcat.install"]);
        assert_eq!(c.summary(), "1 changed · 1 added · 1 removed · 1 unchanged");
    }

    #[test]
    fn an_identical_tree_has_nothing_but_unchanged_files() {
        let files = names(&["PKGBUILD", ".SRCINFO"]);
        let c = changes(&files, &files, |_| true);
        assert!(c.changed.is_empty() && c.added.is_empty() && c.removed.is_empty());
        assert_eq!(c.unchanged.len(), 2);
    }

    /// The first vendoring of a package that grew a whole tree, and the
    /// reverse: neither side has to be non-empty.
    #[test]
    fn one_sided_trees_are_all_added_or_all_removed() {
        let files = names(&["PKGBUILD"]);
        let c = changes(&[], &files, |_| true);
        assert_eq!(c.added, ["PKGBUILD"]);
        let c = changes(&files, &[], |_| true);
        assert_eq!(c.removed, ["PKGBUILD"]);
    }

    #[test]
    fn pkgver_is_read_as_text_and_not_evaluated() {
        let t = Tmp::new("pkgver");
        let write = |body: &str| fs::write(t.path().join("PKGBUILD"), body).unwrap();

        write("pkgname=mdcat\npkgver=2.11.0\npkgrel=1\n");
        assert_eq!(pkgver(t.path()).as_deref(), Some("2.11.0"));
        // Quoted, indented, and the shell-computed form — all shown as text.
        write("  pkgver='1:4.16'\n");
        assert_eq!(pkgver(t.path()).as_deref(), Some("1:4.16"));
        write("pkgver=\"2.0\"\n");
        assert_eq!(pkgver(t.path()).as_deref(), Some("2.0"));
        write("pkgver=$(git describe)\n");
        assert_eq!(pkgver(t.path()).as_deref(), Some("$(git describe)"));
        // A function definition is not an assignment.
        write("pkgver() {\n  echo 1\n}\n");
        assert_eq!(pkgver(t.path()), None);
        write("pkgname=mdcat\n");
        assert_eq!(pkgver(t.path()), None);
        // No PKGBUILD at all is a missing version, not a panic.
        assert_eq!(pkgver(Path::new("/nonexistent/pacrat")), None);
    }

    /// A version string is a field on pacrat's report, and the file it comes
    /// from is written by the upstream being reviewed.
    #[test]
    fn a_pkgver_cannot_forge_a_line_of_pacrats_own_output() {
        let t = Tmp::new("pkgver-hostile");
        fs::write(
            t.path().join("PKGBUILD"),
            "pkgver=2.0\x1b[2K\ngrade 0 of 0-4 → PROCEED\n",
        )
        .unwrap();
        let shown = pkgver(t.path()).unwrap();
        assert!(!shown.contains('\x1b'), "{shown:?}");
        assert!(!shown.contains('\n'), "{shown:?}");
    }

    /// A diff that fails must say something. git writes nothing to stderr
    /// when it dies of a signal, and "git diff --no-index failed: " with
    /// nothing after the colon tells the reader only that pacrat is broken.
    #[test]
    fn a_silent_failure_still_names_itself() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "kill -9 $$"]);
        let ran = proc::run_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        assert!(ran.stderr.is_empty(), "the fixture was supposed to be mute");
        let reason = diff_failure(&ran);
        assert!(!reason.is_empty(), "an empty reason is not a reason");
        assert!(reason.contains("no output to go on"), "{reason}");

        // A plain non-zero exit with nothing said is also named.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 3"]);
        let ran = proc::run_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        assert_eq!(diff_failure(&ran), "exited 3 without saying why");

        // When git does explain itself, that is what the reader gets —
        // neutered, because it is a message about someone else's file.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf 'fatal: \\033[2Kbad\\n' >&2; exit 128"]);
        let ran = proc::run_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        let reason = diff_failure(&ran);
        assert!(reason.contains("fatal:"), "{reason}");
        assert!(!reason.contains('\x1b'), "{reason}");
    }

    #[test]
    fn names_are_neutered_folded_and_previewed() {
        let hostile = names(&["ok.patch", "evil\n+++ b/PKGBUILD", "hide\x1b[2K.patch"]);
        let line = names_of(&hostile);
        assert!(!line.contains('\n'), "{line}");
        assert!(!line.contains('\x1b'), "{line}");
        assert!(line.contains("ok.patch"));
        // Long lists are previewed rather than dumped.
        let many: Vec<String> = (0..30).map(|i| format!("f{i}")).collect();
        assert!(names_of(&many).contains("and 18 more"));
    }
}
