use clap::{Parser, Subcommand};

mod about;
mod add;
mod art;
mod aur;
mod build;
mod ctx;
mod custody;
mod decisions;
mod fstree;
mod git;
mod grade;
mod hosts;
mod info;
mod live;
mod out;
mod pacman;
mod proc;
mod push;
mod review;
mod search;
mod setup;
mod status;
mod sync;
mod tui;
mod update;
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
    /// Open the TUI, whatever `default_ui` says
    ///
    /// Bare `pacrat` honours the preference; this insists. It needs a
    /// terminal on stdout and says so rather than filling a pipe with
    /// escape sequences.
    Tui,
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
    /// One-shot update loop: detect → grade → decide → build → served
    ///
    /// The headless half of every verb below it, and the entry point a timer
    /// uses. BLOCK always holds here and there is no override — that door is
    /// `adopt-update --override-block`, where a human writes down why.
    ///
    /// Exits 0 when there was nothing to do or everything pending was
    /// adopted and built, 10 when anything was held, and 1 when the loop
    /// could not do its job (a failed build, or an upstream it could not ask
    /// about at all).
    Update {
        /// How much runs without asking (default: config `update_mode`, semi)
        #[arg(long, value_enum)]
        mode: Option<update::ModeArg>,
        /// Output format
        #[arg(long, value_enum, default_value_t = updates::Format::Text)]
        format: updates::Format,
    },
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
        /// Adopt past a BLOCK. The only path there is, it needs --commit and
        /// --reason, and it records the decision in the store's ledger
        /// permanently (ADR-001 decision 2).
        #[arg(long)]
        override_block: bool,
        /// Why the BLOCK is being accepted. Required with --override-block;
        /// this sentence is the friction, and it is what a reader six months
        /// from now will have to go on.
        #[arg(long)]
        reason: Option<String>,
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
    /// Accepted risks on the record: overrides and trust decisions
    Decisions,
    /// Host-vs-manifest matrix
    Hosts,
    /// Plan this host toward the manifest (prints commands, runs none)
    ///
    /// There is no <host> argument: sync transport — ssh to remote hosts, or
    /// each host syncing itself — is ADR-001's open question 4 and is not
    /// settled, so pacrat plans only for the machine it runs on. Close another
    /// host's drift by running `pacrat sync` there.
    ///
    /// Exits 0 when this host matches the store, 10 when a plan was printed
    /// (deliberately not acted on — the commands are yours to run), and 1 when
    /// the check itself could not run.
    Sync {
        /// Also print the removals for installed-but-untracked packages
        #[arg(long)]
        prune: bool,
        /// One JSON object instead of the report
        #[arg(long)]
        json: bool,
    },
    /// Publish a maintained package to its upstream (queues while the AUR is
    /// read-only).
    ///
    /// With no package, works the publish queue: one write probe, then every
    /// queued package whose store tree is still the tree that was queued.
    /// Exits 0 published (or already current), 10 queued/blocked/declined, and
    /// 1 on the tamper alarm — a checksum that changed for a source of an
    /// already-published version.
    Push {
        /// Package to publish; omit to work the publish queue
        package: Option<String>,
        /// Work the publish queue (the same as passing no package)
        #[arg(long)]
        retry: bool,
        /// Skip the confirmation prompt (scripting)
        #[arg(long)]
        yes: bool,
    },
    /// Install the [dotfiles-aur] repo section and guard hooks
    Setup {
        /// Do the steps that need no root (repo dir, empty db, staging the
        /// root-owned files); the sudo commands are still only printed.
        #[arg(long)]
        apply: bool,
    },
    /// The petit chef: the mascot, the version, and where the source lives
    About,
}

/// Exit code for "ran fine, deliberately did not act". ADR-001 gives 10 that
/// meaning across the update loop — 0 clean, 10 held, 1 failure — and every
/// verb that can decline shares it: a refused review and a pending update are
/// the same kind of answer to a script or a timer.
pub const HELD: i32 = 10;

/// `--reason` belongs to `--override-block` and to nothing else.
///
/// Checked here rather than in `review`, because this is a statement about
/// the *command line* — a reason with no override is a user who thinks they
/// asked for something they did not, and the quiet reading of it (adopt
/// normally, discard the sentence) is the one that ends with somebody
/// believing an override was recorded.
fn adopt_update(
    ctx: &ctx::Ctx,
    package: &str,
    commit: Option<&str>,
    yes: bool,
    override_block: bool,
    reason: &Option<String>,
) -> Result<(), String> {
    let override_block = match (override_block, reason.as_deref()) {
        (true, None) => {
            return Err(
                "--override-block needs --reason \"…\": the justification is the \
                 friction, and it is what the decision ledger records"
                    .into(),
            )
        }
        (false, Some(_)) => {
            return Err(
                "--reason has no meaning without --override-block — nothing else \
                 about an adoption is recorded as a decision"
                    .into(),
            )
        }
        (true, some) => some,
        (false, None) => None,
    };
    review::adopt(
        ctx,
        package,
        &review::Adopt {
            commit,
            yes,
            override_block,
        },
    )
}

fn main() {
    let cli = Cli::parse();
    // `about` first, before the store is even looked for: it answers "what
    // is this binary?", which is exactly the question a machine with no
    // store yet has, and failing it with "store not found" would be
    // answering a different question than the one asked.
    let result = match cli.command {
        Some(Command::About) => about::run(),
        command => dispatch(command),
    };
    if let Err(e) = result {
        eprintln!("pacrat: {e}");
        std::process::exit(1);
    }
}

fn dispatch(command: Option<Command>) -> Result<(), String> {
    ctx::Ctx::resolve().and_then(|ctx| match command {
        // Bare `pacrat` is the only command that asks the config what to be.
        None => tui::run_default(&ctx),
        Some(Command::Tui) => tui::run(&ctx),
        Some(Command::Status) => status::run(&ctx),
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
            override_block,
            ref reason,
        }) => adopt_update(
            &ctx,
            package,
            commit.as_deref(),
            yes,
            override_block,
            reason,
        ),
        Some(Command::Update { mode, format }) => update::run(&ctx, mode.map(Into::into), format),
        Some(Command::Decisions) => decisions::run(&ctx),
        Some(Command::Reject {
            ref package,
            ref note,
        }) => review::reject(&ctx, package, note.as_deref()),
        Some(Command::Build { ref packages }) => build::run(&ctx, packages),
        Some(Command::Sync { prune, json }) => sync::run(&ctx, prune, json),
        Some(Command::Push {
            ref package,
            retry,
            yes,
        }) => push::run(&ctx, package.as_deref(), retry, yes),
        // Dispatched in `main` before the store is resolved; this arm only
        // completes the match, and would be correct anyway.
        Some(Command::About) => about::run(),
    })
}
