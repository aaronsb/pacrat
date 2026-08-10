# pacrat

Store-backed package curation for Arch — browse the repos and the AUR, vendor
what you trust into the dotfiles store, collect gradings from any configured
analyzer (yay-friend first), and serve every machine from a pacman repo you
control, so plain `yay` just works and can't bypass curation.

The rat curates the pantry; the little chef does the tasting.

- **Design:** `docs/architecture/ADR-001-store-backed-package-curation.md`
- **Grading contract:** `docs/grading-contract.md` — the `pacrat-grade/v1`
  spec, for anyone writing a grader
- **TUI mockup (rev 3):** `docs/design/mockup-rev3.html` — open in a browser
- **Mascot:** `art/petit-chef.html` — the 38×32 block-character grid the
  about screen renders as half-blocks

Status: scaffold. `pacrat-core` holds the first model types (custody ladder,
grade→verdict thresholds); `pacrat-cli` holds the clap surface matching the
mockup's CLI-parity table. Nothing is implemented yet — ADR-001's open
questions come first.

## Graders

A grader is any program that prints a `pacrat-grade/v1` report on stdout when
run with `{package}`, `{tree}` and `{commit}`. It returns a number on a scale
it declares; PROCEED/WARN/BLOCK is pacrat's, derived from that number by the
host's own thresholds. A grader that fails, times out, or answers about the
wrong subject is UNGRADED, which holds — failure is never PROCEED.

    [[graders]]
    name = "my-grader"
    cmd = ["/path/to/grader",
           "--package", "{package}", "--tree", "{tree}", "--commit", "{commit}"]
    timeout_s = 300
    scale = { min = 0, max = 4 }

**`docs/grading-contract.md` is the spec** — tool-neutral, and everything you
need to write a grader. pacrat holds no knowledge of any particular analyzer;
adapters for tools that do not speak the contract yet live in
`contrib/graders/`, on their own side of the boundary.

`contrib/graders/yay-friend-grade` is the first of those, translating
[yay-friend][yf]'s analysis cache — already keyed by AUR commit hash, so only
the shape has to change. It needs `jq`, wants `timeout_s = 600` because a
miss calls a model, and refuses rather than grade the wrong commit when AUR
HEAD has moved. Its own tests are `contrib/graders/test-yay-friend-grade.sh`;
pacrat's side is tested against a generic fake grader in
`crates/pacrat-cli/tests/grader_contract.rs`.

[yf]: https://github.com/aaronsb/yay-friend

## Layout

    crates/pacrat-core   pure model — no I/O, no deps
    crates/pacrat-cli    the `pacrat` binary (clap now, ratatui next)
    contrib/graders      adapters from other tools to pacrat-grade/v1
    docs/architecture    ADRs
    docs/design          mockups
    art                  petit chef

Nested in the dotfiles store like dotfiles-tui: its own repo, gitignored by
the store.
