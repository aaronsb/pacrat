//! Live-system queries: what is actually installed on this host, per source.
//! All subprocess use in pacrat stays in the CLI crate, and every external
//! command pacrat runs is printable — verbs that call these show the argv.

use std::process::Command;

use pacrat_core::pkg::{drift, normalize, Drift, Source};

use crate::ctx::Ctx;

/// The exact command a source query runs, for display.
pub fn query_argv(source: Source) -> &'static str {
    match source {
        Source::Native => "pacman -Qqen",
        Source::Aur => "pacman -Qqem",
        Source::Flatpak => "flatpak list --app --columns=application",
    }
}

/// Installed packages for a source. A missing `flatpak` binary means no
/// flatpaks (empty), a missing `pacman` is an error — this is an Arch tool.
pub fn installed(source: Source) -> Result<Vec<String>, String> {
    let (bin, args): (&str, &[&str]) = match source {
        Source::Native => ("pacman", &["-Qqen"]),
        Source::Aur => ("pacman", &["-Qqem"]),
        Source::Flatpak => ("flatpak", &["list", "--app", "--columns=application"]),
    };
    let out = match Command::new(bin).args(args).output() {
        Ok(out) => out,
        Err(e) if source == Source::Flatpak => {
            let _ = e;
            return Ok(Vec::new());
        }
        Err(e) => return Err(format!("{bin}: {e}")),
    };
    if !out.status.success() {
        return Err(format!(
            "{} failed: {}",
            query_argv(source),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(normalize(&String::from_utf8_lossy(&out.stdout)))
}

/// One source's answer to "does this host match the store": the tracked list
/// (desired), what is installed (actual), and the difference.
pub struct SourceDrift {
    pub source: Source,
    /// This host's list under `packages/<host>/<source>.txt`.
    pub tracked: Vec<String>,
    pub installed: Vec<String>,
    pub drift: Drift,
}

/// This host's drift, every source, in display order.
///
/// One home for the computation because `status` and `sync` have to agree
/// about what "missing" means down to the package: status reports the counts
/// and sync turns the very same sets into commands, and two copies of the
/// pairing (tracked list ← store, installed list ← this machine) would be two
/// places for that pairing to drift apart.
///
/// Always *this* host on both halves. Comparing another host's tracked list
/// against this machine's reality is a different question ("what would I need
/// to look like slab?"), and giving it the same name as this one is how the
/// two get confused.
pub fn host_drift(ctx: &Ctx) -> Result<Vec<SourceDrift>, String> {
    Source::ALL
        .iter()
        .map(|&source| {
            let tracked = ctx.tracked(&ctx.host, source);
            let installed = installed(source)?;
            let d = drift(&tracked, &installed);
            Ok(SourceDrift {
                source,
                tracked,
                installed,
                drift: d,
            })
        })
        .collect()
}
