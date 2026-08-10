//! The publish queue — what `pacrat push` could not publish yet, and why.
//!
//! This file exists because of the June 2026 AUR incident: uploads and ssh
//! went read-only for months, and a maintainer who tried to publish during
//! that window got a refusal and nothing else. The queue turns "the AUR said
//! no" from a lost command into a recorded intent, so the answer to "what am
//! I waiting to publish?" survives the shell it was typed in.
//!
//! **Host state, not store state** (ADR-001, data placement): it lives under
//! `$XDG_STATE_HOME/pacrat/pushes/queue.toml` and is never synced. A queued
//! publish is a fact about one machine's keys and one machine's unfinished
//! errand, not about the fleet — the *reviewed* fact is already in the store's
//! ledger, and that is what the fleet reads.
//!
//! Every field is validated at parse, the rule `Sources::from_toml` follows:
//! the package name becomes a command pacrat runs against the store, the
//! commit and digest become comparisons, and `last_answer` is a **remote's own
//! words** — the one field here written by someone else, so it is capped in
//! length at parse and neutered by whoever prints it.
//!
//! ## Why an entry carries a digest as well as a commit
//!
//! The commit says which upstream state the ledger had reviewed when the
//! publish was queued. The digest says what the store's bytes actually were.
//! ADR-001 is emphatic that these are different claims — a tree can be edited
//! by a hand, a sync, or a half-finished git operation without any commit
//! moving — and the queue needs the second one: it exists to answer "is the
//! thing I am about to publish still the thing I asked to publish?" days
//! later, and only the digest can answer that. Both sides of that comparison
//! are computed by pacrat over local bytes, which is exactly the use
//! [`crate::hash`] sanctions.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::hash::valid_digest;
use crate::pkg::valid_name;
use crate::sources::valid_commit;

/// How much of a remote's answer is kept.
///
/// The AUR's maintenance notice is one short line; a hostile or broken server
/// can answer with a megabyte. This is a host-local file that `pacrat status`
/// renders on one line, so the cap is enforced at parse and pacrat truncates
/// before it ever writes.
pub const ANSWER_MAX: usize = 200;

/// One publish that has not happened yet.
///
/// The package name is the map key rather than a field: a second queued
/// publish of the same package is the same errand with newer facts, and a
/// shape that cannot express two of them is a shape that cannot disagree with
/// itself about which one is current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publish {
    /// The ledger's reviewed commit when this was queued.
    pub commit: String,
    /// The store tree's content digest when this was queued — what the
    /// maintainer actually looked at and asked to publish.
    pub digest: String,
    /// Unix seconds. When the errand started, preserved across re-probes: the
    /// age of a blocked publish is the interesting number.
    pub queued_at: i64,
    /// Unix seconds of the most recent probe.
    pub last_probe: i64,
    /// What the remote said, verbatim and capped. Neutered at display, never
    /// at rest — a stored answer that had been "cleaned up" would be pacrat's
    /// paraphrase of evidence.
    pub last_answer: String,
    /// Set when the store moved on after queueing: the entry was re-queued
    /// against the new bytes rather than published stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    #[serde(default)]
    pub pushes: BTreeMap<String, Publish>,
}

impl Queue {
    /// Parse and validate. As with `Sources::from_toml`, there is no path
    /// through this type that yields an unvalidated queue.
    pub fn from_toml(s: &str) -> Result<Self, String> {
        let queue: Self = toml::from_str(s).map_err(|e| e.to_string())?;
        queue.validate()?;
        Ok(queue)
    }

    fn validate(&self) -> Result<(), String> {
        for (package, entry) in &self.pushes {
            // The key names a package pacrat will look up in the ledger and
            // print into commands. Checked here, at the door, so no consumer
            // has to remember to.
            if !valid_name(package) {
                return Err(format!(
                    "pushes.{package:?} is not a package name — expected letters, \
                     digits and @._+- (no leading hyphen or dot)"
                ));
            }
            if !valid_commit(&entry.commit) {
                return Err(format!(
                    "pushes.{package}.commit is {:?}, which is not a commit hash",
                    entry.commit
                ));
            }
            if !valid_digest(&entry.digest) {
                return Err(format!(
                    "pushes.{package}.digest is {:?}, which is not a tree digest — \
                     expected 64 lowercase hex characters",
                    entry.digest
                ));
            }
            if entry.last_answer.chars().count() > ANSWER_MAX {
                return Err(format!(
                    "pushes.{package}.last_answer is {} characters — the cap is \
                     {ANSWER_MAX}; it holds what a server said, not what a server sent",
                    entry.last_answer.chars().count()
                ));
            }
        }
        Ok(())
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("Queue is always serializable")
    }

    pub fn is_empty(&self) -> bool {
        self.pushes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pushes.len()
    }

    /// Record a blocked publish, or refresh one already recorded.
    ///
    /// `queued_at` survives: the errand did not restart because it was asked
    /// about again, and "queued four days ago, probed a minute ago" is the
    /// pair of facts a maintainer actually wants.
    pub fn block(&mut self, package: &str, commit: &str, digest: &str, now: i64, answer: &str) {
        let answer = cap(answer);
        match self.pushes.get_mut(package) {
            Some(entry) => {
                entry.commit = commit.to_string();
                entry.digest = digest.to_string();
                entry.last_probe = now;
                entry.last_answer = answer;
            }
            None => {
                self.pushes.insert(
                    package.to_string(),
                    Publish {
                        commit: commit.to_string(),
                        digest: digest.to_string(),
                        queued_at: now,
                        last_probe: now,
                        last_answer: answer,
                        note: None,
                    },
                );
            }
        }
    }

    /// Note what every entry was told by the most recent probe.
    pub fn probed(&mut self, now: i64, answer: &str) {
        let answer = cap(answer);
        for entry in self.pushes.values_mut() {
            entry.last_probe = now;
            entry.last_answer.clone_from(&answer);
        }
    }

    /// The most recent thing any queued entry was told — what `pacrat status`
    /// shows on its one line.
    pub fn latest_answer(&self) -> Option<&str> {
        self.pushes
            .values()
            .max_by_key(|e| e.last_probe)
            .map(|e| e.last_answer.as_str())
    }
}

/// Keep a remote's answer to its cap, in characters, marking nothing: this is
/// evidence, and a truncation mark inside a quoted answer would read as part
/// of what the server said. The cap is generous enough that the AUR's whole
/// notice fits.
fn cap(answer: &str) -> String {
    answer.chars().take(ANSWER_MAX).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "5a4705a4aaa2e7f10a7dd6c302256dd373516e56";
    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn queued() -> Queue {
        let mut q = Queue::default();
        q.block("playtimed", COMMIT, DIGEST, 1_000, "The AUR is down");
        q
    }

    #[test]
    fn an_entry_roundtrips() {
        let q = queued();
        let back = Queue::from_toml(&q.to_toml()).unwrap();
        assert_eq!(back, q);
        assert_eq!(back.len(), 1);
        assert!(!back.is_empty());
        assert_eq!(Queue::from_toml("").unwrap(), Queue::default());
        // The common case writes no `note` key at all.
        assert!(!q.to_toml().contains("note"), "{}", q.to_toml());
    }

    /// Re-probing an errand does not restart it: how long a publish has been
    /// blocked is the number a maintainer is actually watching.
    #[test]
    fn re_queueing_keeps_the_original_queued_at() {
        let mut q = queued();
        q.block("playtimed", COMMIT, DIGEST, 9_000, "still down");
        let entry = &q.pushes["playtimed"];
        assert_eq!(entry.queued_at, 1_000);
        assert_eq!(entry.last_probe, 9_000);
        assert_eq!(entry.last_answer, "still down");
    }

    #[test]
    fn a_probe_updates_every_entry_and_the_latest_answer_is_the_newest() {
        let mut q = queued();
        q.block("mdcat", COMMIT, DIGEST, 2_000, "older");
        q.probed(5_000, "The AUR is down due to maintenance.");
        assert_eq!(
            q.latest_answer(),
            Some("The AUR is down due to maintenance.")
        );
        assert!(q.pushes.values().all(|e| e.last_probe == 5_000));
        assert_eq!(Queue::default().latest_answer(), None);
    }

    /// The one field a server writes. Capped on the way in, so a hostile
    /// answer cannot become a host-state file nobody can load.
    #[test]
    fn a_servers_answer_is_capped_at_the_door() {
        let mut q = Queue::default();
        q.block("playtimed", COMMIT, DIGEST, 1, &"x".repeat(10_000));
        assert_eq!(
            q.pushes["playtimed"].last_answer.chars().count(),
            ANSWER_MAX
        );
        assert!(Queue::from_toml(&q.to_toml()).is_ok());

        // And a file that arrived oversized anyway is refused rather than
        // trimmed: pacrat did not write it, so something else did.
        let hostile = format!(
            "[pushes.playtimed]\ncommit = {COMMIT:?}\ndigest = {DIGEST:?}\n\
             queued_at = 1\nlast_probe = 1\nlast_answer = {:?}\n",
            "y".repeat(ANSWER_MAX + 1)
        );
        let err = Queue::from_toml(&hostile).unwrap_err();
        assert!(err.contains("last_answer"), "{err}");
    }

    /// The queue drives `pacrat push` against the store, so its key is a name
    /// pacrat will act on. A file holding a path traversal must not parse.
    #[test]
    fn a_key_that_is_not_a_package_name_fails_the_file() {
        for name in ["../../escape", "-force", ".hidden", "two words"] {
            let toml = format!(
                "[pushes.{name:?}]\ncommit = {COMMIT:?}\ndigest = {DIGEST:?}\n\
                 queued_at = 1\nlast_probe = 1\nlast_answer = \"\"\n"
            );
            let err = match Queue::from_toml(&toml) {
                Ok(_) => panic!("{name:?} should have been refused"),
                Err(e) => e,
            };
            assert!(
                err.contains("package name") || err.contains("expected"),
                "{err}"
            );
        }
    }

    #[test]
    fn a_commit_or_digest_that_is_neither_fails_the_file() {
        let toml = format!(
            "[pushes.playtimed]\ncommit = \"HEAD~1\"\ndigest = {DIGEST:?}\n\
             queued_at = 1\nlast_probe = 1\nlast_answer = \"\"\n"
        );
        assert!(Queue::from_toml(&toml).unwrap_err().contains("commit"));

        let toml = format!(
            "[pushes.playtimed]\ncommit = {COMMIT:?}\ndigest = \"../../PWNED\"\n\
             queued_at = 1\nlast_probe = 1\nlast_answer = \"\"\n"
        );
        assert!(Queue::from_toml(&toml).unwrap_err().contains("digest"));
    }
}
