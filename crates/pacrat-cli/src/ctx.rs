//! Shared context for every verb: where the store is, who this host is,
//! what the local preferences say. Store discovery mirrors dotfiles-cli:
//! `$DOTFILES_DIR` → `~/.dotfiles`.

use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use pacrat_core::config::Config;
use pacrat_core::pkg::{normalize, Source};
use pacrat_core::sources::Sources;

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
    pub fn tracked_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = fs::read_dir(self.packages_dir())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        hosts.sort();
        hosts
    }

    /// A host's tracked list for one source; missing file is an empty list.
    pub fn tracked(&self, host: &str, source: Source) -> Vec<String> {
        let path = self
            .packages_dir()
            .join(host)
            .join(format!("{}.txt", source.name()));
        fs::read_to_string(path)
            .map(|raw| normalize(&raw))
            .unwrap_or_default()
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
