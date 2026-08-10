//! `aur/decisions.toml` — the store's decision ledger: the record of a human
//! deliberately accepting a named risk.
//!
//! ADR-001's decisions 2 and 6 describe two escapes — overriding a BLOCK,
//! trusting an unsigned package — and observe that they share a shape: a
//! person passes through friction and accepts something pacrat would
//! otherwise refuse. The ADR settles that both write **one** ledger rather
//! than growing parallel bookkeeping, and leaves the file shape to the first
//! implementer. This is that shape.
//!
//! Three properties follow from what the file is for:
//!
//! 1. **Append-only, and synced.** It lives in the store beside
//!    `sources.toml`, so a risk one host accepted is a fact the fleet can
//!    read, grep and review. Nothing here rewrites or removes an entry;
//!    `Decisions::push` adds one, and the writer re-reads before it does
//!    (`Ctx::save_*`'s rule — a whole-file write over a stale copy silently
//!    reverts whatever another process recorded meanwhile).
//!
//! 2. **A decision names bytes, not a package.** Every entry carries the
//!    commit it was made about, because "we decided mdcat was fine" is not
//!    a statement anybody can act on later — the thing that was accepted was
//!    one specific candidate. The commit is validated exactly as
//!    [`crate::sources::SourceEntry::reviewed`] is, and for the same reason:
//!    this is a synced file whose values are printed and compared.
//!
//! 3. **The reason is the friction.** ADR-001 decision 2 puts the CLI's
//!    override behind "authoring the justification" — so an entry with no
//!    reason is not a cheaper entry, it is a broken one, and it fails to
//!    parse. It is bounded for the same reason a rejection note is
//!    ([`crate::sources::NOTE_MAX`]): every host has to render this file.
//!
//! What is *not* here is any notion of a decision expiring, being revoked,
//! or applying to a later commit. A record is a record of what happened. The
//! gate re-asks its question every time, which is what keeps one accepted
//! risk from becoming a standing exemption.

use serde::{Deserialize, Serialize};

use crate::grading::PACRAT_SCALE;
use crate::pkg::valid_name;
use crate::sources::valid_commit;

/// How long a justification may be. The same cap, for the same reason, as a
/// rejection note: this file is synced to every host, and an unbounded
/// free-text field in a synced file grows legs.
pub const REASON_MAX: usize = 500;

/// How long a host name may be. Generous against
/// `HOST_NAME_MAX`; a bound rather than a grammar, because the value is
/// neutered at render like every other string pacrat did not author.
pub const HOST_MAX: usize = 128;

/// What kind of risk was accepted.
///
/// Two members, one writer. `TrustUnsigned` is ADR-001 decision 6's escape —
/// a specific unsigned package a human chose to serve anyway — and package
/// signing is phase-2 work, so **nothing writes it yet**. It is modelled now
/// because the decision that both flows share a ledger is already made, and
/// a kind added later would mean either a second file or a schema migration
/// on a file the whole fleet reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A BLOCK verdict adopted anyway (`adopt-update --override-block`).
    OverrideBlock,
    /// An unsigned package trusted for serving. Written by the signing work;
    /// see the type note.
    TrustUnsigned,
}

impl Kind {
    /// The word in the file, and in every report.
    pub fn label(self) -> &'static str {
        match self {
            Kind::OverrideBlock => "override-block",
            Kind::TrustUnsigned => "trust-unsigned",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One accepted risk.
///
/// `PartialEq` but not `Eq`: `extra` holds arbitrary TOML, and a TOML float
/// is an `f64`. Nothing here compares decisions for identity — the tests
/// compare round trips — so the weaker bound costs nothing and keeping the
/// unknown fields is worth a great deal more.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub kind: Kind,
    pub package: String,
    /// The candidate this was decided about.
    pub commit: String,
    /// The verdict's worst grade at the moment of the decision, when there
    /// was one. `None` is a real answer — an override of a BLOCK that came
    /// from a grade is not the same event as one recorded against a subject
    /// nothing could grade — so it is optional rather than defaulted to
    /// zero, which would read as PROCEED.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grade: Option<u8>,
    /// Why. Required: see the module note.
    pub reason: String,
    /// The host that made the decision. Recorded because the file is shared
    /// and "who accepted this" is the first question a reader has.
    pub host: String,
    /// When, as `YYYY-MM-DDTHH:MM:SSZ`.
    pub at: String,
    /// Fields this pacrat has never heard of, kept verbatim.
    ///
    /// The ledger is append-only, synced, and rewritten in full by whichever
    /// host records the next decision — so without this, one machine running
    /// last month's pacrat quietly deletes a field this month's added, for
    /// every entry in the file, as a side effect of an unrelated override.
    /// That is the worst failure mode a *record* can have: silent, total, and
    /// caused by the routine use of the thing.
    ///
    /// The grade cache makes the same move for the same reason — it keeps a
    /// grader's JSON as a raw `Value` so an unknown key survives a version
    /// that does not know it. Forward compatibility the contract promises to
    /// graders, promised here to future pacrats.
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl Decision {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_name(&self.package) {
            return Err(format!(
                "package {:?} is not a package name — expected letters, digits, \
                 and @._+- (no leading hyphen or dot)",
                self.package
            ));
        }
        if !valid_commit(&self.commit) {
            return Err(format!(
                "commit {:?} is not a commit hash — expected 7-64 hex characters \
                 as `git rev-parse` prints them",
                self.commit
            ));
        }
        if let Some(grade) = self.grade {
            if !PACRAT_SCALE.contains(grade) {
                return Err(format!(
                    "grade {grade} is outside pacrat's scale {}-{}",
                    PACRAT_SCALE.min, PACRAT_SCALE.max
                ));
            }
        }
        // Blank is empty. A reason of three spaces is the same non-answer as
        // no reason at all, and the friction ADR-001 asks for is the writing,
        // not the typing.
        if self.reason.trim().is_empty() {
            return Err(
                "reason is empty — a recorded override with no justification is \
                 exactly the thing the friction exists to prevent"
                    .into(),
            );
        }
        let length = self.reason.chars().count();
        if length > REASON_MAX {
            return Err(format!(
                "reason is {length} characters — the cap is {REASON_MAX}; the ledger \
                 is synced to every host, and a reason that long is something other \
                 than a reason"
            ));
        }
        if self.host.trim().is_empty() {
            return Err("host is empty".into());
        }
        if self.host.chars().count() > HOST_MAX {
            return Err(format!(
                "host is {} characters — the cap is {HOST_MAX}",
                self.host.chars().count()
            ));
        }
        if !valid_timestamp(&self.at) {
            return Err(format!(
                "at {:?} is not a timestamp — expected YYYY-MM-DDTHH:MM:SSZ (UTC)",
                self.at
            ));
        }
        Ok(())
    }
}

/// Is this the shape pacrat writes into `at`?
///
/// A fixed profile of RFC 3339, not a parser: pacrat writes exactly one
/// spelling — UTC, second precision, `Z` — and a validator that accepted
/// more than the writer produces would let a hand-edited file carry a value
/// no reader can sort against the others. Sorting is the whole reason the
/// field is a string in this format rather than epoch seconds.
pub fn valid_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 20 {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| b[range].iter().all(u8::is_ascii_digit);
    digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && b[10] == b'T'
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19)
        && b[19] == b'Z'
}

/// The ledger: every decision, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Decisions {
    /// `[[decision]]` in the file; a list rather than a map because two
    /// decisions about one package at two commits are two facts, and neither
    /// replaces the other.
    #[serde(default, rename = "decision", skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<Decision>,
}

impl Decisions {
    /// Parse and validate. As with [`crate::sources::Sources`], there is no
    /// path through this type that yields an unvalidated ledger — a consumer
    /// holding a `Decisions` holds one whose commits are commits.
    pub fn from_toml(s: &str) -> Result<Self, String> {
        let ledger: Self = toml::from_str(s).map_err(|e| e.to_string())?;
        for (i, decision) in ledger.decisions.iter().enumerate() {
            decision
                .validate()
                .map_err(|e| format!("decision {}: {e}", i + 1))?;
        }
        Ok(ledger)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("Decisions is always serializable")
    }

    /// Append one, refusing an entry the file could not be re-read with.
    ///
    /// Validated *before* it joins the list rather than at the next parse:
    /// writing an unparseable decision would break the ledger for every host
    /// in the fleet, over a field this side could see was wrong.
    pub fn push(&mut self, decision: Decision) -> Result<(), String> {
        decision.validate()?;
        self.decisions.push(decision);
        Ok(())
    }

    /// Every decision recorded about one package, in file order.
    pub fn about<'a>(&'a self, package: &'a str) -> impl Iterator<Item = &'a Decision> {
        self.decisions.iter().filter(move |d| d.package == package)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision() -> Decision {
        Decision {
            kind: Kind::OverrideBlock,
            package: "mdcat".into(),
            commit: "5a4705a4aaa2e7f10a7dd6c302256dd373516e56".into(),
            grade: Some(4),
            reason: "upstream's new post_install is the packaging fix I asked for".into(),
            host: "north".into(),
            at: "2026-08-10T12:34:56Z".into(),
            extra: toml::Table::new(),
        }
    }

    #[test]
    fn a_decision_roundtrips_through_the_file() {
        let mut ledger = Decisions::default();
        ledger.push(decision()).unwrap();
        let text = ledger.to_toml();
        assert!(text.contains("[[decision]]"), "{text}");
        assert!(text.contains("override-block"), "{text}");
        assert_eq!(Decisions::from_toml(&text).unwrap(), ledger);
    }

    #[test]
    fn an_absent_file_is_an_empty_ledger_and_writes_no_table() {
        assert_eq!(Decisions::from_toml("").unwrap(), Decisions::default());
        assert_eq!(Decisions::default().to_toml().trim(), "");
    }

    /// Two decisions about one package are two facts. A map keyed by package
    /// would have made the second silently replace the first.
    #[test]
    fn two_decisions_about_one_package_both_survive() {
        let mut ledger = Decisions::default();
        ledger.push(decision()).unwrap();
        let mut later = decision();
        later.commit = "7d02e4c1aaa2e7f10a7dd6c302256dd373516e56".into();
        later.at = "2026-09-01T00:00:00Z".into();
        ledger.push(later).unwrap();
        assert_eq!(ledger.about("mdcat").count(), 2);
        assert_eq!(ledger.about("yay").count(), 0);
        assert_eq!(Decisions::from_toml(&ledger.to_toml()).unwrap(), ledger);
    }

    /// The grade is optional, and its absence is a different fact from a
    /// zero — which would read as PROCEED, the one thing an override is not.
    #[test]
    fn an_ungraded_decision_omits_the_grade_rather_than_zeroing_it() {
        let mut d = decision();
        d.grade = None;
        let mut ledger = Decisions::default();
        ledger.push(d).unwrap();
        let text = ledger.to_toml();
        assert!(!text.contains("grade"), "{text}");
        assert_eq!(Decisions::from_toml(&text).unwrap(), ledger);
    }

    /// The friction ADR-001 asks for is the justification. An entry without
    /// one does not parse and cannot be pushed — both doors, because a
    /// `Decisions` can be built in memory without passing the parser.
    #[test]
    fn a_decision_with_no_reason_is_not_a_decision() {
        for reason in ["", "   ", "\n\t "] {
            let mut d = decision();
            d.reason = reason.into();
            assert!(d.validate().is_err(), "{reason:?} was accepted");
            assert!(Decisions::default().push(d).is_err());
        }
    }

    #[test]
    fn a_reason_longer_than_the_cap_is_refused_and_counted_in_characters() {
        let mut d = decision();
        d.reason = "x".repeat(REASON_MAX + 1);
        let err = d.validate().unwrap_err();
        assert!(err.contains("501") && err.contains("cap"), "{err}");
        // At the cap is fine, and a reason in Greek is not half a reason.
        d.reason = "é".repeat(REASON_MAX);
        assert!(d.validate().is_ok());
    }

    /// The same hostile values `sources` refuses, refused here: this file is
    /// synced, its commits are compared against `git rev-parse` output, and
    /// its package names are printed into commands.
    #[test]
    fn hostile_fields_fail_to_parse() {
        /// One way to break an entry, and the field the error must name.
        type Case = (&'static str, fn(&mut Decision));

        let cases: [Case; 6] = [
            ("commit", |d| d.commit = "../../../../PWNED".into()),
            ("commit", |d| d.commit = "HEAD~1".into()),
            ("package", |d| d.package = "../escape".into()),
            ("package", |d| d.package = "-upload-pack=x".into()),
            ("grade", |d| d.grade = Some(9)),
            ("host", |d| d.host = String::new()),
        ];
        for (field, break_it) in cases {
            let mut d = decision();
            break_it(&mut d);
            let err = d.validate().unwrap_err();
            assert!(err.contains(field), "{field}: unhelpful error {err:?}");
            // And the whole file goes down with it, naming which entry.
            let ledger = Decisions { decisions: vec![d] };
            let err = Decisions::from_toml(&ledger.to_toml()).unwrap_err();
            assert!(err.contains("decision 1"), "{err}");
        }
    }

    /// A field a future pacrat adds must survive this one rewriting the
    /// file. The ledger is rewritten whole by whoever records next, so a
    /// dropped key is not one host's local loss — it is the fleet's record,
    /// deleted by an older binary doing its job.
    #[test]
    fn a_field_this_version_has_never_heard_of_survives_a_rewrite() {
        let newer = "[[decision]]\n\
                     kind = \"override-block\"\n\
                     package = \"mdcat\"\n\
                     commit = \"5a4705a4\"\n\
                     reason = \"read it\"\n\
                     host = \"north\"\n\
                     at = \"2026-08-10T12:34:56Z\"\n\
                     signature = \"a-thing-phase-2-adds\"\n\
                     expires_at = \"2027-01-01T00:00:00Z\"\n\
                     [decision.reviewer]\n\
                     name = \"someone\"\n";
        let ledger = Decisions::from_toml(newer).unwrap();
        assert_eq!(ledger.decisions[0].extra.len(), 3);

        // Read, appended to, and written back by *this* version.
        let mut ledger = ledger;
        let mut mine = decision();
        mine.package = "yay".into();
        ledger.push(mine).unwrap();
        let text = ledger.to_toml();
        assert!(text.contains("a-thing-phase-2-adds"), "{text}");
        assert!(text.contains("expires_at"), "{text}");
        assert!(text.contains("[decision.reviewer]"), "{text}");
        // And the round trip is stable, so the next host does not see churn.
        assert_eq!(Decisions::from_toml(&text).unwrap(), ledger);

        // An entry with nothing extra writes nothing extra.
        assert!(!Decisions {
            decisions: vec![decision()]
        }
        .to_toml()
        .contains("extra"));
    }

    /// A timestamp is a sort key. Anything that does not sort like the ones
    /// pacrat writes is refused rather than accepted and mis-ordered.
    #[test]
    fn only_the_one_timestamp_spelling_is_accepted() {
        assert!(valid_timestamp("2026-08-10T12:34:56Z"));
        assert!(valid_timestamp("1970-01-01T00:00:00Z"));
        for bad in [
            "",
            "2026-08-10",
            "2026-08-10T12:34:56",       // no zone
            "2026-08-10T12:34:56+02:00", // a zone that is not UTC
            "2026-08-10T12:34:56.123Z",  // finer than pacrat writes
            "2026-8-10T12:34:56Z",       // unpadded
            "yesterday",
            "2026-08-10T12:34:5xZ",
        ] {
            assert!(!valid_timestamp(bad), "{bad:?} was accepted");
        }
    }

    /// An unknown kind is a file this pacrat cannot act on, so it is a parse
    /// failure rather than an entry quietly dropped: a decision nobody can
    /// read is not the same as a decision nobody made.
    #[test]
    fn an_unknown_kind_fails_rather_than_disappearing() {
        let toml = "[[decision]]\nkind = \"trust-me\"\npackage = \"mdcat\"\n\
                    commit = \"5a4705a4\"\nreason = \"x\"\nhost = \"north\"\n\
                    at = \"2026-08-10T12:34:56Z\"\n";
        assert!(Decisions::from_toml(toml).is_err());
        // The two real ones parse.
        for kind in ["override-block", "trust-unsigned"] {
            let toml = toml.replace("trust-me", kind);
            assert!(Decisions::from_toml(&toml).is_ok(), "{kind}");
        }
    }
}
