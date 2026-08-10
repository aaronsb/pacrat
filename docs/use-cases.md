# Use cases — the journeys the UX is derived from

**These use cases guide the UX flow.** Each one states the operator's
situation, the experience the surfaces owe them, how the journey runs
today (CLI and TUI), what gets recorded, and — honestly — where the
built surface still falls short of the journey. When a screen, a key, or
a next-step line is being designed, this document is what it answers to;
the overview screen's "what now?" (issue #28) is these journeys turned
into a pressable to-do.

The six: **enrollment** (a host joins), **steady state** (the loop),
**refinement** (removing items), **remediation** (re-scanning),
**identifying a threat** (a scan found one), **uninstallation**
(something better came along).

---

## 1 · Enrollment — a host joins pacrat

**Situation.** A machine with AUR packages installed the old way — yay,
squint, hope — and none of it in anybody's manifest. Or a fresh install
about to be populated.

**The experience owed.** One bulk act, then done: *select all the
untracked AUR packages and move them to be tracked in pacrat*. After
enrollment, tracking is individual — a package at a time, as they enter
the machine's life. Enrollment is the only moment "all of them" is the
right granularity.

**The journey today.**

1. `pacrat setup` — the guided first run: interview (UI, update mode,
   grader registration — it probes for native `yay-friend grade`), then
   each root-owned step shown and confirmed through sudo. `sudo pacrat
   setup` is the maintenance-mode variant. The gate keeps every other
   verb refused until this is done.
2. Bulk track: `pacrat add --all-installed`, or on the hosts screen —
   the `+` rows are the enrollment surface — mark and `A`.
3. Commit the store; other hosts see the manifest on their next pull.

**Recorded:** the host's tracked lists in the store, git-versioned.

**Gaps.** `add --all-installed` takes everything installed; enrollment
wants it scoped to AUR (`--source aur`) so native stays deliberate. The
selection language has select-all but not select-a-class ("all unmanaged
AUR rows"). The overview does not yet offer enrollment as its first-run
next step. All three: issue #28.

---

## 2 · Steady state — the loop

**Situation.** Enrolled, curated, serving. Upstreams move; the operator
wants updates that were read by *something* before they were installed
by *anything*.

**The experience owed.** The loop runs itself up to the gates and never
past them: detected → graded → decided → built → served. The operator's
attention is spent only where a gate holds. The TUI's updates screen is
the whole decision on one screen — diff beside gradings, because that is
the actual decision.

**The journey today.**

- `pacrat update` (or a timer running `--mode auto`): probes every
  ledger row, grades staged candidates (cache-keyed by commit, so
  unmoved candidates cost nothing), adopts per the mode table, builds
  into `[dotfiles-aur]`, serves. PROCEED adopts; WARN asks (semi) or
  holds (auto); BLOCK and UNGRADED always hold. A held update is simply
  never built — hosts keep the last approved version, zero pinning.
- TUI: `[3]` lists drift; `enter` fetches, stages, diffs, and shows the
  gradings on file for those bytes; `[1]` overview surfaces what needs
  attention; `[4]` hosts shows fleet manifest vs this machine.
- New AUR wants: `pacrat vendor <pkg>` (PKGBUILD shown in full, custody
  taken at a pinned commit) → `grade` → `build` → it is a repo package
  from then on. Individual, per enrollment's rule.
- Commit the store when it changes; `pacrat sync` prints what would
  converge this host, `sync --run` walks it with confirms.

**Recorded:** `aur/sources.toml` (reviewed commits), grading cache
(per-bytes verdicts), the pacman repo itself.

**Gaps.** Vendor/grade/build complete only in the CLI until ADR-006's
parity work lands (`v` will suspend into the real vendor review; `s`
walks the sync plan; `a`/`x` adopt and reject from the updates screen).
No timer unit ships yet (packaging, #23).

---

## 3 · Refinement — removing items

**Situation.** The manifest has accumulated: packages nobody uses,
tracked on hosts that no longer want them. The operator prunes by hand
with pacman — pacrat is not the uninstaller — and the manifest must
follow reality.

**The experience owed.** Removal is *drift reconciliation*, not an
imperative act (ADR-002 amendment): pacrat only tracks the delta, and
"tracked here, not installed here" **is** the removal surface. The
operator uninstalls with the tool that owns the machine (pacman), and
pacrat presents the drift with a resolving act in each direction —
reinstall (manifest wins) or untrack (reality wins).

**The journey today.**

- Uninstall by hand: `sudo pacman -Rns <pkg>` — pacrat plays no part.
- The hosts screen's `✗` rows show the drift, each with its two-reading
  explanation. Mark the rows that were deliberate, `x` — the naming
  confirm, then the manifest converges through `untrack` (add's exact
  inverse). Or `pacrat untrack <pkg>…` directly.
- The other direction: `pacrat sync` reinstalls what the manifest still
  claims.
- Demotion works too: untrack something still installed and `sync
  --prune`'s printed plan surfaces the uninstall.

**Recorded:** the tracked lists shrink; git history carries when and
what.

**Gaps.** None structural — this journey landed whole with ADR-002 as
amended. The `✗`-class select (mark all drifted rows at once) shares
issue #28's class-select ask.

---

## 4 · Remediation — re-scanning packages

**Situation.** The graders got smarter — a new yay-friend version, a new
provider, new threat intelligence — and the gradings on file were made
by the old eyes. Or a grading looks wrong and the operator wants a
second opinion on the same bytes.

**The experience owed.** Re-asking is cheap to express and honest about
cost: `--refresh` means "ask again", not "stop remembering". Fresh
answers overwrite the cache; the verdict recomputes from the new worst.

**The journey today.**

- One package: `pacrat grade <pkg> --refresh` — skips cache reads, runs
  every configured grader against the store tree, files fresh gradings.
- The verdict line names the thresholds, because the same grade is a
  different verdict on another host's config.
- A re-scan that comes back worse flows into journey 5.

**Recorded:** fresh gradings in the cache, keyed to the same tree
digest; nothing about the old grading is kept.

**Gaps.** No fleet-wide sweep (`pacrat grade --all --refresh` does not
exist); remediation after a grader upgrade is one package at a time or a
shell loop. No scheduled re-scan story. Worth an issue when the need is
real rather than speculative.

---

## 5 · Identifying a threat — a scan found one

**Situation.** A grading came back BLOCK. Two very different moments:
the threat is in a **candidate** (caught at the gate, nothing happened
yet), or the threat is in bytes **already adopted and installed** (a
re-scan or smarter grader flagged what previously passed).

**The experience owed.** The gate case is the system working: the hold
is calm, the diff and the findings are on one screen, and every way
forward is explicit. The installed case is an incident: the operator
needs to see blast radius (which hosts), act with pacman, and leave a
record.

**The journey today — candidate (the built path).**

- The update loop holds: BLOCK always holds, this verb has no override.
  The row says so; `pacrat review <pkg>` / the updates screen's `enter`
  shows the diff and the findings that earned the verdict.
- Three ways forward, all deliberate: fix what the graders flagged
  upstream and re-grade; `pacrat reject <pkg> --note "…"` to refuse the
  commit on the record; or accept the risk on the record —
  `adopt-update --commit <c> --override-block --reason "…"`, which
  writes a permanent, fleet-synced entry in `aur/decisions.toml`
  (`pacrat decisions` lists every one). The TUI's door is `o`: the
  childlock hold, then the reason.
- A held update is never built. Hosts keep the last approved version
  with no pinning machinery at all.

**The journey today — installed (the gap).**

- What exists: `grade --refresh` records the BLOCK against the store
  tree; the exit-10 chain stops `build`; the hosts screen shows which
  hosts track the package. Response is manual: `sudo pacman -Rns` on
  affected hosts, untrack or hold, and prose in the store's commit
  message.
- What does not exist: a quarantine state, a "which hosts have these
  exact bytes installed" answer (pacrat knows installed-here only —
  ADR-001's self-only rule — so fleet blast radius is each host's own
  question), and a decisions-ledger entry type for "we pulled this".

**Recorded:** the BLOCK grading (bytes-keyed), any rejection with its
note, any override with its reason — permanently.

**Gaps.** The installed-threat response deserves its own design pass
before it is ever needed in anger: likely a `pacrat advisory`/quarantine
notion recorded in the store so every host's `sync`/overview surfaces
it. That is a future ADR, and this use case is its requirements
statement.

---

## 6 · Uninstallation — something better came along

**Situation.** A curated package is done: upstream died, a better tool
shipped, or the official repos absorbed it. This is retirement, not
refinement — the package leaves not just the manifest but curation
itself.

**The experience owed.** Retirement unwinds enrollment's whole ladder in
one deliberate act, with the same honesty about what is destroyed:
manifest entries (every host), the vendored tree, the ledger row, the
grading cache, the built artifacts in the repo.

**The journey today (manual — the verb does not exist).**

1. Untrack everywhere it is tracked (each host's list — editable
   centrally in the store, applied by each host's own `sync --prune`).
2. Remove the vendored tree `aur/packages/<pkg>/` and the
   `[packages.<pkg>]` entry in `aur/sources.toml`.
3. `repo-remove` the built package from `[dotfiles-aur]`; let hosts
   `sync --prune`.
4. Commit the store with the why — the commit message is the retirement
   record.

**Recorded:** only what git preserves. Decisions made along the way
(especially "we replaced X with Y because…") live in commit messages.

**Gaps.** ADR-002's amendment explicitly deferred curated retirement to
its own ADR; this journey is that ADR's requirements. A `pacrat retire
<pkg>` should walk the four steps with the same show-and-confirm
discipline as setup, refuse while other hosts still track the package
(or say what will happen to them), and leave a ledger entry richer than
a commit message. Its system-level sibling is `setup --remove`
(issue #24), which retires the host rather than a package.

---

## The shape these six make

Enrollment is wide, everything after is narrow: one bulk act to arrive,
then a steady loop where attention is spent only at gates. Refinement
and uninstallation are the two exits — one from the manifest, one from
curation — and both are reconciliation-shaped, never proxying pacman.
Remediation and threat-response are the same muscle at two intensities:
re-ask, and act on a worse answer. Every surface decision — which screen
opens first, what the overview says, which keys exist — should be
traceable to one of these six.
