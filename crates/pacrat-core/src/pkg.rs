//! Package-tracking set logic, lifted verbatim from dotfiles-core's `pkg`
//! module (ADR-001 consequence: dotfiles retires `pkg`, pacrat inherits the
//! math). Originally a port of the bash `lib/pkg.sh` comparison logic.
//!
//! No I/O and no shelling out: this module only normalizes package lists and
//! computes drift / pairwise / N-way differences. The CLI layer queries the live
//! system (pacman / AUR helper / flatpak) and reads the tracked `packages/<host>/
//! <source>.txt` files, then feeds the resulting lists here.
//!
//! Tracked lists are the *desired* state ("what should be"); a live query is the
//! *actual* state ("what is").

use std::collections::BTreeSet;

/// A package source on the Arch family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Official repos — `pacman -Qqen`.
    Native,
    /// Foreign / AUR / manual — `pacman -Qqem`.
    Aur,
    /// Flatpak applications.
    Flatpak,
}

impl Source {
    /// Every source, in display order.
    pub const ALL: [Source; 3] = [Source::Native, Source::Aur, Source::Flatpak];

    /// Stable lowercase name, used for the tracked file `<name>.txt`.
    pub fn name(self) -> &'static str {
        match self {
            Source::Native => "native",
            Source::Aur => "aur",
            Source::Flatpak => "flatpak",
        }
    }
}

/// Normalize raw command / file output into a sorted, de-duplicated list:
/// split on lines, trim each, drop blanks.
pub fn normalize(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Drift between a tracked (desired) and a live (actual) list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Drift {
    /// Tracked but not live — `sync` would install these.
    pub missing: Vec<String>,
    /// Live but not tracked — `capture` records them / `--prune` removes them.
    pub extra: Vec<String>,
}

impl Drift {
    /// True when tracked and live agree exactly.
    pub fn in_sync(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty()
    }
}

/// Compute drift of `live` against `tracked`.
pub fn drift(tracked: &[String], live: &[String]) -> Drift {
    let t: BTreeSet<&String> = tracked.iter().collect();
    let l: BTreeSet<&String> = live.iter().collect();
    Drift {
        missing: t.difference(&l).map(|s| (*s).clone()).collect(),
        extra: l.difference(&t).map(|s| (*s).clone()).collect(),
    }
}

/// A pairwise comparison of two hosts' tracked lists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PairDiff {
    /// Count of packages present in both.
    pub shared: usize,
    /// Present only on the first host.
    pub only_a: Vec<String>,
    /// Present only on the second host.
    pub only_b: Vec<String>,
}

/// Compare two hosts' lists: shared count plus each side's exclusive set.
pub fn pair_diff(a: &[String], b: &[String]) -> PairDiff {
    let sa: BTreeSet<&String> = a.iter().collect();
    let sb: BTreeSet<&String> = b.iter().collect();
    PairDiff {
        shared: sa.intersection(&sb).count(),
        only_a: sa.difference(&sb).map(|s| (*s).clone()).collect(),
        only_b: sb.difference(&sa).map(|s| (*s).clone()).collect(),
    }
}

/// An N-way comparison across all participating hosts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NwayDiff {
    /// Count of packages present in every participating host.
    pub common: usize,
    /// Per host, the packages unique to it (present on exactly one host).
    /// Only participating (non-empty) hosts appear, in input order.
    pub unique: Vec<(String, Vec<String>)>,
}

/// Diff package lists across hosts. A host with an empty list does not
/// participate (mirrors `pkg_diff_all`'s non-empty-file rule): `common` is over
/// participating hosts only, and a package is "unique" when it appears on
/// exactly one host.
pub fn nway_diff(hosts: &[(String, Vec<String>)]) -> NwayDiff {
    // Participating = non-empty list. Dedup each defensively.
    let participating: Vec<(&String, BTreeSet<&String>)> = hosts
        .iter()
        .filter(|(_, list)| !list.is_empty())
        .map(|(name, list)| (name, list.iter().collect::<BTreeSet<_>>()))
        .collect();
    let np = participating.len();
    if np == 0 {
        return NwayDiff::default();
    }

    // Occurrence count of each package across participating hosts.
    let mut counts: std::collections::BTreeMap<&String, usize> = std::collections::BTreeMap::new();
    for (_, set) in &participating {
        for pkg in set {
            *counts.entry(*pkg).or_insert(0) += 1;
        }
    }

    let common = counts.values().filter(|&&c| c == np).count();
    let unique = participating
        .iter()
        .map(|(name, set)| {
            let only: Vec<String> = set
                .iter()
                .filter(|pkg| counts.get(**pkg) == Some(&1))
                .map(|pkg| (*pkg).clone())
                .collect();
            ((*name).clone(), only)
        })
        .collect();

    NwayDiff { common, unique }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn normalize_sorts_trims_and_dedups() {
        assert_eq!(normalize("  b\na\n\n  a  \n c\n"), v(&["a", "b", "c"]));
        assert_eq!(normalize(""), Vec::<String>::new());
        assert_eq!(normalize("   \n\n"), Vec::<String>::new());
    }

    #[test]
    fn drift_splits_missing_and_extra() {
        let d = drift(&v(&["a", "b", "c"]), &v(&["b", "c", "d"]));
        assert_eq!(d.missing, v(&["a"])); // tracked, not live -> install
        assert_eq!(d.extra, v(&["d"])); // live, not tracked -> record/prune
        assert!(!d.in_sync());

        let same = drift(&v(&["a", "b"]), &v(&["a", "b"]));
        assert!(same.in_sync());
        assert_eq!(drift(&v(&[]), &v(&[])), Drift::default());
    }

    #[test]
    fn pair_diff_shared_and_exclusive() {
        let p = pair_diff(&v(&["a", "b", "c"]), &v(&["b", "c", "d", "e"]));
        assert_eq!(p.shared, 2);
        assert_eq!(p.only_a, v(&["a"]));
        assert_eq!(p.only_b, v(&["d", "e"]));

        let disjoint = pair_diff(&v(&["x"]), &v(&["y"]));
        assert_eq!(disjoint.shared, 0);
        assert_eq!(disjoint.only_a, v(&["x"]));
        assert_eq!(disjoint.only_b, v(&["y"]));
    }

    #[test]
    fn nway_common_and_unique() {
        let hosts = vec![
            ("alpha".to_string(), v(&["common", "onlyA", "ab"])),
            ("beta".to_string(), v(&["common", "onlyB", "ab"])),
            ("gamma".to_string(), v(&["common", "onlyG"])),
        ];
        let n = nway_diff(&hosts);
        // "common" is on all 3; "ab" is on 2 (not unique, not common).
        assert_eq!(n.common, 1);
        let uniq: std::collections::BTreeMap<_, _> = n.unique.into_iter().collect();
        assert_eq!(uniq["alpha"], v(&["onlyA"]));
        assert_eq!(uniq["beta"], v(&["onlyB"]));
        assert_eq!(uniq["gamma"], v(&["onlyG"]));
    }

    #[test]
    fn nway_ignores_empty_hosts() {
        let hosts = vec![
            ("alpha".to_string(), v(&["x", "y"])),
            ("empty".to_string(), v(&[])),
            ("beta".to_string(), v(&["x", "z"])),
        ];
        let n = nway_diff(&hosts);
        // Only alpha + beta participate; "x" common to both.
        assert_eq!(n.common, 1);
        assert_eq!(n.unique.len(), 2);
        let uniq: std::collections::BTreeMap<_, _> = n.unique.into_iter().collect();
        assert_eq!(uniq["alpha"], v(&["y"]));
        assert_eq!(uniq["beta"], v(&["z"]));
        assert!(!uniq.contains_key("empty"));
    }

    #[test]
    fn nway_empty_input() {
        assert_eq!(nway_diff(&[]), NwayDiff::default());
        let all_empty = vec![("a".to_string(), v(&[])), ("b".to_string(), v(&[]))];
        assert_eq!(nway_diff(&all_empty), NwayDiff::default());
    }
}
