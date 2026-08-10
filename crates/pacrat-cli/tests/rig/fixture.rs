//! The hermetic sandbox a scenario runs in — `docs/demo/record.sh`'s world,
//! rebuilt in Rust for the rig.
//!
//! Everything the TUI can ask the system is answered from here: a throwaway
//! store, PATH shims standing in for pacman, flatpak, curl and git (the
//! always-argv rule, ADR-001, is what makes a shell script a sufficient
//! stand-in for the AUR), and an `env -i`-equivalent environment so nothing
//! of the developer's machine reaches the binary. The host is named through
//! the `PACRAT_HOSTNAME` fixture door: the goldens are committed, so the
//! screens must say "north" on every machine that ever runs them.
//!
//! The fixture is mutable on purpose — a scenario that wants to prove marks
//! survive a reload rewrites a live list and presses `r` — so the writers
//! stay public.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// The host the sandbox claims to be. Two syllables the unit tests already
/// speak: this machine is north, the other one is slab.
pub const HOST: &str = "north";
pub const OTHER_HOST: &str = "slab";

pub struct Fixture {
    pub root: PathBuf,
}

impl Fixture {
    /// Build the sandbox. `tag` keeps parallel scenarios out of each
    /// other's directories.
    pub fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("pacrat-rig-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in [
            "bin",
            "live",
            "state",
            "config/pacrat",
            &format!("store/packages/{HOST}"),
            &format!("store/packages/{OTHER_HOST}"),
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let fixture = Self { root };
        fixture.shims();
        fixture.store();
        fixture
    }

    /// The environment the binary runs under — cleared first by the rig, so
    /// this list is the *whole* environment, the way `record.sh` runs under
    /// `env -i`. TERM and COLORTERM are pinned because the goldens are a
    /// record of what one particular kind of terminal was told.
    pub fn env(&self) -> Vec<(String, String)> {
        let sb = |p: &str| self.root.join(p).display().to_string();
        vec![
            ("PATH".into(), format!("{}:/usr/bin:/bin", sb("bin"))),
            ("HOME".into(), self.root.display().to_string()),
            ("TERM".into(), "xterm-256color".into()),
            ("COLORTERM".into(), "truecolor".into()),
            ("LANG".into(), "C.UTF-8".into()),
            ("TZ".into(), "UTC".into()),
            ("DOTFILES_DIR".into(), sb("store")),
            ("XDG_CONFIG_HOME".into(), sb("config")),
            ("XDG_STATE_HOME".into(), sb("state")),
            ("PACRAT_SETUP_GATE".into(), "off".into()),
            ("PACRAT_HOSTNAME".into(), HOST.into()),
        ]
    }

    /// Overwrite one host's tracked list for one source.
    pub fn track(&self, host: &str, source: &str, names: &str) {
        fs::write(
            self.root
                .join("store/packages")
                .join(host)
                .join(format!("{source}.txt")),
            names,
        )
        .unwrap();
    }

    /// Overwrite what the pacman shim claims is installed here.
    pub fn install(&self, source: &str, names: &str) {
        fs::write(self.root.join("live").join(format!("{source}.txt")), names).unwrap();
    }

    /// The shims. Each answers exactly the argv shapes pacrat's own modules
    /// build (`live.rs`, `pacman.rs`, `aur.rs`); anything unexpected exits
    /// loudly rather than falling through to the real tool — the rig must
    /// never touch the machine it runs on.
    fn shims(&self) {
        let live = self.root.join("live").display().to_string();
        // A shim miss must reach the failure report, not just a stderr
        // nobody surfaces — the rig prints this log when a scenario fails.
        let log = self.root.join("shim-errors.log").display().to_string();
        self.shim(
            "pacman",
            &format!(
                r#"#!/bin/sh
# Answers the queries pacrat asks, from fixture files. Never the real pacman.
case "$*" in
  -Qqen) exec cat "{live}/native.txt" ;;
  -Qqem) exec cat "{live}/aur.txt" ;;
  -Q) exec cat "{live}/versions.txt" ;;
  '-Ss -- '*) exec cat "{live}/search.txt" ;;
  '-Si -- rat-hole') exec cat "{live}/si-rat-hole.txt" ;;
  '-Si -- '*) echo "error: package was not found" >&2; exit 1 ;;
  '-Qi -- '*) exit 1 ;;
  *) echo "pacman shim: unexpected argv: $*" >&2
     echo "pacman shim: unexpected argv: $*" >>"{log}"; exit 9 ;;
esac
"#
            ),
        );
        self.shim(
            "flatpak",
            "#!/bin/sh\n# No flatpaks in the fixture, ever.\nexit 0\n",
        );
        self.shim(
            "curl",
            &format!(
                r#"#!/bin/sh
# The AUR RPC, answered from fixture files. The URL is the last argument.
for url; do :; done
case "$url" in
  *'/search?'*) exec cat "{live}/aur-search.json" ;;
  *'/info?'*) exec cat "{live}/aur-info.json" ;;
  *) echo "curl shim: unexpected url: $url" >&2
     echo "curl shim: unexpected url: $url" >>"{log}"; exit 22 ;;
esac
"#
            ),
        );
        self.shim(
            "git",
            &format!(
                "#!/bin/sh\n# The ledger is empty, so nothing here should ever probe an upstream.\n\
                 echo \"git shim: unexpected argv: $*\" >&2\n\
                 echo \"git shim: unexpected argv: $*\" >>\"{log}\"; exit 9\n"
            ),
        );
    }

    fn shim(&self, name: &str, body: &str) {
        let path = self.root.join("bin").join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The store and the live answers: a spread of drift states the hosts
    /// matrix legend names — `fd` ✓, `cowsay` ✗ (tracked here, not
    /// installed), `zoxide` and `bat` + (installed, not in this host's
    /// manifest) — plus a search fixture themed to the term the browse
    /// scenario types.
    fn store(&self) {
        self.track(HOST, "native", "cowsay\nfd\nripgrep\n");
        self.track(HOST, "aur", "");
        self.track(OTHER_HOST, "native", "ripgrep\nzoxide\n");
        self.install("native", "bat\nfd\nripgrep\nzoxide\n");
        self.install("aur", "");
        let live = |name: &str, body: &str| {
            fs::write(self.root.join("live").join(name), body).unwrap();
        };
        live(
            "versions.txt",
            "bat 0.24.0-1\nfd 10.2.0-1\nripgrep 14.1.1-1\nzoxide 0.9.6-1\n",
        );
        // What `pacman -Ss -- rat` says. Two repo rows, fixed versions.
        live(
            "search.txt",
            "extra/rat-hole 1.2.0-1\n    A place to stash things\n\
             extra/rat-trap 0.9.1-2\n    Catches the unwary\n",
        );
        // What `pacman -Si -- rat-hole` says, for the browse enter scenario.
        live(
            "si-rat-hole.txt",
            "Name            : rat-hole\n\
             Version         : 1.2.0-1\n\
             Description     : A place to stash things\n\
             URL             : https://example.invalid/rat-hole\n\
             Licenses        : MIT\n\n",
        );
        // The AUR's half of the same search, one package, fixed epochs so
        // the dates the detail pane derives are the same on every machine.
        live(
            "aur-search.json",
            r#"{"resultcount":1,"results":[{"Name":"rat-stash","Version":"2.0.0-1","Description":"Stashes rats","URL":"https://example.invalid/rat-stash","Maintainer":"petit-chef","NumVotes":42,"Popularity":5.5,"LastModified":1754784000,"OutOfDate":null}],"type":"search","version":5}"#,
        );
        live(
            "aur-info.json",
            r#"{"resultcount":0,"results":[],"type":"multiinfo","version":5}"#,
        );
        // A config so `setup::state` has a repo name to look for. The path
        // is inside the sandbox and never appears on a snapshotted screen.
        fs::write(
            self.root.join("config/pacrat/config.toml"),
            format!(
                "[repo]\nname = \"dotfiles-aur\"\npath = \"{}\"\n",
                self.root.join("repo").display()
            ),
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
