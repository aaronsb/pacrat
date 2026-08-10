//! `pacrat about` — the petit chef, and the facts of the build.
//!
//! The one verb with nothing to ask a store or a host: everything it prints
//! was baked in by cargo, plus the mascot from [`crate::art`]. The art is
//! skipped for a pipe and under NO_COLOR; the facts print regardless.

use std::io::IsTerminal;

use crate::art;
use crate::say;

pub fn run() -> Result<(), String> {
    if art::wanted(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
    ) {
        let depth = art::depth(std::env::var("COLORTERM").ok().as_deref());
        for line in art::ansi_lines(depth) {
            // Escape codes and all, deliberately not through `out::visible`:
            // that filter is for text pacrat did not author — PKGBUILDs,
            // grader output — and this is pacrat's own compiled-in art,
            // whose colour codes are the point. `art::wanted` above already
            // keeps them away from pipes.
            say!("{line}");
        }
        say!();
    }
    say!("name      pacrat · petit chef");
    say!("version   {}", env!("CARGO_PKG_VERSION"));
    say!("about     {}", env!("CARGO_PKG_DESCRIPTION"));
    say!("source    {}", env!("CARGO_PKG_REPOSITORY"));
    Ok(())
}
