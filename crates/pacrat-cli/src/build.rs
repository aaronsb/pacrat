//! `pacrat build` — turn reviewed store trees into packages in the local
//! pacman repo. This is the serving half of ADR-001's model: a build puts a
//! curated package where pacman can already see it, and `pacman -Sy <pkg>`
//! installs it like any other repo package. The verb stops at "served".
//!
//! Four rules shape the module:
//!
//! 1. **Nothing is built in the store.** makepkg writes `src/`, `pkg/` and
//!    tarballs beside the PKGBUILD, and the store holds pristine reviewed
//!    trees and nothing else. Every build runs from a copy in scratch, made
//!    with [`crate::fstree`] so the symlink refusal that guarded vendoring
//!    guards this too, and `PKGDEST` points at scratch so the artifacts are
//!    somewhere known rather than wherever makepkg.conf would have put them.
//!
//! 2. **pacrat does not install, and never calls pacman.** ADR-001 gives
//!    `build` a contract with the guard hook: write `.pacrat-transaction` in
//!    the repo directory before invoking pacman, remove it after. That
//!    contract does not bite here, because there is no pacman invocation to
//!    bracket — deliberately. makepkg is run *without* `-s`/`--syncdeps`,
//!    the flag that would have makepkg call `pacman -S` itself: that call
//!    would need the marker, and it would also mean building one package
//!    silently installs others on this host. Missing makedepends are
//!    therefore a build failure that names `pacman -S --asdeps <deps>` as
//!    the human's move. Any future flag that installs must write the marker
//!    first and remove it after (`setup.rs`, `MARKER`).
//!
//! 3. **A ledger entry is not a licence to build.** Only a package with a
//!    tree in the store is built; an entry whose tree is missing is a broken
//!    store, and saying so is more useful than fetching something to fill
//!    the hole with.
//!
//! 4. **One package's failure is one package's failure.** The loop keeps
//!    going and the summary at the end is the report; the exit code is the
//!    machine-readable version of it.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ctx::Ctx;
use crate::fstree;
use crate::out::shell_quote;
use crate::setup;
use crate::vendor::valid_name;

/// Exit code for "ran fine, deliberately did not act" — ADR-001's 10, the
/// same code `vendor` gives a declined review. A run that held every package
/// back must not let `pacrat build x && sudo pacman -Sy x` read the hold as
/// a go.
const HELD: i32 = 10;

/// makepkg's argv. `--force` overwrites a package file already sitting in
/// PKGDEST, `--noconfirm` because there is no human in this loop, and
/// `--cleanbuild` because a fresh copy of the tree must not inherit a
/// previous run's `$srcdir`. Conspicuously absent: `-s`/`--syncdeps` and
/// `-i`/`--install`, either of which would have makepkg drive pacman — see
/// rule 2 in the module note.
const MAKEPKG_ARGV: [&str; 3] = ["--force", "--noconfirm", "--cleanbuild"];

/// What to do when makepkg stops. pacrat cannot be told to fix this itself:
/// see rule 2 in the module note. Pre-wrapped, because the summary indents
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

    println!();
    println!("repo      {}", db.display());
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
    fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    fstree::install(&tree, &files, &src)?;
    println!(
        "tree      {} ({} file{})",
        tree.display(),
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    );
    println!("copy      {}", src.display());
    println!("pkgdest   {}", out.display());

    let mut makepkg = Command::new("makepkg");
    makepkg
        .args(MAKEPKG_ARGV)
        .current_dir(&src)
        // Overrides makepkg.conf (makepkg restores these from the
        // environment after sourcing it), so the artifacts are always where
        // this code looks for them and never in the user's usual PKGDEST.
        .env("PKGDEST", &out);
    match run_visible(&mut makepkg) {
        Ok(()) => {}
        Err(RunErr::Spawn(e)) => {
            return Err(format!("{e} — makepkg ships with the `pacman` package"))
        }
        Err(RunErr::Status(e)) => return Err(format!("{e}\n{DEPS_HINT}")),
    }

    let built = artifacts(&out)?;
    if built.is_empty() {
        return Err(format!(
            "makepkg reported success but left no package in {} — \
             nothing to serve",
            out.display()
        ));
    }

    let mut served: Vec<PathBuf> = Vec::new();
    for artifact in &built {
        let name = artifact
            .file_name()
            .ok_or_else(|| format!("{}: no file name", artifact.display()))?;
        let dest = repo_path.join(name);
        fs::copy(artifact, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        println!("serve     {}", dest.display());
        // A signature only exists when the PKGBUILD or makepkg.conf asked for
        // one, and repo-add looks for it beside the package file, so it has
        // to travel with it.
        if sig(artifact).exists() {
            fs::copy(sig(artifact), sig(&dest))
                .map_err(|e| format!("{}: {e}", sig(&dest).display()))?;
            println!("serve     {}", sig(&dest).display());
        }
        served.push(dest);
    }

    let mut repo_add = Command::new("repo-add");
    repo_add.arg(db).args(&served);
    match run_visible(&mut repo_add) {
        Ok(()) => {}
        Err(RunErr::Spawn(e)) => {
            return Err(format!("{e} — repo-add ships with the `pacman` package"))
        }
        Err(RunErr::Status(e)) => return Err(e),
    }
    Ok(served.len())
}

fn sig(package_file: &Path) -> PathBuf {
    let mut path = package_file.as_os_str().to_os_string();
    path.push(".sig");
    PathBuf::from(path)
}

/// Everything makepkg left in `PKGDEST` that is a package, sorted.
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

/// Is this file name one of makepkg's package files?
///
/// The shape is `<pkgname>-<version>-<rel>-<arch>.pkg.tar<.compression>`;
/// `PKGEXT` decides whether there is a compression suffix and which, so the
/// tail is matched loosely rather than against a list of algorithms. The two
/// exclusions are what matters: a detached signature is not a package (it
/// travels with one), and a partial download or a `.pkg.tar.zst.tmp` left by
/// something else is not either.
fn is_artifact(name: &str) -> bool {
    if name.ends_with(".sig") {
        return false;
    }
    let Some((stem, tail)) = name.split_once(".pkg.tar") else {
        return false;
    };
    if stem.is_empty() || stem.starts_with('.') {
        return false;
    }
    tail.is_empty() || (tail.starts_with('.') && !tail[1..].is_empty() && !tail[1..].contains('.'))
}

// --------------------------------------------------------------- plumbing

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
    let shown = cmd.get_args().fold(program.clone(), |mut line, arg| {
        line.push(' ');
        line.push_str(&shell_quote(&arg.to_string_lossy()));
        line
    });
    println!("run       {shown}");
    let status = cmd
        .status()
        .map_err(|e| RunErr::Spawn(format!("{program}: {e}")))?;
    if !status.success() {
        return Err(RunErr::Status(format!("{shown} failed ({status})")));
    }
    Ok(())
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
