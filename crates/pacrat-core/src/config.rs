//! `~/.config/pacrat/config.toml` — host-local preferences. Every field has
//! a default so a missing file is a valid config. Unknown keys are ignored
//! (forward compatibility across pacrat versions on different hosts).
//!
//! `[repo]` is validated rather than escaped. Its values are substituted
//! into two grammars pacrat does not own — pacman.conf, and a shell script
//! that runs as root out of a pacman hook — so a value that would need
//! escaping is a configuration error. Rejecting a small, well-defined set
//! is auditable; getting escaping right in two grammars forever is not.

use std::path::Path;

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

impl Repo {
    /// Reject anything that could not be written literally into pacman.conf
    /// and into a single-quoted shell word. Callers may then substitute
    /// these values directly; see the module note on validate-don't-escape.
    pub fn validate(&self) -> Result<(), String> {
        // pacman's own section-name grammar. A newline here would smuggle
        // directives into pacman.conf ("x\nSigLevel = Never"); the rest of
        // the excluded set has no meaning in a section name anyway.
        if self.name.is_empty() {
            return Err("repo.name is empty".into());
        }
        if let Some(bad) = self
            .name
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || "_.@+-".contains(*c)))
        {
            return Err(format!(
                "repo.name {:?} contains {bad:?}: pacman repository names are [A-Za-z0-9_.@+-]+",
                self.name
            ));
        }

        if !Path::new(&self.path).is_absolute() {
            return Err(format!(
                "repo.path {:?} must be absolute — it is written into pacman.conf and into \
                 a root-run hook script, where a relative path has no fixed meaning",
                self.path
            ));
        }
        // Quotes and backslash could end or escape the single-quoted shell
        // word the path is substituted into; `$` and backticks would expand
        // if that word is ever double-quoted; control characters (newline
        // included) break both pacman.conf and the printed commands.
        if let Some(bad) = self
            .path
            .chars()
            .find(|c| c.is_control() || matches!(c, '\'' | '"' | '`' | '\\' | '$'))
        {
            return Err(format!(
                "repo.path {:?} contains {bad:?}, which pacrat will not substitute into \
                 pacman.conf or into the root-run guard script",
                self.path
            ));
        }

        // The server URL reaches pacman.conf on the same line grammar, so a
        // newline there is the same directive-smuggling hole as in the name.
        if let Some(url) = &self.server {
            if url.is_empty() {
                return Err("repo.server is empty — omit the key instead".into());
            }
            if let Some(bad) = url.chars().find(|c| c.is_control() || c.is_whitespace()) {
                return Err(format!(
                    "repo.server {url:?} contains {bad:?}: a Server line is one unbroken URL"
                ));
            }
        }
        Ok(())
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
    /// Parse and validate. Validation lives here, not at the use site, so
    /// every consumer of a `Config` holds one that is already safe to
    /// substitute — there is no unvalidated path through this type.
    pub fn from_toml(s: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(s).map_err(|e| e.to_string())?;
        config.repo.validate()?;
        Ok(config)
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
        assert!(c
            .repo
            .server
            .as_deref()
            .unwrap()
            .starts_with("https://codeberg.org"));
    }

    /// The default must survive its own validator, or every host with no
    /// config file fails to start.
    #[test]
    fn default_repo_is_valid() {
        assert_eq!(Repo::default().validate(), Ok(()));
    }

    #[test]
    fn hostile_repo_names_are_rejected() {
        // Each of these reaches pacman.conf as a section header and the
        // guard script as a shell literal.
        let hostile = [
            "x\nSigLevel = Never",           // F2: smuggled pacman directive
            "x'; rm -rf /tmp/pwned; echo '", // F1: breaks out of '...'
            "x\"y",
            "x`id`",
            "x$(id)",
            "with space",
            "x]\n[core",
            "",
        ];
        for name in hostile {
            let repo = Repo {
                name: name.into(),
                ..Repo::default()
            };
            assert!(
                repo.validate().is_err(),
                "repo.name {name:?} should have been rejected"
            );
        }
    }

    #[test]
    fn hostile_repo_paths_are_rejected() {
        let hostile = [
            "/var/cache/it's", // F1: the reviewer's apostrophe
            "relative/path",
            "/var/cache\nSigLevel = Never",
            "/var/cache/$(id)",
            "/var/cache/`id`",
            "/var/cache/back\\slash",
            "/var/cache/quote\"here",
            "/var/cache/bell\u{7}",
        ];
        for path in hostile {
            let repo = Repo {
                path: path.into(),
                ..Repo::default()
            };
            assert!(
                repo.validate().is_err(),
                "repo.path {path:?} should have been rejected"
            );
        }
    }

    #[test]
    fn hostile_server_urls_are_rejected() {
        for url in ["https://x/\nSigLevel = Never", "https://x y", ""] {
            let repo = Repo {
                server: Some(url.into()),
                ..Repo::default()
            };
            assert!(
                repo.validate().is_err(),
                "repo.server {url:?} should have been rejected"
            );
        }
    }

    /// A path with a space is legal on disk and is *not* an injection: it
    /// stays allowed, and the CLI quotes it when it renders shell.
    #[test]
    fn unusual_but_harmless_values_are_allowed() {
        let repo = Repo {
            name: "dotfiles-aur.v2+1".into(),
            path: "/var/cache/my repo".into(),
            server: Some("https://example.invalid/r".into()),
        };
        assert_eq!(repo.validate(), Ok(()));
    }

    #[test]
    fn a_hostile_config_file_fails_to_parse() {
        let err = Config::from_toml("[repo]\nname = \"x\\nSigLevel = Never\"\n").unwrap_err();
        assert!(err.contains("repo.name"), "unhelpful error: {err}");
    }
}
