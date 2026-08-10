//! `pacrat vendor` — take custody of a package's build tree: the
//! tracked → vendored rung of the custody ladder. Clones the upstream to a
//! scratch dir, shows the human what they are about to trust, and on their
//! say-so installs the pristine tree into `<store>/aur/packages/<name>/` and
//! records the reviewed commit in the ledger.
//!
//! Upstream is any git URL. The AUR is only the default, never a special
//! case — no forge-specific parsing lives here (ADR-001, sources.rs).
//!
//! Vendoring is a one-time entry to the ladder: later commits arrive through
//! the review gate, which is why an existing tree or ledger entry is refused
//! rather than silently overwritten.
//!
//! Everything cloned here is untrusted input, and the review step is the
//! only thing standing between it and every host in the fleet. That shapes
//! three choices: the tree is validated before it is shown ([`crate::fstree`]),
//! the text is neutered before it is printed ([`crate::out::visible`]), and
//! declining exits non-zero so `vendor && build` cannot read a "no" as a go.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use pacrat_core::sources::{Role, SourceEntry, Sources};

use crate::ctx::Ctx;
use crate::fstree;
use crate::out::{list_preview, visible};

/// Exit code for "ran fine, deliberately did not act". ADR-001 gives this
/// meaning to 10 for the headless update loop (0 clean, 10 holds present,
/// 1 failure); a declined review is the same kind of outcome, and sharing
/// the code keeps `pacrat vendor x && pacrat build x` honest.
const HELD: i32 = 10;

/// The ledger `Role` as a CLI flag; core owns the model, clap owns the words.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum RoleArg {
    /// Third-party: pull-only, updates adopted through the review gate
    Vendored,
    /// Ours: vendored plus push rights back to the upstream
    Maintained,
}

impl From<RoleArg> for Role {
    fn from(role: RoleArg) -> Self {
        match role {
            RoleArg::Vendored => Role::Vendored,
            RoleArg::Maintained => Role::Maintained,
        }
    }
}

impl RoleArg {
    fn name(self) -> &'static str {
        match self {
            RoleArg::Vendored => "vendored",
            RoleArg::Maintained => "maintained",
        }
    }
}

/// One resolved invocation: everything decided before the network is touched.
struct Plan<'a> {
    package: &'a str,
    upstream: String,
    role: RoleArg,
    yes: bool,
    force: bool,
    /// Scratch clone, ours to delete.
    clone: PathBuf,
    /// Final home in the store.
    tree: PathBuf,
}

/// Why a package cannot be vendored again. The three states are not the same
/// mistake, and the advice differs, so they are not the same message.
#[derive(Debug, PartialEq, Eq)]
enum Held {
    /// Ledger entry and tree: a finished vendoring.
    Vendored,
    /// Ledger entry, no tree: someone deleted the tree, or a sync half-landed.
    LedgerOnly,
    /// Tree, no ledger entry: a vendoring that died before its last step.
    TreeOnly,
}

fn held(in_ledger: bool, on_disk: bool) -> Option<Held> {
    match (in_ledger, on_disk) {
        (true, true) => Some(Held::Vendored),
        (true, false) => Some(Held::LedgerOnly),
        (false, true) => Some(Held::TreeOnly),
        (false, false) => None,
    }
}

impl Held {
    fn explain(&self, package: &str, tree: &Path) -> String {
        match self {
            // The review gate is the way forward only when there is an entry
            // to review against.
            Held::Vendored => format!(
                "{package} is already vendored — a new upstream commit is adopted \
                 through the review gate (`pacrat review {package}`), not by \
                 re-vendoring; pass --force to replace the tree and ledger entry"
            ),
            Held::LedgerOnly => format!(
                "{package} is in aur/sources.toml but has no tree at {} — \
                 the store is inconsistent; --force re-vendors it from upstream",
                tree.display()
            ),
            Held::TreeOnly => format!(
                "{package} has a tree at {} but no ledger entry — an earlier \
                 vendoring did not finish, so there is nothing to review \
                 against; --force replaces the tree and writes the entry",
                tree.display()
            ),
        }
    }
}

/// Package names: pacman's alphabet, as an allowlist. The name becomes a
/// path component in the store and an argument to git, so anything outside
/// this set is refused rather than escaped.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '+' | '-'))
}

pub fn run(
    ctx: &Ctx,
    package: &str,
    upstream: Option<&str>,
    role: RoleArg,
    yes: bool,
    force: bool,
) -> Result<(), String> {
    if !valid_name(package) {
        return Err(format!(
            "{package:?} is not a package name — expected letters, digits, \
             and @._+- (no leading hyphen or dot)"
        ));
    }
    let upstream = match upstream {
        // A URL that looks like a flag would be one, to git.
        Some(u) if u.starts_with('-') => {
            return Err(format!(
                "{u:?} is not a URL — upstreams cannot begin with '-'"
            ))
        }
        Some("") => return Err("--upstream is empty".into()),
        Some(u) => u.to_string(),
        None => format!("https://aur.archlinux.org/{package}.git"),
    };

    let tree = ctx.store.join("aur").join("packages").join(package);
    // Refuse before the network: the answer does not depend on upstream.
    if !force {
        let sources = ctx.load_sources()?;
        if let Some(h) = held(sources.packages.contains_key(package), tree.exists()) {
            return Err(h.explain(package, &tree));
        }
    }

    let plan = Plan {
        package,
        upstream,
        role,
        yes,
        force,
        clone: scratch_dir(package)?,
        tree,
    };

    let outcome = execute(ctx, &plan);
    // Scratch is ours either way; on failure it is evidence, so it stays.
    match outcome {
        Ok(Outcome::Vendored) => {
            let _ = fs::remove_dir_all(&plan.clone);
            Ok(())
        }
        Ok(Outcome::Declined) => {
            let _ = fs::remove_dir_all(&plan.clone);
            println!("not vendored");
            std::process::exit(HELD);
        }
        Err(e) if plan.clone.exists() => Err(format!(
            "{e}\n       clone kept at {}",
            plan.clone.display()
        )),
        Err(e) => Err(e),
    }
}

enum Outcome {
    Vendored,
    Declined,
}

fn execute(ctx: &Ctx, plan: &Plan) -> Result<Outcome, String> {
    println!("package   {}", plan.package);
    println!("upstream  {}", plan.upstream);
    // `--` so that an upstream cannot be read as a git option.
    git([
        OsStr::new("clone"),
        OsStr::new("--"),
        OsStr::new(&plan.upstream),
        plan.clone.as_os_str(),
    ])?;

    // Validates as it walks: symlinks, odd file types and unprintable names
    // are refused here rather than surprising us at copy time.
    let files = fstree::files(&plan.clone)?;
    if !files.iter().any(|f| f == "PKGBUILD") {
        // An AUR name that does not exist is not a git error: the server
        // hands back a perfectly good empty repository.
        return Err(if files.is_empty() {
            format!(
                "{} cloned empty — no such package upstream (an AUR name that \
                 does not exist yields an empty repo)",
                plan.upstream
            )
        } else {
            format!(
                "{} has no PKGBUILD — not a package repository",
                plan.upstream
            )
        });
    }

    let commit = git([
        OsStr::new("-C"),
        plan.clone.as_os_str(),
        OsStr::new("rev-parse"),
        OsStr::new("HEAD"),
    ])?;
    println!("commit    {commit}");
    println!("files     {}", list_preview(&files, 12));

    render_review(&plan.clone, &files)?;

    if !plan.yes && !confirm(plan.package, short(&commit))? {
        return Ok(Outcome::Declined);
    }

    commit_to_store(ctx, plan, &files, &commit)?;
    Ok(Outcome::Vendored)
}

/// Show the human everything that runs as them or as root: the PKGBUILD and
/// every `*.install` (whose hooks pacman executes as root at install time —
/// the classic place to hide a payload). Everything else is named, not
/// dumped, so the important text is not buried.
fn render_review(clone: &Path, files: &[String]) -> Result<(), String> {
    let mut shown: Vec<&String> = files
        .iter()
        .filter(|f| *f == "PKGBUILD" || f.ends_with(".install"))
        .collect();
    shown.sort_by_key(|f| (*f != "PKGBUILD", (*f).clone()));

    for rel in &shown {
        println!();
        println!("--- {rel} ---");
        let text = read_untrusted(&clone.join(rel))?;
        let (safe, hidden) = visible(&text);
        print!("{safe}");
        if !safe.ends_with('\n') {
            println!();
        }
        println!("--- end {rel} ---");
        if hidden > 0 {
            println!(
                "warning   {hidden} control character{} in {rel} shown as ␛-style \
                 stand-ins — text that tries to hide from a reviewer",
                if hidden == 1 { "" } else { "s" }
            );
        }
    }

    // A reading aid, not a parser: makepkg evaluates shell, we only echo the
    // lines, so this narrows where to look rather than claiming completeness.
    let pkgbuild = read_untrusted(&clone.join("PKGBUILD"))?;
    let declared = source_lines(&pkgbuild);
    if !declared.is_empty() {
        println!();
        println!("declared sources (PKGBUILD text, not evaluated):");
        for line in declared {
            let (safe, _) = visible(&line);
            println!("  {}", safe.trim());
        }
    }

    let others: Vec<String> = files
        .iter()
        .filter(|f| !shown.iter().any(|s| s == f))
        .cloned()
        .collect();
    if !others.is_empty() {
        println!();
        println!(
            "also in the tree (not shown): {}",
            list_preview(&others, 20)
        );
    }
    println!();
    Ok(())
}

/// Read a file we do not trust: invalid UTF-8 is shown lossily rather than
/// failing, so a mangled PKGBUILD cannot dodge the review by erroring.
fn read_untrusted(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Lines belonging to a `source=(…)` / `source_x86_64=(…)` array.
fn source_lines(pkgbuild: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    for line in pkgbuild.lines() {
        if depth == 0 {
            let opens_source = line
                .trim_start()
                .split_once("=(")
                .is_some_and(|(key, _)| key == "source" || key.starts_with("source_"));
            if !opens_source {
                continue;
            }
        }
        out.push(line.to_string());
        depth += line.matches('(').count();
        depth = depth.saturating_sub(line.matches(')').count());
    }
    out
}

/// Install the tree, then record it. Order matters: a tree with no ledger
/// entry is inert (nothing builds it) and is reported as a half-finished
/// vendoring, whereas an entry with no matching tree would be the ledger
/// lying about what was reviewed — the one thing this tool exists to prevent.
fn commit_to_store(ctx: &Ctx, plan: &Plan, files: &[String], commit: &str) -> Result<(), String> {
    // Re-read: the clone and the prompt above took unbounded time, and the
    // ledger is a whole-file write. Anything another process added in the
    // meantime has to survive ours.
    let mut sources: Sources = ctx.load_sources()?;
    if !plan.force {
        if let Some(h) = held(
            sources.packages.contains_key(plan.package),
            plan.tree.exists(),
        ) {
            return Err(format!(
                "{} (appeared during review)",
                h.explain(plan.package, &plan.tree)
            ));
        }
    }

    fstree::install(&plan.clone, files, &plan.tree)
        .map_err(|e| format!("{e}\n       the store's {} is unchanged", plan.package))?;

    sources.packages.insert(
        plan.package.to_string(),
        SourceEntry {
            upstream: plan.upstream.clone(),
            reviewed: commit.to_string(),
            role: plan.role.into(),
            note: None,
        },
    );
    ctx.save_sources(&sources).map_err(|e| {
        format!(
            "{e}\n       the tree at {} landed but the ledger did not — \
             re-run with --force",
            store_rel(&ctx.store, &plan.tree)
        )
    })?;

    println!(
        "vendored  {} file{} → {}",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        store_rel(&ctx.store, &plan.tree)
    );
    println!(
        "ledger    {} · {} @ {} ({})",
        store_rel(&ctx.store, &ctx.sources_path()),
        plan.package,
        short(commit),
        plan.role.name()
    );
    println!();
    println!(
        "next      commit the store, then `pacrat build {}`",
        plan.package
    );
    Ok(())
}

/// A scratch clone target under the system temp dir, created here rather
/// than left to git: 0700 up front means the untrusted tree is never
/// world-readable, and `create` failing on collision closes the window a
/// check-then-create would leave open.
fn scratch_dir(package: &str) -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pacrat-vendor-{package}-{}-{nanos}",
        std::process::id()
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// Run git, showing the argv first — every external call pacrat makes is
/// visible, and that rule has no exceptions for the quiet ones (ADR-001).
/// Returns trimmed stdout; a non-zero exit is an error carrying stderr.
fn git<const N: usize>(argv: [&OsStr; N]) -> Result<String, String> {
    let shown = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    println!("run       git {shown}");
    let out = Command::new("git")
        .args(argv)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {shown} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(8)]
}

/// A store path as the user thinks of it: relative to the store root.
fn store_rel(store: &Path, path: &Path) -> String {
    path.strip_prefix(store)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Ask. A closed stdin is a "no": unattended runs must use --yes explicitly
/// rather than have silence read as consent.
fn confirm(package: &str, commit: &str) -> Result<bool, String> {
    print!("vendor {package} at {commit}? [y/N] ");
    io::stdout().flush().map_err(|e| format!("stdout: {e}"))?;
    let mut answer = String::new();
    let read = io::stdin()
        .read_line(&mut answer)
        .map_err(|e| format!("stdin: {e}"))?;
    if read == 0 {
        println!();
        println!("no answer on stdin (use --yes to vendor unattended)");
        return Ok(false);
    }
    if !io::stdin().is_terminal() {
        // The typed newline is not echoed when stdin is a pipe.
        println!();
    }
    Ok(matches!(answer.trim(), "y" | "Y"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_an_allowlist() {
        for ok in ["mdcat", "python-a", "gtk+", "a.b_c@1", "0ad", "lib32-glibc"] {
            assert!(valid_name(ok), "{ok} should be valid");
        }
        for bad in [
            "",
            "-rf",
            ".hidden",
            "../escape",
            "a/b",
            "a b",
            "a;rm -rf /",
            "naïve",
            "a\nb",
            "$(id)",
        ] {
            assert!(!valid_name(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn held_covers_every_combination() {
        assert_eq!(held(true, true), Some(Held::Vendored));
        assert_eq!(held(true, false), Some(Held::LedgerOnly));
        assert_eq!(held(false, true), Some(Held::TreeOnly));
        assert_eq!(held(false, false), None);
    }

    #[test]
    fn only_a_finished_vendoring_is_sent_to_the_review_gate() {
        let tree = Path::new("/store/aur/packages/mdcat");
        // There is nothing to review against without a ledger entry, so the
        // other two states must not offer that advice.
        assert!(Held::Vendored
            .explain("mdcat", tree)
            .contains("pacrat review"));
        assert!(!Held::TreeOnly
            .explain("mdcat", tree)
            .contains("pacrat review"));
        assert!(!Held::LedgerOnly
            .explain("mdcat", tree)
            .contains("pacrat review"));
        // Each names the path the user has to look at.
        assert!(Held::TreeOnly
            .explain("mdcat", tree)
            .contains("/store/aur/packages/mdcat"));
        assert!(Held::LedgerOnly
            .explain("mdcat", tree)
            .contains("/store/aur/packages/mdcat"));
    }

    #[test]
    fn source_arrays_are_extracted_including_multiline() {
        let pkgbuild = "pkgname=x\nsource=(\"a.tar.gz\"\n        \"b.patch\")\nsha256sums=('x')\n";
        assert_eq!(
            source_lines(pkgbuild),
            vec!["source=(\"a.tar.gz\"", "        \"b.patch\")"]
        );
        assert_eq!(source_lines("pkgname=x\n"), Vec::<String>::new());
        // Not a source array, and must not be mistaken for one.
        assert_eq!(source_lines("sha256sums=('a')\n"), Vec::<String>::new());
        assert_eq!(
            source_lines("source_x86_64=('u')\n"),
            vec!["source_x86_64=('u')"]
        );
    }

    #[test]
    fn short_is_safe_on_odd_lengths() {
        assert_eq!(short("abc"), "abc");
        assert_eq!(short("0123456789abcdef"), "01234567");
        assert_eq!(short(""), "");
    }
}
