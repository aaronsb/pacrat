# pacrat

![pacrat demo](docs/demo/pacrat.gif)

Store-backed package curation for Arch — browse the repos and the AUR, vendor
what you trust into your dotfiles store, collect gradings from any analyzer
that speaks the contract, and serve every machine from a pacman repo you
control, so plain `yay` just works and can't bypass curation.

pacrat is the little chef: it reads every PKGBUILD, grades what changed, and
plans the moves — but it can't operate in the human world. Your hands are on
the pans. Installs print as commands for you to run, holds stay held until
you decide, and the one door past a BLOCK requires you to write down why.

The recording above is the whole loop in about a minute — a real `makepkg`,
a real pacman repo, and a grader that gets a vote. `make demo` re-records it;
everything in it happens in a throwaway store under `/tmp` and never touches
yours.

- **Design:** `docs/architecture/ADR-001-store-backed-package-curation.md` —
  Accepted, with the reasoning for every rule below
- **Grading contract:** `docs/grading-contract.md` — the `pacrat-grade/v1`
  spec, for anyone writing a grader
- **TUI mockup (rev 3):** `docs/design/mockup-rev3.html` — open in a browser
- **Mascot:** `art/petit-chef.html` — the 38×32 block-character grid the
  about screen renders as half-blocks

## The custody ladder

Every package is on exactly one rung, and the rung is what every screen and
verb organizes around:

    unmanaged → tracked → vendored → maintained

`pacrat add` adopts an installed package into your per-host manifest
(`--all-installed` for the first run). `pacrat vendor` takes custody: the
PKGBUILD tree is committed to the store at a commit you reviewed, and from
then on updates arrive only through the review gate. `pacrat build` turns
reviewed trees into packages in your local `[dotfiles-aur]` repo — after
which pacman treats them as repo packages and stops consulting the AUR.
A held update is simply never built; hosts keep the last approved version
with zero pinning. Maintained packages are yours: `pacrat push` publishes
them back upstream.

## Quickstart

    pacrat setup --apply      # first-run questions, then the system steps —
                              # every sudo command shown and confirmed, one y/n each
    pacrat add --all-installed
    pacrat vendor <pkg>       # review the PKGBUILD, take custody
    pacrat build <pkg>        # serve it from [dotfiles-aur]
    pacrat update             # the whole loop, whenever you like

`pacrat setup` is first for a reason: until this host is on the serving
model — repo section in pacman.conf, repo database, guard hooks — every
other verb answers with one line naming what is missing and exits 1. A fresh
machine's two working commands are `pacrat about` and `pacrat setup`, which
is exactly the pair it needs. The interview at the top of `setup` writes
`~/.config/pacrat/config.toml` for you; change it later with `pacrat config
list / get / set` — the file stays hand-editable, but nothing requires it.

`pacrat status`, `search`, `info`, `updates`, `hosts` and `sync` are
read-only; `sync` prints the exact commands that would close the gap between
this host and the store, and `sync --run` walks them at the terminal, one
confirm per command — sudo authenticates you itself, and there is no
`--yes`. Bare `pacrat` opens the TUI when the config says `default_ui =
"tui"`.

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

A grader is any program that prints a `pacrat-grade/v1` report on stdout.
pacrat tells it the subject through the environment — `PACRAT_PACKAGE`,
`PACRAT_TREE`, `PACRAT_COMMIT` — and `cmd` can be one line of shell, which
covers both kinds of tool there are: one built for pacrat, and one that has
never heard of it:

    [[graders]]
    name = "yay-friend"
    cmd = "yay-friend grade"
    timeout_s = 600

    [[graders]]
    name = "sometool"
    cmd = "sometool --json \"$PACRAT_TREE\" | jq '{contract: \"pacrat-grade/v1\", …}'"
    timeout_s = 300
    scale = { min = 0, max = 4 }

Nothing is ever substituted into a string cmd — it reaches `sh` exactly as
written, so its only author is the config's owner. The argv form remains as
the exec-exact alternative: no shell anywhere, `{package}`, `{tree}` and
`{commit}` substituted per element, each value staying exactly one argument:

    [[graders]]
    name = "my-grader"
    cmd = ["/path/to/grader",
           "--package", "{package}", "--tree", "{tree}", "--commit", "{commit}"]
    timeout_s = 300
    scale = { min = 0, max = 4 }

Either way the grader returns a number on a scale it declares;
PROCEED/WARN/BLOCK is pacrat's, derived from that number by the host's own
thresholds. A grader that fails, times out, or answers about the wrong
subject is UNGRADED, which holds — failure is never PROCEED.

**`docs/grading-contract.md` is the spec** — tool-neutral, and everything you
need to write a grader. pacrat holds no knowledge of any particular analyzer;
adapters for tools that do not speak the contract yet live in
`contrib/graders/`, on their own side of the boundary.

`contrib/graders/yay-friend-grade` is the first of those, translating
[yay-friend][yf]'s analysis cache — already keyed by AUR commit hash, so only
the shape has to change. It needs `jq`, wants `timeout_s = 600` because a
miss calls a model, and refuses rather than grade the wrong commit when AUR
HEAD has moved. `pacrat setup` probes the installed yay-friend for the
native `grade` subcommand first and registers `yay-friend grade` when it
answers; otherwise it offers the adapter when it finds yay-friend and jq on
PATH. Its own tests are `contrib/graders/test-yay-friend-grade.sh`;
pacrat's side is tested against a generic fake grader in
`crates/pacrat-cli/tests/grader_contract.rs`.

[yf]: https://github.com/aaronsb/yay-friend

## Layout

    crates/pacrat-core   pure model — the custody ladder, verdicts, ledgers
    crates/pacrat-cli    the `pacrat` binary — clap CLI + ratatui TUI
    contrib/graders      adapters from other tools to pacrat-grade/v1
    docs/architecture    ADRs
    docs/design          mockups
    docs/demo            the recording above, and the script that re-makes it
    art                  petit chef

Nested in the dotfiles store like dotfiles-tui: its own repo, gitignored by
the store. `make demo` re-records the gif; everything it shows runs in a
sandbox, never against your real store.
