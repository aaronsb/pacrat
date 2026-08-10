//! Pure model for pacrat. No I/O and no subprocess — the same split
//! dotfiles-core keeps: everything that talks to git/pacman/graders lives in
//! the CLI crate.

pub mod config;
pub mod pkg;
pub mod sources;
pub mod version;

/// Where a package sits on the custody ladder (mockup §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Custody {
    /// Installed somewhere, invisible to the manifest.
    Unmanaged,
    /// In the manifest with host/profile membership; updates from upstream.
    Tracked,
    /// PKGBUILD tree in the store at a reviewed commit; served from [dotfiles-aur].
    Vendored,
    /// Vendored and ours: push rights back to the AUR.
    Maintained,
}

/// pacrat's verdict on a graded candidate. Graders return numbers (the
/// pacrat-grade/v1 contract); the verdict is derived here and only here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Proceed,
    Warn,
    Block,
    /// No valid grading for this candidate commit. Never treated as Proceed.
    Ungraded,
}

/// Grade→verdict thresholds. Config data, not grader data: changing risk
/// posture is a config edit, not a grader release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Thresholds {
    pub warn_at: u8,
    pub block_at: u8,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            warn_at: 2,
            block_at: 4,
        }
    }
}

impl Thresholds {
    pub fn verdict(&self, grade: Option<u8>) -> Verdict {
        match grade {
            None => Verdict::Ungraded,
            Some(g) if g >= self.block_at => Verdict::Block,
            Some(g) if g >= self.warn_at => Verdict::Warn,
            Some(_) => Verdict::Proceed,
        }
    }

    /// Aggregate multiple gradings for one candidate: worst grade wins
    /// (ADR-001 open question 5 — revisit if graders grow weights/vetoes).
    pub fn verdict_all<I: IntoIterator<Item = u8>>(&self, grades: I) -> Verdict {
        self.verdict(grades.into_iter().max())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_thresholds() {
        let t = Thresholds::default();
        assert_eq!(t.verdict(Some(0)), Verdict::Proceed);
        assert_eq!(t.verdict(Some(2)), Verdict::Warn);
        assert_eq!(t.verdict(Some(4)), Verdict::Block);
        assert_eq!(t.verdict(None), Verdict::Ungraded);
    }

    #[test]
    fn worst_grade_wins() {
        let t = Thresholds::default();
        assert_eq!(t.verdict_all([0, 0, 4]), Verdict::Block);
        assert_eq!(t.verdict_all([0, 1]), Verdict::Proceed);
        assert_eq!(t.verdict_all([]), Verdict::Ungraded);
    }
}
