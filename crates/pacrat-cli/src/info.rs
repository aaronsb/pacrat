//! `pacrat info` — one package, everything pacrat knows about it.
//!
//! The layout is deliberately the same for every rung of the custody ladder
//! (ADR-001: "browsing shows identical metadata … at every rung"). An
//! unmanaged package prints the same rows as a maintained one, with an em
//! dash where an answer does not exist. That is the point: you can see at a
//! glance what curating a package *would* give you, and comparing two
//! packages never means comparing two shapes.
//!
//! Three sources are merged — the sync databases (`pacman -Si`), this host's
//! local db (`pacman -Qi`), and the AUR RPC — because no one of them covers
//! the whole ladder: a vendored package served from [dotfiles-aur] is not in
//! the AUR's answer for "what is installed", and an AUR package that is not
//! installed here is in no pacman db at all.

use std::collections::BTreeMap;

use pacrat_core::Custody;

use crate::aur::{self, AurPkg};
use crate::ctx::Ctx;
use crate::custody::Index;
use crate::out::{epoch_date, list_preview};
use crate::pacman::{self, field};

/// Width of the label column, wide enough for every label plus a gap.
const LABEL: usize = 9;

pub fn run(ctx: &Ctx, package: &str) -> Result<(), String> {
    let rpc_url = aur::info_url(package);
    println!("  pacman -Si -- {package}");
    println!("  pacman -Qi -- {package}");
    println!("  {}", aur::argv(&rpc_url));
    println!();

    let index = Index::build(ctx)?;
    let sync = pacman::sync_info(package);
    let local = pacman::local_info(package);
    let (remote, rpc_error) = match aur::info(package) {
        Ok(hits) => (hits.into_iter().next(), None),
        Err(e) => (None, Some(e)),
    };

    if sync.is_none() && local.is_none() && remote.is_none() {
        return Err(match rpc_error {
            Some(e) => format!(
                "{package}: no such package in the sync databases or the local db, \
                 and the AUR could not be asked ({e})"
            ),
            None => format!(
                "{package}: no such package in the sync databases, the local db, or the AUR"
            ),
        });
    }

    identity(package, sync.as_ref(), local.as_ref(), remote.as_ref());
    origin(sync.as_ref(), remote.as_ref(), rpc_error.as_deref());
    custody(package, &index);
    install(local.as_ref(), package, &index);
    Ok(())
}

type Fields = BTreeMap<String, String>;

fn row(label: &str, value: &str) {
    println!("{label:<LABEL$}{value}");
}

/// Same rows every time; an em dash is an answer.
fn maybe(label: &str, value: Option<String>) {
    row(label, value.as_deref().unwrap_or("—"));
}

fn identity(package: &str, sync: Option<&Fields>, local: Option<&Fields>, remote: Option<&AurPkg>) {
    // The version on offer, not the one on disk — that is `install`'s row.
    let version = pick(sync, "Version")
        .or_else(|| remote.map(|p| p.version.clone()))
        .or_else(|| pick(local, "Version"));
    let description = pick(sync, "Description")
        .or_else(|| remote.and_then(|p| p.description.clone()))
        .or_else(|| pick(local, "Description"));
    let upstream = pick(sync, "URL")
        .or_else(|| remote.and_then(|p| p.url.clone()))
        .or_else(|| pick(local, "URL"));

    row("name", package);
    maybe("version", version);
    maybe("summary", description);
    maybe("upstream", upstream);
}

fn origin(sync: Option<&Fields>, remote: Option<&AurPkg>, rpc_error: Option<&str>) {
    let mut lines: Vec<String> = Vec::new();
    if let Some(f) = sync {
        let mut parts = vec![format!(
            "repo {}",
            field(f, "Repository").unwrap_or("official")
        )];
        if let Some(p) = field(f, "Packager") {
            parts.push(format!("packager {p}"));
        }
        if let Some(b) = field(f, "Build Date") {
            parts.push(format!("built {b}"));
        }
        lines.push(parts.join(" · "));
    }
    if let Some(p) = remote {
        let mut parts = vec!["aur".to_string()];
        parts.push(format!(
            "maintainer {}",
            p.maintainer.as_deref().unwrap_or("orphaned")
        ));
        if let Some(v) = p.votes {
            parts.push(format!("votes {v}"));
        }
        if let Some(pop) = p.popularity {
            parts.push(format!("popularity {pop:.2}"));
        }
        if let Some(t) = p.last_modified {
            parts.push(format!("updated {}", epoch_date(t)));
        }
        if let Some(t) = p.out_of_date {
            parts.push(format!("FLAGGED out of date {}", epoch_date(t)));
        }
        lines.push(parts.join(" · "));
    }
    if let Some(e) = rpc_error {
        lines.push(format!("aur unknown — the RPC could not be asked ({e})"));
    }
    if lines.is_empty() {
        // Installed, but no repo and no AUR listing: a hand-built package,
        // or one whose upstream has since vanished.
        lines.push("local only — no repo and no AUR listing".into());
    }
    println!();
    row("origin", &lines[0]);
    for extra in &lines[1..] {
        row("", extra);
    }
}

fn custody(package: &str, index: &Index) {
    let rung = index.custody(package);
    let mut parts = vec![match rung {
        Some(Custody::Maintained) => "maintained",
        Some(Custody::Vendored) => "vendored",
        Some(Custody::Tracked) => "tracked",
        // The bottom rung and "pacrat has never heard of it" read the same
        // from the manifest's point of view.
        Some(Custody::Unmanaged) | None => "unmanaged",
    }
    .to_string()];
    if let Some(entry) = index.ledger_entry(package) {
        parts.push(format!("reviewed {}", entry.reviewed));
        parts.push(format!("upstream {}", entry.upstream));
        if let Some(note) = &entry.note {
            parts.push(note.clone());
        }
    }
    row("custody", &parts.join(" · "));

    let hosts = index.hosts_tracking(package);
    row(
        "",
        &if hosts.is_empty() {
            "no host tracks it".to_string()
        } else {
            format!(
                "tracked on {} host{}: {}",
                hosts.len(),
                if hosts.len() == 1 { "" } else { "s" },
                list_preview(hosts, 8)
            )
        },
    );
}

fn install(local: Option<&Fields>, package: &str, index: &Index) {
    let line = match local {
        Some(f) => {
            let mut parts = vec![field(f, "Version").unwrap_or("installed").to_string()];
            // The install date is the whole reason this section exists: it
            // is how you tell a deliberate choice from something that came
            // in as a dependency years ago.
            parts.push(format!(
                "installed {}",
                field(f, "Install Date").unwrap_or("date unknown")
            ));
            if let Some(reason) = field(f, "Install Reason") {
                parts.push(reason.to_lowercase());
            }
            parts.join(" · ")
        }
        // The live sets and -Qi should never disagree; if they do, say so
        // rather than quietly printing the friendlier of the two.
        None if index.is_installed(package) => {
            "not in the local db, though the live query lists it".into()
        }
        None => "not installed on this host".into(),
    };
    row("install", &line);
}

fn pick(fields: Option<&Fields>, key: &str) -> Option<String> {
    fields.and_then(|f| field(f, key)).map(str::to_string)
}
