# pacrat

Store-backed package curation for Arch — browse the repos and the AUR, vendor
what you trust into the dotfiles store, collect gradings from any configured
analyzer (yay-friend first), and serve every machine from a pacman repo you
control, so plain `yay` just works and can't bypass curation.

The rat curates the pantry; the little chef does the tasting.

![pacrat: status, vendor, grade, build, then the update loop closing itself](docs/demo/pacrat.gif)

The whole loop in about a minute — a real `makepkg`, a real pacman repo, and a
grader that gets a vote. `make demo` re-records it; everything in it happens in
a throwaway store under `/tmp` and never touches yours.

- **Design:** `docs/architecture/ADR-001-store-backed-package-curation.md`
- **Grading contract:** `docs/grading-contract.md` — the `pacrat-grade/v1`
  spec, for anyone writing a grader
- **TUI mockup (rev 3):** `docs/design/mockup-rev3.html` — open in a browser
- **Mascot:** `art/petit-chef.html` — the 38×32 block-character grid the
  about screen renders as half-blocks

Status: the CLI works. `status`, `hosts`, `add`, `setup`, `vendor`, `search`,
`info`, `build`, `updates`, `grade`, `review`/`adopt-update`/`reject`, `sync`,
`push`, the one-shot loop (`update`) and the decision ledger (`decisions`) are
implemented and tested, and the TUI shell is up. The mockup shows where it's
all headed.

## The update loop

`pacrat update` is the whole loop in one command — detect, grade, decide,
build, served — and the entry point a systemd timer uses. `--mode` says how
much of it runs without being asked:

| verdict | `auto` | `semi` (default) | `manual` |
|---------|--------|------------------|----------|
| PROCEED | adopt  | adopt            | ask      |
| WARN    | hold   | ask              | ask      |
| BLOCK   | hold   | hold             | hold     |
| UNGRADED| hold   | hold             | ask      |

BLOCK always holds and this verb has no override — the one door past it is
`pacrat adopt-update <pkg> --commit <c> --override-block --reason "…"`, whose
friction is writing the justification and whose price is a permanent entry in
`aur/decisions.toml`, synced to the fleet and listed by `pacrat decisions`.
A prompt is answered no unless a human at a terminal types `y`; a pipe is not
a human, because one `y` would answer a whole run.

Exits 0 when there was nothing to do or everything pending was adopted and
built, 10 when anything was held, and 1 when the run could not do its job.
`--format json` puts one object on stdout and every human line on stderr.

## Publishing

`pacrat push <package>` publishes a **maintained** package's store tree to its
upstream — clone, mirror the store over it, regenerate `.SRCINFO`, show the
diff, ask, commit, push. Never a force-push, and never a `vendored` package:
that role is pull-only.

What goes out is exactly the store's tree plus a freshly generated `.SRCINFO`
— stated to git rather than discovered by it, so an ignore rule cannot
silently drop a patch from the publish and a side effect of reading the
PKGBUILD cannot silently join it.

Two things it will not do quietly. If the upstream is the AUR it asks first
whether the AUR is accepting publishes (`ssh aur@aur.archlinux.org help`,
read-only) and, when the answer is no, records the errand in a publish queue
instead — `pacrat push` with no arguments probes again and works the queue,
and `pacrat status` carries a line while anything is waiting. A blocked probe
says *which* kind of blocked: a refused ssh key is your setup, not an outage.
And if a checksum has changed for a source of an **already-published
version**, it stops: an immutable tag whose tarball changed is an incident,
not a re-sum. It also says how many sources it was able to compare, because a
silent alarm and a blind one look identical otherwise.

Exit codes: 0 published (or already current), 10 queued/blocked/declined, 1
the alarm or a failure.

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
    docs/demo            the recording above, and the script that makes it
    art                  petit chef

Nested in the dotfiles store like dotfiles-tui: its own repo, gitignored by
the store.
