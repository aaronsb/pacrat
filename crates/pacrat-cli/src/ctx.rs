//! Shared context for every verb: where the store is, who this host is,
//! what the local preferences say. Store discovery mirrors dotfiles-cli:
//! `$DOTFILES_DIR` → `~/.dotfiles`.

use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use pacrat_core::config::Config;
use pacrat_core::pkg::{normalize, valid_name, Source};
use pacrat_core::sources::Sources;

use crate::out::visible_line;

pub struct Ctx {
    pub store: PathBuf,
    pub host: String,
    pub config: Config,
}

impl Ctx {
    pub fn resolve() -> Result<Self, String> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set")?;
        let store = env::var_os("DOTFILES_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".dotfiles"));
        if !store.is_dir() {
            return Err(format!(
                "store not found at {} (set DOTFILES_DIR to your dotfiles checkout)",
                store.display()
            ));
        }

        let config_path = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("pacrat")
            .join("config.toml");
        let config = match fs::read_to_string(&config_path) {
            Ok(text) => {
                Config::from_toml(&text).map_err(|e| format!("{}: {e}", config_path.display()))?
            }
            Err(_) => Config::default(),
        };

        Ok(Self {
            store,
            host: hostname(),
            config,
        })
    }

    pub fn sources_path(&self) -> PathBuf {
        self.store.join("aur").join("sources.toml")
    }

    /// The ledger; a store without one yet is an empty ledger, not an error.
    pub fn load_sources(&self) -> Result<Sources, String> {
        match fs::read_to_string(self.sources_path()) {
            Ok(text) => Sources::from_toml(&text)
                .map_err(|e| format!("{}: {e}", self.sources_path().display())),
            Err(_) => Ok(Sources::default()),
        }
    }

    /// Write the ledger, atomically.
    ///
    /// `fs::write` truncates first, so a crash mid-write would leave the
    /// store with a half-written — or empty — record of every vendored
    /// package. Writing a sibling temp file and renaming it into place makes
    /// the update all-or-nothing. Callers must load immediately before they
    /// modify: this replaces the whole file, so a stale copy silently
    /// reverts whatever another process wrote in the meantime.
    pub fn save_sources(&self, sources: &Sources) -> Result<(), String> {
        let path = self.sources_path();
        let dir = path
            .parent()
            .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = dir.join(format!(".sources.toml.{}.new", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(sources.to_toml().as_bytes())?;
            f.sync_all()
        };
        write().map_err(|e| format!("{}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
    }

    pub fn packages_dir(&self) -> PathBuf {
        self.store.join("packages")
    }

    /// Host directories under packages/, sorted.
    ///
    /// Names go through the same door as the tracked lists — `valid_name`
    /// is about a *displayable, printable* identifier, and a host name is
    /// used as one everywhere: it is printed into `pacrat sync`'s report and
    /// into the TUI's host column. The escape-hiding argument on
    /// [`Ctx::tracked`] applies unchanged, plus one that is specific to a
    /// column: padding happens before the terminal ever sees the string, so
    /// a zero-width or double-width character in a host name misaligns
    /// `{host:<12}` for every row beneath it even where the bytes are
    /// eventually filtered out on their way to the screen.
    ///
    /// Unlike a tracked list this **skips** rather than refuses, and the
    /// difference is what the two things are. A malformed line in
    /// `native.txt` is a corrupted member of a list pacrat is claiming to
    /// manage, so dropping it silently would shorten an answer. A directory
    /// under `packages/` that is not a legal name was never a host — it is
    /// something else that happens to live there — and the honest reading
    /// of it is not to count it, rather than to refuse to report on the
    /// four real machines beside it.
    pub fn tracked_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = fs::read_dir(self.packages_dir())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|name| valid_name(name))
                    .collect()
            })
            .unwrap_or_default();
        hosts.sort();
        hosts
    }

    /// A host's tracked list for one source; a missing file is an empty list.
    ///
    /// Every name is checked against the package grammar here, at the door,
    /// and a list containing one that fails takes the whole run down. Two
    /// reasons it is a refusal and not a filter:
    ///
    /// 1. **These names are printed into commands.** `pacrat sync` renders
    ///    them into a `sudo pacman -S …` line for a human to read and run,
    ///    and a name carrying `ESC[8m` hides the rest of that line in most
    ///    terminals. "The human reading the list is the gate" is only true
    ///    if the list cannot lie to them. Shell-quoting does not help: the
    ///    bytes are *inside* the quotes and the terminal acts on them anyway.
    /// 2. **Silently dropping a name is its own failure.** A tracked package
    ///    that quietly vanishes from every count and every plan is a package
    ///    the store thinks it manages and pacrat has stopped mentioning.
    ///
    /// The file is a text file in a synced git repo, so this is not a
    /// hypothetical attacker so much as a hand, an editor, or a bad merge —
    /// which is exactly why it should stop the run rather than be tidied
    /// away. Validating at the door rather than at each use site is the rule
    /// `Sources::from_toml` already follows for `reviewed`: every consumer of
    /// a tracked list gets the guarantee without knowing to ask for it.
    pub fn tracked(&self, host: &str, source: Source) -> Result<Vec<String>, String> {
        let path = self
            .packages_dir()
            .join(host)
            .join(format!("{}.txt", source.name()));
        let list = match fs::read_to_string(&path) {
            Ok(raw) => normalize(&raw),
            Err(_) => return Ok(Vec::new()),
        };
        if let Some(bad) = list.iter().find(|name| !valid_name(name)) {
            // The report of a hostile name must not itself be hostile: the
            // same bytes would otherwise hide this very error.
            let (shown, hidden) = visible_line(bad);
            return Err(format!(
                "{}: {shown:?} is not a package name{} — expected letters, \
                 digits and @._+- (no leading hyphen or dot).\n       \
                 Refusing the whole list: pacrat prints these names into \
                 commands for a human to read before running them, and a \
                 name that can hide text defeats that reading.",
                path.display(),
                match hidden {
                    0 => String::new(),
                    1 => " (1 character shown above is a stand-in for an invisible one)".into(),
                    n => format!(" ({n} characters shown above are stand-ins for invisible ones)"),
                }
            ));
        }
        Ok(list)
    }
}

/// This host's pacrat state directory — job queues, probe history, the grade
/// cache, staged root-owned files (ADR-001, data placement). Host scratch,
/// never the store: nothing under here is synced to the fleet.
pub fn state_dir() -> Result<PathBuf, String> {
    let base = match env::var_os("XDG_STATE_HOME") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env::var_os("HOME").ok_or("HOME is not set")?)
            .join(".local")
            .join("state"),
    };
    Ok(base.join("pacrat"))
}

fn hostname() -> String {
    if let Ok(name) = fs::read_to_string("/etc/hostname") {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pacrat_core::config::Config;

    /// A store with one host's list in it, and nothing else.
    fn store_with(tag: &str, source: Source, contents: &str) -> (Ctx, PathBuf) {
        let store = env::temp_dir().join(format!("pacrat-ctx-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&store);
        let dir = store.join("packages").join("north");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.txt", source.name()));
        fs::write(&path, contents).unwrap();
        let ctx = Ctx {
            store,
            host: "north".into(),
            config: Config::default(),
        };
        (ctx, path)
    }

    #[test]
    fn an_ordinary_list_reads_back_normalized() {
        let (ctx, _) = store_with("ok", Source::Native, "  ripgrep\nfd\n\nfd\n");
        assert_eq!(
            ctx.tracked("north", Source::Native).unwrap(),
            vec!["fd".to_string(), "ripgrep".to_string()]
        );
        // A host with no file at all is an empty list, not a failure.
        assert!(ctx.tracked("slab", Source::Native).unwrap().is_empty());
        let _ = fs::remove_dir_all(&ctx.store);
    }

    /// The attack the refusal exists for: a tracked name carrying `ESC[8m`
    /// travels into `sudo pacman -S …`, where the terminal hides the rest of
    /// the line from the human who is deciding whether to run it. Shell
    /// quoting does not help — the bytes are inside the quotes.
    #[test]
    fn a_name_that_could_hide_a_line_fails_the_list_and_names_the_file() {
        let (ctx, path) = store_with("hostile", Source::Aur, "ripgrep\nfd\u{1b}[8m\n");
        let err = ctx.tracked("north", Source::Aur).unwrap_err();
        assert!(
            err.contains(&path.display().to_string()),
            "the file must be named: {err}"
        );
        // The error itself must not carry the escape it is reporting.
        assert!(
            !err.contains('\u{1b}'),
            "the report re-emitted ESC: {err:?}"
        );
        assert!(err.contains("␛[8m"), "the bytes are not shown: {err}");
        assert!(
            err.contains("stand-in"),
            "the hidden count is missing: {err}"
        );
        let _ = fs::remove_dir_all(&ctx.store);
    }

    /// Refusal, not filtering: the legal names in the same file do not come
    /// back without the illegal one. A silently shortened list is a store
    /// pacrat has stopped telling the truth about.
    #[test]
    fn one_bad_name_takes_the_whole_list_down() {
        let (ctx, _) = store_with("partial", Source::Native, "fd\n../escape\nripgrep\n");
        assert!(ctx.tracked("north", Source::Native).is_err());
        let _ = fs::remove_dir_all(&ctx.store);
    }

    /// A host name is printed into commands and into a padded column, so it
    /// goes through the same grammar. Here the answer is to skip rather than
    /// to refuse: the real hosts beside it are still real.
    #[test]
    fn a_directory_that_is_not_a_legal_name_is_not_a_host() {
        let (ctx, _) = store_with("hosts", Source::Native, "fd\n");
        let packages = ctx.packages_dir();
        for name in ["slab", "cube\u{1b}[8m", "wide\u{200b}name", ".hidden"] {
            fs::create_dir_all(packages.join(name)).unwrap();
        }
        // A file, not a directory, is not a host either.
        fs::write(packages.join("README.md"), "notes").unwrap();
        assert_eq!(
            ctx.tracked_hosts(),
            vec!["north".to_string(), "slab".to_string()],
            "the door let something through, or turned a real host away"
        );
        let _ = fs::remove_dir_all(&ctx.store);
    }
}
