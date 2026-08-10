//! Live-system queries: what is actually installed on this host, per source.
//! All subprocess use in pacrat stays in the CLI crate, and every external
//! command pacrat runs is printable — verbs that call these show the argv.

use std::process::Command;

use pacrat_core::pkg::{normalize, Source};

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
