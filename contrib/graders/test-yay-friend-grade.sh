#!/usr/bin/env bash
#
# Tests for the yay-friend → pacrat-grade/v1 adapter.
#
# Everything here is fabricated: a fake XDG data home holding a cache entry
# in yay-friend's real shape (captured from a live one), and a fake
# yay-friend on PATH. Nothing calls a provider, nothing touches the store,
# and nothing costs money — which is what makes it runnable in CI, where the
# real yay-friend does not exist.
#
#     ./contrib/graders/test-yay-friend-grade.sh
#
# The end-to-end counterpart — the same fake cache read through pacrat's own
# grade engine — is crates/pacrat-cli/tests/yay_friend_adapter.rs.

set -uo pipefail

here=$(cd -- "$(dirname -- "$0")" && pwd)
adapter="$here/yay-friend-grade"
[ -x "$adapter" ] || {
	echo "not executable: $adapter" >&2
	exit 1
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

pass=0 fail=0
ok() {
	pass=$((pass + 1))
	printf '  ok   %s\n' "$1"
}
no() {
	fail=$((fail + 1))
	printf '  FAIL %s\n' "$1"
	[ $# -gt 1 ] && printf '       %s\n' "$2"
	return 0
}

# --- the fixtures -----------------------------------------------------------

readonly COMMIT=51cec6333515471681ec8aa00943145d420311fa
readonly OTHER=a7c5e8780f8ab4249a1f5d90e9a910ff01c9f99f

cache="$tmp/data/yay-friend/cache"
mkdir -p "$cache/hello" "$tmp/bin" "$tmp/nothing" "$tmp/tree"

# A real cache entry, trimmed: the fields the adapter reads, in the shapes
# yay-friend actually writes them (levels as integers, findings with type /
# entropy / severity / description / line_number).
cat >"$cache/hello/$COMMIT.json" <<EOF
{
  "cache_metadata": {
    "commit_hash": "$COMMIT",
    "package_name": "hello",
    "cached_at": "2025-09-23T14:26:44.421622887-05:00",
    "cache_version": "1.0",
    "yay_friend_version": "1.0.0"
  },
  "analysis": {
    "package_name": "hello",
    "overall_entropy": 1,
    "overall_level": 1,
    "findings": [
      {
        "type": "source_analysis",
        "entropy": 2,
        "severity": 2,
        "description": "Source downloaded from official GNU FTP\n  server",
        "line_number": 12,
        "context": "source=(https://ftp.gnu.org/gnu/hello/\$pkgname-\$pkgver.tar.gz)",
        "suggestion": "Legitimate source location"
      },
      {
        "type": "maintainer_trust",
        "entropy": 1,
        "severity": 1,
        "description": "Package has low popularity but is actively maintained"
      }
    ],
    "summary": "Clean package.",
    "recommendation": "PROCEED",
    "provider": "claude"
  }
}
EOF

# The tree pacrat would hand us. Only .SRCINFO is read, for the version.
printf 'pkgbase = hello\n\tpkgver = 2.12.1\n\tpkgrel = 1\n\npkgname = hello\n' \
	>"$tmp/tree/.SRCINFO"

# A fake yay-friend. `analyze` writes a cache entry for whichever commit
# $FAKE_HEAD names — that is how the miss path and the moved-HEAD refusal are
# told apart. FAKE_FAIL makes it exit nonzero instead.
cat >"$tmp/bin/yay-friend" <<'EOF'
#!/usr/bin/env bash
[ -n "${FAKE_FAIL:-}" ] && { echo "provider error" >&2; exit 1; }
[ "$1" = analyze ] || { echo "unexpected argv: $*" >&2; exit 2; }
pkg=$2
dir="${XDG_DATA_HOME:?}/yay-friend/cache/$pkg"
mkdir -p "$dir"
echo "analyzing $pkg (fake)"
cat >"$dir/${FAKE_HEAD:?}.json" <<JSON
{ "cache_metadata": { "commit_hash": "$FAKE_HEAD", "package_name": "$pkg",
                      "yay_friend_version": "1.0.0" },
  "analysis": { "package_name": "$pkg", "overall_entropy": 3,
                "findings": [], "summary": "Fresh.", "recommendation": "REVIEW",
                "provider": "claude" } }
JSON
EOF
chmod +x "$tmp/bin/yay-friend"

export XDG_DATA_HOME="$tmp/data"
export PATH="$tmp/bin:$PATH"

# Run the adapter, capturing the two streams apart: stdout must be the
# grading and nothing else, so a diagnostic leaking into it is a bug the
# tests have to be able to see.
run() {
	out=$("$adapter" "$@" 2>"$tmp/err")
	status=$?
	# Read with a redirection, not `cat`: some cases run with a PATH that
	# has been stripped down to nothing on purpose.
	err=$(<"$tmp/err")
	return $status
}

# Assert a jq filter against the grading the last `run` produced.
check() {
	if printf '%s' "$out" | jq -e "$1" >/dev/null 2>&1; then
		ok "$2"
	else
		no "$2" "$out"
	fi
}

# --- cache hit --------------------------------------------------------------

if run --package hello --tree "$tmp/tree" --commit "$COMMIT"; then
	ok "cache hit exits 0"

	got=$(printf '%s' "$out" | jq -S . 2>/dev/null)
	want=$(jq -S . <<-EOF
		{ "contract": "pacrat-grade/v1",
		  "grader": "yay-friend",
		  "subject": { "package": "hello", "commit": "$COMMIT", "version": "2.12.1-1" },
		  "grade": 1,
		  "scale": { "min": 0, "max": 4 },
		  "findings": [
		    { "level": 2,
		      "title": "source_analysis: Source downloaded from official GNU FTP server",
		      "span": "PKGBUILD:12" },
		    { "level": 1,
		      "title": "maintainer_trust: Package has low popularity but is actively maintained" }
		  ],
		  "meta": { "adapter": "yay-friend-grade/v1", "cached": true, "provider": "claude",
		            "note": "Clean package.", "recommendation": "PROCEED",
		            "yay_friend_version": "1.0.0" } }
	EOF
	)
	if [ "$got" = "$want" ]; then
		ok "cache hit translates exactly (grade, scale, findings, subject, meta)"
	else
		no "cache hit translation" "$(diff <(echo "$want") <(echo "$got") | head -20)"
	fi

	# stdout has to parse without jq's leniency, and the contract string is
	# the first thing pacrat checks.
	if printf '%s' "$out" | python3 -m json.tool >/dev/null 2>&1; then
		ok "stdout is valid JSON (python3 json.tool)"
	else
		no "stdout is valid JSON"
	fi
	if printf '%s' "$out" | grep -q '"contract": *"pacrat-grade/v1"'; then
		ok "stdout declares the pacrat-grade/v1 contract"
	else
		no "contract string missing"
	fi

	# A finding's description contained a newline; a title with one in it
	# would let a grader forge a line of pacrat's own report.
	check '[.findings[].title] | all(test("\n") | not)' \
		"finding titles are flattened to one line"

	if [ -z "$err" ]; then
		ok "cache hit says nothing on stderr"
	else
		no "cache hit wrote to stderr" "$err"
	fi
else
	no "cache hit exits 0" "$err"
fi

# No .SRCINFO: the version is optional, not a failure.
if run --package hello --tree "$tmp/nothing" --commit "$COMMIT"; then
	check 'has("subject") and (.subject | has("version") | not)' \
		"a tree without .SRCINFO omits subject.version"
else
	no "a tree without .SRCINFO is not a failure" "$err"
fi

# --- cache miss -------------------------------------------------------------

# yay-friend's analyze lands on the commit we asked about: the honest hit.
if FAKE_HEAD=$OTHER run --package fresh --tree "$tmp/tree" --commit "$OTHER"; then
	ok "cache miss runs analyze and grades the result"
	check '.meta.cached == false and .grade == 3' \
		"a freshly analyzed grading is marked cached:false"
	check ".subject.commit == \"$OTHER\"" \
		"the miss path reports the requested commit"
	case $err in
	*"analyzing AUR HEAD"*) ok "the analyze call is announced on stderr" ;;
	*) no "no diagnostic about running analyze" "$err" ;;
	esac
	# The fake's own chatter must not have reached stdout.
	case $out in
	*"analyzing fresh (fake)"*) no "yay-friend's output leaked into stdout" ;;
	*) ok "yay-friend's own output stays off stdout" ;;
	esac
else
	no "cache miss + analyze" "$err"
fi

# AUR HEAD has moved past the commit pacrat asked about. Refuse: an analysis
# of another tree filed under this commit is worse than no grading.
if FAKE_HEAD=abc1234def5678abc1234def5678abc1234def56 \
	run --package moved --tree "$tmp/tree" --commit "$COMMIT"; then
	no "a moved HEAD must not produce a grading" "$out"
else
	ok "a moved HEAD is refused (exit $status)"
	case $err in
	*"is not $COMMIT"*) ok "the refusal names the commit that was asked for" ;;
	*) no "unhelpful moved-HEAD message" "$err" ;;
	esac
	if [ -z "$out" ]; then
		ok "a refusal writes nothing to stdout"
	else
		no "a refusal wrote to stdout" "$out"
	fi
fi

if FAKE_FAIL=1 FAKE_HEAD=$OTHER run --package broken --tree "$tmp/tree" --commit "$COMMIT"; then
	no "a failing yay-friend must not produce a grading"
else
	ok "a failing yay-friend is a nonzero exit"
fi

# --- missing dependencies ---------------------------------------------------

# Two stripped PATHs. Both keep bash, because the adapter's shebang has to
# resolve before it can complain about anything.
onlybash="$tmp/onlybash" onlyjq="$tmp/onlyjq"
mkdir -p "$onlybash" "$onlyjq"
ln -sf "$(command -v bash)" "$onlybash/bash"
ln -sf "$(command -v bash)" "$onlyjq/bash"
ln -sf "$(command -v jq)" "$onlyjq/jq"

# jq is the one dependency, and its absence has to say so rather than
# emitting half a grading.
if PATH="$onlybash" run --package hello --tree "$tmp/tree" --commit "$COMMIT"; then
	no "missing jq must fail"
else
	case $err in
	*jq*) ok "missing jq says jq (exit $status)" ;;
	*) no "missing jq message does not mention jq" "$err" ;;
	esac
fi

# yay-friend itself is only needed on the miss path — a cache hit is a file
# read, and demanding the binary for it would fail an answerable question.
if PATH="$onlyjq" run --package hello --tree "$tmp/tree" --commit "$COMMIT"; then
	ok "a cache hit does not need yay-friend installed"
else
	no "a cache hit should not need yay-friend" "$err"
fi
if PATH="$onlyjq" run --package absent --tree "$tmp/tree" --commit "$COMMIT"; then
	no "a miss without yay-friend must fail"
else
	case $err in
	*"not installed"*) ok "a miss without yay-friend says so" ;;
	*) no "unhelpful missing-yay-friend message" "$err" ;;
	esac
fi

# --- entries that are not about what was asked ------------------------------

mkdir -p "$cache/liar" "$cache/mislabeled" "$cache/garbage" "$cache/worded"

# A copy of the hello entry under another package's name, correct in every
# way except the one thing each case is about. Without renaming it, every
# one of these would be refused for the same uninteresting reason.
as_package() {
	jq --arg p "$1" '.cache_metadata.package_name = $p | .analysis.package_name = $p' \
		"$cache/hello/$COMMIT.json"
}

# The filename says one commit, the file says another.
as_package liar | jq --arg c "$OTHER" '.cache_metadata.commit_hash = $c' \
	>"$cache/liar/$COMMIT.json"
if run --package liar --tree "$tmp/tree" --commit "$COMMIT"; then
	no "an entry recording another commit must be refused" "$out"
else
	case $err in
	*"records commit $OTHER"*) ok "an entry whose recorded commit differs is refused" ;;
	*) no "refused for the wrong reason" "$err" ;;
	esac
fi

# The file is an analysis of some other package.
as_package somethingelse >"$cache/mislabeled/$COMMIT.json"
if run --package mislabeled --tree "$tmp/tree" --commit "$COMMIT"; then
	no "an entry about another package must be refused" "$out"
else
	case $err in
	*"analysis of somethingelse"*) ok "an entry about another package is refused" ;;
	*) no "refused for the wrong reason" "$err" ;;
	esac
fi

# An entropy the adapter cannot place on 0-4 is not a grade.
as_package garbage | jq '.analysis.overall_entropy = "banana"' \
	>"$cache/garbage/$COMMIT.json"
if run --package garbage --tree "$tmp/tree" --commit "$COMMIT"; then
	no "an unplaceable entropy must be refused" "$out"
else
	ok "an entropy that is not a 0-4 level is refused"
	if [ -z "$out" ]; then
		ok "no partial grading on stdout"
	else
		no "partial grading on stdout" "$out"
	fi
fi

# yay-friend's own prompt asks the model for MINIMAL..CRITICAL, so the word
# form has to translate even though today's cache holds integers.
as_package worded |
	jq '.analysis.overall_entropy = "CRITICAL" | .analysis.findings[0].entropy = "HIGH"' \
		>"$cache/worded/$COMMIT.json"
if run --package worded --tree "$tmp/tree" --commit "$COMMIT"; then
	check '.grade == 4 and .findings[0].level == 3' \
		"named entropy levels translate (CRITICAL → 4, HIGH → 3)"
else
	no "named entropy levels should translate" "$err"
fi

# --- bounds -----------------------------------------------------------------

mkdir -p "$cache/many"
as_package many | jq '.analysis.findings = [range(60) | {type: "source_analysis",
	entropy: 2, description: "finding \(.)", line_number: .}]' \
	>"$cache/many/$COMMIT.json"
if run --package many --tree "$tmp/tree" --commit "$COMMIT"; then
	check '(.findings | length) == 25' "findings are capped at 25"
else
	no "a long findings list should still grade" "$err"
fi

# --- argument handling ------------------------------------------------------

if run --commit "$COMMIT" --tree "$tmp/tree" --package hello; then
	ok "arguments are accepted in any order"
else
	no "argument order matters" "$err"
fi

for bad in "--package hello --tree $tmp/tree" "--package hello --commit $COMMIT" \
	"--tree $tmp/tree --commit $COMMIT" "--package ../etc --tree $tmp/tree --commit $COMMIT" \
	"--package hello --tree $tmp/tree --commit nothex" \
	"--package hello --tree $tmp/tree --commit abc123"; do
	# shellcheck disable=SC2086 # deliberate word splitting: these are argv
	if run $bad; then
		no "should have been rejected: $bad"
	else
		ok "rejected: $bad"
	fi
done

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
