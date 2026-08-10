use clap::{Parser, Subcommand};

mod add;
mod ctx;
mod hosts;
mod live;
mod out;
mod status;

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
    /// Adopt installed packages into the manifest (unmanaged → tracked)
    Add {
        packages: Vec<String>,
        /// Host list to write (default: this host)
        #[arg(long)]
        host: Option<String>,
    },
    /// Vendor an AUR package's tree into the store (fetch → grade → review)
    Vendor { package: String },
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
    Setup,
    /// The petit chef
    About,
}

fn main() {
    let cli = Cli::parse();
    let result = ctx::Ctx::resolve().and_then(|ctx| match cli.command {
        // Bare `pacrat`: the TUI once it exists; the overview meanwhile.
        None | Some(Command::Status) => status::run(&ctx),
        Some(Command::Hosts) => hosts::run(&ctx),
        Some(Command::Add {
            ref packages,
            ref host,
        }) => add::run(&ctx, packages, host.as_deref()),
        Some(_) => Err("not yet implemented — see ADR-001 and `pacrat --help`".into()),
    });
    if let Err(e) = result {
        eprintln!("pacrat: {e}");
        std::process::exit(1);
    }
}
