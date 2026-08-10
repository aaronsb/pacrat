#!/usr/bin/env bash
# The demo driver: pacrat's whole loop — vendor, grade, build, serve, update
# — played once through, at reading speed, for asciinema to record.
#
# EVERYTHING HAPPENS IN A SANDBOX. The store is a throwaway tree under
# /tmp, the "AUR" is a local bare-less git repo reached over file://, the
# grader is a twelve-line stub, and the pacman repo is a directory that gets
# deleted on the way out. Every pacrat invocation goes through the `pacrat`
# function below, which runs the binary under `env -i` with DOTFILES_DIR and
# the XDG paths pointed at the sandbox — so the operator's real store cannot
# be reached even by accident, and an inherited DOTFILES_DIR cannot answer
# for the fixture. Nothing here needs root and nothing here calls sudo.
#
# Invoked by `make demo` inside `asciinema rec -c …`. Runnable on its own if
# you want to watch it without recording.
set -uo pipefail

BIN=${PACRAT_BIN:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/target/release/pacrat}
[ -x "$BIN" ] || { echo "no pacrat binary at $BIN — run: cargo build --release" >&2; exit 1; }

# A fixed short path, because it is on screen in every `store` line and a
# mktemp suffix would be twelve characters of noise the viewer has to read
# past. Guarded rather than trusted: this script only ever deletes a real
# directory it put here itself.
SB=/tmp/pacrat-demo
case "$SB" in /tmp/pacrat-demo) ;; *) echo "refusing: sandbox is not /tmp/pacrat-demo" >&2; exit 1 ;; esac
[ -L "$SB" ] && { echo "refusing: $SB is a symlink" >&2; exit 1; }
rm -rf "$SB"
trap 'rm -rf "$SB"' EXIT

# pacrat reads the host from /etc/hostname, so the sandbox has to file its
# tracked lists under the same name or the store would have no lists for
# "this host" and the drift table would be a note instead of a table.
HOST=$(tr -d '[:space:]' < /etc/hostname 2>/dev/null)
[ -n "$HOST" ] || HOST=unknown

# ── pacing ─────────────────────────────────────────────────────────────────
# Typed at a human's speed with a little jitter, because a line that appears
# all at once reads as output rather than as something someone asked for.
# ASCII on purpose: a chevron (U+276F) falls off the end of agg's font
# fallback chain and renders as a clipped sliver in column 0 of the gif.
PROMPT=$'\033[38;5;213m$\033[0m '

type_out() {
  local s=$1 i
  for (( i = 0; i < ${#s}; i++ )); do
    printf '%s' "${s:i:1}"
    sleep "0.0$(( 2 + RANDOM % 4 ))"
  done
  printf '\n'
}

# A narration line, typed as a shell comment — the demo explaining itself
# without a voiceover.
say() {
  printf '%b' "$PROMPT\033[2m"
  type_out "# $1"
  printf '\033[0m'
  sleep 0.5
}

# Open a new act on a clean screen. Two of these verbs print more than a
# thirty-row window holds — a real makepkg transcript is most of it — so each
# act gets the window to itself rather than the previous act's tail. The
# scroll inside an act is left alone: watching a build scroll is the honest
# picture of what `build` costs.
act() { clear; sleep 0.4; say "$1"; }

# Echo the command as though it were typed, run it, then hold long enough to
# read what it printed. BEAT is the hold; callers set it per act.
run() {
  printf '%b' "$PROMPT"
  type_out "$1"
  sleep 0.3
  eval "$1"
  local rc=$?
  sleep "${BEAT:-2.2}"
  echo
  return $rc
}

# Every pacrat run, sandboxed. `env -i` and not an export: the point is that
# nothing of the operator's environment reaches the binary.
pacrat() {
  command env -i \
    PATH="$SB/bin:/usr/bin:/bin" \
    HOME="$SB" \
    TERM="${TERM:-xterm-256color}" \
    LANG=C.UTF-8 \
    DOTFILES_DIR="$SB/store" \
    XDG_CONFIG_HOME="$SB/config" \
    XDG_STATE_HOME="$SB/state" \
    "$BIN" "$@"
}

# git in the demo upstream, with an identity of its own so the operator's
# ~/.gitconfig is neither needed nor consulted.
ugit() {
  command env -i PATH=/usr/bin:/bin HOME="$SB" \
    GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null \
    GIT_AUTHOR_NAME='pacrat demo' GIT_AUTHOR_EMAIL='demo@localhost' \
    GIT_COMMITTER_NAME='pacrat demo' GIT_COMMITTER_EMAIL='demo@localhost' \
    git -C "$SB/upstream" "$@" >/dev/null
}

# ── the sandbox ────────────────────────────────────────────────────────────
mkdir -p "$SB"/{bin,upstream,repo,state,config/pacrat,store/packages/"$HOST"}

# What this host has installed, as far as the demo is concerned. pacrat asks
# `pacman -Qqen` / `-Qqem`; these stubs answer for the fixture and hand every
# other invocation to the real pacman, which makepkg still needs for its
# dependency checks. Read-only either way: no stub ever installs anything.
cat > "$SB/bin/pacman" <<EOF
#!/bin/sh
case "\$*" in
  '-Qqen') exec cat "$SB/live/native.txt" ;;
  '-Qqem') exec cat "$SB/live/aur.txt" ;;
esac
exec /usr/bin/pacman "\$@"
EOF
cat > "$SB/bin/flatpak" <<EOF
#!/bin/sh
case "\$*" in
  'list --app --columns=application') exec cat "$SB/live/flatpak.txt" ;;
esac
exit 0
EOF
chmod 755 "$SB/bin/pacman" "$SB/bin/flatpak"

mkdir -p "$SB/live"
printf 'base\ngit\nneovim\ntmux\nzsh\n'          > "$SB/live/native.txt"
printf ''                                        > "$SB/live/aur.txt"
printf ''                                        > "$SB/live/flatpak.txt"

# The store's tracked lists: what this host is *supposed* to have. One native
# package short of reality and one AUR package short, so the opening drift
# table is small enough to read and the closing `sync` has something to plan.
printf 'base\ngit\nneovim\nripgrep\ntmux\nzsh\n' > "$SB/store/packages/$HOST/native.txt"
printf 'demo-pkg\n'                              > "$SB/store/packages/$HOST/aur.txt"
printf ''                                        > "$SB/store/packages/$HOST/flatpak.txt"

# The upstream: a real git repository with a real, buildable PKGBUILD. Local
# source, checksummed, no network in build() — which is exactly what the stub
# grader is about to notice.
cat > "$SB/upstream/pantry.sh" <<'EOF'
#!/usr/bin/env bash
# Reports on the pantry. The rat curates it; the chef does the tasting.
printf 'pantry: %d jars, %d of them labelled\n' 12 9
EOF
write_pkgbuild() {
  local ver=$1 sum
  sum=$(sha256sum "$SB/upstream/pantry.sh" | cut -d' ' -f1)
  cat > "$SB/upstream/PKGBUILD" <<EOF
# Maintainer: pacrat demo <demo@localhost>
pkgname=demo-pkg
pkgver=$ver
pkgrel=1
pkgdesc='Reports on the pantry, badly'
arch=('any')
url='https://example.invalid/demo-pkg'
license=('MIT')
depends=('bash')
options=(!debug)
source=('pantry.sh')
sha256sums=('$sum')

package() {
  install -Dm755 "\$srcdir/pantry.sh" "\$pkgdir/usr/bin/pantry"
}
EOF
}
write_pkgbuild 1.4.0
ugit init -q -b main .
ugit add -A
ugit commit -q -m 'demo-pkg 1.4.0'

# The grader: any program that prints a pacrat-grade/v1 report about the
# subject it was handed. Twelve lines is enough to be a real one.
cat > "$SB/grader.sh" <<'EOF'
#!/bin/sh
# A stand-in analyzer. The real ones read the tree; this one has already
# made up its mind, so the demo does not wait on a model.
sleep 1
cat <<REPORT
{ "contract": "pacrat-grade/v1", "grader": "petit-chef",
  "subject": { "package": "$2", "commit": "$6" },
  "grade": 0, "scale": { "min": 0, "max": 4 },
  "findings": [ { "level": 0,
                  "title": "every source is checksum-pinned; build() never reaches the network",
                  "span": "PKGBUILD:11" } ],
  "meta": { "provider": "demo" } }
REPORT
EOF
chmod 755 "$SB/grader.sh"

cat > "$SB/config/pacrat/config.toml" <<EOF
[repo]
name = "dotfiles-aur"
path = "$SB/repo"

[[graders]]
name = "petit-chef"
cmd = ["$SB/grader.sh",
       "--package", "{package}", "--tree", "{tree}", "--commit", "{commit}"]
timeout_s = 60
EOF

# ── the demo ───────────────────────────────────────────────────────────────
act "a throwaway store, a local 'AUR', and one package to curate"
BEAT=4.5 run "pacrat status"

act "vendor it: the build tree comes into the store, under review"
BEAT=4.0 run "pacrat vendor demo-pkg --upstream file://$SB/upstream --yes"

act "grade it: any program that speaks pacrat-grade/v1 gets a vote"
BEAT=4.5 run "pacrat grade demo-pkg"

act "build it: real makepkg, into a pacman repo this host serves"
BEAT=4.0 run "pacrat build demo-pkg"

# Upstream moves. This is the event the timer exists for.
write_pkgbuild 1.5.0
ugit add -A
ugit commit -q -m 'demo-pkg 1.5.0'

act "upstream just published 1.5.0 — this is what the timer runs"
BEAT=3.0 run "pacrat update --mode auto"
say "exit $? — detected, graded, adopted, built, served"
sleep 2.5

act "and the host itself? sync plans it, and runs none of it"
BEAT=5.0 run "pacrat sync"

sleep 1.5
