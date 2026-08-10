//! `aur/sources.toml` — the store's vendored-package ledger: for each
//! vendored or maintained package, where it comes from and which commit a
//! human last reviewed. Lives in the dotfiles store, synced to every host.
//!
//! `upstream` is a full git URL on purpose: the AUR is the common case, but
//! any git server (GitHub, GitLab, Gitea, Codeberg, …) is a valid origin —
//! pacrat has no forge-specific behavior here.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::Custody;

/// A package's role in the ledger. Absence from the ledger entirely means
/// unmanaged or merely tracked — the ledger only holds the top two rungs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Third-party: pull-only; updates adopted through the review gate.
    Vendored,
    /// Ours: vendored plus push rights back to the upstream.
    Maintained,
}

impl From<Role> for Custody {
    fn from(role: Role) -> Self {
        match role {
            Role::Vendored => Custody::Vendored,
            Role::Maintained => Custody::Maintained,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEntry {
    /// Full git URL of the origin (any git server).
    pub upstream: String,
    /// The commit last reviewed and merged into the store tree. Gradings and
    /// diffs are computed against this.
    pub reviewed: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sources {
    #[serde(default)]
    pub packages: BTreeMap<String, SourceEntry>,
}

impl Sources {
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("Sources is always serializable")
    }

    pub fn custody(&self, package: &str) -> Option<Custody> {
        self.packages.get(package).map(|e| e.role.into())
    }

    pub fn count(&self, role: Role) -> usize {
        self.packages.values().filter(|e| e.role == role).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let toml = r#"
[packages.mdcat]
upstream = "https://aur.archlinux.org/mdcat.git"
reviewed = "3f9c21ab"
role = "vendored"

[packages.playtimed]
upstream = "https://aur.archlinux.org/playtimed.git"
reviewed = "aa00bb11"
role = "maintained"
note = "ours"
"#;
        let s = Sources::from_toml(toml).unwrap();
        assert_eq!(s.packages.len(), 2);
        assert_eq!(s.custody("mdcat"), Some(Custody::Vendored));
        assert_eq!(s.custody("playtimed"), Some(Custody::Maintained));
        assert_eq!(s.custody("absent"), None);
        assert_eq!(s.count(Role::Vendored), 1);

        let back = Sources::from_toml(&s.to_toml()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn empty_and_missing_table() {
        assert_eq!(Sources::from_toml("").unwrap(), Sources::default());
    }
}
