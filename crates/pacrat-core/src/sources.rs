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
    /// A candidate commit a human looked at and refused (`pacrat reject`).
    ///
    /// Recorded rather than remembered, because the alternative is a drift
    /// report that asks the same question every hour and trains the reader
    /// to dismiss it. The refusal is scoped to *that* commit: upstream
    /// moving past it is a new candidate and a new question, which is why
    /// this holds a commit and not a flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected: Option<String>,
    /// Why it was refused. Optional in the same way `--note` is optional on
    /// a rejection: the commit is the decision, the note is the reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_note: Option<String>,
}

/// Is this the shape of a git object id?
///
/// The ledger is a synced file that any host can edit, and `reviewed` flows
/// outward from it into places where its *shape* is load-bearing: it becomes
/// a path component of the grade cache, and an argument to git. A value like
/// `../../../../PWNED` is a perfectly good TOML string and a perfectly good
/// path traversal. Validating here rather than at each use site means every
/// future consumer of a `Sources` gets the guarantee without knowing to ask
/// for it — the same argument `Config::from_toml` makes for `[repo]`.
///
/// Seven is git's own shortest abbreviation; sixty-four admits SHA-256
/// object ids, which git is migrating toward.
pub fn valid_commit(commit: &str) -> bool {
    (7..=64).contains(&commit.len()) && commit.chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sources {
    #[serde(default)]
    pub packages: BTreeMap<String, SourceEntry>,
}

impl Sources {
    /// Parse and validate. As with `Config::from_toml`, there is no path
    /// through this type that yields an unvalidated ledger.
    pub fn from_toml(s: &str) -> Result<Self, String> {
        let sources: Self = toml::from_str(s).map_err(|e| e.to_string())?;
        sources.validate()?;
        Ok(sources)
    }

    fn validate(&self) -> Result<(), String> {
        for (package, entry) in &self.packages {
            // `rejected` is held to the same standard as `reviewed`, and for
            // the same reason: it is compared against a commit and printed,
            // and a field that can hold anything eventually reaches somewhere
            // its shape matters.
            for (field, value) in [
                ("reviewed", Some(&entry.reviewed)),
                ("rejected", entry.rejected.as_ref()),
            ] {
                let Some(value) = value else { continue };
                if !valid_commit(value) {
                    return Err(format!(
                        "packages.{package}.{field} is {value:?}, which is not a commit hash — \
                         expected 7-64 hex characters as `git rev-parse` prints them"
                    ));
                }
            }
        }
        Ok(())
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

    /// A refusal is part of the ledger row, and survives a round trip — it
    /// is what keeps `pacrat updates` from re-asking a question a human has
    /// already answered.
    #[test]
    fn a_rejection_roundtrips_and_is_absent_when_there_is_none() {
        let toml = r#"
[packages.mdcat]
upstream = "https://aur.archlinux.org/mdcat.git"
reviewed = "3f9c21ab"
role = "vendored"
rejected = "7d02e4c1"
rejected_note = "post_install gained curl … | sh"
"#;
        let s = Sources::from_toml(toml).unwrap();
        let entry = &s.packages["mdcat"];
        assert_eq!(entry.rejected.as_deref(), Some("7d02e4c1"));
        assert_eq!(
            entry.rejected_note.as_deref(),
            Some("post_install gained curl … | sh")
        );
        assert_eq!(Sources::from_toml(&s.to_toml()).unwrap(), s);

        // A row with no refusal writes no keys at all: the common case must
        // not grow two empty lines per package.
        let plain = Sources::from_toml(
            "[packages.mdcat]\nupstream = \"u\"\nreviewed = \"3f9c21ab\"\nrole = \"vendored\"\n",
        )
        .unwrap();
        let text = plain.to_toml();
        assert!(!text.contains("rejected"), "{text}");
    }

    /// `rejected` reaches the same places `reviewed` does — a comparison
    /// against a git hash and a printed line — so it is validated the same.
    #[test]
    fn a_rejected_commit_that_is_not_a_commit_fails_to_parse_too() {
        for rejected in ["../../../../PWNED", "", "HEAD~1", "zzzzzzzz"] {
            let toml = format!(
                "[packages.mdcat]\n\
                 upstream = \"u\"\n\
                 reviewed = \"3f9c21ab\"\n\
                 role = \"vendored\"\n\
                 rejected = {rejected:?}\n"
            );
            let err = match Sources::from_toml(&toml) {
                Ok(_) => panic!("rejected {rejected:?} should have been rejected"),
                Err(e) => e,
            };
            assert!(
                err.contains("rejected") && err.contains("mdcat"),
                "unhelpful error for {rejected:?}: {err}"
            );
        }
    }

    #[test]
    fn empty_and_missing_table() {
        assert_eq!(Sources::from_toml("").unwrap(), Sources::default());
    }

    /// The ledger is synced between hosts, so a hostile entry is a hostile
    /// *file*, and it must not parse. `reviewed` reaches a cache path and a
    /// git argument; the traversal below is the one a reviewer demonstrated
    /// writing outside the state directory.
    #[test]
    fn a_reviewed_commit_that_is_not_a_commit_fails_to_parse() {
        let hostile = [
            "../../../../PWNED",
            "/etc/passwd",
            "..",
            "",
            "abc",    // shorter than git will abbreviate
            "HEAD~1", // a revision, not an object id
            "3f9c21ab/../../x",
            "3f9c21ab.manual", // would collide with a grade-cache filename
            "3f9c21ab\nSigLevel = Never",
            "z9c21abz",
            &"a".repeat(65),
        ];
        for reviewed in hostile {
            let toml = format!(
                "[packages.mdcat]\n\
                 upstream = \"https://aur.archlinux.org/mdcat.git\"\n\
                 reviewed = {reviewed:?}\n\
                 role = \"vendored\"\n"
            );
            let err = match Sources::from_toml(&toml) {
                Ok(_) => panic!("reviewed {reviewed:?} should have been rejected"),
                Err(e) => e,
            };
            assert!(
                err.contains("reviewed") && err.contains("mdcat"),
                "unhelpful error for {reviewed:?}: {err}"
            );
        }
    }

    #[test]
    fn real_object_ids_are_accepted_at_both_lengths() {
        for commit in ["3f9c21ab", &"a".repeat(40), &"F".repeat(64)] {
            assert!(valid_commit(commit), "{commit} should be valid");
        }
        let toml = format!(
            "[packages.mdcat]\nupstream = \"u\"\nreviewed = {:?}\nrole = \"vendored\"\n",
            "a".repeat(40)
        );
        assert!(Sources::from_toml(&toml).is_ok());
    }
}
