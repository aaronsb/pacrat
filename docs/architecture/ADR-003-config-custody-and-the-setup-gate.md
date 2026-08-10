# ADR-003: config custody, the setup gate, and the sudo flow

**Status:** Accepted (2026-08-10 — the three boundaries below were decided by
Aaron; the sudo flow was his addition in the same conversation;
implementation follows this document)
**Date:** 2026-08-10
**Extends:** ADR-001 (the serving model, the guard); **amends** its
"pacrat never elevates" rule — see "The sudo flow"

## Context

The first real host setup (north) surfaced the gap: step 2 of the procedure
was "hand-write `~/.config/pacrat/config.toml`". The config module's stated
posture — *every field has a default, a missing file is a valid config* —
was written for forward compatibility, but it also means an entirely
unconfigured pacrat will happily act: `grade` runs with nobody's graders and
answers UNGRADED, `build` publishes into a repo pacman has never heard of,
and the guard that makes the serving model safe may not be installed at all.
A tool whose whole argument is "curation you cannot accidentally go around"
should not itself run half-assembled.

Two asks, decided together because they are one posture:

1. Users should not have to edit the TOML — pacrat owns its config model
   and can be configured by invoking pacrat.
2. An un-set-up pacrat refuses to run anything else, so it cannot mess
   things up.

## Decision

### The gate is the full serving model

pacrat is *set up* on a host when `setup`'s system state is complete: the
`[repo]` section present in `/etc/pacman.conf`, the repo directory and its
database existing, and both halves of the PreTransaction guard installed.
This is the strictest of the candidate markers, chosen deliberately:
nothing runs until the host is actually on the serving model, sudo steps
included. The stamped config file the interview writes is a product of
setup, not the gate — the gate asks about the system, because the system is
what makes acting safe.

### Everything but `about` and `setup` refuses

One clean rule, no boundary to explain: `pacrat about`, `pacrat setup`,
`--help` and `--version` work on any machine; every other verb — read-only
ones and the TUI included — answers with one line naming what is missing
and pointing at `pacrat setup`, exit 1. A fresh machine's first two
commands are exactly the two that work.

### Config is managed by invoking pacrat

- **`pacrat setup` grows a first-run interview** (terminal only; every
  question has a flag so scripts skip the interview): UI preference,
  update mode, and grader registration — it detects known analyzers
  (yay-friend, plus `jq` for its adapter) and offers to register the
  contrib adapter with absolute paths. It writes `config.toml` itself,
  stamped as pacrat-written, then continues into the system-side flow —
  `--apply` does the user-doable steps and walks the root-owned ones
  through the sudo flow below, so a full setup is one command on one
  terminal.
- **`pacrat config list / get <key> / set <key> <value>`** handles changes
  afterward. `set` covers the scalar keys (`default_ui`, `update_mode`,
  `thresholds.warn_at`, `thresholds.block_at`, `repo.name`, `repo.path`,
  `repo.server`) and validates by round-tripping the same parser the load
  path uses — a value `set` accepts is a value pacrat can read back.
  Graders are structured; the interview registers them, `list` shows
  them, and the file remains hand-editable for the rest — possible, never
  required.

### The sudo flow (amending ADR-001's "never elevates")

Added by Aaron in the same conversation: anything that requires sudo
should have a flow through pacrat, not a copy-paste intermission. The
rule sharpens rather than falls: **pacrat never elevates unasked, and
never authenticates itself.** One shared primitive:

1. the exact argv is shown, `sudo` included — the always-print rule was
   never negotiable;
2. a y/n is asked on a real terminal — there is no `--yes` past this
   question, because an elevated command answered by a pipe is precisely
   the accident the question exists to catch;
3. pacrat runs `sudo <argv>` with the terminal attached, so sudo does its
   own prompting and caching — pacrat never reads, stores, or forwards a
   credential;
4. no terminal, no flow: headless runs get the printed commands exactly
   as before, and a timer cannot elevate.

Where it lands: `setup --apply` walks its root-owned steps through it,
and `sync` gains `--run` — the plan stays the default output, `--run`
walks the plan's commands with a per-command confirm. Both surfaces use
the one primitive, so "what has pacrat run as root" has a one-code-path
answer.

### The escape hatch, announced

The demo sandbox and the integration tests run acting verbs on machines
that are deliberately not set up, and always will. `PACRAT_SETUP_GATE=off`
skips the gate and announces itself on every run it affects — the same
posture as the guard's `PACRAT_BYPASS`: this gate is accident resistance
for a human, not access control, and a script that states its intent in
the environment has stated it.

## Consequences

- The "missing file is a valid config" posture retires for *acting* — it
  survives as "unset keys default", which is what it was really for.
  Unknown keys stay ignored (fleet hosts on different pacrat versions).
- `Ctx::resolve` stays cheap; the gate check reads three paths and one
  file section and runs before dispatch, not inside verbs.
- The demo's `env -i` wrapper sets `PACRAT_SETUP_GATE=off`; the test
  fixtures gain the same line. CI needs no new privileges.
- The README quickstart reorders: `pacrat setup` is the first command on a
  new machine, and the procedure that prompted this ADR loses its
  hand-edit step.
- `setup` remains the only verb that can make the gate open, which keeps
  the answer to "why won't it run" one word long.
- ADR-001's "pacrat never elevates (prints sudo commands)" reads with this
  ADR's correction from now on: printing remains the headless truth and
  the default for `sync`; on a terminal, the confirmed sudo flow is the
  supported path. The guard's threat model is unchanged — pacrat still
  holds no credentials and runs nothing as root that was not shown and
  confirmed first.

## Amendment (2026-08-10): the guided flow is the default

Ratified by Aaron after the first run of the shipped `setup`: bare
`pacrat setup` printed the full homework — the pacman.conf section, both
guard files inline, four sudo commands to copy-paste — at a terminal that
could simply have asked. His reading: *good software does not hand the
user a huge, error-prone task list; the software should handle it.* He is
right, and the flags invert:

- **On a terminal, bare `pacrat setup` IS the guided flow** — interview,
  user-owned steps, then each root-owned step through the sudo flow. The
  `--apply` flag becomes a no-op synonym and stays accepted.
- **`--print` asks for the wall**: the full sections, file contents, and
  copy-paste commands, exactly today's output — the audit and
  documentation mode.
- **Headless keeps printing**, as ADR-003 already requires: no terminal,
  no questions, the wall is the only honest output.
- Per-step display slims to what a confirm needs: the command, the
  destination, and the staged file's path (readable before answering).
  The full file contents belong to `--print`; a hundred inline lines of
  guard script are documentation, not a question.

The declined path stays graceful: any step answered n is reported at the
end with the exact command to run later, so a partial setup degrades into
a short list instead of a wall.
