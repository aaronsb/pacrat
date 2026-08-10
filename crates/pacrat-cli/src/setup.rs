//! `pacrat setup` — put this host on the serving model: the first-run
//! interview that writes the config, then the pacman.conf section for the
//! local repo, the repo directory and its (empty) database, and the
//! PreTransaction guard that aborts installs going around curation. It is
//! the only verb that can open the setup gate (`gate.rs`), which is why it
//! and `about` are the two commands a fresh machine can run.
//!
//! The interview comes first (ADR-003): UI preference, update mode, grader
//! registration — asked at a terminal with the current values as defaults,
//! answered by flags for scripts, skipped with a note when there is neither.
//! It writes `~/.config/pacrat/config.toml` through `config::save`, so a
//! full setup is one command on one terminal.
//!
//! Which of the five flows a run gets is one pure decision ([`flow`]):
//! euid, `SUDO_USER`, the terminal and the flags in, a [`Flow`] out. At a
//! terminal, bare `pacrat setup` IS the guided flow (ADR-003, as amended):
//! the interview, the user-doable steps, then each root-owned step through
//! `elevate::Session` — argv shown, y/n asked, sudo authenticating on the
//! terminal itself — with anything declined reported at the end as the
//! exact commands to run later. `--print` asks for the wall instead: every
//! section, file body and copy-paste command, nothing run or changed.
//! Headless runs print, as they always did, and `--apply` (now a synonym
//! at a terminal) headless still performs the user-doable steps and stages
//! the root-owned files so the printed `sudo install` lines work verbatim.
//!
//! Root is maintenance mode (ADR-003, second amendment): `sudo pacrat
//! setup` reads the OPERATOR's config via `SUDO_USER` — never root's own —
//! refuses the interview (config.toml is the user's file), and runs the
//! system steps directly, each still shown and confirmed; the privilege
//! line was crossed by the operator typing sudo, so the commands lose
//! their `sudo` prefix and nothing else. Root never writes config.toml
//! and never touches the store.
//!
//! pacrat never elevates *unasked* (ADR-003, amending ADR-001), and never
//! authenticates itself — in any flow, every command that runs was printed
//! first and answered by a human.
//!
//! Repo values are substituted into pacman.conf and into a script that runs
//! as root out of a pacman hook. They are safe to substitute because
//! `Repo::validate` (pacrat-core) has already rejected everything that could
//! escape either grammar; `sh` below quotes what remains.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use pacrat_core::config::{Cmd, Config, Grader, Mode, Repo, Ui};
use pacrat_core::grading::Scale;

use crate::ctx::Ctx;

/// Where the guard's two halves live once installed. The hook is data for
/// alpm; the script is the part with the judgement in it.
const HOOK_DEST: &str = "/etc/pacman.d/hooks/pacrat-guard.hook";
const SCRIPT_DEST: &str = "/usr/share/pacrat/pacrat-guard.sh";
const PACMAN_CONF: &str = "/etc/pacman.conf";

/// Dropped in the repo directory for the duration of pacrat's own
/// transactions; the guard passes while it is *fresh*. Contract for the
/// future `build`/`sync` verbs (ADR-001):
///
/// ```text
/// pid=<pid of the pacrat process> started=<unix seconds>
/// ```
///
/// Write it before invoking pacman, remove it after. A marker whose pid is
/// gone, whose timestamp is older than `MARKER_MAX_AGE_S`, or that does not
/// parse is ignored and announced — a Ctrl-C'd build must not disable the
/// guard forever.
const MARKER: &str = ".pacrat-transaction";
const MARKER_MAX_AGE_S: u64 = 3600;

// ------------------------------------------------------------------- flow

/// Which of the five shapes this run of `setup` gets. Decided once, by
/// [`flow`], from the four facts that matter — euid, `SUDO_USER`, the
/// terminal, the flags — and nothing else, so the table is testable
/// without being root or holding a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    /// A user at a terminal: the interview, the user-doable steps, then
    /// each root-owned step confirmed and run through the sudo flow.
    Guided,
    /// `--print` at a terminal: the wall — every section, file body and
    /// command — with nothing run and nothing changed. The audit view.
    Print,
    /// No terminal: print everything, run nothing. Today's output for a
    /// script, byte for byte; flags still answer the interview.
    HeadlessPrint,
    /// No terminal, `--apply`: the printed wall plus the user-doable work —
    /// staged files, the repo dir and db where this user can already write.
    HeadlessApply,
    /// Root at a terminal: maintenance mode. No interview, no store; the
    /// system steps run directly — shown and confirmed, minus the sudo the
    /// operator already typed. `operator` is who this setup is *for*.
    RootDirect { operator: String },
    /// Root without a terminal, or root `--print`: the wall, rendered from
    /// the operator's config.
    RootPrint { operator: String },
}

impl Flow {
    /// Root maintenance mode? (Decides whose scratch the staging uses and
    /// which refusal the interview section prints.)
    fn as_root(&self) -> bool {
        matches!(self, Flow::RootDirect { .. } | Flow::RootPrint { .. })
    }

    /// The operator a root-mode run is for; `None` in every user flow.
    pub fn operator(&self) -> Option<&str> {
        match self {
            Flow::RootDirect { operator } | Flow::RootPrint { operator } => Some(operator),
            _ => None,
        }
    }

    /// Does this flow write the three staged files? Exactly the flows that
    /// go on to run (or, headless `--apply`, to print runnable) commands
    /// that read them.
    fn stages(&self) -> bool {
        matches!(
            self,
            Flow::Guided | Flow::HeadlessApply | Flow::RootDirect { .. }
        )
    }

    /// The banner's `mode` line. The two headless strings are the ones
    /// scripts have always seen and stay byte-identical.
    fn describe(&self) -> &'static str {
        match self {
            Flow::Guided => {
                "guided (the first-run questions, then each system step shown and asked \
                 before it runs)"
            }
            Flow::Print => "--print (system steps are printed, not run; nothing changes)",
            Flow::HeadlessPrint => {
                "print only (system steps are printed, not run; interview answers are still saved to config.toml)"
            }
            Flow::HeadlessApply => {
                "--apply (user-owned steps run now; each root-owned step asks before sudo runs it)"
            }
            Flow::RootDirect { .. } => {
                "root (each system step is shown and asked, then run directly)"
            }
            Flow::RootPrint { .. } => "print only, as root (system steps are printed, not run)",
        }
    }
}

/// The one mode decision (ADR-003, both amendments), pure: the caller
/// gathers the four facts, this ranks them. Root outranks everything —
/// and root with no `SUDO_USER` is refused, because pacrat cannot tell
/// whose serving model it would be assembling. Then `--print`, then the
/// terminal; `--apply` keeps its old meaning headless and is a synonym
/// for the default at a terminal.
pub fn flow(
    euid: u32,
    sudo_user: Option<&str>,
    tty: bool,
    print: bool,
    apply: bool,
) -> Result<Flow, String> {
    if euid == 0 {
        let operator = sudo_user
            .filter(|user| !user.is_empty() && *user != "root")
            .ok_or(
                "running as root with no user behind it (SUDO_USER is not set) — pacrat \
                 cannot tell whose setup this would be. Run `sudo pacrat setup` from your \
                 own login, or `pacrat setup` as yourself.",
            )?
            .to_string();
        return Ok(if print || !tty {
            Flow::RootPrint { operator }
        } else {
            Flow::RootDirect { operator }
        });
    }
    if print {
        return Ok(if tty {
            Flow::Print
        } else {
            Flow::HeadlessPrint
        });
    }
    if !tty {
        return Ok(if apply {
            Flow::HeadlessApply
        } else {
            Flow::HeadlessPrint
        });
    }
    Ok(Flow::Guided)
}

/// How the three steps behave, distilled from the [`Flow`]: print the wall,
/// or walk the commands through a confirmed session. `sudo` is the only
/// difference between the guided walk and root's — the argv is otherwise
/// identical, so the two modes cannot drift. `owner` is who the repo
/// directory is installed for: `$USER` in the guided flow, the operator in
/// root mode.
enum Steps {
    Print {
        apply: bool,
    },
    Walk {
        session: crate::elevate::Session,
        sudo: bool,
        owner: String,
    },
}

/// One step command, built from the same argv whichever side of the
/// privilege line it runs on.
fn step_cmd(sudo: bool, args: &[&str]) -> crate::elevate::Cmd {
    if sudo {
        crate::elevate::Cmd::sudo(args)
    } else {
        crate::elevate::Cmd::plain(args.iter().map(|a| (*a).to_string()).collect())
    }
}

pub fn run(ctx: &Ctx, flow: Flow, answers: &Interview) -> Result<(), String> {
    let repo = &ctx.config.repo;
    let repo_path = PathBuf::from(&repo.path);
    let db = repo_path.join(format!("{}.db.tar.zst", repo.name));
    let staged = staging_dir(flow.as_root())?;
    let st = state(repo);

    println!("pacrat setup — {} onto the serving model", ctx.host);
    println!("repo    [{}] {}", repo.name, repo.path);
    if let Some(operator) = flow.operator() {
        println!("user    {operator} (from SUDO_USER — the config and repo values are theirs)");
    }
    println!("mode    {}", flow.describe());
    println!();

    match &flow {
        Flow::Guided => interview(ctx, answers, true)?,
        // Headless keeps today's behavior to the byte: flags answer and
        // save, no flags leaves the file alone with a note.
        Flow::HeadlessPrint | Flow::HeadlessApply => interview(ctx, answers, false)?,
        Flow::Print => {
            println!("config  --print changes nothing — run `pacrat setup` without it to");
            println!("        answer the first-run questions.");
            println!();
        }
        Flow::RootDirect { operator } | Flow::RootPrint { operator } => {
            println!("config  the first-run questions are skipped as root — config.toml");
            println!(
                "        belongs to {operator}. Run `pacrat setup` as {operator} to answer them."
            );
            println!();
        }
    }

    println!("state   repo dir      {}", yn(st.repo_dir));
    println!("        repo db       {}", yn(st.repo_db));
    println!("        pacman.conf   {}", yn(st.conf_section));
    println!("        guard hook    {}", yn(st.hook && st.script));
    println!();
    if st.complete() {
        println!("This host is already set up. Each step below reports what it found;");
        println!("re-running is safe — nothing is overwritten.");
        println!();
    }

    if flow.stages() {
        stage(&staged, "pacman.conf.section", &pacman_conf_section(repo))?;
        stage(&staged, "pacrat-guard.hook", &guard_hook())?;
        stage(&staged, "pacrat-guard.sh", &guard_script(repo))?;
        if flow.as_root() {
            println!("staged  {} (three files; gone at reboot)", staged.display());
        } else {
            println!("staged  {} (three files, user-owned)", staged.display());
        }
        println!();
    }

    let mut steps = match &flow {
        Flow::Guided => Steps::Walk {
            session: crate::elevate::Session::at_terminal(),
            sudo: true,
            owner: env::var("USER").unwrap_or_else(|_| "$USER".into()),
        },
        Flow::RootDirect { operator } => Steps::Walk {
            session: crate::elevate::Session::at_terminal(),
            sudo: false,
            owner: operator.clone(),
        },
        Flow::HeadlessApply => Steps::Print { apply: true },
        Flow::Print | Flow::HeadlessPrint | Flow::RootPrint { .. } => Steps::Print { apply: false },
    };

    step_pacman_conf(repo, &staged, &st, &mut steps);
    step_repo(&repo_path, &db, &st, &mut steps)?;
    step_guard(repo, &staged, &st, &mut steps);

    if let Steps::Walk { session, .. } = &steps {
        session.report();
        if session.any_declined() {
            println!();
            println!("The declined commands above are yours to run later, exactly as");
            println!("shown — or re-run `pacrat setup` to be asked again.");
        }
        if session.any_failed() {
            return Err(
                "a confirmed command failed — see above; re-run `pacrat setup` once it \
                 is sorted"
                    .into(),
            );
        }
        if !st.complete() && state(repo).complete() {
            println!();
            println!("done      this host is on the serving model — the setup gate is open.");
        }
    }

    Ok(())
}

// -------------------------------------------------------------- interview

/// The interview's non-interactive answers, straight from the flags. A flag
/// answers its question and skips it; at a terminal the rest are asked, each
/// defaulting to the current effective value, so pressing Enter through the
/// whole interview changes nothing.
pub struct Interview {
    pub ui: Option<Ui>,
    pub mode: Option<Mode>,
    pub register_yay_friend: bool,
    pub no_graders: bool,
}

impl Interview {
    /// Any answer flag at all — the line between "script that answered"
    /// and "script that gets the leave-it-alone note", and (in root mode)
    /// between silence and a refusal.
    pub fn any_flag(&self) -> bool {
        self.ui.is_some() || self.mode.is_some() || self.register_yay_friend || self.no_graders
    }
}

/// The first-run questions (ADR-003): UI, update mode, graders. Ends by
/// writing `~/.config/pacrat/config.toml` through the one stamped writer
/// (`config::save`), carrying every grader it did not touch. `ask` is the
/// flow's decision, not a fresh terminal probe: only the guided flow asks,
/// and with `ask` false and no flags it says so and leaves the file alone
/// — today's headless behavior.
fn interview(ctx: &Ctx, answers: &Interview, ask: bool) -> Result<(), String> {
    if !ask && !answers.any_flag() {
        println!("config  no terminal and no answer flags (--ui, --mode,");
        println!("        --register-yay-friend, --no-graders) — leaving the config file");
        println!("        alone. `pacrat config` changes it any time.");
        println!();
        return Ok(());
    }

    let mut config = ctx.config.clone();

    config.default_ui = match answers.ui {
        Some(ui) => ui,
        None if ask => {
            match ask_choice(
                "ui      what should bare `pacrat` open?",
                &["cli", "tui"],
                config.default_ui.label(),
            )?
            .as_str()
            {
                "tui" => Ui::Tui,
                _ => Ui::Cli,
            }
        }
        None => config.default_ui,
    };

    config.update_mode = match answers.mode {
        Some(mode) => mode,
        None if ask => {
            match ask_choice(
                "update  how much of `pacrat update` runs without asking?",
                &["auto", "semi", "manual"],
                config.update_mode.label(),
            )?
            .as_str()
            {
                "auto" => Mode::Auto,
                "manual" => Mode::Manual,
                _ => Mode::Semi,
            }
        }
        None => config.update_mode,
    };

    grader_question(ctx, &mut config, answers, ask)?;

    let path = crate::config::save(&config)?;
    println!(
        "config  wrote {} (stamped as pacrat-written)",
        path.display()
    );
    println!();
    Ok(())
}

/// The one structured question. Graders other than yay-friend are never
/// touched here, and even yay-friend's own entry is only ever changed with
/// a yes at a terminal — a hand-tuned entry survives every re-run.
fn grader_question(
    ctx: &Ctx,
    config: &mut Config,
    answers: &Interview,
    ask: bool,
) -> Result<(), String> {
    if answers.no_graders {
        println!("grader  --no-graders — registering none.");
        return Ok(());
    }
    if let Some(i) = config.graders.iter().position(|g| g.name == "yay-friend") {
        return modernize_question(config, i, ask);
    }
    if answers.register_yay_friend {
        if probe_native() {
            register_native(config);
            return Ok(());
        }
        let adapter = adapter_path(ctx).ok_or(
            "--register-yay-friend: the installed yay-friend has no `grade` subcommand, \
             and contrib/graders/yay-friend-grade was not found near this binary or in \
             the store. Run `pacrat setup` at a terminal to type the path, or copy the \
             [[graders]] entry from the adapter's own header into the config file.",
        )?;
        register(config, &adapter)?;
        return Ok(());
    }
    if !ask {
        // No flag and nobody to ask: the question simply does not come up.
        return Ok(());
    }

    if !on_path("yay-friend") {
        println!("grader  yay-friend is not on PATH — skipping grader registration. The");
        println!("        contrib adapter is in contrib/graders/ when you want it.");
        return Ok(());
    }
    if probe_native() {
        println!("grader  this yay-friend emits pacrat-grade/v1 itself (`yay-friend grade`).");
        if ask_choice("        register it as a grader?", &["y", "n"], "n")? == "y" {
            register_native(config);
        } else {
            println!("grader  skipped — `pacrat setup` offers again any time.");
        }
        return Ok(());
    }
    if !on_path("jq") {
        println!("grader  this yay-friend has no `grade` subcommand, and jq is not on PATH");
        println!("        (the contrib adapter is a jq translation) — skipping grader");
        println!("        registration. pacman -S jq, then `pacrat setup` offers again.");
        return Ok(());
    }

    let adapter = match adapter_path(ctx) {
        Some(path) => path,
        None => {
            println!("grader  yay-friend and jq are on PATH, but the contrib adapter was not");
            println!("        found near this binary or in the store.");
            let typed = ask_line("        path to yay-friend-grade (empty to skip): ")?;
            if typed.is_empty() {
                println!("grader  skipped.");
                return Ok(());
            }
            let path = PathBuf::from(typed);
            if !path.is_file() {
                println!("grader  {} is not a file — skipping.", path.display());
                return Ok(());
            }
            path
        }
    };

    println!("grader  this yay-friend has no `grade` subcommand; the contrib adapter at");
    println!("        {}", adapter.display());
    println!("        translates its analysis cache to pacrat-grade/v1.");
    if ask_choice("        register it as a grader?", &["y", "n"], "n")? == "y" {
        register(config, &adapter)?;
    } else {
        println!("grader  skipped — `pacrat setup` offers again any time.");
    }
    Ok(())
}

/// An existing yay-friend entry. An argv-form one is offered the ADR-004
/// rewrite — one string, subject in the environment — at a terminal, and
/// only that: declining leaves it byte-for-byte as it was, a string-form
/// one has nothing to modernize, and no other grader is ever considered.
fn modernize_question(config: &mut Config, i: usize, ask: bool) -> Result<(), String> {
    if !ask || matches!(config.graders[i].cmd, Cmd::Line(_)) {
        println!("grader  yay-friend is already registered — leaving it as it is.");
        return Ok(());
    }
    let native = probe_native();
    let Some(updated) = modernized(&config.graders[i], native) else {
        println!("grader  yay-friend is already registered — leaving it as it is.");
        return Ok(());
    };
    println!("grader  yay-friend is registered in the argv form. The subject now");
    println!("        arrives as PACRAT_* environment variables, so one line is");
    println!("        enough:");
    println!("          cmd = \"{}\"", updated.cmd);
    if native && config.graders[i].scale.is_some() {
        println!("        (the native subcommand declares its own scale, so the pin");
        println!("        would be dropped)");
    }
    if ask_choice("        rewrite it to the string form?", &["y", "n"], "n")? == "y" {
        config.graders[i] = updated;
        println!("grader  rewrote yay-friend to the string form.");
    } else {
        println!("grader  left as it is.");
    }
    Ok(())
}

/// The string-form rewrite of an argv-form yay-friend entry: the native
/// subcommand when the installed binary has it, the adapter the entry
/// already points at otherwise. `None` when there is nothing to rewrite.
/// The entry's own timeout survives either way; the scale pin survives the
/// adapter rewrite (it still pins the same file) and is dropped for the
/// native one, whose report declares its own scale.
fn modernized(g: &Grader, native: bool) -> Option<Grader> {
    let Cmd::Argv(argv) = &g.cmd else { return None };
    let cmd = if native {
        Cmd::Line(NATIVE_CMD.into())
    } else {
        let program = argv.first().filter(|p| !p.is_empty())?;
        Cmd::Line(crate::out::shell_quote(program))
    };
    Some(Grader {
        name: g.name.clone(),
        cmd,
        timeout_s: g.timeout_s,
        scale: if native { None } else { g.scale },
    })
}

/// The one line the whole feature was named for: yay-friend speaking the
/// contract itself, subject from the environment, adapter not involved.
const NATIVE_CMD: &str = "yay-friend grade";

/// Does the installed yay-friend emit the contract itself? `yay-friend
/// grade --help` exits 0 exactly when the subcommand exists — cobra fails
/// nonzero on an unknown one — so the probe reads nothing, only the exit
/// status. Announced before it runs, like every external call.
fn probe_native() -> bool {
    if !on_path("yay-friend") {
        return false;
    }
    println!("grader  probing: yay-friend grade --help");
    subcommand_exists("yay-friend")
}

/// The probe itself, on any program, so the stubs in the tests can answer
/// both ways without a yay-friend installed.
fn subcommand_exists(program: &str) -> bool {
    Command::new(program)
        .args(["grade", "--help"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Register the native subcommand (ADR-004, as amended): one string, no
/// scale pin — the report declares its scale, and pinning what the tool
/// owns is the tool's business now. The timeout stays 600s because a cache
/// miss still calls a model.
fn register_native(config: &mut Config) {
    config.graders.push(Grader {
        name: "yay-friend".into(),
        cmd: Cmd::Line(NATIVE_CMD.into()),
        timeout_s: 600,
        scale: None,
    });
    println!("grader  registered yay-friend (native `{NATIVE_CMD}`).");
}

/// Register the contrib adapter, in the string form its header documents
/// (ADR-004): the subject arrives as PACRAT_* environment variables, so
/// the cmd is the adapter's absolute path and nothing else — quoted only
/// when the path needs it, because the line is handed to sh. A 600s
/// timeout (the miss path calls an LLM) and the 0-4 scale pin, as before.
fn register(config: &mut Config, adapter: &Path) -> Result<(), String> {
    let adapter = adapter
        .canonicalize()
        .map_err(|e| format!("{}: {e}", adapter.display()))?;
    config.graders.push(Grader {
        name: "yay-friend".into(),
        cmd: Cmd::Line(crate::out::shell_quote(&adapter.to_string_lossy())),
        timeout_s: 600,
        scale: Some(Scale { min: 0, max: 4 }),
    });
    println!("grader  registered yay-friend via {}", adapter.display());
    Ok(())
}

/// Is `program` an executable-looking file on PATH? The same lookup exec
/// will do; asked up front so the offer is only made when accepting it
/// could work.
fn on_path(program: &str) -> bool {
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// The contrib adapter, found without asking (ADR-004): the development
/// checkout first, then the store. Only when both rungs miss does the
/// interview fall back to asking for the path.
fn adapter_path(ctx: &Ctx) -> Option<PathBuf> {
    checkout_adapter().or_else(|| store_adapter(&ctx.store))
}

/// Rung 1: a checkout has `target/{debug,release}/pacrat` with `contrib/`
/// two levels up, so walking the ancestors of `current_exe()` finds it
/// wherever the target directory landed. An installed binary has no
/// checkout above it, which is the `None`.
fn checkout_adapter() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?.canonicalize().ok()?;
    exe.ancestors()
        .find_map(|dir| offerable(dir.join("contrib").join("graders").join("yay-friend-grade")))
}

/// Rung 2: the installed case, which the first real setup run was missing —
/// the binary lives in `~/.local/bin`, but the store is on every machine at
/// a place `Ctx` already knows, and pacrat nests inside it as `pacrat/`.
fn store_adapter(store: &Path) -> Option<PathBuf> {
    offerable(
        store
            .join("pacrat")
            .join("contrib")
            .join("graders")
            .join("yay-friend-grade"),
    )
}

/// A found path is offered only when accepting the offer could work: it
/// must exist and be executable, because what gets registered is a cmd
/// that sh will exec.
fn offerable(candidate: PathBuf) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::metadata(&candidate).ok()?;
    (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then_some(candidate)
}

/// One question, a fixed set of answers, a default that pressing Enter (or
/// EOF) keeps. Loops on anything else: a typo should re-ask, not silently
/// become the default.
fn ask_choice(prompt: &str, options: &[&str], default: &str) -> Result<String, String> {
    loop {
        let answer = ask_line(&format!("{prompt} [{}] ({default}): ", options.join("/")))?;
        if answer.is_empty() {
            return Ok(default.into());
        }
        if options.contains(&answer.as_str()) {
            return Ok(answer);
        }
        println!("        the answer is one of: {}", options.join(", "));
    }
}

/// Print a prompt, read one trimmed line. EOF reads as an empty answer.
fn ask_line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout().flush().map_err(|e| format!("stdout: {e}"))?;
    let mut answer = String::new();
    let read = io::stdin()
        .read_line(&mut answer)
        .map_err(|e| format!("stdin: {e}"))?;
    if read == 0 {
        println!();
        return Ok(String::new());
    }
    Ok(answer.trim().to_string())
}

// ------------------------------------------------------------------ state

/// What of the serving model is already in place on this host. One probe,
/// shared by `pacrat status` and by setup's own per-step "already present"
/// reporting, so the two can never disagree.
pub struct State {
    pub repo_dir: bool,
    pub repo_db: bool,
    /// A `[name]` section in /etc/pacman.conf. World-readable, so this is
    /// answerable without root. Known gap: `Include =` files are not
    /// followed, so a section hidden in an included file reads as absent.
    pub conf_section: bool,
    pub hook: bool,
    pub script: bool,
}

impl State {
    pub fn complete(&self) -> bool {
        self.repo_dir && self.repo_db && self.conf_section && self.hook && self.script
    }

    pub fn untouched(&self) -> bool {
        !self.repo_dir && !self.repo_db && !self.conf_section && !self.hook && !self.script
    }

    /// The pieces still missing, for a one-line report.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if !self.repo_dir || !self.repo_db {
            m.push("repo");
        }
        if !self.conf_section {
            m.push("pacman.conf");
        }
        if !self.hook || !self.script {
            m.push("guard");
        }
        m
    }
}

pub fn state(repo: &Repo) -> State {
    let repo_path = Path::new(&repo.path);
    let section = format!("[{}]", repo.name);
    let conf_section = fs::read_to_string(PACMAN_CONF)
        .map(|text| text.lines().any(|l| l.trim() == section))
        .unwrap_or(false);
    State {
        repo_dir: repo_path.is_dir(),
        repo_db: repo_path.join(format!("{}.db.tar.zst", repo.name)).exists(),
        conf_section,
        hook: Path::new(HOOK_DEST).exists(),
        script: Path::new(SCRIPT_DEST).exists(),
    }
}

fn yn(present: bool) -> &'static str {
    if present {
        "present"
    } else {
        "missing"
    }
}

// ---------------------------------------------------------------- step 1

fn step_pacman_conf(repo: &Repo, staged: &Path, st: &State, steps: &mut Steps) {
    println!("1. pacman.conf section — teach pacman about the repo");
    println!();
    if st.conf_section {
        println!(
            "   [{}] is already in {PACMAN_CONF} — nothing to do.",
            repo.name
        );
        println!();
        return;
    }
    let file = staged.join("pacman.conf.section");
    match steps {
        Steps::Print { apply } => {
            block(&pacman_conf_section(repo));
            println!();
            println!("   append it to {PACMAN_CONF} (root):");
            println!(
                "     sudo tee -a {PACMAN_CONF} <{} >/dev/null",
                sh(&file.to_string_lossy())
            );
            if !*apply {
                println!("     (`pacrat setup --apply` writes that staged file first)");
            }
        }
        Steps::Walk { session, sudo, .. } => {
            // A walked step shows what a confirm needs (ADR-003, amended):
            // the destination and the staged file, readable before
            // answering — the section's body belongs to `--print`. The
            // command is a tee whose stdin is that same staged file, so
            // what was readable is what lands; tee echoes it back as the
            // receipt.
            println!("   append to  {PACMAN_CONF}");
            println!("   staged at  {}", file.display());
            println!("   (`pacrat setup --print` shows the section itself)");
            session.run(step_cmd(*sudo, &["tee", "-a", PACMAN_CONF]).reading(file.clone()));
        }
    }
    println!("   appending puts the section last, so official repos win any name");
    println!("   collision — the safe default. Move it above [core] by hand if you");
    println!("   want curated builds to shadow official packages.");
    println!("   the database in step 2 must exist before the next `pacman -Sy`.");
    println!();
}

fn pacman_conf_section(repo: &Repo) -> String {
    let mut s = format!("[{}]\n", repo.name);
    match &repo.server {
        // Signing posture is per *section* in pacman.conf — there is no
        // per-Server SigLevel — so the moment this repo is fetched over a
        // network, the whole section has to be the stricter of the two.
        Some(url) => {
            s.push_str("# PROVISIONAL (pending Aaron's call): this repo is served over a\n");
            s.push_str("# network, so it is set to require package signatures. pacrat has\n");
            s.push_str("# no signing infrastructure yet (phase 2) — until packages are\n");
            s.push_str("# signed, pacman will refuse them, including your own local builds.\n");
            s.push_str("# Sign with `makepkg --sign` and import the key, or drop back to\n");
            s.push_str("# `SigLevel = Optional TrustAll` knowing the mirror is then trusted\n");
            s.push_str("# on the strength of the transport alone.\n");
            s.push_str("SigLevel = Required DatabaseOptional\n");
            s.push_str("# built here: this host's own repo directory, consulted first\n");
            s.push_str(&format!("Server = file://{}\n", repo.path));
            s.push_str("# served there: the shared URL other hosts fetch from (repo.server)\n");
            s.push_str(&format!("Server = {url}\n"));
        }
        // Local-only: nothing crosses a network, and the packages were built
        // on this machine from a reviewed tree, so TrustAll costs nothing
        // that signing would buy back.
        None => {
            s.push_str("# local only — nothing is fetched over a network, so unsigned\n");
            s.push_str("# packages built on this host are accepted as-is. Setting\n");
            s.push_str("# repo.server in config.toml switches this section to requiring\n");
            s.push_str("# signatures; see `pacrat setup` for what that then needs.\n");
            s.push_str("SigLevel = Optional TrustAll\n");
            s.push_str(&format!("Server = file://{}\n", repo.path));
        }
    }
    s
}

// ---------------------------------------------------------------- step 2

fn step_repo(repo_path: &Path, db: &Path, st: &State, steps: &mut Steps) -> Result<(), String> {
    println!("2. repo directory and empty database");
    println!();
    if st.repo_dir && st.repo_db {
        println!(
            "   {} already holds {} — nothing to do.",
            repo_path.display(),
            db.file_name().unwrap_or_default().to_string_lossy()
        );
        println!();
        return Ok(());
    }
    match steps {
        Steps::Print { apply } => {
            let user = env::var("USER").unwrap_or_else(|_| "$USER".into());
            println!(
                "     sudo install -d -o {user} {}",
                sh(&repo_path.to_string_lossy())
            );
            println!("     repo-add {}", sh(&db.to_string_lossy()));
            println!();
            if !*apply {
                println!("   `pacrat setup --apply` runs these itself when the path is already");
                println!("   yours to write; the sudo line is only for a root-owned location.");
                println!();
                return Ok(());
            }
            match ensure_dir(repo_path) {
                Ok(()) => {}
                Err(Denied) => {
                    // A root-owned location, headless: the two printed
                    // lines are the answer, as they always were.
                    println!(
                        "   {} needs root — run the two lines above, then",
                        repo_path.display()
                    );
                    println!("   re-run `pacrat setup --apply`.");
                    println!();
                    return Ok(());
                }
                Err(Failed(e)) => return Err(e),
            }
        }
        Steps::Walk {
            session,
            sudo: true,
            owner,
        } => {
            println!("   directory  {}", repo_path.display());
            match ensure_dir(repo_path) {
                Ok(()) => {}
                Err(Denied) => {
                    // A root-owned location. The confirmed flow makes the
                    // directory — owned by this user, so repo-add stays a
                    // plain unprivileged call below.
                    let ran = session.run(crate::elevate::Cmd::sudo(&[
                        "install",
                        "-d",
                        "-o",
                        owner,
                        &repo_path.to_string_lossy(),
                    ])) == crate::elevate::Verdict::Ran;
                    if !ran {
                        println!("   the database needs that directory first — nothing more");
                        println!("   to do here until it exists.");
                        println!();
                        return Ok(());
                    }
                }
                Err(Failed(e)) => return Err(e),
            }
        }
        Steps::Walk {
            session,
            sudo: false,
            owner,
        } => {
            // Root maintenance mode: nothing here is "user-doable" — even
            // making the directory is a root action now, so it is shown
            // and asked like every other one. `-o <operator>` because the
            // directory belongs to the user whose serving model this is:
            // user-mode `build` writes into it.
            println!("   directory  {}", repo_path.display());
            if !st.repo_dir {
                let ran = session.run(step_cmd(
                    false,
                    &["install", "-d", "-o", owner, &repo_path.to_string_lossy()],
                )) == crate::elevate::Verdict::Ran;
                if !ran {
                    println!("   the database needs that directory first — nothing more");
                    println!("   to do here until it exists.");
                    println!();
                    return Ok(());
                }
            }
            if db.exists() {
                println!("   db   {} already present — left alone", db.display());
            } else {
                session.run(step_cmd(false, &["repo-add", &db.to_string_lossy()]));
            }
            println!();
            return Ok(());
        }
    }
    println!("   dir  {} ready", repo_path.display());

    if db.exists() {
        println!("   db   {} already present — left alone", db.display());
        println!();
        return Ok(());
    }
    // The always-visible-calls rule: show the argv, then run it.
    println!("   $ repo-add {}", sh(&db.to_string_lossy()));
    let status = Command::new("repo-add")
        .arg(db)
        .status()
        .map_err(|e| format!("repo-add: {e} (it ships with the `pacman` package)"))?;
    if !status.success() {
        return Err(format!("repo-add {} failed ({status})", db.display()));
    }
    println!("   db   {} created", db.display());
    println!();
    Ok(())
}

/// `ensure_dir` distinguishes "you need root for this" from a real failure;
/// only the former is a normal outcome of setup on a fresh host.
enum DirErr {
    Denied,
    Failed(String),
}
use DirErr::{Denied, Failed};

fn ensure_dir(dir: &Path) -> Result<(), DirErr> {
    if !dir.is_dir() {
        return match fs::create_dir_all(dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Err(Denied),
            Err(e) => Err(Failed(format!("{}: {e}", dir.display()))),
        };
    }
    // The directory exists, so the question is whether *this* user can write
    // into it. Mode bits don't answer that on their own (supplementary
    // groups, ACLs, read-only mounts), so ask the filesystem by trying.
    let probe = dir.join(".pacrat-write-probe");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(_) => Err(Denied),
    }
}

// ---------------------------------------------------------------- step 3

fn step_guard(repo: &Repo, staged: &Path, st: &State, steps: &mut Steps) {
    println!("3. guard hook — abort installs that went around curation");
    println!();
    if st.hook && st.script {
        println!("   {HOOK_DEST} and");
        println!("   {SCRIPT_DEST} are both installed — nothing to do.");
        println!("   (pacrat does not diff them; re-install from the staged copies");
        println!("   after upgrading pacrat.)");
        println!();
        return;
    }
    let script = staged.join("pacrat-guard.sh");
    let hook = staged.join("pacrat-guard.hook");
    match steps {
        Steps::Print { apply } => {
            println!("   {HOOK_DEST}");
            block(&guard_hook());
            println!();
            println!("   {SCRIPT_DEST}");
            block(&guard_script(repo));
            println!();
            println!("   install both (root):");
            println!(
                "     sudo install -Dm755 {} {SCRIPT_DEST}",
                sh(&script.to_string_lossy())
            );
            println!(
                "     sudo install -Dm644 {} {HOOK_DEST}",
                sh(&hook.to_string_lossy())
            );
            if !*apply {
                println!("     (`pacrat setup --apply` writes those staged files first)");
            }
            println!();
            println!("   the guard stands aside for pacrat's own transactions via a marker");
            println!("   file:");
            println!(
                "     {}",
                sh(&Path::new(&repo.path).join(MARKER).to_string_lossy())
            );
            println!("   holding `pid=<pid> started=<unix seconds>`. It is honoured only while");
            println!(
                "   that pid is alive and the timestamp is under {MARKER_MAX_AGE_S}s, and every use"
            );
            println!("   of it is announced on stderr — an interrupted build leaves a stale");
            println!("   marker, and a stale marker is ignored rather than trusted.");
        }
        Steps::Walk { session, sudo, .. } => {
            // Destinations and the staged files, readable before answering;
            // the hundred lines of guard script belong to `--print`.
            println!("   script to  {SCRIPT_DEST}");
            println!("   staged at  {}", script.display());
            println!("   hook to    {HOOK_DEST}");
            println!("   staged at  {}", hook.display());
            println!("   (`pacrat setup --print` shows both files in full)");
            // Script before hook, deliberately: a hook whose Exec target is
            // not there yet would fail every transaction in the window
            // between the two confirms.
            session.run(step_cmd(
                *sudo,
                &["install", "-Dm755", &script.to_string_lossy(), SCRIPT_DEST],
            ));
            session.run(step_cmd(
                *sudo,
                &["install", "-Dm644", &hook.to_string_lossy(), HOOK_DEST],
            ));
            println!();
        }
    }
}

fn guard_hook() -> String {
    format!(
        "# pacrat guard — installed by `pacrat setup`; logic lives in the Exec script.\n\
         [Trigger]\n\
         Operation = Install\n\
         Operation = Upgrade\n\
         Type = Package\n\
         Target = *\n\
         \n\
         [Action]\n\
         Description = pacrat: checking that these installs came through curation...\n\
         When = PreTransaction\n\
         Exec = {SCRIPT_DEST}\n\
         NeedsTargets\n\
         AbortOnFail\n"
    )
}

/// The guard's actual logic. Kept as a template with `@TOKEN@` holes rather
/// than a `format!` so the shell's own `${...}` stays readable. Every hole
/// is filled from a `Repo` that `Repo::validate` has already vetted, so the
/// single-quoted shell literals below cannot be broken out of.
const GUARD_SH: &str = r##"#!/bin/sh
# pacrat guard — the Exec half of /etc/pacman.d/hooks/pacrat-guard.hook.
# Generated by `pacrat setup`; edit pacrat, not this file.
#
# WHAT IT CATCHES: a target that exists in no sync database. That is the
# signature of an AUR helper building a package and handing the file to
# `pacman -U` — curation bypassed. Anything pacman can resolve from a sync
# db passes untouched: the official repos, and this one
#     [@REPO@]
# once pacrat has built into it, which is exactly the curated path.
#
# KNOWN LIMIT: the check is by package *name*. Rebuilding a name that the
# curated repo already carries, and installing that file with `pacman -U`,
# passes — the name resolves. Comparing version and identity is future
# hardening; see ADR-001.
#
# WHY NOT AN ENV MARKER: pacman hooks run as root and do not inherit the
# invoking user's environment. yay -> sudo -> pacman drops PACRAT_BYPASS
# before this script ever sees it, so an env check alone would be a guard
# that never fires. Hence the two escapes below.
#
# ESCAPE 1 — the marker file
#     @MARKER@
# which pacrat's own build/sync flows create for the duration of their
# transaction and remove afterwards. It must hold `pid=<pid> started=<unix
# seconds>`, and it is honoured only while that pid is alive and the
# timestamp is under @MAXAGE@ seconds: an interrupted build leaves a stale
# marker, and a stale marker that silently disabled the guard forever would
# be worse than no guard. Every use is announced on stderr. Any user who can
# write the repo directory can still forge one by hand — this is a rail
# against brain-farts, not an access control, and it is not trying to be one.
#
# ESCAPE 2 — PACRAT_BYPASS=1, which only survives if you carry it across
# sudo deliberately:
#     PACRAT_BYPASS=1 sudo -E pacman -U ./something.pkg.tar.zst
# That verbosity is the point; there is no way to set it once and forget.
#
# FAILURE POSTURE: if the guard cannot answer the question (no mktemp, no
# readable sync dbs) it passes and says so. A broken rail must not leave a
# machine unable to install packages.
set -u

marker='@MARKER@'

if [ "${PACRAT_BYPASS:-}" = "1" ]; then
	printf 'pacrat guard: PACRAT_BYPASS=1 — allowing this transaction.\n' >&2
	exit 0
fi

if [ -e "$marker" ]; then
	mpid=$(sed -n 's/.*pid=\([0-9][0-9]*\).*/\1/p' "$marker" 2>/dev/null | head -1)
	mts=$(sed -n 's/.*started=\([0-9][0-9]*\).*/\1/p' "$marker" 2>/dev/null | head -1)
	now=$(date +%s 2>/dev/null || echo 0)
	stale=''
	if [ -z "$mpid" ] || [ -z "$mts" ]; then
		stale='it has no "pid=<pid> started=<seconds>" line'
	elif ! kill -0 "$mpid" 2>/dev/null; then
		stale="its owner (pid $mpid) is gone"
	elif [ "$now" -gt 0 ] && [ "$((now - mts))" -gt @MAXAGE@ ]; then
		stale="it is $((now - mts))s old (limit @MAXAGE@s)"
	fi
	if [ -z "$stale" ]; then
		printf 'pacrat guard: standing aside — pacrat transaction in progress (pid %s).\n' "$mpid" >&2
		exit 0
	fi
	printf 'pacrat guard: ignoring stale marker %s — %s. Delete it if no build is running.\n' "$marker" "$stale" >&2
fi

targets=$(cat)
[ -n "$targets" ] || exit 0

known=$(mktemp) || {
	printf 'pacrat guard: mktemp failed — passing (a rail, not a gate).\n' >&2
	exit 0
}
trap 'rm -f "$known"' EXIT INT TERM

pacman -Sl 2>/dev/null | cut -d' ' -f2 | sort -u >"$known"
if [ ! -s "$known" ]; then
	printf 'pacrat guard: no sync databases readable — passing rather than wedging pacman.\n' >&2
	exit 0
fi

foreign=$(printf '%s\n' "$targets" | sort -u | comm -23 - "$known")
[ -n "$foreign" ] || exit 0

{
	echo
	echo 'pacrat guard: refusing an install that went around curation.'
	echo
	echo 'These targets are in no sync repository, so they came from a local'
	echo 'package file — something built them outside pacrat:'
	echo
	printf '  %s\n' $foreign
	echo
	echo 'The curated path:'
	echo '  pacrat vendor <pkg>     review the PKGBUILD, pin the commit'
	echo '  pacrat build  <pkg>     build into [@REPO@]'
	echo '  sudo pacman -Sy <pkg>   install it like any other repo package'
	echo
	echo 'Or, deliberately, this once:'
	echo '  PACRAT_BYPASS=1 sudo -E pacman -U <file>'
	echo
} >&2
exit 1
"##;

fn guard_script(repo: &Repo) -> String {
    let marker = Path::new(&repo.path).join(MARKER);
    GUARD_SH
        .replace("@MARKER@", &marker.to_string_lossy())
        .replace("@REPO@", &repo.name)
        .replace("@MAXAGE@", &MARKER_MAX_AGE_S.to_string())
}

// ---------------------------------------------------------------- plumbing

/// Render one shell word. `Repo::validate` has already rejected quotes,
/// backslashes, `$`, backticks and control characters, so single-quoting is
/// total here: nothing survives that could end the quote. Ordinary paths are
/// left bare so the printed commands stay readable.
fn sh(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:@%+-=,".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{word}'")
    }
}

/// Where the root-owned files are staged so the install commands have
/// something real to copy from. As the user: XDG state (ADR-001 puts host
/// scratch there). As root: /run/pacrat — sudo does not reliably say whose
/// HOME is in the environment, and root-owned files must never land in the
/// operator's home, so a root-owned tmpfs path is the honest scratch.
fn staging_dir(root: bool) -> Result<PathBuf, String> {
    if root {
        return Ok(PathBuf::from("/run/pacrat/setup"));
    }
    Ok(crate::ctx::state_dir()?.join("setup"))
}

fn stage(dir: &Path, name: &str, content: &str) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(name);
    fs::write(&path, content).map_err(|e| format!("{}: {e}", path.display()))
}

/// Indent a printed file body so it reads as content, not as instructions.
fn block(text: &str) {
    for line in text.lines() {
        if line.is_empty() {
            println!();
        } else {
            println!("     {line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn repo() -> Repo {
        Repo {
            name: "dotfiles-aur".into(),
            path: "/var/cache/pacrat/repo".into(),
            server: None,
        }
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    // ---- the flow decision (ADR-003, both amendments) ----

    /// The whole table: root/user × SUDO_USER × terminal × flags. Pure
    /// data in, pure data out — nothing here is root, holds a terminal,
    /// or touches this machine.
    #[test]
    fn the_flow_table() {
        use Flow::*;
        let root = |op: &str| RootDirect {
            operator: op.into(),
        };
        let root_print = |op: &str| RootPrint {
            operator: op.into(),
        };
        //     euid, sudo_user, tty, print, apply → the flow
        type Case<'a> = (u32, Option<&'a str>, bool, bool, bool, Flow);
        let cases: &[Case] = &[
            // A user at a terminal gets the guided flow; `--apply` is a
            // synonym, and a lingering SUDO_USER means nothing off-root.
            (1000, None, true, false, false, Guided),
            (1000, None, true, false, true, Guided),
            (1000, Some("aaron"), true, false, false, Guided),
            // `--print` at a terminal is the wall, changing nothing.
            (1000, None, true, true, false, Print),
            // Headless keeps printing; `--apply` keeps its headless
            // meaning (user-doable steps plus the staged files).
            (1000, None, false, false, false, HeadlessPrint),
            (1000, None, false, true, false, HeadlessPrint),
            (1000, None, false, false, true, HeadlessApply),
            // Root is maintenance mode, for the operator SUDO_USER names.
            (0, Some("aaron"), true, false, false, root("aaron")),
            (0, Some("aaron"), true, false, true, root("aaron")),
            (0, Some("aaron"), true, true, false, root_print("aaron")),
            (0, Some("aaron"), false, false, false, root_print("aaron")),
            (0, Some("aaron"), false, false, true, root_print("aaron")),
        ];
        for (euid, sudo_user, tty, print, apply, want) in cases {
            assert_eq!(
                flow(*euid, *sudo_user, *tty, *print, *apply).as_ref(),
                Ok(want),
                "euid {euid}, sudo_user {sudo_user:?}, tty {tty}, print {print}, apply {apply}"
            );
        }
    }

    /// Root with nobody behind it is refused: no SUDO_USER, an empty one,
    /// and root-sudoing-root all read as "whose setup would this be?".
    #[test]
    fn root_with_no_operator_is_refused() {
        for sudo_user in [None, Some(""), Some("root")] {
            for (tty, print) in [(true, false), (false, false), (true, true)] {
                let err = flow(0, sudo_user, tty, print, false)
                    .expect_err(&format!("{sudo_user:?} must refuse"));
                assert!(err.contains("SUDO_USER"), "{err}");
                assert!(err.contains("pacrat setup"), "no pointer at the fix: {err}");
            }
        }
    }

    // ---- rendering ----

    #[test]
    fn section_is_local_and_trusting_without_a_server() {
        let s = pacman_conf_section(&repo());
        assert!(s.starts_with("[dotfiles-aur]\n"));
        assert!(s.contains("SigLevel = Optional TrustAll"));
        assert!(s.contains("Server = file:///var/cache/pacrat/repo"));
        assert_eq!(s.matches("Server =").count(), 1);
    }

    #[test]
    fn a_served_repo_requires_signatures_and_names_both_roles() {
        let mut r = repo();
        r.server = Some("https://example.invalid/repo".into());
        let s = pacman_conf_section(&r);
        assert!(s.contains("SigLevel = Required DatabaseOptional"));
        // Comments may *mention* TrustAll as the opt-out; no live directive
        // may set it once the repo is fetched over a network.
        assert!(
            !s.lines()
                .any(|l| !l.trim_start().starts_with('#') && l.contains("TrustAll")),
            "a directive line still sets TrustAll:\n{s}"
        );
        assert!(s.contains("PROVISIONAL"));
        assert_eq!(s.matches("Server =").count(), 2);
        // Local build output is consulted before the shared mirror.
        let local = s.find("file:///var/cache").unwrap();
        let shared = s.find("https://example.invalid").unwrap();
        assert!(local < shared);
    }

    #[test]
    fn hook_aborts_and_feeds_targets_to_the_script() {
        let h = guard_hook();
        assert!(h.contains("When = PreTransaction"));
        assert!(h.contains("NeedsTargets"));
        assert!(h.contains("AbortOnFail")); // without this a failure is only logged
        assert!(h.contains(&format!("Exec = {SCRIPT_DEST}")));
    }

    #[test]
    fn every_template_hole_is_filled() {
        let s = guard_script(&repo());
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains("/var/cache/pacrat/repo/.pacrat-transaction"));
        assert!(s.contains("[dotfiles-aur]"));
        for hole in ["@MARKER@", "@REPO@", "@MAXAGE@"] {
            assert!(!s.contains(hole), "{hole} survived substitution");
        }
    }

    #[test]
    fn shell_words_are_quoted_only_when_they_need_it() {
        assert_eq!(sh("/var/cache/pacrat/repo"), "/var/cache/pacrat/repo");
        assert_eq!(sh("/var/cache/my repo"), "'/var/cache/my repo'");
    }

    /// F1's real contract: a hostile path is rejected by the validator, so
    /// it never reaches the renderer at all.
    #[test]
    fn a_hostile_repo_path_never_reaches_the_script() {
        for path in ["/var/cache/it's", "/var/cache/$(id)", "relative"] {
            let r = Repo {
                path: path.into(),
                ..repo()
            };
            assert!(r.validate().is_err(), "{path:?} should be rejected");
        }
    }

    // ---- grader registration (ADR-004) ----

    #[test]
    fn the_store_rung_offers_only_an_executable_adapter() {
        let dir = env::temp_dir().join(format!("pacrat-setup-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let graders = dir.join("pacrat/contrib/graders");
        fs::create_dir_all(&graders).unwrap();

        // Nothing there: the rung misses rather than guessing a path.
        assert_eq!(store_adapter(&dir), None);

        // Present but not executable: found is not offerable — registering
        // it would write a cmd sh cannot exec.
        let adapter = graders.join("yay-friend-grade");
        fs::write(&adapter, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store_adapter(&dir), None);

        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(store_adapter(&dir), Some(adapter.clone()));

        // A directory wearing the right name is not an adapter.
        fs::remove_file(&adapter).unwrap();
        fs::create_dir_all(&adapter).unwrap();
        assert_eq!(store_adapter(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The native probe reads nothing but the exit status: a yay-friend
    /// with the `grade` subcommand answers 0 to `grade --help`, and a cobra
    /// CLI without it fails nonzero on the unknown subcommand.
    #[test]
    fn the_native_probe_is_an_exit_status() {
        let dir = env::temp_dir().join(format!("pacrat-setup-probe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let stub = |name: &str, body: &str| {
            let path = dir.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let native = stub(
            "native",
            "#!/bin/sh\n[ \"$1\" = grade ] && exit 0\nexit 1\n",
        );
        let old = stub(
            "old",
            "#!/bin/sh\necho 'Error: unknown command \"grade\"' >&2\nexit 1\n",
        );
        assert!(subcommand_exists(&native.to_string_lossy()));
        assert!(!subcommand_exists(&old.to_string_lossy()));
        // No binary at all is not a native yay-friend either.
        assert!(!subcommand_exists("/nonexistent/yay-friend"));
        let _ = fs::remove_dir_all(&dir);
    }

    fn argv_yay_friend() -> Grader {
        Grader {
            name: "yay-friend".into(),
            cmd: Cmd::Argv(vec![
                "/opt/adapters/yay-friend-grade".into(),
                "--package".into(),
                "{package}".into(),
                "--tree".into(),
                "{tree}".into(),
                "--commit".into(),
                "{commit}".into(),
            ]),
            timeout_s: 700,
            scale: Some(Scale { min: 0, max: 4 }),
        }
    }

    #[test]
    fn modernizing_keeps_the_adapter_and_the_owners_settings() {
        let updated = modernized(&argv_yay_friend(), false).unwrap();
        assert_eq!(
            updated.cmd,
            Cmd::Line("/opt/adapters/yay-friend-grade".into())
        );
        assert_eq!(updated.timeout_s, 700, "the owner's timeout survives");
        assert_eq!(
            updated.scale,
            Some(Scale { min: 0, max: 4 }),
            "the pin still pins the same file"
        );
        // The rewritten entry is one the config writer will accept.
        assert_eq!(updated.validate(), Ok(()));

        // A path sh would word-split is quoted on its way into the line.
        let mut spaced = argv_yay_friend();
        spaced.cmd = Cmd::Argv(vec!["/opt/my adapters/yay-friend-grade".into()]);
        assert_eq!(
            modernized(&spaced, false).unwrap().cmd,
            Cmd::Line("'/opt/my adapters/yay-friend-grade'".into())
        );
    }

    #[test]
    fn modernizing_to_the_native_subcommand_drops_the_pin() {
        let updated = modernized(&argv_yay_friend(), true).unwrap();
        assert_eq!(updated.cmd, Cmd::Line(NATIVE_CMD.into()));
        assert_eq!(updated.timeout_s, 700, "the owner's timeout survives");
        assert_eq!(
            updated.scale, None,
            "the native report declares its own scale"
        );
        assert_eq!(updated.validate(), Ok(()));
    }

    #[test]
    fn a_string_entry_and_a_broken_entry_have_nothing_to_modernize() {
        let mut g = argv_yay_friend();
        g.cmd = Cmd::Line("/opt/adapters/yay-friend-grade".into());
        assert_eq!(modernized(&g, false), None);
        g.cmd = Cmd::Argv(vec![]);
        assert_eq!(modernized(&g, false), None);
    }

    /// Headless, an existing entry is never touched: the rewrite needs a
    /// terminal to say yes at, and this path never even probes.
    #[test]
    fn without_a_terminal_an_existing_entry_is_left_alone() {
        let mut config = Config::default();
        config.graders.push(argv_yay_friend());
        let before = config.clone();
        modernize_question(&mut config, 0, false).unwrap();
        assert_eq!(config, before);
    }

    // ---- the guard, actually executed ----

    struct Fixture {
        dir: PathBuf,
        script: PathBuf,
        marker: PathBuf,
        bin: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// A rendered guard script plus a stub `pacman` whose `-Sl` output is
    /// the only sync-db truth these tests depend on. Nothing here touches
    /// the real system.
    fn fixture(tag: &str) -> Fixture {
        let dir = env::temp_dir().join(format!("pacrat-guard-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let repo_dir = dir.join("repo");
        let bin = dir.join("bin");
        fs::create_dir_all(&repo_dir).unwrap();
        fs::create_dir_all(&bin).unwrap();

        let pacman = bin.join("pacman");
        fs::write(
            &pacman,
            "#!/bin/sh\n\
             [ \"$1\" = \"-Sl\" ] || exit 1\n\
             cat <<'EOF'\n\
             core bash 5.2 [installed]\n\
             extra coreutils 9.5\n\
             dotfiles-aur curated 1.0\n\
             EOF\n",
        )
        .unwrap();
        fs::set_permissions(&pacman, fs::Permissions::from_mode(0o755)).unwrap();

        let r = Repo {
            name: "dotfiles-aur".into(),
            path: repo_dir.to_string_lossy().into_owned(),
            server: None,
        };
        r.validate().expect("fixture repo must be valid");
        let script = dir.join("pacrat-guard.sh");
        fs::write(&script, guard_script(&r)).unwrap();

        Fixture {
            marker: repo_dir.join(MARKER),
            dir,
            script,
            bin,
        }
    }

    /// Run the guard for real: `sh script`, targets on stdin, a PATH whose
    /// `pacman` is the stub. Returns (passed, stderr).
    fn run_guard(f: &Fixture, targets: &str, bypass: bool) -> (bool, String) {
        let mut cmd = Command::new("sh");
        cmd.arg(&f.script)
            .env("PATH", format!("{}:/usr/bin:/bin", f.bin.display()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if bypass {
            cmd.env("PACRAT_BYPASS", "1");
        } else {
            cmd.env_remove("PACRAT_BYPASS");
        }
        let mut child = cmd.spawn().expect("sh");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(targets.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    #[test]
    fn guard_blocks_exactly_the_targets_no_repo_offers() {
        let f = fixture("targets");
        let cases: &[(&str, bool, &str)] = &[
            ("bash\ncoreutils\n", true, ""),
            // A name the curated repo already carries is the curated path.
            ("curated\n", true, ""),
            ("bash\nsneaky-aur-thing\n", false, "sneaky-aur-thing"),
            ("", true, ""),
        ];
        for (targets, want_pass, needle) in cases {
            let (passed, err) = run_guard(&f, targets, false);
            assert_eq!(
                passed, *want_pass,
                "targets {targets:?} — stderr was:\n{err}"
            );
            if !needle.is_empty() {
                assert!(err.contains(needle), "expected {needle:?} in:\n{err}");
                assert!(err.contains("pacrat vendor"), "no pointer message:\n{err}");
            }
        }
    }

    #[test]
    fn bypass_env_passes_and_announces_itself() {
        let f = fixture("bypass");
        let (passed, err) = run_guard(&f, "sneaky-aur-thing\n", true);
        assert!(passed, "stderr:\n{err}");
        assert!(err.contains("PACRAT_BYPASS=1"), "silent bypass:\n{err}");
    }

    #[test]
    fn fresh_marker_passes_and_announces_itself() {
        let f = fixture("fresh");
        fs::write(
            &f.marker,
            format!("pid={} started={}\n", std::process::id(), now()),
        )
        .unwrap();
        let (passed, err) = run_guard(&f, "sneaky-aur-thing\n", false);
        assert!(passed, "fresh marker should pass; stderr:\n{err}");
        assert!(
            err.contains("standing aside") && err.contains("transaction in progress"),
            "marker escape must not be silent:\n{err}"
        );
    }

    #[test]
    fn stale_markers_are_ignored_and_the_guard_still_bites() {
        // A pid that has certainly exited: spawn and reap one.
        let mut done = Command::new("true").spawn().unwrap();
        done.wait().unwrap();
        let dead_pid = done.id();

        let cases: &[(&str, String)] = &[
            (
                "age",
                format!(
                    "pid={} started={}\n",
                    std::process::id(),
                    now() - MARKER_MAX_AGE_S * 2
                ),
            ),
            ("deadpid", format!("pid={dead_pid} started={}\n", now())),
            ("garbage", "touched by hand\n".to_string()),
            ("empty", String::new()),
        ];
        for (tag, content) in cases {
            let f = fixture(tag);
            fs::write(&f.marker, content).unwrap();
            let (passed, err) = run_guard(&f, "sneaky-aur-thing\n", false);
            assert!(
                !passed,
                "stale marker ({tag}) must not disable the guard; stderr:\n{err}"
            );
            assert!(
                err.contains("ignoring stale marker"),
                "stale marker ({tag}) must say so:\n{err}"
            );
        }
    }
}
