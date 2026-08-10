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

## Graders

A grader is any program that prints a `pacrat-grade/v1` report on stdout when
run with `{package}`, `{tree}` and `{commit}`. pacrat maps the number it
returns to PROCEED/WARN/BLOCK with its own thresholds; a grader that fails,
times out, or answers about the wrong subject is UNGRADED, which holds.

`contrib/graders/yay-friend-grade` adapts [yay-friend][yf] to that contract —
its analysis cache is already keyed by AUR commit hash, so only the output
shape has to change. It needs `jq`.

    [[graders]]
    name = "yay-friend"
    cmd = ["/path/to/contrib/graders/yay-friend-grade",
           "--package", "{package}", "--tree", "{tree}", "--commit", "{commit}"]
    timeout_s = 600
    scale = { min = 0, max = 4 }

Pinning `scale` is worth the line: pacrat rescales a foreign scale onto its
own 0-4, so a grader that silently moved to 0-100 would keep producing
plausible verdicts with every grade four times less alarming.

The adapter grades from yay-friend's cache. On a miss it runs
`yay-friend analyze`, which reads **AUR HEAD** — if HEAD has moved past the
commit pacrat asked about, the adapter refuses rather than file an analysis
of one tree under another tree's name. Grading right after a fetch, which is
what the update loop does, is the path that hits.

`contrib/graders/test-yay-friend-grade.sh` tests the translation against a
fabricated cache and a fake yay-friend; `crates/pacrat-cli/tests/` runs the
same fixture through the real grade engine.

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
