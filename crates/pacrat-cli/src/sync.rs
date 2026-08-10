//! `pacrat sync` — this host against the store: what the manifest says should
//! be installed here, what actually is, and the exact commands that would
//! close the gap. It prints them. It runs none of them.
//!
//! ## Self-sync only
//!
//! ADR-001's open question 4 — sync over ssh to remote hosts, or each host
//! syncing itself — is unresolved, so this verb answers only the half that
//! needs no decision: the machine it is running on. There is no `<host>`
//! argument, and another host's drift is closed by running `pacrat sync`
//! *there*. A `--host slab` that quietly planned against this machine's
//! installed set would be the wrong answer to a reasonable-looking question,
//! so the question is not offered.
//!
//! ## Why it executes nothing
//!
//! `setup` set the precedent for the root-owned half of pacrat: print the
//! sudo lines, run only what the invoking user could already run unaided.
//! Sync goes one step further and runs nothing at all — the human reading the
//! list is the gate, which is the same argument the whole curation model
//! makes. The one exception it may take for itself: none.
//!
//! That is also why the marker contract (ADR-001; `setup.rs`, `MARKER`) never
//! comes up here — no pacman transaction, so no PreTransaction hook, so
//! nothing to stand down. **If sync ever grows an `--apply`, the marker must
//! bracket any pacman invocation it makes**: write `pid=<pid> started=<unix
//! seconds>` into the repo directory before, remove it after. `pacman -S`
//! from the curated repo happens to pass the guard on its own (every name it
//! installs resolves in a sync db, which is exactly what the guard checks) —
//! but that is a property of *today's* guard, not a licence to skip the
//! marker, and anything installing a package *file* is aborted without it.
//!
//! ## Exit codes
//!
//! 0 in sync · 10 a plan was printed · 1 the check could not run.
//!
//! Ten is ADR-001's "ran fine, deliberately did not act" — which is every
//! non-empty run of this verb, because acting is the caller's job. Extras
//! without `--prune` are drift too, so they also earn a 10: the run found
//! work, it just found work whose resolution is a choice between two
//! commands rather than one.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use pacrat_core::pkg::Source;
use pacrat_core::sources::Sources;
use serde::Serialize;

use crate::ctx::Ctx;
use crate::live::{self, SourceDrift};
use crate::out::{list_preview, shell_quote};
use crate::setup;

/// How many names a descriptive line prints before it starts counting. The
/// *command* lines are never capped — a truncated command is a lie about what
/// to paste — so this bounds the prose only.
const CAP: usize = 20;

pub fn run(ctx: &Ctx, prune: bool, json: bool) -> Result<(), String> {
    let repo = &ctx.config.repo;
    let repo_path = PathBuf::from(&repo.path);
    let ledger = ctx.load_sources()?;
    let served = served(&repo_path);
    let drifts = live::host_drift(ctx)?;
    let state = setup::state(repo);

    // An empty manifest is not agreement. Every installed package would read
    // as an extra, and a thousand-name prune list is not a plan — it is the
    // shape of a question nobody asked.
    if drifts.iter().all(|d| d.tracked.is_empty()) {
        let installed: usize = drifts.iter().map(|d| d.installed.len()).sum();
        if json {
            let text = serde_json::to_string_pretty(&Untracked {
                host: &ctx.host,
                tracked: false,
                in_sync: false,
                installed,
            })
            .map_err(|e| format!("json: {e}"))?;
            println!("{text}");
        } else {
            println!("pacrat sync — {} against the store", ctx.host);
            println!();
            println!(
                "no tracked lists for {} under {}",
                ctx.host,
                ctx.packages_dir().display()
            );
            println!("the store has no opinion about this host yet, so there is nothing to");
            println!("sync it *to* — all {installed} installed packages would read as extras.");
            println!("`pacrat add <packages>` adopts what belongs here first.");
        }
        std::process::exit(crate::HELD);
    }

    let plan = plan(&drifts, &served, &ledger);
    let commands = plan.commands(prune);

    if json {
        report_json(ctx, &drifts, &plan, &commands, prune, &state)?;
    } else {
        report_text(ctx, &drifts, &plan, &commands, prune, &state);
    }

    if plan.in_sync() {
        Ok(())
    } else {
        std::process::exit(crate::HELD)
    }
}

// ------------------------------------------------------------------ the plan

/// How a missing package gets onto this host — the whole classification, and
/// the only place the answer is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// An official-repo package: `pacman -S` resolves it today.
    Install,
    /// Curated and already built here, so `pacman -S` resolves it too — out of
    /// the local repo. Told apart from `Install` because only this one depends
    /// on the section in pacman.conf and a database new enough to know it.
    InstallCurated,
    /// Vendored, but this host has never built it — nothing serves the name.
    BuildFirst,
    /// Tracked AUR, not vendored. pacrat will not install it, and cannot: the
    /// guard aborts exactly the build-it-yourself path that would.
    VendorFirst,
    /// A flatpak; needs no root and no curation.
    Flatpak,
}

/// What this host needs, split by how it gets it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Missing and installable now, official and curated names together —
    /// they come from sync repos either way, so they are one transaction.
    install: Vec<String>,
    /// The subset of `install` served by the local repo. Not a separate
    /// command, only a separate caveat: those are the names a host without
    /// the pacman.conf section, or with a database older than the build,
    /// will fail to resolve.
    curated: Vec<String>,
    /// Missing, vendored, not built here yet.
    build_first: Vec<String>,
    /// Missing, tracked AUR, never brought through curation.
    vendor_first: Vec<String>,
    /// Missing flatpaks.
    flatpak: Vec<String>,
    /// Installed here and tracked by nothing, per source. Kept split because
    /// an untracked *foreign* package is a different worry from an untracked
    /// official one — something arrived outside curation — even though both
    /// leave by the same `pacman -Rns`.
    extras: Vec<(Source, Vec<String>)>,
}

/// Classify this host's drift. Pure: every input is data, so the matrix is
/// testable without a pacman, a repo directory, or a store.
///
/// `served` is the set of names the local repo has package files for, and
/// `ledger` is `aur/sources.toml` — between them they answer "is this curated,
/// and if so has it been built here".
fn plan(drifts: &[SourceDrift], served: &BTreeSet<String>, ledger: &Sources) -> Plan {
    let mut plan = Plan::default();
    for sd in drifts {
        for package in &sd.drift.missing {
            let class = classify(package, sd.source, served, ledger);
            if class == Class::InstallCurated {
                plan.curated.push(package.clone());
            }
            let bucket = match class {
                Class::Install | Class::InstallCurated => &mut plan.install,
                Class::BuildFirst => &mut plan.build_first,
                Class::VendorFirst => &mut plan.vendor_first,
                Class::Flatpak => &mut plan.flatpak,
            };
            bucket.push(package.clone());
        }
        if !sd.drift.extra.is_empty() {
            plan.extras.push((sd.source, sd.drift.extra.clone()));
        }
    }
    // Drift arrives sorted per source; merging native and curated names into
    // one install list is the only place that ordering is disturbed.
    plan.install.sort();
    plan.install.dedup();
    plan.curated.sort();
    plan.curated.dedup();
    plan
}

fn classify(package: &str, source: Source, served: &BTreeSet<String>, ledger: &Sources) -> Class {
    if source == Source::Flatpak {
        return Class::Flatpak;
    }
    // The ledger outranks the tracked list's source column, including for a
    // name an official repo also carries: a human vendored it deliberately,
    // and the curated build is the one this fleet decided to run.
    if ledger.packages.contains_key(package) {
        return if served.contains(package) {
            Class::InstallCurated
        } else {
            Class::BuildFirst
        };
    }
    match source {
        // Tracked as native means pacman resolves it from an official repo.
        Source::Native => Class::Install,
        // Tracked as foreign and not in the ledger. There is no command that
        // installs this and stays inside the model — naming the curation step
        // is the whole answer, not a fallback for want of a better one.
        Source::Aur | Source::Flatpak => Class::VendorFirst,
    }
}

impl Plan {
    /// Nothing missing and nothing extra: the store and this host agree.
    fn in_sync(&self) -> bool {
        self.missing() == 0 && self.extras.is_empty()
    }

    fn missing(&self) -> usize {
        self.install.len() + self.build_first.len() + self.vendor_first.len() + self.flatpak.len()
    }

    /// Extras that leave by pacman: native and foreign together, sorted, since
    /// one `-Rns` removes both.
    fn extra_pacman(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .extras
            .iter()
            .filter(|(s, _)| *s != Source::Flatpak)
            .flat_map(|(_, names)| names.iter().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn extra_flatpak(&self) -> &[String] {
        self.extras
            .iter()
            .find(|(s, _)| *s == Source::Flatpak)
            .map_or(&[], |(_, names)| names.as_slice())
    }

    /// The commands, in an order that can be run top to bottom.
    ///
    /// Curation first (a vendor is a review, and a build needs the reviewed
    /// tree), then the installs, then the removals — a prune that ran before
    /// an install could take a dependency out from under it. Names bound for
    /// `vendor` and `build` are deliberately absent from the install line:
    /// they are not installable yet, and listing them there would produce a
    /// paste that fails halfway.
    fn commands(&self, prune: bool) -> Vec<String> {
        let mut cmds = Vec::new();
        // `vendor` takes one package and prompts for a review of it; there is
        // no batch form to print, and there should not be.
        for package in &self.vendor_first {
            cmds.push(format!("pacrat vendor {}", shell_quote(package)));
        }
        if !self.build_first.is_empty() {
            cmds.push(format!("pacrat build {}", quoted(&self.build_first)));
        }
        if !self.install.is_empty() {
            cmds.push(format!(
                "sudo pacman -S --needed -- {}",
                quoted(&self.install)
            ));
        }
        if !self.flatpak.is_empty() {
            cmds.push(format!(
                "flatpak install -y --noninteractive {}",
                quoted(&self.flatpak)
            ));
        }
        if prune {
            let extra = self.extra_pacman();
            if !extra.is_empty() {
                cmds.push(format!("sudo pacman -Rns -- {}", quoted(&extra)));
            }
            let flatpaks = self.extra_flatpak();
            if !flatpaks.is_empty() {
                cmds.push(format!(
                    "flatpak uninstall -y --noninteractive {}",
                    quoted(flatpaks)
                ));
            }
        }
        cmds
    }
}

/// Every name shell-quoted, space-joined — the argument tail of a command
/// line. Uncapped on purpose: this is meant to be pasted.
fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| shell_quote(n))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------- the repo

/// The package names the local repo directory has files for.
///
/// "Has it been built here" is a question about files on disk, and that is
/// deliberately not the same question as "can pacman install it" — the latter
/// also needs the section in `pacman.conf` and a synced database, which are
/// `setup`'s business and are reported separately rather than folded in. A
/// package built but unreachable should say *that*, not "not built".
fn served(repo_path: &Path) -> BTreeSet<String> {
    fs::read_dir(repo_path)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| pkgname_of(&e.file_name().to_string_lossy()))
                .collect()
        })
        .unwrap_or_default()
}

/// `python-lsp-server-1.12.0-1-any.pkg.tar.zst` → `python-lsp-server`.
///
/// pkgver, pkgrel and arch may not contain a hyphen — makepkg enforces it,
/// because this filename is the only place the four fields are ever joined —
/// so the last three hyphen-separated fields are always version, release and
/// architecture, and everything before them is the name however many hyphens
/// it has. `.sig` files parse to the same name, which is harmless in a set,
/// and the repo database (`<repo>.db.tar.zst`) has no `.pkg.tar.` in it at all.
fn pkgname_of(file: &str) -> Option<String> {
    let stem = &file[..file.find(".pkg.tar.")?];
    let mut fields = stem.rsplitn(4, '-');
    let (_arch, _pkgrel, _pkgver) = (fields.next()?, fields.next()?, fields.next()?);
    let name = fields.next()?;
    (!name.is_empty()).then(|| name.to_string())
}

// ------------------------------------------------------------------- report

fn report_text(
    ctx: &Ctx,
    drifts: &[SourceDrift],
    plan: &Plan,
    commands: &[String],
    prune: bool,
    state: &setup::State,
) {
    let repo = &ctx.config.repo;
    println!("pacrat sync — {} against the store", ctx.host);
    println!();
    println!("store   {}", ctx.store.display());
    println!(
        "host    {}  (pacrat plans for the machine it runs on)",
        ctx.host
    );
    println!(
        "repo    [{}] {}{}",
        repo.name,
        repo.path,
        if state.complete() {
            String::new()
        } else {
            format!(
                "  (missing {} — `pacrat setup`)",
                state.missing().join(" + ")
            )
        }
    );
    println!();

    println!("drift on {} (tracked vs installed)", ctx.host);
    for sd in drifts {
        let d = &sd.drift;
        if d.in_sync() {
            println!(
                "  {:<8} in sync ({} packages)",
                sd.source.name(),
                sd.tracked.len()
            );
        } else {
            println!(
                "  {:<8} {} missing · {} extra   [{}]",
                sd.source.name(),
                d.missing.len(),
                d.extra.len(),
                live::query_argv(sd.source)
            );
        }
    }
    println!();

    if plan.in_sync() {
        println!("nothing to do — this host matches the store.");
        return;
    }

    println!("plan");
    if !plan.vendor_first.is_empty() {
        println!(
            "  curate   {} tracked AUR package{} never came through curation:",
            plan.vendor_first.len(),
            plural(plan.vendor_first.len())
        );
        println!("           {}", list_preview(&plan.vendor_first, CAP));
        println!("           pacrat cannot install those, and will not pretend to:");
        println!("           building one outside the gate is the bypass the guard");
        println!("           exists to abort. Vendor first — that step is the review.");
    }
    if !plan.build_first.is_empty() {
        println!(
            "  build    {} vendored but not yet built on this host:",
            plan.build_first.len()
        );
        println!("           {}", list_preview(&plan.build_first, CAP));
        println!("           nothing in the repo directory serves those names yet.");
    }
    if !plan.install.is_empty() {
        println!(
            "  install  {} missing package{}:",
            plan.install.len(),
            plural(plan.install.len())
        );
        println!("           {}", list_preview(&plan.install, CAP));
    }
    if !plan.flatpak.is_empty() {
        println!(
            "  flatpak  {} missing app{}: {}",
            plan.flatpak.len(),
            plural(plan.flatpak.len()),
            list_preview(&plan.flatpak, CAP)
        );
    }
    if !plan.extras.is_empty() {
        let total: usize = plan.extras.iter().map(|(_, n)| n.len()).sum();
        println!("  extra    {total} installed here and tracked by nothing:",);
        for (source, names) in &plan.extras {
            println!(
                "           {:<8} {}",
                source.name(),
                list_preview(names, CAP)
            );
        }
        if prune {
            println!("           --prune: the removals are below. `-Rns` takes unused");
            println!("           dependencies with them — read the list pacman prints.");
        } else {
            println!("           two ways out, and they mean opposite things: `pacrat add");
            println!("           <packages>` says they belong here, `pacrat sync --prune`");
            println!("           prints the removals. Neither is a default.");
        }
    }

    println!();
    println!("commands  nothing below has been run — `pacrat sync` only prints");
    for cmd in commands {
        println!("  {cmd}");
    }
    if commands.is_empty() {
        println!("  (none — every item above needs a decision first)");
    }
    // Only worth saying when a curated name is actually in play: for an
    // official-repo-only plan, neither caveat can bite.
    let curated_in_play = !plan.curated.is_empty() || !plan.build_first.is_empty();
    if curated_in_play && !state.conf_section {
        println!();
        println!(
            "note      [{}] is not in /etc/pacman.conf, so curated names cannot",
            repo.name
        );
        println!("          resolve — `pacrat setup` prints the section to add.");
    } else if !plan.curated.is_empty() {
        println!();
        println!(
            "note      {} come{} from the curated repo. If pacman cannot find one,",
            list_preview(&plan.curated, CAP),
            plural_verb(plan.curated.len())
        );
        println!("          its sync database predates the build: `sudo pacman -Syu`");
        println!("          first. A bare `-Sy` before an install is the partial-upgrade");
        println!("          footgun; don't.");
    }
}

/// The verb ending that agrees with a list of `n` names.
fn plural_verb(n: usize) -> &'static str {
    if n == 1 {
        "s"
    } else {
        ""
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// The machine-readable plan. Everything the exit code turns on is in here,
/// and so is the drift it came from: a consumer that has to re-run the
/// queries to learn why a package landed in `vendor_first` does not have a
/// machine format.
#[derive(Serialize)]
struct Report<'a> {
    host: &'a str,
    /// This host has tracked lists at all. False is the early-exit case.
    tracked: bool,
    in_sync: bool,
    /// Were the prune commands asked for? The command list depends on it.
    prune: bool,
    drift: Vec<DriftJson<'a>>,
    missing: MissingJson<'a>,
    extras: Vec<ExtrasJson<'a>>,
    repo: RepoJson<'a>,
    /// Shell-ready, in the order they may be run. Never truncated.
    commands: &'a [String],
}

#[derive(Serialize)]
struct DriftJson<'a> {
    source: &'a str,
    tracked: usize,
    installed: usize,
    missing: &'a [String],
    extra: &'a [String],
    /// The live query behind `installed` — ADR-001's always-visible calls
    /// survive `--json` by travelling inside it.
    query: &'a str,
}

/// Missing packages by what they need, not by which list they came from.
#[derive(Serialize)]
struct MissingJson<'a> {
    install: &'a [String],
    /// The subset of `install` the local repo serves rather than an official
    /// repo — the names that need the pacman.conf section and a current db.
    install_curated: &'a [String],
    build_first: &'a [String],
    vendor_first: &'a [String],
    flatpak: &'a [String],
}

#[derive(Serialize)]
struct ExtrasJson<'a> {
    source: &'a str,
    packages: &'a [String],
}

#[derive(Serialize)]
struct RepoJson<'a> {
    name: &'a str,
    path: &'a str,
    /// Fully set up: repo, pacman.conf section, guard. When false, curated
    /// names may not resolve however well the plan reads.
    serving: bool,
    missing: Vec<&'static str>,
}

/// The shape of an early exit: this host is not in the manifest at all.
#[derive(Serialize)]
struct Untracked<'a> {
    host: &'a str,
    tracked: bool,
    in_sync: bool,
    installed: usize,
}

fn report_json(
    ctx: &Ctx,
    drifts: &[SourceDrift],
    plan: &Plan,
    commands: &[String],
    prune: bool,
    state: &setup::State,
) -> Result<(), String> {
    let report = Report {
        host: &ctx.host,
        tracked: true,
        in_sync: plan.in_sync(),
        prune,
        drift: drifts
            .iter()
            .map(|sd| DriftJson {
                source: sd.source.name(),
                tracked: sd.tracked.len(),
                installed: sd.installed.len(),
                missing: &sd.drift.missing,
                extra: &sd.drift.extra,
                query: live::query_argv(sd.source),
            })
            .collect(),
        missing: MissingJson {
            install: &plan.install,
            install_curated: &plan.curated,
            build_first: &plan.build_first,
            vendor_first: &plan.vendor_first,
            flatpak: &plan.flatpak,
        },
        extras: plan
            .extras
            .iter()
            .map(|(source, packages)| ExtrasJson {
                source: source.name(),
                packages,
            })
            .collect(),
        repo: RepoJson {
            name: &ctx.config.repo.name,
            path: &ctx.config.repo.path,
            serving: state.complete(),
            missing: state.missing(),
        },
        commands,
    };
    let text = serde_json::to_string_pretty(&report).map_err(|e| format!("json: {e}"))?;
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pacrat_core::pkg::Drift;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// One source's drift, straight from data — no store, no pacman.
    fn sd(source: Source, missing: &[&str], extra: &[&str]) -> SourceDrift {
        SourceDrift {
            source,
            tracked: v(missing),
            installed: v(extra),
            drift: Drift {
                missing: v(missing),
                extra: v(extra),
            },
        }
    }

    fn served_set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    const LEDGER: &str = r#"
[packages.mdcat]
upstream = "https://aur.archlinux.org/mdcat.git"
reviewed = "3f9c21ab"
role = "vendored"

[packages.playtimed]
upstream = "https://aur.archlinux.org/playtimed.git"
reviewed = "aa00bb11"
role = "maintained"
"#;

    fn ledger() -> Sources {
        Sources::from_toml(LEDGER).unwrap()
    }

    // ---- the classification matrix ----

    /// Every cell of (source × in ledger × served), which is the whole verb.
    #[test]
    fn classification_matrix() {
        let served = served_set(&["mdcat"]);
        let l = ledger();
        let cases: &[(&str, Source, Class)] = &[
            // Not curated. Native resolves from an official repo; foreign
            // does not resolve at all until it has been through the gate.
            ("ripgrep", Source::Native, Class::Install),
            ("pacseek", Source::Aur, Class::VendorFirst),
            // Curated and built here: an ordinary repo package now.
            ("mdcat", Source::Aur, Class::InstallCurated),
            // Curated, never built here: nothing serves the name.
            ("playtimed", Source::Aur, Class::BuildFirst),
            // Flatpaks are outside the ladder entirely.
            ("org.gnome.Loupe", Source::Flatpak, Class::Flatpak),
        ];
        for (package, source, want) in cases {
            assert_eq!(
                classify(package, *source, &served, &l),
                *want,
                "{package} ({})",
                source.name()
            );
        }
    }

    /// The ledger outranks the source column both ways: a curated name tracked
    /// as native still comes from the curated repo, and still has to be built
    /// before it can.
    #[test]
    fn the_ledger_outranks_the_tracked_lists_source() {
        let l = ledger();
        assert_eq!(
            classify("mdcat", Source::Native, &served_set(&["mdcat"]), &l),
            Class::InstallCurated
        );
        assert_eq!(
            classify("mdcat", Source::Native, &served_set(&[]), &l),
            Class::BuildFirst
        );
    }

    /// A served name that nobody vendored is not curated — the ledger is the
    /// authority, not the directory listing. (A stray file in the repo dir
    /// must not promote a package into the install line.)
    #[test]
    fn a_served_file_alone_does_not_make_a_package_curated() {
        let l = Sources::default();
        assert_eq!(
            classify("pacseek", Source::Aur, &served_set(&["pacseek"]), &l),
            Class::VendorFirst
        );
    }

    #[test]
    fn a_plan_splits_missing_by_class_and_keeps_extras_by_source() {
        let drifts = vec![
            sd(Source::Native, &["ripgrep", "mdcat"], &["cowsay"]),
            sd(Source::Aur, &["pacseek", "playtimed"], &["sneaky"]),
            sd(Source::Flatpak, &["org.gnome.Loupe"], &[]),
        ];
        let p = plan(&drifts, &served_set(&["mdcat"]), &ledger());
        assert_eq!(p.install, v(&["mdcat", "ripgrep"]));
        // ripgrep comes from an official repo; only mdcat needs the local one.
        assert_eq!(p.curated, v(&["mdcat"]));
        assert_eq!(p.build_first, v(&["playtimed"]));
        assert_eq!(p.vendor_first, v(&["pacseek"]));
        assert_eq!(p.flatpak, v(&["org.gnome.Loupe"]));
        assert_eq!(
            p.extras,
            vec![
                (Source::Native, v(&["cowsay"])),
                (Source::Aur, v(&["sneaky"]))
            ]
        );
        assert_eq!(p.extra_pacman(), v(&["cowsay", "sneaky"]));
        assert!(p.extra_flatpak().is_empty());
        assert!(!p.in_sync());
        assert_eq!(p.missing(), 5);
    }

    #[test]
    fn agreement_is_an_empty_plan() {
        let drifts = vec![
            sd(Source::Native, &[], &[]),
            sd(Source::Aur, &[], &[]),
            sd(Source::Flatpak, &[], &[]),
        ];
        let p = plan(&drifts, &served_set(&[]), &ledger());
        assert!(p.in_sync());
        assert_eq!(p, Plan::default());
        assert!(p.commands(true).is_empty());
    }

    /// Extras alone are still drift: something is installed that the store
    /// does not account for, and the exit code has to say so.
    #[test]
    fn extras_alone_are_not_in_sync() {
        let drifts = vec![sd(Source::Native, &[], &["cowsay"])];
        let p = plan(&drifts, &served_set(&[]), &ledger());
        assert!(!p.in_sync());
        assert_eq!(p.missing(), 0);
        // ...but without --prune there is nothing to run.
        assert!(p.commands(false).is_empty());
    }

    // ---- the commands ----

    #[test]
    fn commands_run_top_to_bottom_in_dependency_order() {
        let drifts = vec![
            sd(Source::Native, &["ripgrep"], &["cowsay"]),
            sd(Source::Aur, &["pacseek", "playtimed", "mdcat"], &[]),
            sd(Source::Flatpak, &["org.gnome.Loupe"], &["org.old.App"]),
        ];
        let p = plan(&drifts, &served_set(&["mdcat"]), &ledger());
        assert_eq!(
            p.commands(true),
            vec![
                "pacrat vendor pacseek",
                "pacrat build playtimed",
                "sudo pacman -S --needed -- mdcat ripgrep",
                "flatpak install -y --noninteractive org.gnome.Loupe",
                "sudo pacman -Rns -- cowsay",
                "flatpak uninstall -y --noninteractive org.old.App",
            ]
        );
    }

    /// Removals are opt-in, and their absence must not change anything else.
    #[test]
    fn without_prune_the_removals_are_simply_absent() {
        let drifts = vec![sd(Source::Native, &["ripgrep"], &["cowsay"])];
        let p = plan(&drifts, &served_set(&[]), &ledger());
        assert_eq!(
            p.commands(false),
            vec!["sudo pacman -S --needed -- ripgrep"]
        );
    }

    /// Names that need vendoring or building are not in the install line —
    /// a paste that fails halfway is worse than one command fewer.
    #[test]
    fn uninstallable_names_stay_out_of_the_install_line() {
        let drifts = vec![sd(Source::Aur, &["pacseek", "playtimed"], &[])];
        let p = plan(&drifts, &served_set(&[]), &ledger());
        let cmds = p.commands(false);
        assert!(!cmds.iter().any(|c| c.contains("pacman -S")), "{cmds:?}");
        assert_eq!(
            cmds,
            vec!["pacrat vendor pacseek", "pacrat build playtimed"]
        );
    }

    /// `vendor` takes one package at a time, so N packages are N lines.
    #[test]
    fn every_vendor_gets_its_own_line() {
        let drifts = vec![sd(Source::Aur, &["a-pkg", "b-pkg", "c-pkg"], &[])];
        let p = plan(&drifts, &served_set(&[]), &Sources::default());
        assert_eq!(
            p.commands(false),
            vec![
                "pacrat vendor a-pkg",
                "pacrat vendor b-pkg",
                "pacrat vendor c-pkg"
            ]
        );
    }

    /// A package name is data out of a synced, hand-editable text file, and
    /// these lines are meant to be pasted into a shell. Two different defences
    /// are doing two different jobs, and each covers what the other cannot:
    /// quoting stops a name being read as *syntax*, and `--` stops a name that
    /// needs no quoting at all — `-Rns` is bare word characters throughout —
    /// being read as an *option*.
    #[test]
    fn hostile_package_names_cannot_escape_the_printed_command() {
        let drifts = vec![sd(
            Source::Native,
            &["a;rm -rf ~", "-Rns"],
            &["$(id)", "two words"],
        )];
        let p = plan(&drifts, &served_set(&[]), &Sources::default());
        let cmds = p.commands(true);
        assert_eq!(
            cmds,
            vec![
                "sudo pacman -S --needed -- -Rns 'a;rm -rf ~'",
                "sudo pacman -Rns -- '$(id)' 'two words'",
            ]
        );
        for cmd in &cmds {
            assert!(cmd.contains(" -- "), "no argument separator: {cmd}");
        }
    }

    /// The flatpak lines have no `--` because flatpak's own parser has no such
    /// convention — so an app id is quoted and nothing more is claimed for it.
    /// Application ids are reverse-DNS and cannot begin with a hyphen; if that
    /// ever stops being true, this is where it bites.
    #[test]
    fn flatpak_lines_quote_their_app_ids() {
        let drifts = vec![sd(
            Source::Flatpak,
            &["org.gnome.Loupe"],
            &["org.evil.$(id)"],
        )];
        let p = plan(&drifts, &served_set(&[]), &Sources::default());
        assert_eq!(
            p.commands(true),
            vec![
                "flatpak install -y --noninteractive org.gnome.Loupe",
                "flatpak uninstall -y --noninteractive 'org.evil.$(id)'",
            ]
        );
    }

    // ---- reading the repo directory ----

    #[test]
    fn a_package_file_yields_its_name_however_many_hyphens_it_has() {
        assert_eq!(
            pkgname_of("mdcat-2.7.1-1-x86_64.pkg.tar.zst").as_deref(),
            Some("mdcat")
        );
        assert_eq!(
            pkgname_of("python-lsp-server-1.12.0-1-any.pkg.tar.zst").as_deref(),
            Some("python-lsp-server")
        );
        // Other compressions, and a signature beside its package.
        assert_eq!(
            pkgname_of("foo-1.0-2-any.pkg.tar.xz").as_deref(),
            Some("foo")
        );
        assert_eq!(
            pkgname_of("foo-1.0-2-any.pkg.tar.zst.sig").as_deref(),
            Some("foo")
        );
        // An epoch lives in pkgver, which is the field before pkgrel.
        assert_eq!(
            pkgname_of("bar-2:1.0-1-x86_64.pkg.tar.zst").as_deref(),
            Some("bar")
        );
    }

    #[test]
    fn everything_else_in_the_repo_directory_is_not_a_package() {
        for file in [
            "dotfiles-aur.db.tar.zst",
            "dotfiles-aur.files.tar.zst",
            "dotfiles-aur.db",
            ".pacrat-transaction",
            "toofew-1.pkg.tar.zst",
            "",
        ] {
            assert_eq!(pkgname_of(file), None, "{file:?} parsed as a package");
        }
    }

    #[test]
    fn served_reads_names_out_of_a_directory_and_survives_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("pacrat-sync-served-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for file in [
            "mdcat-2.7.1-1-x86_64.pkg.tar.zst",
            "mdcat-2.7.1-1-x86_64.pkg.tar.zst.sig",
            "python-lsp-server-1.12.0-1-any.pkg.tar.zst",
            "dotfiles-aur.db.tar.zst",
        ] {
            fs::write(dir.join(file), "").unwrap();
        }
        let names = served(&dir);
        assert_eq!(names, served_set(&["mdcat", "python-lsp-server"]));
        fs::remove_dir_all(&dir).unwrap();
        // A host that has never run `pacrat setup` has no repo directory;
        // that is "nothing is built here", not an error.
        assert!(served(&dir).is_empty());
    }
}
