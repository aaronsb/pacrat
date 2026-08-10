//! Which rung of the custody ladder a package sits on — answered for a
//! whole result set at once.
//!
//! The three lookups behind the answer (the ledger, every host's tracked
//! lists, this host's live db) each cost a file walk or a subprocess, so a
//! search builds the index once and asks it per row rather than the other
//! way round.
//!
//! Precedence is the ladder read from the top down: the ledger is
//! authoritative because it is the only rung a human had to review into
//! existence; then membership in *any* host's manifest; then mere presence
//! on this machine. A package none of the three know is not "unmanaged" so
//! much as simply out there, and gets no state at all.

use std::collections::{BTreeMap, BTreeSet};

use pacrat_core::pkg::Source;
use pacrat_core::sources::{SourceEntry, Sources};
use pacrat_core::Custody;

use crate::ctx::Ctx;
use crate::live;

pub struct Index {
    ledger: Sources,
    /// Package → the hosts whose manifest lists it, sorted.
    tracked: BTreeMap<String, Vec<String>>,
    /// Installed on this host, native and foreign alike.
    installed: BTreeSet<String>,
}

impl Index {
    pub fn build(ctx: &Ctx) -> Result<Self, String> {
        let mut tracked: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for host in ctx.tracked_hosts() {
            for source in Source::ALL {
                for pkg in ctx.tracked(&host, source)? {
                    let hosts = tracked.entry(pkg).or_default();
                    if !hosts.contains(&host) {
                        hosts.push(host.clone());
                    }
                }
            }
        }

        let mut installed = BTreeSet::new();
        for source in [Source::Native, Source::Aur] {
            installed.extend(live::installed(source)?);
        }

        Ok(Self {
            ledger: ctx.load_sources()?,
            tracked,
            installed,
        })
    }

    /// The rung, or None when nothing in the store or on this host knows the
    /// package.
    pub fn custody(&self, package: &str) -> Option<Custody> {
        if let Some(role) = self.ledger.custody(package) {
            return Some(role);
        }
        if self.tracked.contains_key(package) {
            return Some(Custody::Tracked);
        }
        if self.installed.contains(package) {
            return Some(Custody::Unmanaged);
        }
        None
    }

    /// Every host whose manifest lists the package.
    pub fn hosts_tracking(&self, package: &str) -> &[String] {
        self.tracked.get(package).map_or(&[], Vec::as_slice)
    }

    /// The ledger row behind a vendored or maintained package.
    pub fn ledger_entry(&self, package: &str) -> Option<&SourceEntry> {
        self.ledger.packages.get(package)
    }

    pub fn is_installed(&self, package: &str) -> bool {
        self.installed.contains(package)
    }
}

/// The custody column's word. Blank when the package is merely out there —
/// an empty cell reads as "no relationship" better than any label would.
pub fn label(custody: Option<Custody>) -> &'static str {
    match custody {
        Some(Custody::Maintained) => "maintained",
        Some(Custody::Vendored) => "vendored",
        Some(Custody::Tracked) => "tracked",
        Some(Custody::Unmanaged) => "installed",
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(ledger: &str, tracked: &[(&str, &[&str])], installed: &[&str]) -> Index {
        Index {
            ledger: Sources::from_toml(ledger).unwrap(),
            tracked: tracked
                .iter()
                .map(|(p, hosts)| {
                    (
                        (*p).to_string(),
                        hosts.iter().map(|h| (*h).to_string()).collect(),
                    )
                })
                .collect(),
            installed: installed.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    const LEDGER: &str = r#"
[packages.mdcat]
upstream = "https://aur.archlinux.org/mdcat.git"
reviewed = "3f9c21ab"
role = "vendored"

[packages.playtimed]
upstream = "https://aur.archlinux.org/playtimed.git"
reviewed = "aa00bb11"
role = "maintained"
"#;

    #[test]
    fn ledger_outranks_tracked_and_installed() {
        // Every rung is also tracked and installed — the ledger still wins.
        let i = index(
            LEDGER,
            &[("mdcat", &["cube"]), ("playtimed", &["cube"])],
            &["mdcat", "playtimed"],
        );
        assert_eq!(i.custody("mdcat"), Some(Custody::Vendored));
        assert_eq!(i.custody("playtimed"), Some(Custody::Maintained));
    }

    #[test]
    fn tracked_outranks_installed() {
        let i = index("", &[("ripgrep", &["north"])], &["ripgrep"]);
        assert_eq!(i.custody("ripgrep"), Some(Custody::Tracked));
    }

    #[test]
    fn tracked_counts_on_any_host_not_just_this_one() {
        let i = index("", &[("fd", &["slab"])], &[]);
        assert_eq!(i.custody("fd"), Some(Custody::Tracked));
        assert_eq!(i.hosts_tracking("fd"), ["slab"]);
    }

    #[test]
    fn installed_but_untracked_is_the_bottom_rung() {
        let i = index("", &[], &["cowsay"]);
        assert_eq!(i.custody("cowsay"), Some(Custody::Unmanaged));
        assert!(i.hosts_tracking("cowsay").is_empty());
    }

    #[test]
    fn unknown_has_no_state() {
        let i = index(LEDGER, &[("fd", &["slab"])], &["cowsay"]);
        assert_eq!(i.custody("pacseek"), None);
        assert!(!i.is_installed("pacseek"));
    }

    #[test]
    fn labels_cover_every_rung() {
        assert_eq!(label(Some(Custody::Maintained)), "maintained");
        assert_eq!(label(Some(Custody::Vendored)), "vendored");
        assert_eq!(label(Some(Custody::Tracked)), "tracked");
        assert_eq!(label(Some(Custody::Unmanaged)), "installed");
        assert_eq!(label(None), "");
    }
}
