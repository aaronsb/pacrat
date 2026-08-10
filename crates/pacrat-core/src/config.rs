//! `~/.config/pacrat/config.toml` — host-local preferences. Every field has
//! a default so a missing file is a valid config. Unknown keys are ignored
//! (forward compatibility across pacrat versions on different hosts).

use serde::{Deserialize, Serialize};

use crate::Thresholds;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Ui {
    /// Default until the TUI lands; bare `pacrat` prints status.
    #[default]
    Cli,
    Tui,
}

/// The local pacman repo pacrat builds into and serves from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Repo {
    /// Section name in pacman.conf.
    pub name: String,
    /// Where the repo db and built packages live on this host.
    pub path: String,
    /// Optional shared-serving URL (phase 2): any URL pacman can fetch from —
    /// a forge's release artifacts (GitHub, Gitea, Codeberg, …), a plain
    /// https server, or file://. Deliberately forge-agnostic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

impl Default for Repo {
    fn default() -> Self {
        Self {
            name: "dotfiles-aur".into(),
            path: "/var/cache/pacrat/repo".into(),
            server: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub default_ui: Ui,
    pub thresholds: Thresholds,
    pub repo: Repo,
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_defaults() {
        let c = Config::from_toml("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.repo.name, "dotfiles-aur");
        assert_eq!(c.thresholds.warn_at, 2);
        assert_eq!(c.thresholds.block_at, 4);
    }

    #[test]
    fn partial_override_and_unknown_keys() {
        let c = Config::from_toml(
            r#"
default_ui = "tui"
future_knob = true

[thresholds]
warn_at = 1

[repo]
name = "fleet"
server = "https://codeberg.org/aaronsb/fleet-repo/releases/latest/download"
"#,
        )
        .unwrap();
        assert_eq!(c.default_ui, Ui::Tui);
        assert_eq!(c.thresholds.warn_at, 1);
        assert_eq!(c.thresholds.block_at, 4); // untouched default
        assert_eq!(c.repo.name, "fleet");
        assert!(c.repo.server.as_deref().unwrap().starts_with("https://codeberg.org"));
    }
}
