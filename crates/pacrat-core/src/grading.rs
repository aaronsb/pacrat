//! The `pacrat-grade/v1` contract (ADR-001): the only thing a grader is
//! allowed to say. Graders return a *number*; PROCEED/WARN/BLOCK is pacrat's
//! data, derived from that number by [`crate::Thresholds`] and nowhere else.
//!
//! Everything here is parsing and validation of text pacrat did not write.
//! A grader is a subprocess — often an LLM wrapper — so its output is
//! untrusted twice over: it can be malformed by accident, and it can be
//! steered by the very PKGBUILD it was asked to judge. Two rules follow, and
//! they are the reason this module exists rather than a bare `serde_json`
//! call at the use site:
//!
//! 1. **A grading pacrat does not fully understand is not a grading.** A
//!    wrong contract string, or a grade outside the grader's own declared
//!    scale, is rejected outright — the caller then treats the grader as
//!    having produced nothing, which holds. There is no lenient path.
//! 2. **Unknown fields are fine.** Hosts in a fleet run different pacrat
//!    versions, and a newer grader adding a field must not break an older
//!    reader. Forward compatibility is tolerance about *additions* only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The only contract string this pacrat reads.
pub const CONTRACT: &str = "pacrat-grade/v1";

/// pacrat's own scale — the one [`crate::Thresholds`] are written against.
pub const PACRAT_SCALE: Scale = Scale { min: 0, max: 4 };

/// The range a grader's number lives on. Declared per grading because the
/// contract lets a grader use its own scale; pacrat maps it onto
/// [`PACRAT_SCALE`] before comparing it to thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scale {
    pub min: u8,
    pub max: u8,
}

impl Default for Scale {
    fn default() -> Self {
        PACRAT_SCALE
    }
}

impl Scale {
    pub fn contains(self, grade: u8) -> bool {
        grade >= self.min && grade <= self.max
    }

    /// Express a grade on pacrat's 0-4 scale.
    ///
    /// Thresholds are bare numbers with no scale attached — `block_at = 4`
    /// only means anything on 0-4. Comparing a 7-of-10 to it directly would
    /// read a middling grade as BLOCK, so a grader on another scale is
    /// mapped first. Rounding is **up**, toward risk: a grading that lands
    /// between two pacrat grades takes the worse of the two, because the
    /// cost of a missed warning is not the cost of a spurious one.
    pub fn to_pacrat(self, grade: u8) -> u8 {
        if self == PACRAT_SCALE {
            return grade;
        }
        let grade = grade.clamp(self.min, self.max);
        // `validate` rejects min >= max, so the span is never zero.
        let span = u32::from(self.max.saturating_sub(self.min)).max(1);
        let offset = u32::from(grade - self.min);
        (offset * u32::from(PACRAT_SCALE.max)).div_ceil(span) as u8
    }
}

/// What was graded. `commit` is the cache key's other half: a moved upstream
/// HEAD is a different subject, never a stale grading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub package: String,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// One thing the grader wants a human to look at. Advisory: findings do not
/// move the grade, the grade moves the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Severity on the grading's own scale. Not validated against it: a
    /// grader that mislabels one finding should still have its *grade*
    /// honored, and throwing away a whole grading over an annotation would
    /// convert a cosmetic bug into a hold.
    #[serde(default)]
    pub level: u8,
    #[serde(default)]
    pub title: String,
    /// Where to look, e.g. `PKGBUILD:3`. Free text from the grader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<String>,
}

/// One grader's answer about one subject.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GradeReport {
    pub contract: String,
    pub grader: String,
    pub subject: Subject,
    pub grade: u8,
    #[serde(default)]
    pub scale: Scale,
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// Whatever the grader wants to record about the run — provider, model,
    /// duration, its own cache hit. Opaque to pacrat by design.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, Value>,
}

impl GradeReport {
    /// Parse and validate grader output. The two steps are not separable:
    /// there is no constructor that yields an unvalidated report from text.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let report: Self = serde_json::from_str(text).map_err(|e| format!("not a grading: {e}"))?;
        report.validate()?;
        Ok(report)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a GradeReport is always serializable")
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.contract != CONTRACT {
            return Err(format!(
                "contract is {:?}, not {CONTRACT:?} — pacrat will not read a grading \
                 written to a contract it does not know",
                self.contract
            ));
        }
        if self.grader.trim().is_empty() {
            return Err("grader is empty — a grading has to say who produced it".into());
        }
        if self.subject.package.trim().is_empty() {
            return Err("subject.package is empty".into());
        }
        if self.subject.commit.trim().is_empty() {
            return Err("subject.commit is empty — the commit is half the cache key".into());
        }
        if self.scale.min >= self.scale.max {
            return Err(format!(
                "scale {}-{} spans nothing — a grade on it would carry no information",
                self.scale.min, self.scale.max
            ));
        }
        if !self.scale.contains(self.grade) {
            return Err(format!(
                "grade {} is outside the grader's own declared scale {}-{}",
                self.grade, self.scale.min, self.scale.max
            ));
        }
        Ok(())
    }

    /// The grade on pacrat's 0-4 scale — the number thresholds compare.
    pub fn pacrat_grade(&self) -> u8 {
        self.scale.to_pacrat(self.grade)
    }

    /// Refuse a grading of something other than what was asked about.
    ///
    /// The cache is keyed by (grader, package, commit) but the *content* is
    /// the grader's own word for what it looked at. A grader that answers
    /// about a different subject — confused, or steered — would otherwise be
    /// filed under our key and read later as a grading of our tree.
    pub fn is_about(&self, package: &str, commit: &str) -> Result<(), String> {
        if self.subject.package != package {
            return Err(format!(
                "graded {:?}, not {package:?} — a grading of another package is not \
                 a grading of this one",
                self.subject.package
            ));
        }
        if !commit_matches(&self.subject.commit, commit) {
            return Err(format!(
                "graded commit {:?}, not {commit:?}",
                self.subject.commit
            ));
        }
        Ok(())
    }

    /// The findings worth showing first: worst level first, input order
    /// within a level so a grader's own ranking survives the sort.
    pub fn top_findings(&self, max: usize) -> Vec<&Finding> {
        let mut ranked: Vec<&Finding> = self.findings.iter().collect();
        // A stable sort, so equal levels keep the grader's own order.
        ranked.sort_by_key(|f| std::cmp::Reverse(f.level));
        ranked.truncate(max);
        ranked
    }
}

/// Commit equality that tolerates abbreviation on either side. Compared as
/// bytes: a grader can put anything in that string, and slicing a `str` at a
/// non-boundary would panic. Seven characters is the shortest prefix git
/// itself will abbreviate to; anything shorter names too many trees to be
/// called a match.
pub fn commit_matches(a: &str, b: &str) -> bool {
    let n = a.len().min(b.len());
    n >= 7 && a.as_bytes()[..n].eq_ignore_ascii_case(&b.as_bytes()[..n])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ADR's own example, verbatim — the contract's reference shape.
    const ADR_EXAMPLE: &str = r#"
{ "contract": "pacrat-grade/v1",
  "grader": "yay-friend",
  "subject": { "package": "mdcat", "commit": "3f9c21ab55de", "version": "2.1.1-1" },
  "grade": 0, "scale": { "min": 0, "max": 4 },
  "findings": [ { "level": 0, "title": "no network in build()", "span": "PKGBUILD:3" } ],
  "meta": { "provider": "claude", "duration_s": 41, "cached": true } }
"#;

    #[test]
    fn the_adr_example_parses() {
        let r = GradeReport::from_json(ADR_EXAMPLE).unwrap();
        assert_eq!(r.grader, "yay-friend");
        assert_eq!(r.subject.package, "mdcat");
        assert_eq!(r.subject.version.as_deref(), Some("2.1.1-1"));
        assert_eq!(r.grade, 0);
        assert_eq!(r.scale, PACRAT_SCALE);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].span.as_deref(), Some("PKGBUILD:3"));
        // meta is opaque but preserved, values and all.
        assert_eq!(r.meta["provider"], Value::from("claude"));
        assert_eq!(r.meta["cached"], Value::from(true));
        assert_eq!(r.meta["duration_s"], Value::from(41));
    }

    /// A newer grader adding fields must not break an older reader.
    #[test]
    fn unknown_fields_are_tolerated() {
        let r = GradeReport::from_json(
            r#"{"contract":"pacrat-grade/v1","grader":"yf",
                "subject":{"package":"mdcat","commit":"3f9c21ab","extra":"ignored"},
                "grade":2,"scale":{"min":0,"max":4},
                "findings":[{"level":2,"title":"t","confidence":0.9}],
                "confidence":"high","policy_version":7}"#,
        )
        .unwrap();
        assert_eq!(r.grade, 2);
        assert_eq!(r.findings[0].title, "t");
    }

    /// Optional fields really are optional: the minimum a grader must say.
    #[test]
    fn the_minimum_grading_is_accepted() {
        let r = GradeReport::from_json(
            r#"{"contract":"pacrat-grade/v1","grader":"yf",
                "subject":{"package":"mdcat","commit":"3f9c21ab"},"grade":1}"#,
        )
        .unwrap();
        assert_eq!(r.scale, PACRAT_SCALE, "a missing scale is pacrat's");
        assert!(r.findings.is_empty());
        assert!(r.meta.is_empty());
        assert_eq!(r.subject.version, None);
    }

    #[test]
    fn a_foreign_contract_is_rejected() {
        for contract in [
            "pacrat-grade/v2",
            "pacrat-grade/v1 ",
            "PACRAT-GRADE/V1",
            "yay-friend/v3",
            "",
        ] {
            let json = format!(
                r#"{{"contract":"{contract}","grader":"yf",
                    "subject":{{"package":"m","commit":"3f9c21ab"}},"grade":0}}"#
            );
            let err = GradeReport::from_json(&json)
                .unwrap_err_or_panic(&format!("contract {contract:?} should be rejected"));
            assert!(err.contains("contract"), "unhelpful error: {err}");
        }
    }

    #[test]
    fn a_grade_outside_the_declared_scale_is_rejected() {
        // Above its own max, and below its own min.
        for (grade, min, max) in [(5u8, 0u8, 4u8), (255, 0, 4), (0, 1, 5), (11, 0, 10)] {
            let json = format!(
                r#"{{"contract":"pacrat-grade/v1","grader":"yf",
                    "subject":{{"package":"m","commit":"3f9c21ab"}},
                    "grade":{grade},"scale":{{"min":{min},"max":{max}}}}}"#
            );
            let err = GradeReport::from_json(&json)
                .unwrap_err_or_panic(&format!("grade {grade} on {min}-{max} should be rejected"));
            assert!(err.contains("outside"), "unhelpful error: {err}");
        }
    }

    #[test]
    fn a_grading_that_identifies_nothing_is_rejected() {
        let cases = [
            (
                r#""grader":"","subject":{"package":"m","commit":"3f9c21ab"}"#,
                "grader",
            ),
            (
                r#""grader":"yf","subject":{"package":"","commit":"3f9c21ab"}"#,
                "package",
            ),
            (
                r#""grader":"yf","subject":{"package":"m","commit":""}"#,
                "commit",
            ),
        ];
        for (body, word) in cases {
            let json = format!(r#"{{"contract":"pacrat-grade/v1",{body},"grade":0}}"#);
            let err = GradeReport::from_json(&json)
                .unwrap_err_or_panic(&format!("{body} should be rejected"));
            assert!(err.contains(word), "unhelpful error: {err}");
        }
    }

    #[test]
    fn a_degenerate_scale_is_rejected() {
        let json = r#"{"contract":"pacrat-grade/v1","grader":"yf",
            "subject":{"package":"m","commit":"3f9c21ab"},
            "grade":3,"scale":{"min":3,"max":3}}"#;
        let err = GradeReport::from_json(json).unwrap_err_or_panic("min == max should be rejected");
        assert!(err.contains("spans nothing"), "unhelpful error: {err}");
    }

    #[test]
    fn garbage_is_not_a_grading() {
        for text in ["", "not json at all", "{", "[]", "null", "{\"grade\":2}"] {
            assert!(
                GradeReport::from_json(text).is_err(),
                "{text:?} parsed as a grading"
            );
        }
    }

    #[test]
    fn foreign_scales_map_onto_pacrats_rounding_up() {
        let ten = Scale { min: 0, max: 10 };
        assert_eq!(ten.to_pacrat(0), 0);
        assert_eq!(ten.to_pacrat(1), 1); // 0.4 → 1: never rounds a finding away
        assert_eq!(ten.to_pacrat(5), 2);
        assert_eq!(ten.to_pacrat(7), 3); // 2.8 → 3, not the BLOCK a raw 7 would be
        assert_eq!(ten.to_pacrat(10), 4);

        // A scale that does not start at zero.
        let letters = Scale { min: 1, max: 5 };
        assert_eq!(letters.to_pacrat(1), 0);
        assert_eq!(letters.to_pacrat(5), 4);

        // pacrat's own scale is passed through untouched.
        for g in 0..=4 {
            assert_eq!(PACRAT_SCALE.to_pacrat(g), g);
        }
    }

    #[test]
    fn a_ten_scale_grading_does_not_read_as_block() {
        let json = r#"{"contract":"pacrat-grade/v1","grader":"yf",
            "subject":{"package":"m","commit":"3f9c21ab"},
            "grade":7,"scale":{"min":0,"max":10}}"#;
        let r = GradeReport::from_json(json).unwrap();
        assert_eq!(r.grade, 7, "the grader's own number is preserved");
        assert_eq!(r.pacrat_grade(), 3);
        assert_eq!(
            crate::Thresholds::default().verdict(Some(7)),
            crate::Verdict::Block
        );
        assert_eq!(
            crate::Thresholds::default().verdict(Some(r.pacrat_grade())),
            crate::Verdict::Warn
        );
    }

    #[test]
    fn a_grading_of_another_subject_is_refused() {
        let r = GradeReport::from_json(ADR_EXAMPLE).unwrap();
        assert_eq!(r.is_about("mdcat", "3f9c21ab55de"), Ok(()));
        // Abbreviation on either side is the same commit.
        assert_eq!(r.is_about("mdcat", "3f9c21ab55de0011"), Ok(()));
        assert_eq!(r.is_about("mdcat", "3F9C21AB"), Ok(()));
        assert!(r.is_about("pacseek", "3f9c21ab55de").is_err());
        assert!(r.is_about("mdcat", "deadbeefcafe").is_err());
        // Too short to mean anything: a 3-character "commit" matches
        // thousands of trees, so it is not a match at all.
        assert!(r.is_about("mdcat", "3f9").is_err());
    }

    #[test]
    fn commit_matching_never_panics_on_hostile_text() {
        // Multi-byte characters at the slice boundary.
        assert!(!commit_matches("ééééééé", "3f9c21ab"));
        assert!(!commit_matches("", ""));
        assert!(!commit_matches("3f9c21ab", ""));
    }

    #[test]
    fn top_findings_are_worst_first_and_capped() {
        let r = GradeReport::from_json(
            r#"{"contract":"pacrat-grade/v1","grader":"yf",
                "subject":{"package":"m","commit":"3f9c21ab"},"grade":4,
                "findings":[{"level":1,"title":"a"},{"level":4,"title":"b"},
                            {"level":2,"title":"c"},{"level":4,"title":"d"}]}"#,
        )
        .unwrap();
        let top: Vec<&str> = r.top_findings(3).iter().map(|f| f.title.as_str()).collect();
        assert_eq!(top, ["b", "d", "c"]);
        assert_eq!(r.top_findings(0).len(), 0);
        assert_eq!(r.top_findings(99).len(), 4);
    }

    /// A recorded grading must read back as the same grading: the cache
    /// stores this JSON and a later run trusts what it reads.
    #[test]
    fn roundtrip_through_json() {
        let r = GradeReport::from_json(ADR_EXAMPLE).unwrap();
        assert_eq!(GradeReport::from_json(&r.to_json()).unwrap(), r);
    }

    /// `Result::unwrap_err` needs `T: Debug` and a message; this says which
    /// case failed without repeating the assert at every call site.
    trait UnwrapErrOrPanic {
        fn unwrap_err_or_panic(self, msg: &str) -> String;
    }
    impl UnwrapErrOrPanic for Result<GradeReport, String> {
        fn unwrap_err_or_panic(self, msg: &str) -> String {
            match self {
                Ok(r) => panic!("{msg} — parsed as grade {}", r.grade),
                Err(e) => e,
            }
        }
    }
}
