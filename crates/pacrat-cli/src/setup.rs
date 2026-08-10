//! `pacrat setup` — put this host on the serving model: the pacman.conf
//! section for the local repo, the repo directory and its (empty) database,
//! and the PreTransaction guard that aborts installs going around curation.
//!
//! pacrat never elevates. Everything root-owned is *printed* for the user to
//! run; `--apply` performs only the steps this user can already do without
//! sudo, and stages the root-owned files somewhere user-writable so the
//! printed `sudo install` lines work verbatim.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use pacrat_core::config::Repo;

use crate::ctx::Ctx;

/// Where the guard's two halves live once installed. The hook is data for
/// alpm; the script is the part with the judgement in it.
const HOOK_DEST: &str = "/etc/pacman.d/hooks/pacrat-guard.hook";
const SCRIPT_DEST: &str = "/usr/share/pacrat/pacrat-guard.sh";

/// Dropped in the repo directory for the duration of pacrat's own
/// transactions; the guard passes while it exists. See `guard_script`.
const MARKER: &str = ".pacrat-transaction";

pub fn run(ctx: &Ctx, apply: bool) -> Result<(), String> {
    let repo = &ctx.config.repo;
    let repo_path = PathBuf::from(&repo.path);
    let db = repo_path.join(format!("{}.db.tar.zst", repo.name));
    let staged = staging_dir()?;

    println!("pacrat setup — {} onto the serving model", ctx.host);
    println!("repo    [{}] {}", repo.name, repo.path);
    println!(
        "mode    {}",
        if apply {
            "--apply (user-writable steps run here; root steps still printed)"
        } else {
            "print only (nothing is written; add --apply to do the user-owned steps)"
        }
    );
    println!();

    if apply {
        stage(&staged, "pacman.conf.section", &pacman_conf_section(repo))?;
        stage(&staged, "pacrat-guard.hook", &guard_hook())?;
        stage(&staged, "pacrat-guard.sh", &guard_script(repo))?;
        println!("staged  {} (three files, user-owned)", staged.display());
        println!();
    }

    step_pacman_conf(repo, &staged, apply);
    step_repo(&repo_path, &db, apply)?;
    step_guard(repo, &staged, apply);

    Ok(())
}

// ---------------------------------------------------------------- step 1

fn step_pacman_conf(repo: &Repo, staged: &Path, apply: bool) {
    println!("1. pacman.conf section — teach pacman about the repo");
    println!();
    block(&pacman_conf_section(repo));
    println!();
    println!("   append it to /etc/pacman.conf (root):");
    println!(
        "     sudo tee -a /etc/pacman.conf <{} >/dev/null",
        staged.join("pacman.conf.section").display()
    );
    if !apply {
        println!("     (`pacrat setup --apply` writes that staged file first)");
    }
    println!("   appending puts the section last, so official repos win any name");
    println!("   collision — the safe default. Move it above [core] by hand if you");
    println!("   want curated builds to shadow official packages.");
    println!("   the database in step 2 must exist before the next `pacman -Sy`.");
    println!();
}

fn pacman_conf_section(repo: &Repo) -> String {
    let mut s = format!("[{}]\nSigLevel = Optional TrustAll\n", repo.name);
    match &repo.server {
        Some(url) => {
            s.push_str("# built here: this host's own repo directory, consulted first\n");
            s.push_str(&format!("Server = file://{}\n", repo.path));
            s.push_str("# served there: the shared URL other hosts fetch from (repo.server)\n");
            s.push_str(&format!("Server = {url}\n"));
        }
        None => {
            s.push_str("# built and served locally; set repo.server in config.toml to\n");
            s.push_str("# publish this repo to the rest of the fleet\n");
            s.push_str(&format!("Server = file://{}\n", repo.path));
        }
    }
    s
}

// ---------------------------------------------------------------- step 2

fn step_repo(repo_path: &Path, db: &Path, apply: bool) -> Result<(), String> {
    let user = env::var("USER").unwrap_or_else(|_| "$USER".into());
    println!("2. repo directory and empty database");
    println!();
    println!("     sudo install -d -o {user} {}", repo_path.display());
    println!("     repo-add {}", db.display());
    println!();

    if !apply {
        println!("   `pacrat setup --apply` runs these itself when the path is already");
        println!("   yours to write; the sudo line is only for a root-owned location.");
        println!();
        return Ok(());
    }

    match ensure_dir(repo_path) {
        Ok(()) => {}
        Err(Denied) => {
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
    println!("   dir  {} ready", repo_path.display());

    if db.exists() {
        println!("   db   {} already present — left alone", db.display());
        println!();
        return Ok(());
    }
    // The always-visible-calls rule: show the argv, then run it.
    println!("   $ repo-add {}", db.display());
    let status = Command::new("repo-add").arg(db).status().map_err(|e| {
        format!(
            "repo-add: {e} (it ships with pacman's devtools — install pacman-contrib/base-devel)"
        )
    })?;
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

fn step_guard(repo: &Repo, staged: &Path, apply: bool) {
    println!("3. guard hook — abort installs that went around curation");
    println!();
    println!("   {HOOK_DEST}");
    block(&guard_hook());
    println!();
    println!("   {SCRIPT_DEST}");
    block(&guard_script(repo));
    println!();
    println!("   install both (root):");
    println!(
        "     sudo install -Dm755 {} {SCRIPT_DEST}",
        staged.join("pacrat-guard.sh").display()
    );
    println!(
        "     sudo install -Dm644 {} {HOOK_DEST}",
        staged.join("pacrat-guard.hook").display()
    );
    if !apply {
        println!("     (`pacrat setup --apply` writes those staged files first)");
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
/// than a `format!` so the shell's own `${...}` stays readable.
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
# WHY NOT AN ENV MARKER: pacman hooks run as root and do not inherit the
# invoking user's environment. yay -> sudo -> pacman drops PACRAT_BYPASS
# before this script ever sees it, so an env check alone would be a guard
# that never fires. Hence the two escapes below.
#
# ESCAPE 1 — the marker file
#     @MARKER@
# which pacrat's own build/sync flows create for the duration of their
# transaction and remove afterwards. Any user who can write the repo
# directory can create it by hand: this is a rail against brain-farts, not
# an access control, and it is not trying to be one.
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
	exit 0
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
}

// ---------------------------------------------------------------- plumbing

/// XDG state, where the root-owned files are staged so the printed `sudo
/// install` commands have something real to copy from (ADR-001 puts host
/// scratch under XDG state).
fn staging_dir() -> Result<PathBuf, String> {
    let base = match env::var_os("XDG_STATE_HOME") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env::var_os("HOME").ok_or("HOME is not set")?)
            .join(".local")
            .join("state"),
    };
    Ok(base.join("pacrat").join("setup"))
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

    fn repo() -> Repo {
        Repo {
            name: "dotfiles-aur".into(),
            path: "/var/cache/pacrat/repo".into(),
            server: None,
        }
    }

    #[test]
    fn section_is_local_only_without_a_server() {
        let s = pacman_conf_section(&repo());
        assert!(s.starts_with("[dotfiles-aur]\n"));
        assert!(s.contains("SigLevel = Optional TrustAll"));
        assert!(s.contains("Server = file:///var/cache/pacrat/repo"));
        assert_eq!(s.matches("Server =").count(), 1);
    }

    #[test]
    fn section_names_both_roles_with_a_server() {
        let mut r = repo();
        r.server = Some("https://example.invalid/repo".into());
        let s = pacman_conf_section(&r);
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
    fn script_carries_the_configured_repo_and_marker() {
        let s = guard_script(&repo());
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains("/var/cache/pacrat/repo/.pacrat-transaction"));
        assert!(s.contains("[dotfiles-aur]"));
        assert!(!s.contains('@'), "every template hole should be filled");
    }
}
