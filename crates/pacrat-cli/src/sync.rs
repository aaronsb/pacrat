//! `pacrat sync` — this host against the store: what the manifest says should
//! be installed here, what actually is, and the exact commands that move it
//! toward the store. It prints them; by default it runs none of them, and
//! `--run` (ADR-003) walks them one confirmed step at a time.
//!
//! *Toward*, not *into line*: a plan is not always one pass. A package that
//! has to be vendored and built is not installable until those steps have
//! happened, so the install line of the first run cannot mention it. The
//! report says so where it matters, and a second `pacrat sync` after the
//! curation steps is the rest of the answer.
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
//! ## Why the default executes nothing, and what `--run` changes
//!
//! `setup` set the precedent for the root-owned half of pacrat: print the
//! sudo lines, run only what the invoking user could already run unaided.
//! Sync's default goes one step further and runs nothing at all — the human
//! reading the list is the gate, which is the same argument the whole
//! curation model makes.
//!
//! `--run` (ADR-003) does not weaken that gate, it moves it per command:
//! every plan line walks through `elevate::Session` — printed, confirmed
//! with a y/n on a real terminal, then executed exactly as printed. The
//! argv is the plan's own data; the printed line is derived from it, never
//! the reverse, so what was confirmed is what runs. No terminal, no walk:
//! the plan prints as always and the commands stay yours. There is no
//! `--yes`, deliberately — a plan answered by a pipe is the accident the
//! question exists to catch.
//!
//! The marker contract (ADR-001; `setup.rs`, `MARKER`) still never comes up
//! here: `--run` executes the same commands the human would have pasted,
//! and those install from sync databases — which is exactly what the guard
//! checks — never package files. **If sync ever runs anything that hands
//! pacman a package *file*, the marker must bracket it**: write `pid=<pid>
//! started=<unix seconds>` into the repo directory before, remove it after.
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
//!
//! `--run` keeps the same scale: 0 when everything offered was confirmed
//! and ran, 10 when anything was declined (or there was no terminal to
//! ask on), 1 when a confirmed command failed.

use std::collections::{BTreeMap, BTreeSet};
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

pub fn run(ctx: &Ctx, prune: bool, json: bool, walk: bool) -> Result<(), String> {
    if json && walk {
        return Err(
            "--run asks questions at a terminal and --json is one object for a machine — \
             use one or the other"
                .into(),
        );
    }
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
                // This path still asked the system three questions, and a
                // count nobody can trace back to a command is a number with
                // no provenance — the same rule the full report follows.
                counted: counted(&drifts),
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
            println!();
            println!("counted with");
            for sd in &drifts {
                println!(
                    "  {:<8} {:>4}   [{}]",
                    sd.source.name(),
                    sd.installed.len(),
                    live::query_argv(sd.source)
                );
            }
        }
        std::process::exit(crate::HELD);
    }

    let plan = plan(&drifts, &served, &ledger);
    let commands = plan.commands(&repo.name, prune);

    if json {
        report_json(ctx, &drifts, &plan, &commands, prune, &state)?;
    } else {
        report_text(ctx, &drifts, &plan, &commands, prune, &state);
    }

    if plan.in_sync() {
        return Ok(());
    }
    if !walk {
        std::process::exit(crate::HELD)
    }
    walk_plan(&commands)
}

/// `--run`: the printed plan, walked through the confirmed flow — one y/n
/// per command, executed exactly as shown. Every rule is `elevate`'s; what
/// belongs here is only the fallback (no terminal: the plan stands, nothing
/// runs) and the exit code.
fn walk_plan(commands: &[PlannedCmd]) -> Result<(), String> {
    println!();
    if commands.is_empty() {
        // Drift with no commands — extras without --prune. There is nothing
        // to walk, and the drift is still drift.
        std::process::exit(crate::HELD)
    }
    let Some(mut flow) = crate::elevate::Session::open() else {
        println!("run       --run needs a terminal to ask on — nothing was run; the");
        println!("          commands above are yours.");
        std::process::exit(crate::HELD)
    };
    println!("run       walking the plan — one confirm per command, no way to say");
    println!("          yes to all of it at once");
    for cmd in commands {
        flow.run(crate::elevate::Cmd::plain(cmd.argv.clone()));
    }
    flow.report();
    if flow.any_failed() {
        return Err("a command failed — see above. `pacrat sync` again shows what is left.".into());
    }
    if flow.any_declined() {
        std::process::exit(crate::HELD)
    }
    Ok(())
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
    /// Missing names that appear in more than one of this host's tracked
    /// lists, with the lists that claim them. A store inconsistency, not a
    /// drift: the plan resolves it by taking the stricter class, and says so.
    double_tracked: Vec<(String, Vec<Source>)>,
}

/// Classify this host's drift. Pure: every input is data, so the matrix is
/// testable without a pacman, a repo directory, or a store.
///
/// `served` is the set of names the local repo has package files for, and
/// `ledger` is `aur/sources.toml` — between them they answer "is this curated,
/// and if so has it been built here".
fn plan(drifts: &[SourceDrift], served: &BTreeSet<String>, ledger: &Sources) -> Plan {
    let mut plan = Plan::default();
    // Which source lists asked for each missing name. Almost always one; the
    // exceptions are what `double_tracked` reports.
    let mut asked_by: BTreeMap<&String, Vec<Source>> = BTreeMap::new();
    for sd in drifts {
        for package in &sd.drift.missing {
            asked_by.entry(package).or_default().push(sd.source);
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
    // Drift arrives sorted per source; merging the sources into one bucket
    // apiece is the only place that ordering is disturbed.
    for bucket in [
        &mut plan.install,
        &mut plan.curated,
        &mut plan.build_first,
        &mut plan.vendor_first,
        &mut plan.flatpak,
    ] {
        bucket.sort();
        bucket.dedup();
    }

    // One name in two tracked lists otherwise plans two contradictory things
    // for it — `pacrat vendor foo` *and* `sudo pacman -S foo`, the second of
    // which is the bypass the first exists to prevent. The stricter class
    // wins, so the name leaves the install line; the store inconsistency
    // behind it is reported rather than silently absorbed, because one
    // package belongs to one source and no plan can make that true again.
    plan.install
        .retain(|p| !contains(&plan.vendor_first, p) && !contains(&plan.build_first, p));
    plan.curated
        .retain(|p| !contains(&plan.build_first, p) && !contains(&plan.vendor_first, p));
    plan.double_tracked = asked_by
        .into_iter()
        .filter(|(_, sources)| sources.len() > 1)
        .map(|(package, sources)| (package.clone(), sources))
        .collect();
    plan
}

/// Membership in one of the sorted buckets.
fn contains(sorted: &[String], name: &str) -> bool {
    sorted.binary_search_by(|p| p.as_str().cmp(name)).is_ok()
}

fn classify(package: &str, source: Source, served: &BTreeSet<String>, ledger: &Sources) -> Class {
    // Pairing the source with the ledger answer in one match keeps every cell
    // of the matrix visible and leaves no arm that cannot be reached.
    match (source, ledger.packages.contains_key(package)) {
        // Flatpaks are outside the custody ladder: no ledger, no curation.
        (Source::Flatpak, _) => Class::Flatpak,
        // The ledger outranks the tracked list's source column, including for
        // a name an official repo also carries: a human vendored it
        // deliberately, and the curated build is the one this fleet runs.
        (_, true) if served.contains(package) => Class::InstallCurated,
        (_, true) => Class::BuildFirst,
        // Tracked as native means pacman resolves it from an official repo.
        (Source::Native, false) => Class::Install,
        // Tracked as foreign and not in the ledger. There is no command that
        // installs this and stays inside the model — naming the curation step
        // is the whole answer, not a fallback for want of a better one.
        (Source::Aur, false) => Class::VendorFirst,
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
    ///
    /// Pure — `repo` is passed in rather than read from the config, so the
    /// whole rendering stays testable as data.
    fn commands(&self, repo: &str, prune: bool) -> Vec<PlannedCmd> {
        let mut cmds = Vec::new();
        // `vendor` takes one package and prompts for a review of it; there is
        // no batch form to print, and there should not be.
        for package in &self.vendor_first {
            cmds.push(PlannedCmd::new(
                ["pacrat", "vendor"],
                std::slice::from_ref(package),
            ));
        }
        if !self.build_first.is_empty() {
            cmds.push(PlannedCmd::new(["pacrat", "build"], &self.build_first));
        }
        if !self.install.is_empty() {
            let qualified: Vec<String> =
                self.install.iter().map(|p| self.qualify(repo, p)).collect();
            cmds.push(PlannedCmd::new(
                ["sudo", "pacman", "-S", "--needed", "--"],
                &qualified,
            ));
        }
        if !self.flatpak.is_empty() {
            cmds.push(PlannedCmd::new(
                ["flatpak", "install", "-y", "--noninteractive"],
                &self.flatpak,
            ));
        }
        if prune {
            let extra = self.extra_pacman();
            if !extra.is_empty() {
                cmds.push(PlannedCmd::new(["sudo", "pacman", "-Rns", "--"], &extra));
            }
            let flatpaks = self.extra_flatpak();
            if !flatpaks.is_empty() {
                cmds.push(PlannedCmd::new(
                    ["flatpak", "uninstall", "-y", "--noninteractive"],
                    flatpaks,
                ));
            }
        }
        cmds
    }

    /// A curated name written the way pacman disambiguates it: `<repo>/<pkg>`.
    ///
    /// Without the prefix, a curated package that shares a name with an
    /// official one installs whichever repo pacman.conf lists first — and
    /// `pacrat setup` appends the section deliberately, so official wins and
    /// the human gets the uncurated build from a command pacrat printed. The
    /// qualified form asks for the one that was reviewed, and it also retires
    /// the "move the section above [core] by hand" advice for this path: that
    /// edit exists to win name collisions globally, and a plan that names its
    /// repo per package does not need to win anything.
    ///
    /// `/` is already a bare character in `shell_quote`, and `Repo::validate`
    /// has rejected anything a shell would mind, so the pair quotes as one
    /// word or not at all.
    fn qualify(&self, repo: &str, package: &str) -> String {
        if contains(&self.curated, package) {
            format!("{repo}/{package}")
        } else {
            package.to_string()
        }
    }
}

/// One planned command. The argv is the real thing — `--run` hands exactly
/// this vector to exec, no shell anywhere — and the printed line is derived
/// from it, never the reverse, so the plan cannot show one command and run
/// another.
#[derive(Debug, PartialEq, Eq)]
struct PlannedCmd {
    argv: Vec<String>,
}

impl PlannedCmd {
    fn new<const N: usize>(program: [&str; N], args: &[String]) -> Self {
        let mut argv: Vec<String> = program.iter().map(|s| (*s).to_string()).collect();
        argv.extend(args.iter().cloned());
        Self { argv }
    }

    /// The pasteable line: every element shell-quoted, space-joined, never
    /// truncated — this is meant to be pasted (or, under `--run`, it is the
    /// line the confirm question is about).
    fn line(&self) -> String {
        self.argv
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    }
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
/// it has.
///
/// The name must *end* in the package extension, not merely contain it. A
/// half-downloaded `…pkg.tar.zst.part` names a package that is not there, and
/// counting it as served would send the human to `pacman -S` for something
/// the repo cannot hand over. A lone `.sig` is excluded for the same reason:
/// a signature is not a package. The repo database (`<repo>.db.tar.zst`) has
/// no `.pkg.tar` in it at all.
fn pkgname_of(file: &str) -> Option<String> {
    const EXT: &str = ".pkg.tar";
    let cut = file.find(EXT)?;
    // What follows `.pkg.tar` may be nothing (uncompressed) or exactly one
    // compression suffix. A second dot means something was appended — `.part`,
    // `.sig`, an editor's backup — and that file is not a served package.
    let tail = &file[cut + EXT.len()..];
    let bare_suffix = tail.is_empty()
        || (tail.starts_with('.') && tail[1..].chars().all(|c| c.is_ascii_alphanumeric()));
    if !bare_suffix {
        return None;
    }
    let mut fields = file[..cut].rsplitn(4, '-');
    let (_arch, _pkgrel, _pkgver) = (fields.next()?, fields.next()?, fields.next()?);
    let name = fields.next()?;
    (!name.is_empty()).then(|| name.to_string())
}

// ------------------------------------------------------------------- report

fn report_text(
    ctx: &Ctx,
    drifts: &[SourceDrift],
    plan: &Plan,
    commands: &[PlannedCmd],
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

    if !plan.double_tracked.is_empty() {
        println!();
        println!("store     this host's lists disagree with themselves:");
        for (package, sources) in &plan.double_tracked {
            println!(
                "          {package} is tracked as {}",
                sources
                    .iter()
                    .map(|s| s.name())
                    .collect::<Vec<_>>()
                    .join(" and as ")
            );
        }
        println!("          a package belongs to one source. The plan above takes the");
        println!("          stricter reading and names it once — but no command fixes");
        println!("          this, only an edit to packages/{}/.", ctx.host);
    }

    println!();
    println!("commands  nothing below has been run — bare `pacrat sync` only prints");
    for cmd in commands {
        println!("  {}", cmd.line());
    }
    if commands.is_empty() {
        println!("  (none — every item above needs a decision first)");
    }
    if !plan.vendor_first.is_empty() || !plan.build_first.is_empty() {
        println!("          curation steps change what is installable — re-run");
        println!("          `pacrat sync` afterwards for the install line they earn.");
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
            "note      {} come{} from [{}], not from an official repo. If pacman",
            list_preview(&plan.curated, CAP),
            plural_verb(plan.curated.len()),
            repo.name
        );
        println!("          reports one of those as not found, there are two causes and");
        println!("          pacrat does not read the database to tell them apart: the");
        println!("          build may never have reached repo-add (that is `pacrat");
        println!("          build`'s job, and it reports what it served), or this host's");
        println!("          copy of the db may predate the build — `sudo pacman -Syu`");
        println!("          refreshes it. A bare `-Sy` before an install is the");
        println!("          partial-upgrade footgun; don't.");
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
    /// Names this host tracks in more than one list. A store inconsistency
    /// the plan works around; empty is the normal case.
    double_tracked: Vec<DoubleTrackedJson<'a>>,
    repo: RepoJson<'a>,
    /// Shell-ready lines, in the order they may be run. Never truncated.
    /// Derived from the same argvs `--run` executes.
    commands: Vec<String>,
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
struct DoubleTrackedJson<'a> {
    package: &'a str,
    sources: Vec<&'static str>,
}

/// What a source query counted, and the command that counted it. Shared by
/// the full report's `drift` rows and the untracked exit, so no path reports
/// a number without the call behind it.
#[derive(Serialize)]
struct CountedJson {
    source: &'static str,
    installed: usize,
    query: &'static str,
}

fn counted(drifts: &[SourceDrift]) -> Vec<CountedJson> {
    drifts
        .iter()
        .map(|sd| CountedJson {
            source: sd.source.name(),
            installed: sd.installed.len(),
            query: live::query_argv(sd.source),
        })
        .collect()
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
    counted: Vec<CountedJson>,
}

fn report_json(
    ctx: &Ctx,
    drifts: &[SourceDrift],
    plan: &Plan,
    commands: &[PlannedCmd],
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
        double_tracked: plan
            .double_tracked
            .iter()
            .map(|(package, sources)| DoubleTrackedJson {
                package,
                sources: sources.iter().map(|s| s.name()).collect(),
            })
            .collect(),
        repo: RepoJson {
            name: &ctx.config.repo.name,
            path: &ctx.config.repo.path,
            serving: state.complete(),
            missing: state.missing(),
        },
        commands: commands.iter().map(PlannedCmd::line).collect(),
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

    /// The repo name a curated install line is qualified with.
    const REPO: &str = "dotfiles-aur";

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
        assert!(p.commands(REPO, true).is_empty());
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
        assert!(p.commands(REPO, false).is_empty());
    }

    // ---- the commands ----

    /// The plan's commands as their printed lines. Most assertions below are
    /// about the rendering, which is derived from the argvs `--run` executes
    /// — one direction, so testing the lines tests both.
    fn lines(p: &Plan, repo: &str, prune: bool) -> Vec<String> {
        p.commands(repo, prune)
            .iter()
            .map(PlannedCmd::line)
            .collect()
    }

    /// The other direction, once: the argv really is data — a name that the
    /// *line* must quote stays exactly one unquoted element of the vector
    /// exec receives, because there is no shell on the `--run` path at all.
    #[test]
    fn the_argv_is_data_and_the_line_is_derived_from_it() {
        let drifts = vec![sd(Source::Native, &["two words"], &[])];
        let p = plan(&drifts, &served_set(&[]), &Sources::default());
        let cmds = p.commands(REPO, false);
        assert_eq!(
            cmds[0].argv,
            ["sudo", "pacman", "-S", "--needed", "--", "two words"]
        );
        assert_eq!(cmds[0].line(), "sudo pacman -S --needed -- 'two words'");
    }

    #[test]
    fn commands_run_top_to_bottom_in_dependency_order() {
        let drifts = vec![
            sd(Source::Native, &["ripgrep"], &["cowsay"]),
            sd(Source::Aur, &["pacseek", "playtimed", "mdcat"], &[]),
            sd(Source::Flatpak, &["org.gnome.Loupe"], &["org.old.App"]),
        ];
        let p = plan(&drifts, &served_set(&["mdcat"]), &ledger());
        assert_eq!(
            lines(&p, REPO, true),
            vec![
                "pacrat vendor pacseek",
                "pacrat build playtimed",
                "sudo pacman -S --needed -- dotfiles-aur/mdcat ripgrep",
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
            lines(&p, REPO, false),
            vec!["sudo pacman -S --needed -- ripgrep"]
        );
    }

    /// Names that need vendoring or building are not in the install line —
    /// a paste that fails halfway is worse than one command fewer.
    #[test]
    fn uninstallable_names_stay_out_of_the_install_line() {
        let drifts = vec![sd(Source::Aur, &["pacseek", "playtimed"], &[])];
        let p = plan(&drifts, &served_set(&[]), &ledger());
        let cmds = lines(&p, REPO, false);
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
            lines(&p, REPO, false),
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
        let cmds = lines(&p, REPO, true);
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

    /// The install line must ask for the *curated* build by name.
    ///
    /// `pacrat setup` appends its section, so official repos win a name
    /// collision. A bare `pacman -S mdcat` would therefore install the
    /// official mdcat from a command pacrat printed to install the reviewed
    /// one — the curation model defeated by the resolution order. Qualifying
    /// the name is what makes the printed command mean what it says.
    #[test]
    fn curated_names_are_repo_qualified_and_official_ones_are_not() {
        let drifts = vec![sd(Source::Native, &["mdcat", "ripgrep"], &[])];
        let p = plan(&drifts, &served_set(&["mdcat"]), &ledger());
        assert_eq!(
            lines(&p, REPO, false),
            vec!["sudo pacman -S --needed -- dotfiles-aur/mdcat ripgrep"]
        );
        // The prefix is the configured repo's, not a constant.
        assert_eq!(
            lines(&p, "other-repo", false),
            vec!["sudo pacman -S --needed -- other-repo/mdcat ripgrep"]
        );
        // `/` needs no quoting, so the pair stays one readable word.
        assert!(!lines(&p, REPO, false)[0].contains('\''));
    }

    /// A name in two of this host's lists asked for two different things —
    /// `pacrat vendor foo` and `sudo pacman -S foo`, the second being the
    /// bypass the first exists to prevent. The stricter class wins, the name
    /// is planned once, and the store's inconsistency is reported.
    #[test]
    fn a_double_tracked_name_yields_one_command_and_a_report() {
        let drifts = vec![
            sd(Source::Native, &["ripgrep", "vv"], &[]),
            sd(Source::Aur, &["vv"], &[]),
        ];
        let p = plan(&drifts, &served_set(&[]), &Sources::default());
        assert_eq!(p.vendor_first, v(&["vv"]));
        assert_eq!(p.install, v(&["ripgrep"]), "vv must leave the install line");
        assert_eq!(p.missing(), 2, "counted once, not twice");
        assert_eq!(
            p.double_tracked,
            vec![("vv".to_string(), vec![Source::Native, Source::Aur])]
        );
        assert_eq!(
            lines(&p, REPO, false),
            vec!["pacrat vendor vv", "sudo pacman -S --needed -- ripgrep"]
        );
    }

    /// Same rule for the vendored-but-unbuilt class: build wins over install,
    /// so the name is not proposed for an install that cannot resolve.
    #[test]
    fn build_first_also_outranks_the_install_line() {
        let drifts = vec![
            sd(Source::Native, &["playtimed"], &[]),
            sd(Source::Aur, &["playtimed"], &[]),
        ];
        let p = plan(&drifts, &served_set(&[]), &ledger());
        assert_eq!(p.build_first, v(&["playtimed"]));
        assert!(p.install.is_empty());
        assert!(p.curated.is_empty());
        assert_eq!(lines(&p, REPO, false), vec!["pacrat build playtimed"]);
    }

    /// One name, one list: the ordinary case reports no inconsistency.
    #[test]
    fn a_normal_plan_reports_no_double_tracking() {
        let drifts = vec![
            sd(Source::Native, &["ripgrep"], &[]),
            sd(Source::Aur, &["pacseek"], &[]),
        ];
        let p = plan(&drifts, &served_set(&[]), &Sources::default());
        assert!(p.double_tracked.is_empty());
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
            lines(&p, REPO, true),
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
        // Other compressions, and the uncompressed form.
        assert_eq!(
            pkgname_of("foo-1.0-2-any.pkg.tar.xz").as_deref(),
            Some("foo")
        );
        assert_eq!(pkgname_of("foo-1.0-2-any.pkg.tar").as_deref(), Some("foo"));
        // An epoch lives in pkgver, which is the field before pkgrel.
        assert_eq!(
            pkgname_of("bar-2:1.0-1-x86_64.pkg.tar.zst").as_deref(),
            Some("bar")
        );
    }

    /// A name that only *contains* the extension is a different file. The
    /// `.part` case is the one that would lie: an interrupted download names
    /// a package the repo cannot hand over, and calling it served sends the
    /// human to `pacman -S` for something that is not there.
    #[test]
    fn an_appended_suffix_means_it_is_not_a_served_package() {
        for file in [
            "mdcat-2.7.1-1-x86_64.pkg.tar.zst.part",
            "mdcat-2.7.1-1-x86_64.pkg.tar.zst.sig",
            "mdcat-2.7.1-1-x86_64.pkg.tar.zst~",
            "mdcat-2.7.1-1-x86_64.pkg.tar.zst.bak",
        ] {
            assert_eq!(pkgname_of(file), None, "{file:?} counted as served");
        }
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

    /// Tagged by the test that owns it: two tests sharing a directory is a
    /// failure that only shows up when they run in parallel, which is always.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pacrat-sync-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn served_reads_names_out_of_a_directory_and_survives_a_missing_one() {
        let dir = temp_dir("served");
        for file in [
            "mdcat-2.7.1-1-x86_64.pkg.tar.zst",
            "mdcat-2.7.1-1-x86_64.pkg.tar.zst.sig",
            "python-lsp-server-1.12.0-1-any.pkg.tar.zst",
            "half-1.0-1-any.pkg.tar.zst.part",
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
