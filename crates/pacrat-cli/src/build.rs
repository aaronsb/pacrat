//! `pacrat build` — turn reviewed store trees into packages in the local
//! pacman repo. This is the serving half of ADR-001's model: a build puts a
//! curated package where pacman can already see it, and `pacman -Sy <pkg>`
//! installs it like any other repo package. The verb stops at "served".
//!
//! Five rules shape the module:
//!
//! 1. **Nothing is built in the store.** makepkg writes `src/`, `pkg/` and
//!    tarballs beside the PKGBUILD, and the store holds pristine reviewed
//!    trees and nothing else. Every build runs from a copy in scratch, made
//!    with [`crate::fstree`] so the symlink refusal that guarded vendoring
//!    guards this too. `PKGDEST` and `BUILDDIR` are both pinned into that
//!    same scratch, so neither the artifacts nor the build itself can be
//!    relocated by a makepkg.conf this code never read.
//!
//! 2. **What gets served is what the PKGBUILD declared.** The contents of
//!    `PKGDEST` are not evidence of anything: `package()` runs arbitrary
//!    code and can drop a file called `evil-9.9-1-any.pkg.tar.zst` there,
//!    and the guard would then wave that name through forever, because it
//!    passes anything a sync database can resolve. So the authority is
//!    `makepkg --packagelist` — the names the PKGBUILD itself declares —
//!    and a package file that no declaration covers is a build *failure*,
//!    not a package. A split PKGBUILD legitimately declares several names
//!    and all of them are served: they are in the tree the human reviewed,
//!    which is exactly what curation means here.
//!
//! 3. **pacrat runs no pacman transaction, so no hook fires, so no marker
//!    is needed.** ADR-001 obliges `build` to write `.pacrat-transaction`
//!    in the repo directory around a pacman invocation and remove it after.
//!    Nothing here needs bracketing, and the reason is narrower than "we
//!    never run pacman": makepkg *does* run it, `pacman -T` once per
//!    dependency (`/usr/bin/makepkg:264`, via `check_deps`). That path is
//!    special-cased in `run_pacman` (`/usr/bin/makepkg:229`) — a `-T` or
//!    `-Q` argv is executed bare, with no `PACMAN_AUTH`/sudo wrapper and no
//!    db-lock wait — because it is a query. A query starts no transaction,
//!    a PreTransaction hook only runs for a transaction, and the marker
//!    exists only to stand that hook down. What would need the marker is
//!    `-s`/`--syncdeps`, which has makepkg call `pacman -S` for real
//!    (`:291`); it is deliberately absent from the argv, since building one
//!    package must not install others on this host as a side effect.
//!    Missing makedepends are therefore a build failure that names
//!    `pacman -S --asdeps <deps>` as the human's move, and any future flag
//!    that installs must write the marker first (`setup.rs`, `MARKER`).
//!
//! 4. **A ledger entry is not a licence to build.** Only a package with a
//!    tree in the store is built; an entry whose tree is missing is a broken
//!    store, and saying so is more useful than fetching something to fill
//!    the hole with.
//!
//! 5. **One package's failure is one package's failure.** The loop keeps
//!    going and the summary at the end is the report; the exit code is the
//!    machine-readable version of it. Nothing half-lands: a package file
//!    only stays in the repo directory if the database took it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use pacrat_core::pkg::valid_name;

use crate::ctx::Ctx;
use crate::fstree;
use crate::out::{list_preview, shell_quote, visible};
use crate::setup;

/// Exit code for "ran fine, but deliberately did not do all of it" —
/// ADR-001's 10, the same code `vendor` gives a declined review.
///
/// Here it means at least one package was **held**: in the ledger, no tree
/// in the store. It does *not* mean nothing happened. A run that serves two
/// packages and holds a third still exits 10, because the alternative —
/// reporting success while a curated package silently was not rebuilt — is
/// the failure mode `pacrat build x && sudo pacman -Sy x` would act on. The
/// code cannot express "partly", so the tail of the output names what was
/// served and what was held, and callers that care read those lines.
use crate::HELD;

/// makepkg's argv.
///
/// `--force` overwrites a package file already sitting in PKGDEST, and
/// `--noconfirm` because there is no human in this loop. `--cleanbuild`
/// removes `$srcdir` before building: with `BUILDDIR` pinned into fresh
/// scratch there is nothing stale for it to find, so it is belt and braces
/// here — but it is the flag that would matter the moment a build reuses a
/// directory, and dropping it would make that a silent difference.
///
/// Conspicuously absent: `-s`/`--syncdeps` and `-i`/`--install`, either of
/// which would have makepkg drive pacman — see rule 3 in the module note.
const MAKEPKG_ARGV: [&str; 3] = ["--force", "--noconfirm", "--cleanbuild"];

/// What to do when makepkg stops. pacrat cannot be told to fix this itself:
/// see rule 3 in the module note. Pre-wrapped, because the summary indents
/// continuation lines rather than reflowing them.
const DEPS_HINT: &str = "if it stopped on unresolved dependencies, install them yourself\n\
     (`sudo pacman -S --asdeps <deps>`) and re-run: pacrat does not pass\n\
     makepkg -s/--syncdeps, because that would make building one package\n\
     install others on this host as a side effect.";

pub fn run(ctx: &Ctx, packages: &[String]) -> Result<(), String> {
    let repo = &ctx.config.repo;
    let repo_path = PathBuf::from(&repo.path);
    if !repo_path.is_dir() {
        return Err(format!(
            "the repo directory {} does not exist — `pacrat setup` creates it \
             and the empty database; nothing can be served before it does",
            repo_path.display()
        ));
    }
    let db = repo_path.join(format!("{}.db.tar.zst", repo.name));

    let sources = ctx.load_sources()?;
    for name in sources.packages.keys() {
        // The ledger is a file a human can edit, and the name becomes a path
        // component under the store and under scratch.
        if !valid_name(name) {
            return Err(format!(
                "{name:?} in {} is not a package name — expected letters, digits, \
                 and @._+- (no leading hyphen or dot)",
                ctx.sources_path().display()
            ));
        }
    }
    let ledger: BTreeMap<String, bool> = sources
        .packages
        .keys()
        .map(|name| (name.clone(), tree_path(ctx, name).is_dir()))
        .collect();
    let plan = resolve(&ledger, packages)?;

    if plan.build.is_empty() && plan.no_tree.is_empty() {
        println!(
            "nothing to build — {} lists no vendored package yet;",
            ctx.sources_path().display()
        );
        println!("`pacrat vendor <package>` puts one there.");
        return Ok(());
    }

    println!("repo      [{}] {}", repo.name, repo_path.display());
    println!("db        {}", db.display());
    println!(
        "packages  {}",
        if plan.build.is_empty() {
            "none buildable".to_string()
        } else {
            plan.build.join(", ")
        }
    );

    let scratch = scratch_root()?;
    println!("scratch   {}", scratch.display());

    let mut results: Vec<(String, Outcome)> = Vec::new();
    for package in &plan.build {
        let outcome = match build_one(ctx, package, &scratch, &repo_path, &db) {
            Ok(served) => Outcome::Built(served),
            Err(e) => Outcome::Failed(e),
        };
        results.push((package.clone(), outcome));
    }
    for package in &plan.no_tree {
        results.push((package.clone(), Outcome::Skipped(no_tree(package))));
    }

    let failed = results
        .iter()
        .filter(|(_, o)| matches!(o, Outcome::Failed(_)))
        .count();
    // Scratch is evidence when something went wrong — a half-built tree and
    // makepkg's own logs are the only way to answer "why". Otherwise it is
    // litter.
    if failed == 0 {
        let _ = fs::remove_dir_all(&scratch);
    }

    report(&results);

    // The exit code says one thing about a run that may have done several.
    // These two lines are where a partial run is legible: what reached the
    // repo, and what did not, by name.
    let built: Vec<String> = results
        .iter()
        .filter(|(_, o)| matches!(o, Outcome::Built(_)))
        .map(|(name, _)| name.clone())
        .collect();
    println!();
    println!("repo      {}", db.display());
    if built.is_empty() {
        println!("served    nothing — no package reached the repo in this run");
    } else {
        println!("served    {}", list_preview(&built, 12));
    }
    if !plan.no_tree.is_empty() {
        // In a mixed run a failure owns the exit code, so name the code only
        // when a hold is actually what the caller will see.
        let exit_note = if failed == 0 {
            format!(" and exit {HELD} reports that;")
        } else {
            String::from(" (a failure owns the exit code);")
        };
        println!(
            "held      {} — not built,{exit_note} whatever is",
            list_preview(&plan.no_tree, 12)
        );
        println!("          listed as served above is in the repo regardless");
    }
    let state = setup::state(repo);
    if state.conf_section {
        // Only worth saying when pacman has actually been told about the
        // repo; otherwise the line is advice that cannot work.
        println!("next      sudo pacman -Sy && sudo pacman -S <package>");
    } else {
        println!(
            "note      [{}] is not in /etc/pacman.conf, so nothing can install from",
            repo.name
        );
        println!("          this repo yet — `pacrat setup` prints the section to add.");
    }
    if failed > 0 {
        println!(
            "scratch   kept at {} (build trees and makepkg output)",
            scratch.display()
        );
        return Err(format!(
            "{failed} of {} package{} failed to build",
            plan.build.len(),
            if plan.build.len() == 1 { "" } else { "s" }
        ));
    }
    if !plan.no_tree.is_empty() {
        std::process::exit(HELD);
    }
    Ok(())
}

/// How one package ended up, in the words the summary uses.
enum Outcome {
    /// Built and served; the number of package files that landed in the repo.
    Built(usize),
    Failed(String),
    Skipped(String),
}

/// The per-package outcome table. Reasons may be several lines; the extra
/// ones are indented into the message column so a long explanation stays
/// readable beside the one-word verdicts.
fn report(results: &[(String, Outcome)]) {
    let width = results
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    println!();
    println!("summary");
    for (name, outcome) in results {
        let (verdict, why) = match outcome {
            Outcome::Built(n) => (
                "built  ",
                format!("{n} package file{}", if *n == 1 { "" } else { "s" }),
            ),
            Outcome::Failed(why) => ("failed ", why.clone()),
            Outcome::Skipped(why) => ("skipped", why.clone()),
        };
        let mut lines = why.lines();
        println!(
            "  {verdict}  {name:<width$}  {}",
            lines.next().unwrap_or_default()
        );
        for line in lines {
            println!("{:indent$}{line}", "", indent = 13 + width);
        }
    }
}

// ------------------------------------------------------------------- plan

/// The trees a run will attempt, and the ledger entries it will not.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    build: Vec<String>,
    /// In the ledger, no tree in the store.
    no_tree: Vec<String>,
}

/// Decide what to build, from the ledger (each name paired with whether its
/// store tree exists) and the names asked for.
///
/// No names means every vendored package that has a tree. The ones without
/// are carried in `no_tree` rather than dropped: a ledger entry with no tree
/// is a broken store, and a sweep that stayed quiet about it would let the
/// breakage sit. A *named* package must be in the ledger — building
/// something the ledger does not describe would serve bytes that no review
/// ever covered, which is the whole thing pacrat exists to prevent.
fn resolve(ledger: &BTreeMap<String, bool>, requested: &[String]) -> Result<Plan, String> {
    if requested.is_empty() {
        return Ok(Plan {
            build: ledger
                .iter()
                .filter(|(_, has_tree)| **has_tree)
                .map(|(name, _)| name.clone())
                .collect(),
            no_tree: ledger
                .iter()
                .filter(|(_, has_tree)| !**has_tree)
                .map(|(name, _)| name.clone())
                .collect(),
        });
    }

    let mut names: Vec<String> = Vec::new();
    for name in requested {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    let unknown: Vec<&str> = names
        .iter()
        .filter(|name| !ledger.contains_key(*name))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "not in aur/sources.toml: {} — only a vendored package can be built, \
             and `pacrat vendor <package>` is what puts one in the ledger",
            unknown.join(", ")
        ));
    }

    let mut plan = Plan::default();
    for name in names {
        if ledger[name.as_str()] {
            plan.build.push(name);
        } else {
            plan.no_tree.push(name);
        }
    }
    Ok(plan)
}

fn tree_path(ctx: &Ctx, package: &str) -> PathBuf {
    ctx.store.join("aur").join("packages").join(package)
}

/// The wording mirrors `vendor`'s held matrix: the same store state gets the
/// same name and the same way out, whichever verb ran into it. Paths are
/// store-relative, as they are in the sentence's other half.
fn no_tree(package: &str) -> String {
    format!(
        "in aur/sources.toml but has no tree at aur/packages/{package} —\n\
         the store is inconsistent; `pacrat vendor {package} --force` re-vendors it"
    )
}

// ------------------------------------------------------------------ build

/// Build one package and add it to the repo. Returns how many package files
/// were served.
fn build_one(
    ctx: &Ctx,
    package: &str,
    scratch: &Path,
    repo_path: &Path,
    db: &Path,
) -> Result<usize, String> {
    println!();
    println!("── {package}");

    let tree = tree_path(ctx, package);
    // Validates as it walks — a symlink that appeared in the store since the
    // review is refused here rather than copied and built.
    let files = fstree::files(&tree)?;
    if !files.iter().any(|f| f == "PKGBUILD") {
        return Err(format!(
            "no PKGBUILD in {} — the tree is not a package",
            tree.display()
        ));
    }

    let work = scratch.join(package);
    let src = work.join("tree");
    let out = work.join("pkgdest");
    // BUILDDIR gets its own subdirectory rather than `work` itself: makepkg
    // builds under `$BUILDDIR/$pkgbase`, and a package whose pkgbase is
    // `tree` or `pkgdest` would otherwise land on top of one of ours.
    let build = work.join("build");
    fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    fs::create_dir_all(&build).map_err(|e| format!("{}: {e}", build.display()))?;
    fstree::install(&tree, &files, &src)?;
    println!(
        "tree      {} ({} file{})",
        tree.display(),
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    );
    println!("copy      {}", src.display());
    println!("pkgdest   {}", out.display());
    println!("builddir  {}", build.display());

    // Asked before the build, so this is the reviewed text's own account of
    // what it produces — `package()` has not run yet and cannot have edited
    // the PKGBUILD to widen it.
    let declared = packagelist(&src, &build, &out)?;
    // makepkg's pkgname lint makes these names tame today; routing them
    // through the same escape as every other untrusted print keeps the rule
    // uniform instead of resting on an upstream lint staying put.
    let (declared_shown, _) = visible(&list_preview(&sorted(&declared), 8));
    println!("declares  {declared_shown}");

    let mut makepkg = makepkg_in(&src, &build, &out);
    makepkg.args(MAKEPKG_ARGV);
    match run_visible(&mut makepkg) {
        Ok(()) => {}
        Err(RunErr::Spawn(e)) => {
            return Err(format!("{e} — makepkg ships with the `pacman` package"))
        }
        Err(RunErr::Status(e)) => return Err(format!("{e}\n{DEPS_HINT}")),
    }

    // Asked again, because a `pkgver()` moves the version *and* rewrites the
    // PKGBUILD in place while doing it (`/usr/bin/makepkg:190`,
    // `update_pkgver`), so the pre-build list has the old version in it for
    // every VCS package. This second list is therefore the file-name
    // authority, and the first stays the *name* authority: between them, a
    // build may rename its own version but may not invent a package.
    let produced = packagelist(&src, &build, &out)?;
    let allowed = pkgnames(&declared);
    if let Some(rogue) = rogue_name(&produced, &allowed) {
        return Err(format!(
            "the build rewrote its own PKGBUILD to declare {} — the reviewed\n\
             tree declares only {}; nothing was served",
            visible(rogue).0,
            list_preview(&sorted_str(&allowed), 8)
        ));
    }

    let present = artifacts(&out)?;
    let present_names: Vec<String> = present
        .iter()
        .filter_map(|p| file_name(p))
        .map(str::to_string)
        .collect();
    let extra = undeclared(&present_names, &produced);
    if !extra.is_empty() {
        return Err(format!(
            "makepkg produced package file{} it did not declare: {} —\n\
             refusing to serve a name the reviewed PKGBUILD never mentioned\n\
             (the guard passes any name a repo carries, so this is how one\n\
             would be smuggled in); nothing was served",
            if extra.len() == 1 { "" } else { "s" },
            list_preview(&extra, 8)
        ));
    }
    // What is left is the intersection of the declaration with what exists.
    // Declared-but-absent is normal, not an error: makepkg lists a debug
    // package whenever the options ask for one, and produces it only when
    // there was something to strip.
    if present.is_empty() {
        return Err(format!(
            "makepkg reported success but left no package in {} — \
             nothing to serve",
            out.display()
        ));
    }

    let mut placed: Vec<Placed> = Vec::new();
    let mut served: Vec<PathBuf> = Vec::new();
    for artifact in &present {
        let dest = match file_name(artifact) {
            Some(name) => repo_path.join(name),
            None => {
                unplace(placed);
                return Err(format!("{}: no file name", artifact.display()));
            }
        };
        // A signature only exists when the PKGBUILD or makepkg.conf asked for
        // one, and repo-add looks for it beside the package file, so it has
        // to travel with it — and it has to be in place *before* repo-add
        // runs, or the database records the package as unsigned.
        for from in travelling(artifact) {
            let to = if from == *artifact {
                dest.clone()
            } else {
                sig(&dest)
            };
            match place(&from, &to) {
                Ok(p) => {
                    println!("serve     {}", safe(&to));
                    placed.push(p);
                }
                Err(e) => {
                    unplace(placed);
                    return Err(format!("{e}\n{UNCHANGED}"));
                }
            }
        }
        served.push(dest);
    }

    let mut repo_add = Command::new("repo-add");
    repo_add.arg(db).args(&served);
    match run_visible(&mut repo_add) {
        Ok(()) => keep(placed),
        Err(RunErr::Spawn(e)) => {
            unplace(placed);
            return Err(format!(
                "{e} — repo-add ships with the `pacman` package\n\
                 {UNCHANGED}"
            ));
        }
        Err(RunErr::Status(e)) => {
            // A package file the database does not describe is worse than no
            // package file: pacman cannot see it, the next repo-add might
            // pick it up, and nobody chose it. Roll the directory back.
            unplace(placed);
            return Err(format!("{e}\n{UNCHANGED}"));
        }
    }
    Ok(served.len())
}

/// Continuation lines carry no indentation of their own: the summary owns
/// the message column and indents them to it.
const UNCHANGED: &str = "the package files were removed again — the repo directory is unchanged";

fn sig(package_file: &Path) -> PathBuf {
    let mut path = package_file.as_os_str().to_os_string();
    path.push(".sig");
    PathBuf::from(path)
}

/// The files that travel to the repo for one built package: the package
/// itself, and its detached signature when there is one.
///
/// A signature exists only when the PKGBUILD or makepkg.conf asked for
/// signing. repo-add looks for it beside the package file, so it has to be
/// in place before repo-add runs — otherwise the database records a package
/// that has a signature as though it did not.
fn travelling(artifact: &Path) -> Vec<PathBuf> {
    let mut files = vec![artifact.to_path_buf()];
    let signature = sig(artifact);
    if signature.exists() {
        files.push(signature);
    }
    files
}

/// The pkgnames a declaration covers.
fn pkgnames(declared: &BTreeSet<String>) -> BTreeSet<&str> {
    declared.iter().filter_map(|f| pkgname_of(f)).collect()
}

/// A declared file whose pkgname is not one the reviewed tree declared.
///
/// The only way to get one is a build that edited its own PKGBUILD, which
/// makepkg itself does for `pkgver()` — legitimately, and only to the
/// version. A *name* appearing that the reviewed text never had is the
/// signature of a tree rewriting what it is.
fn rogue_name<'a>(produced: &'a BTreeSet<String>, allowed: &BTreeSet<&str>) -> Option<&'a str> {
    produced
        .iter()
        .find(|file| !pkgname_of(file).is_some_and(|name| allowed.contains(name)))
        .map(String::as_str)
}

/// Package files present in PKGDEST that the declaration does not cover.
///
/// PKGDEST is fresh for every package, so a stale file cannot be the
/// explanation: anything here was written by this build. Names are rendered
/// through [`crate::out::visible`] because they are attacker-chosen text
/// about to be printed.
fn undeclared(present: &[String], declared: &BTreeSet<String>) -> Vec<String> {
    present
        .iter()
        .filter(|name| !declared.contains(*name))
        .map(|name| visible(name).0)
        .collect()
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|n| n.to_str())
}

/// A path that came out of a directory a PKGBUILD can write into, rendered
/// safe to print: the file name is attacker-chosen text, and a terminal
/// takes control characters in it seriously ([`crate::out::visible`]).
fn safe(path: &Path) -> String {
    visible(&path.display().to_string()).0
}

fn sorted(names: &BTreeSet<String>) -> Vec<String> {
    names.iter().cloned().collect()
}

fn sorted_str(names: &BTreeSet<&str>) -> Vec<String> {
    names.iter().map(|n| (*n).to_string()).collect()
}

/// The package files this PKGBUILD says it will produce, by file name.
///
/// `makepkg --packagelist` sources the PKGBUILD and prints one absolute path
/// per declared package (`print_all_package_names`), built from the same
/// `PKGDEST` and `PKGEXT` the real build will use. Only the file names are
/// kept: makepkg canonicalizes `PKGDEST`, so the directory half can differ
/// from ours by a resolved symlink while meaning the same place.
fn packagelist(src: &Path, build: &Path, out: &Path) -> Result<BTreeSet<String>, String> {
    let mut cmd = makepkg_in(src, build, out);
    cmd.arg("--packagelist");
    let listed = match run_capture(&mut cmd) {
        Ok(text) => text,
        Err(RunErr::Spawn(e)) => {
            return Err(format!("{e} — makepkg ships with the `pacman` package"))
        }
        Err(RunErr::Status(e)) => {
            return Err(format!(
                "{e}\nthe PKGBUILD does not parse, so there is no telling what a\n\
             build of it would produce"
            ))
        }
    };
    let names = declared_names(&listed);
    if names.is_empty() {
        return Err(
            "makepkg --packagelist named no package — the PKGBUILD declares nothing to build"
                .into(),
        );
    }
    Ok(names)
}

/// Parse `--packagelist` output: one path per line, of which only the file
/// name matters.
fn declared_names(listed: &str) -> BTreeSet<String> {
    listed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.rsplit_once('/').map_or(line, |(_, name)| name))
        .map(str::to_string)
        .collect()
}

/// Everything makepkg left in `PKGDEST` that is shaped like a package,
/// sorted. Shape is all this can tell; whether it is *ours* is the
/// declaration's answer, not this function's.
fn artifacts(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        if entry.file_name().to_str().is_some_and(is_artifact) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// Is this file name shaped like one of makepkg's package files?
///
/// The shape is `<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar<.compression>`.
/// The `.pkg.tar` part is not a guess: makepkg's own config lint refuses a
/// `PKGEXT` that does not begin with it
/// (`/usr/share/makepkg/lint_config/ext.sh:34`), so only the compression
/// suffix varies and it is matched loosely rather than against a list of
/// algorithms. The split is from the right because the extension is what is
/// being looked for, and a pkgname is allowed to contain almost anything.
/// Two exclusions carry weight: a detached signature is not a package (it
/// travels with one), and a `.pkg.tar.zst.part` left behind by something
/// else is not one either.
fn is_artifact(name: &str) -> bool {
    if name.ends_with(".sig") {
        return false;
    }
    let Some((stem, tail)) = name.rsplit_once(".pkg.tar") else {
        return false;
    };
    if stem.is_empty() || stem.starts_with('.') {
        return false;
    }
    tail.is_empty() || (tail.starts_with('.') && !tail[1..].is_empty() && !tail[1..].contains('.'))
}

/// The pkgname out of a package file name, read from the right: pkgver,
/// pkgrel and arch may not contain a hyphen and pkgname may, so the last
/// three fields are the ones that can be split off safely.
/// `foo-debug-1.0-1-any.pkg.tar.zst` yields `foo-debug`, which is correct —
/// makepkg's debug package is a pkgname in its own right.
fn pkgname_of(file: &str) -> Option<&str> {
    let (stem, _ext) = file.rsplit_once(".pkg.tar")?;
    let (rest, _arch) = stem.rsplit_once('-')?;
    let (rest, _pkgrel) = rest.rsplit_once('-')?;
    let (name, _pkgver) = rest.rsplit_once('-')?;
    (!name.is_empty()).then_some(name)
}

// ------------------------------------------------------------- the repo dir

/// A file this run put in the repo directory, and whatever it displaced.
struct Placed {
    dest: PathBuf,
    /// What `dest` held before, moved aside so a rollback can put it back.
    backup: Option<PathBuf>,
}

/// Publish one file into the repo directory.
///
/// The bytes are copied under a temp name in the same directory and *renamed*
/// into place, because pacman may be reading that directory while this runs.
/// A reader can briefly find the file absent — which is a retry — but never
/// finds a half-copied one, which it would fetch, checksum, and reject.
fn place(from: &Path, dest: &Path) -> Result<Placed, String> {
    let dir = dest
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", dest.display()))?;
    let name = file_name(dest).ok_or_else(|| format!("{}: unusable file name", dest.display()))?;
    let stamp = std::process::id();
    let tmp = dir.join(format!(".pacrat-{name}.{stamp}.new"));

    if let Err(e) = fs::copy(from, &tmp) {
        // A partial temp copy is inert (pacman reads the db, not the dir),
        // but the other error arms clean up after themselves — match them.
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {e}", safe(&tmp)));
    }
    let backup = if dest.exists() {
        let backup = dir.join(format!(".pacrat-{name}.{stamp}.old"));
        if let Err(e) = fs::rename(dest, &backup) {
            let _ = fs::remove_file(&tmp);
            return Err(format!("{}: {e}", safe(dest)));
        }
        Some(backup)
    } else {
        None
    };
    if let Err(e) = fs::rename(&tmp, dest) {
        if let Some(backup) = &backup {
            let _ = fs::rename(backup, dest);
        }
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: {e}", safe(dest)));
    }
    Ok(Placed {
        dest: dest.to_path_buf(),
        backup,
    })
}

/// Put the repo directory back the way it was found. Reverse order, so a
/// file placed over another is removed before that other is restored.
fn unplace(placed: Vec<Placed>) {
    for p in placed.into_iter().rev() {
        let _ = fs::remove_file(&p.dest);
        if let Some(backup) = p.backup {
            let _ = fs::rename(&backup, &p.dest);
        }
    }
}

/// Let the placements stand: the database now describes them, so the copies
/// they displaced are no longer needed.
fn keep(placed: Vec<Placed>) {
    for p in placed {
        if let Some(backup) = p.backup {
            let _ = fs::remove_file(backup);
        }
    }
}

// --------------------------------------------------------------- plumbing

/// A makepkg invocation in the copied tree, with the three environment
/// settings that decide where it works and what it runs.
///
/// makepkg re-exports `PKGDEST` and `BUILDDIR` from the environment *after*
/// sourcing makepkg.conf (`/usr/share/makepkg/util/config.sh`,
/// `load_makepkg_config`), so these win over the host's config — which is
/// the point. Without them a user's `BUILDDIR=/var/tmp/makepkg` would move
/// the real build out of scratch, making both the "nothing is built in the
/// store" claim and the "scratch is the evidence" claim untrue, and would
/// hand `--cleanbuild` a directory outside scratch to delete.
///
/// `PACMAN` names the binary makepkg executes for its dependency queries; an
/// ambient value would quietly choose a different one, so it is removed
/// rather than trusted.
fn makepkg_in(src: &Path, build: &Path, out: &Path) -> Command {
    let mut cmd = Command::new("makepkg");
    cmd.current_dir(src)
        .env("PKGDEST", out)
        .env("BUILDDIR", build)
        .env_remove("PACMAN");
    cmd
}

/// A subprocess that did not run, versus one that ran and said no. Only the
/// second is worth attaching build advice to.
enum RunErr {
    Spawn(String),
    Status(String),
}

/// Run a subprocess with its argv printed first — ADR-001's
/// always-visible-calls rule, no exceptions for the long ones. Output is left
/// on the terminal: a build the user cannot watch is a build they cannot
/// debug, and makepkg's own diagnostics are the whole story when it fails.
fn run_visible(cmd: &mut Command) -> Result<(), RunErr> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let shown = argv_line(cmd);
    println!("run       {shown}");
    let status = cmd
        .status()
        .map_err(|e| RunErr::Spawn(format!("{program}: {e}")))?;
    if !status.success() {
        return Err(RunErr::Status(format!("{shown} failed ({status})")));
    }
    Ok(())
}

/// Same rule, for a call whose *stdout is the answer* rather than something
/// to watch. stderr still goes to the terminal, so a PKGBUILD that will not
/// parse says why in the place the user is already looking.
fn run_capture(cmd: &mut Command) -> Result<String, RunErr> {
    let shown = argv_line(cmd);
    println!("run       {shown}");
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| RunErr::Spawn(format!("{}: {e}", cmd.get_program().to_string_lossy())))?;
    if !out.status.success() {
        return Err(RunErr::Status(format!("{shown} failed ({})", out.status)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn argv_line(cmd: &Command) -> String {
    cmd.get_args().fold(
        cmd.get_program().to_string_lossy().into_owned(),
        |mut line, arg| {
            line.push(' ');
            line.push_str(&shell_quote(&arg.to_string_lossy()));
            line
        },
    )
}

/// One scratch root per run, 0700 from the moment it exists: build trees are
/// world-readable nowhere, and `create` rather than check-then-create closes
/// the window a collision would open.
fn scratch_root() -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("pacrat-build-{}-{nanos}", std::process::id()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(entries: &[(&str, bool)]) -> BTreeMap<String, bool> {
        entries
            .iter()
            .map(|(name, has_tree)| ((*name).to_string(), *has_tree))
            .collect()
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_sweep_builds_every_tree_and_reports_the_rest() {
        let l = ledger(&[("mdcat", true), ("ghost", false), ("aurutils", true)]);
        let plan = resolve(&l, &[]).unwrap();
        assert_eq!(plan.build, names(&["aurutils", "mdcat"]));
        assert_eq!(plan.no_tree, names(&["ghost"]));
    }

    #[test]
    fn an_empty_ledger_plans_nothing() {
        assert_eq!(resolve(&ledger(&[]), &[]).unwrap(), Plan::default());
    }

    #[test]
    fn named_packages_are_kept_in_the_order_given_and_deduped() {
        let l = ledger(&[("mdcat", true), ("aurutils", true)]);
        let plan = resolve(&l, &names(&["mdcat", "aurutils", "mdcat"])).unwrap();
        assert_eq!(plan.build, names(&["mdcat", "aurutils"]));
    }

    #[test]
    fn a_name_outside_the_ledger_stops_the_run_and_is_named() {
        let l = ledger(&[("mdcat", true)]);
        let err = resolve(&l, &names(&["mdcat", "nope", "also-nope"])).unwrap_err();
        assert!(err.contains("nope") && err.contains("also-nope"), "{err}");
        assert!(err.contains("pacrat vendor"), "no way forward: {err}");
        // The buildable one is not smuggled through on a failed resolution.
        assert!(!err.contains("mdcat"), "{err}");
    }

    #[test]
    fn a_named_package_without_a_tree_is_held_not_built() {
        let l = ledger(&[("ghost", false)]);
        let plan = resolve(&l, &names(&["ghost"])).unwrap();
        assert!(plan.build.is_empty());
        assert_eq!(plan.no_tree, names(&["ghost"]));
    }

    #[test]
    fn a_missing_tree_names_the_path_and_the_way_back() {
        let why = no_tree("ghost");
        assert!(why.contains("aur/packages/ghost"), "{why}");
        assert!(why.contains("the store is inconsistent"), "{why}");
        assert!(why.contains("pacrat vendor ghost --force"), "{why}");
    }

    /// Rule 2, as a test: building must never install. Nothing in the argv
    /// may hand pacman to makepkg, and the advice for a missing makedepend
    /// is for the human to run pacman themselves.
    #[test]
    fn makepkg_is_never_told_to_drive_pacman() {
        for forbidden in ["-s", "--syncdeps", "-i", "--install", "--rmdeps"] {
            assert!(
                !MAKEPKG_ARGV.contains(&forbidden),
                "{forbidden} would make a build install packages"
            );
        }
        assert!(DEPS_HINT.contains("sudo pacman -S --asdeps <deps>"));
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// A scratch directory that cleans itself up.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pacrat-build-test-{tag}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::write(&p, text).unwrap();
            p
        }
        fn read(&self, rel: &str) -> Option<String> {
            fs::read_to_string(self.0.join(rel)).ok()
        }
        fn names(&self) -> Vec<String> {
            let mut v: Vec<String> = fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // Captured from `makepkg --packagelist` on a split PKGBUILD.
    const PACKAGELIST: &str = "\
/tmp/out/probe-a-1.0-1-any.pkg.tar.zst
/tmp/out/probe-b-1.0-1-any.pkg.tar.zst
/tmp/out/probe-debug-1.0-1-any.pkg.tar.zst
";

    #[test]
    fn the_declaration_is_read_as_file_names() {
        assert_eq!(
            declared_names(PACKAGELIST),
            set(&[
                "probe-a-1.0-1-any.pkg.tar.zst",
                "probe-b-1.0-1-any.pkg.tar.zst",
                "probe-debug-1.0-1-any.pkg.tar.zst",
            ])
        );
        // Blank lines, trailing space, and a bare name with no directory.
        assert_eq!(
            declared_names("\n  /a/b/x-1-1-any.pkg.tar  \n\ny-1-1-any.pkg.tar.xz\n"),
            set(&["x-1-1-any.pkg.tar", "y-1-1-any.pkg.tar.xz"])
        );
        assert!(declared_names("").is_empty());
    }

    #[test]
    fn pkgname_is_read_from_the_right() {
        assert_eq!(
            pkgname_of("mdcat-2.10.1-1-x86_64.pkg.tar.zst"),
            Some("mdcat")
        );
        // A pkgname with hyphens, and makepkg's debug package.
        assert_eq!(
            pkgname_of("python-a-b-1.0-2-any.pkg.tar.zst"),
            Some("python-a-b")
        );
        assert_eq!(
            pkgname_of("probe-debug-1.0-1-any.pkg.tar.zst"),
            Some("probe-debug")
        );
        // An epoch lives in the version field, which is not split further.
        assert_eq!(pkgname_of("mdcat-1:2.0-1-any.pkg.tar.zst"), Some("mdcat"));
        // Too few fields to attribute to a package.
        assert_eq!(pkgname_of("x.pkg.tar.zst"), None);
        assert_eq!(pkgname_of("PKGBUILD"), None);
    }

    /// The attack this check exists for: `package()` writes a package file
    /// into PKGDEST that the PKGBUILD never declared. The guard downstream
    /// waves through any name a repo carries, so it has to be caught here.
    #[test]
    fn an_undeclared_package_file_is_not_a_package() {
        let declared = set(&["mdcat-2.10.1-1-x86_64.pkg.tar.zst"]);
        assert!(undeclared(
            &["mdcat-2.10.1-1-x86_64.pkg.tar.zst".to_string()],
            &declared
        )
        .is_empty());
        assert_eq!(
            undeclared(
                &[
                    "mdcat-2.10.1-1-x86_64.pkg.tar.zst".to_string(),
                    "evil-9.9-1-any.pkg.tar.zst".to_string(),
                ],
                &declared
            ),
            vec!["evil-9.9-1-any.pkg.tar.zst"]
        );
        // The report of it cannot itself be an attack on the terminal.
        let hidden = undeclared(
            &["\x1b[8mevil-9.9-1-any.pkg.tar.zst".to_string()],
            &declared,
        );
        assert!(!hidden[0].contains('\x1b'), "{hidden:?}");
    }

    /// Split packages are the reason this compares names and not one name:
    /// every pkgname the reviewed PKGBUILD declares is allowed, and only a
    /// name it never declared is rogue.
    #[test]
    fn a_build_may_move_its_version_but_not_invent_a_package() {
        let reviewed = set(&[
            "probe-a-1.0-1-any.pkg.tar.zst",
            "probe-b-1.0-1-any.pkg.tar.zst",
        ]);
        let allowed = pkgnames(&reviewed);

        // What a pkgver() does: same names, later version.
        let bumped = set(&[
            "probe-a-2.0-1-any.pkg.tar.zst",
            "probe-b-2.0-1-any.pkg.tar.zst",
        ]);
        assert_eq!(rogue_name(&bumped, &allowed), None);

        // What a PKGBUILD that rewrote its own pkgname does.
        let rewritten = set(&[
            "probe-a-2.0-1-any.pkg.tar.zst",
            "evil-9.9-1-any.pkg.tar.zst",
        ]);
        assert_eq!(
            rogue_name(&rewritten, &allowed),
            Some("evil-9.9-1-any.pkg.tar.zst")
        );
    }

    #[test]
    fn a_signature_travels_with_its_package_and_nothing_else_does() {
        let t = Tmp::new("sig");
        let pkg = t.write("mdcat-1-1-any.pkg.tar.zst", "package");
        assert_eq!(travelling(&pkg), vec![pkg.clone()]);

        t.write("mdcat-1-1-any.pkg.tar.zst.sig", "signature");
        assert_eq!(travelling(&pkg), vec![pkg.clone(), sig(&pkg)]);
        // The signature is not itself a package to be served.
        assert!(!is_artifact("mdcat-1-1-any.pkg.tar.zst.sig"));
    }

    /// The repo directory is the thing pacman reads. A run that cannot
    /// finish must leave it exactly as it was found — including the package
    /// it was already serving, which a rebuild of the same version displaces.
    #[test]
    fn a_rolled_back_placement_leaves_the_repo_directory_untouched() {
        let repo = Tmp::new("repo");
        let src = Tmp::new("built");
        repo.write("mdcat-1-1-any.pkg.tar.zst", "the version being served");
        repo.write("dotfiles-aur.db.tar.zst", "the database");
        let before = repo.names();

        let rebuilt = src.write("mdcat-1-1-any.pkg.tar.zst", "the rebuild");
        let signature = src.write("mdcat-1-1-any.pkg.tar.zst.sig", "its signature");
        let fresh = src.write("newpkg-1-1-any.pkg.tar.zst", "a package not served before");

        let mut placed = Vec::new();
        for (from, to) in [
            (&rebuilt, repo.path().join("mdcat-1-1-any.pkg.tar.zst")),
            (
                &signature,
                repo.path().join("mdcat-1-1-any.pkg.tar.zst.sig"),
            ),
            (&fresh, repo.path().join("newpkg-1-1-any.pkg.tar.zst")),
        ] {
            placed.push(place(from, &to).unwrap());
        }
        // Mid-run the new bytes are live.
        assert_eq!(
            repo.read("mdcat-1-1-any.pkg.tar.zst").unwrap(),
            "the rebuild"
        );

        unplace(placed);

        assert_eq!(
            repo.read("mdcat-1-1-any.pkg.tar.zst").as_deref(),
            Some("the version being served"),
            "the displaced package was not put back"
        );
        assert_eq!(repo.names(), before, "the repo directory changed");
    }

    /// The other half: when the database took the packages, what they
    /// displaced is dropped and no scratch names survive.
    #[test]
    fn a_kept_placement_leaves_only_the_new_files() {
        let repo = Tmp::new("keeprepo");
        let src = Tmp::new("keepbuilt");
        repo.write("mdcat-1-1-any.pkg.tar.zst", "old");
        let rebuilt = src.write("mdcat-1-1-any.pkg.tar.zst", "new");

        let placed = vec![place(&rebuilt, &repo.path().join("mdcat-1-1-any.pkg.tar.zst")).unwrap()];
        keep(placed);

        assert_eq!(repo.read("mdcat-1-1-any.pkg.tar.zst").unwrap(), "new");
        assert_eq!(repo.names(), vec!["mdcat-1-1-any.pkg.tar.zst"]);
    }

    #[test]
    fn package_files_are_recognised_and_their_neighbours_are_not() {
        for ok in [
            "mdcat-2.10.1-1-x86_64.pkg.tar.zst",
            "mdcat-2.10.1-1-any.pkg.tar.xz",
            "mdcat-debug-2.10.1-1-x86_64.pkg.tar.zst",
            "gtk+-1.2-1-x86_64.pkg.tar",
        ] {
            assert!(is_artifact(ok), "{ok} should be a package file");
        }
        for bad in [
            "mdcat-2.10.1-1-x86_64.pkg.tar.zst.sig",
            "mdcat-2.10.1-1-x86_64.pkg.tar.zst.part",
            "PKGBUILD",
            "mdcat-2.10.1.src.tar.gz",
            ".pkg.tar.zst",
            "dotfiles-aur.db.tar.zst",
            "",
        ] {
            assert!(!is_artifact(bad), "{bad} should not be a package file");
        }
    }
}
