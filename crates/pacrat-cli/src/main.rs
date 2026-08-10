use clap::{Parser, Subcommand};

mod add;
mod aur;
mod ctx;
mod custody;
mod fstree;
mod hosts;
mod info;
mod live;
mod out;
mod pacman;
mod search;
mod setup;
mod status;
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
    Updates,
    /// Review one pending update (diff since reviewed commit + gradings)
    Review { package: String },
    /// Record a grading (runs configured graders; --grade N for manual)
    Grade { package: String },
    /// Build vendored packages into the local repo
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
        Some(Command::Vendor {
            ref package,
            ref upstream,
            role,
            yes,
            force,
        }) => vendor::run(&ctx, package, upstream.as_deref(), role, yes, force),
        Some(_) => Err("not yet implemented — see ADR-001 and `pacrat --help`".into()),
    });
    if let Err(e) = result {
        eprintln!("pacrat: {e}");
        std::process::exit(1);
    }
}
