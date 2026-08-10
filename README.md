# pacrat

Store-backed package curation for Arch — browse the repos and the AUR, vendor
what you trust into the dotfiles store, collect gradings from any configured
analyzer (yay-friend first), and serve every machine from a pacman repo you
control, so plain `yay` just works and can't bypass curation.

The rat curates the pantry; the little chef does the tasting.

- **Design:** `docs/architecture/ADR-001-store-backed-package-curation.md`
- **TUI mockup (rev 3):** `docs/design/mockup-rev3.html` — open in a browser
- **Mascot:** `art/petit-chef.html` — the 38×32 block-character grid the
  about screen renders as half-blocks

Status: scaffold. `pacrat-core` holds the first model types (custody ladder,
grade→verdict thresholds); `pacrat-cli` holds the clap surface matching the
mockup's CLI-parity table. Nothing is implemented yet — ADR-001's open
questions come first.

## Layout

    crates/pacrat-core   pure model — no I/O, no deps
    crates/pacrat-cli    the `pacrat` binary (clap now, ratatui next)
    docs/architecture    ADRs
    docs/design          mockups
    art                  petit chef

Nested in the dotfiles store like dotfiles-tui: its own repo, gitignored by
the store.
