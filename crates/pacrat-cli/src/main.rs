use clap::{Parser, Subcommand};

mod add;
mod aur;
mod build;
mod ctx;
mod custody;
mod fstree;
mod grade;
mod hosts;
mod info;
mod live;
mod out;
mod pacman;
mod proc;
mod review;
mod search;
mod setup;
mod status;
mod updates;
mod vendor;

/// pacrat — store-backed package curation for Arch.
///
/// Bare `pacrat` will open the default UI (config: default_ui); any
/// subcommand is always CLI. See docs/design/mockup-rev3.html for the
/// screen-by-screen design and docs/architecture/ADR-001 for the decisions.
#[derive(Parser)]
#[command(name = "pacrat", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Overview: custody counts, drift, holds, source status
    Status,
    /// Search official repos and the AUR (with custody state)
    Search { term: String },
    /// Everything pacrat knows about one package
    Info { package: String },
    /// Adopt installed packages into the manifest (unmanaged → tracked)
    Add {
        packages: Vec<String>,
        /// Host list to write (default: this host)
        #[arg(long)]
        host: Option<String>,
    },
    /// Vendor a package's build tree into the store (tracked → vendored)
    Vendor {
        /// Package name (also names the default AUR repo)
        package: String,
        /// Git URL to clone (default: the AUR repo for <package>)
        #[arg(long)]
        upstream: Option<String>,
        /// Ledger role to record
        #[arg(long, value_enum, default_value_t = vendor::RoleArg::Vendored)]
        role: vendor::RoleArg,
        /// Skip the review prompt (scripting)
        #[arg(long)]
        yes: bool,
        /// Overwrite an existing store tree and ledger entry
        #[arg(long)]
        force: bool,
    },
    /// One-shot update loop: detect → grade → decide → build
    Update,
    /// List pending updates with grades
    ///
    /// Exits 0 when nothing is pending, 10 when there are updates to look at,
    /// and 1 when the check itself could not run — so a timer can tell
    /// "all quiet" from "I could not ask" (ADR-001).
    Updates {
        /// Output format
        #[arg(long, value_enum, default_value_t = updates::Format::Text)]
        format: updates::Format,
    },
    /// Review one pending update (diff since reviewed commit + gradings)
    ///
    /// Shows and decides nothing: exits 0 whatever the verdict, because
    /// being shown a BLOCK is not being refused anything. `adopt-update`
    /// and `build` are the verbs that hold.
    Review { package: String },
    /// Adopt a reviewed candidate: its tree into the store, its commit into
    /// the ledger.
    ///
    /// Exits 10 when it deliberately does not act — a BLOCK verdict, a
    /// declined prompt, or a candidate that was rejected before.
    AdoptUpdate {
        package: String,
        /// The candidate you reviewed. Upstream having moved past it is a
        /// refusal, never a substitution.
        #[arg(long)]
        commit: Option<String>,
        /// Skip the prompt (scripting); also required to re-adopt a
        /// candidate that was rejected
        #[arg(long)]
        yes: bool,
    },
    /// Refuse the current candidate, so `updates` stops calling it pending
    Reject {
        package: String,
        /// Why — recorded in the ledger beside the commit
        #[arg(long)]
        note: Option<String>,
    },
    /// Record a grading (runs configured graders; --grade N for manual)
    Grade {
        package: String,
        /// Commit to grade (default: the ledger's reviewed commit)
        #[arg(long)]
        commit: Option<String>,
        /// Record your own grade instead of running graders (0-4)
        #[arg(long)]
        grade: Option<u8>,
        /// Why — required with --grade
        #[arg(long)]
        note: Option<String>,
        /// Re-run graders even if a grading is cached (writes still cache)
        #[arg(long)]
        refresh: bool,
    },
    /// Build vendored packages into the local repo (no args: all of them).
    /// Builds and serves only — installing is `sudo pacman -Sy <package>`.
    Build { packages: Vec<String> },
    /// Host-vs-manifest matrix
    Hosts,
    /// Reconcile a host against the manifest
    Sync { host: Option<String> },
    /// Publish a maintained package to the AUR (queues while read-only)
    Push { package: String },
    /// Install the [dotfiles-aur] repo section and guard hooks
    Setup {
        /// Do the steps that need no root (repo dir, empty db, staging the
        /// root-owned files); the sudo commands are still only printed.
        #[arg(long)]
        apply: bool,
    },
    /// The petit chef
    About,
}

/// Exit code for "ran fine, deliberately did not act". ADR-001 gives 10 that
/// meaning across the update loop — 0 clean, 10 held, 1 failure — and every
/// verb that can decline shares it: a refused review and a pending update are
/// the same kind of answer to a script or a timer.
pub const HELD: i32 = 10;

fn main() {
    let cli = Cli::parse();
    let result = ctx::Ctx::resolve().and_then(|ctx| match cli.command {
        // Bare `pacrat`: the TUI once it exists; the overview meanwhile.
        None | Some(Command::Status) => status::run(&ctx),
        Some(Command::Hosts) => hosts::run(&ctx),
        Some(Command::Search { ref term }) => search::run(&ctx, term),
        Some(Command::Info { ref package }) => info::run(&ctx, package),
        Some(Command::Add {
            ref packages,
            ref host,
        }) => add::run(&ctx, packages, host.as_deref()),
        Some(Command::Setup { apply }) => setup::run(&ctx, apply),
        Some(Command::Updates { format }) => updates::run(&ctx, format),
        Some(Command::Vendor {
            ref package,
            ref upstream,
            role,
            yes,
            force,
        }) => vendor::run(&ctx, package, upstream.as_deref(), role, yes, force),
        Some(Command::Grade {
            ref package,
            ref commit,
            grade,
            ref note,
            refresh,
        }) => grade::run(
            &ctx,
            package,
            commit.as_deref(),
            grade,
            note.as_deref(),
            refresh,
        ),
        Some(Command::Review { ref package }) => review::run(&ctx, package),
        Some(Command::AdoptUpdate {
            ref package,
            ref commit,
            yes,
        }) => review::adopt(&ctx, package, commit.as_deref(), yes),
        Some(Command::Reject {
            ref package,
            ref note,
        }) => review::reject(&ctx, package, note.as_deref()),
        Some(Command::Build { ref packages }) => build::run(&ctx, packages),
        Some(_) => Err("not yet implemented — see ADR-001 and `pacrat --help`".into()),
    });
    if let Err(e) = result {
        eprintln!("pacrat: {e}");
        std::process::exit(1);
    }
}
