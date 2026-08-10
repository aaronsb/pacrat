//! Reading a PKGBUILD as *text*.
//!
//! A PKGBUILD is a bash script, and the only thing that truly knows what it
//! says is bash. pacrat does not run it to find out: sourcing a file to learn
//! its version is executing untrusted code to decide whether to trust it.
//! Everything here is therefore a reading aid — it recognises the plain
//! forms, shows anything else as the text it is, and never claims
//! completeness. Where a real answer is needed, makepkg is asked
//! (`--printsrcinfo`), as a subprocess, on a tree the store already vouches
//! for.
//!
//! What the readings are *for* is the tamper alarm ([`changed_sums`]),
//! generalised from clicue's publish script: a checksum that changed for a
//! source of an already-published version is not a re-sum, it is an incident.
//! The reading being partial is safe in that direction — a form this module
//! does not recognise yields no comparison and therefore no false alarm,
//! while every form it does recognise is the form the alarm was written for.

use std::collections::BTreeMap;

/// The checksum algorithms makepkg understands, as array-name prefixes.
const ALGOS: [&str; 8] = [
    "ck", "md5", "sha1", "sha224", "sha256", "sha384", "sha512", "b2",
];

/// makepkg's "this source is not checksummed" marker (a VCS source, usually).
const SKIP: &str = "SKIP";

/// The value of a plain top-level assignment: `pkgver=2.11.0`.
///
/// Quotes are stripped and nothing is expanded, so a computed
/// `pkgver=$(git describe)` comes back as the literal `$(git describe)` — the
/// honest answer to "what does the file say", and visibly not a version.
/// A `pkgver()` function definition is not an assignment and is not matched.
pub fn field(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let value = line.trim_start().strip_prefix(name)?.strip_prefix('=')?;
        let value = value.trim().trim_matches(['"', '\'']);
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// `pkgver-pkgrel`, the pair that names a published build.
///
/// `None` when the file does not plainly say; a version pacrat had to guess
/// at is not a version it should compare against a published one.
pub fn version(text: &str) -> Option<String> {
    let pkgver = field(text, "pkgver")?;
    let pkgrel = field(text, "pkgrel").unwrap_or_else(|| "1".to_string());
    Some(format!("{pkgver}-{pkgrel}"))
}

/// The entries of a shell array literal: `source=(a "b c" 'd')` → three.
///
/// Handles the multi-line form and both quote styles, keeps `$pkgver` and
/// friends unexpanded (comparisons here are between two files that expand
/// them identically), and stops at the closing parenthesis. An unquoted `#`
/// starts a comment, as bash would have it.
pub fn array(text: &str, name: &str) -> Vec<String> {
    let Some(body) = body_of(text, name) else {
        return Vec::new();
    };
    tokens(&body)
}

/// The text between `name=(` and its matching `)`.
fn body_of(text: &str, name: &str) -> Option<String> {
    // Find an assignment at the start of a line (leading whitespace aside),
    // so `sha256sums=(` is not found inside `_sha256sums=(`.
    let mut rest = text;
    let mut offset = 0usize;
    let opening = loop {
        let at = rest.find(&format!("{name}=("))?;
        let absolute = offset + at;
        let before = text[..absolute].chars().next_back();
        if before.is_none_or(|c| c == '\n') || text[..absolute].ends_with([' ', '\t']) {
            break absolute + name.len() + 2;
        }
        offset = absolute + 1;
        rest = &text[offset..];
    };

    let mut depth = 1usize;
    let mut quote: Option<char> = None;
    let mut comment = false;
    for (i, c) in text[opening..].char_indices() {
        if comment {
            if c == '\n' {
                comment = false;
            }
            continue;
        }
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '#' => comment = true,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(text[opening..opening + i].to_string());
                    }
                }
                _ => {}
            },
        }
    }
    // Unterminated: a file that does not close its array is one this module
    // declines to read rather than guesses about.
    None
}

/// Split an array body into entries, respecting quotes.
fn tokens(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut comment = false;
    for c in body.chars() {
        if comment {
            if c == '\n' {
                comment = false;
            }
            continue;
        }
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                '#' if !started => comment = true,
                c if c.is_whitespace() => {
                    if started {
                        out.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                '\\' => {}
                c => {
                    current.push(c);
                    started = true;
                }
            },
        }
    }
    if started {
        out.push(current);
    }
    out
}

/// Which `source` arrays this file declares: `""` for the plain one, and the
/// architecture suffix for each `source_<arch>`.
fn source_suffixes(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("source") else {
            continue;
        };
        let Some((key, _)) = rest.split_once("=(") else {
            continue;
        };
        if key.is_empty() || key.starts_with('_') {
            let suffix = key.to_string();
            if !found.contains(&suffix) {
                found.push(suffix);
            }
        }
    }
    found
}

/// Each declared source, with every checksum declared for it.
///
/// Sources and sums are paired by position, which is what makepkg does; a
/// sums array of a different length simply contributes fewer pairs. Keyed by
/// the source entry's text, so inserting a patch at the top of the array does
/// not shift every comparison — the question the alarm asks is about a
/// *source*, not about an index.
pub fn sums_by_source(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for suffix in source_suffixes(text) {
        let sources = array(text, &format!("source{suffix}"));
        for algo in ALGOS {
            let sums = array(text, &format!("{algo}sums{suffix}"));
            for (source, sum) in sources.iter().zip(sums.iter()) {
                if sum == SKIP {
                    continue;
                }
                out.entry(source.clone())
                    .or_default()
                    .entry(algo.to_string())
                    .or_insert_with(|| sum.clone());
            }
        }
    }
    out
}

/// A checksum that changed for a source that was already published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumChange {
    pub source: String,
    pub algo: String,
    pub published: String,
    pub now: String,
}

/// The tamper alarm, as data.
///
/// clicue's reasoning, generalised: for a version that has already been
/// published, an immutable source whose checksum is now different means the
/// bytes behind that source changed after publication. A retagged release, a
/// rewritten tarball, a compromised host — every explanation is something to
/// investigate, and none of them is "re-sum it and push".
///
/// Only sources present in *both* files are compared, so adding a patch (with
/// a `pkgrel` bump) is ordinary maintenance and not an alarm, and a `SKIP` on
/// either side is no claim to contradict. Callers must only ask this question
/// of two files at the same version — a checksum changing across versions is
/// simply a new release.
pub fn changed_sums(published: &str, now: &str) -> Vec<SumChange> {
    let before = sums_by_source(published);
    let after = sums_by_source(now);
    let mut changes = Vec::new();
    for (source, old_sums) in &before {
        let Some(new_sums) = after.get(source) else {
            continue;
        };
        for (algo, published_sum) in old_sums {
            let Some(new_sum) = new_sums.get(algo) else {
                continue;
            };
            if !published_sum.eq_ignore_ascii_case(new_sum) {
                changes.push(SumChange {
                    source: source.clone(),
                    algo: algo.clone(),
                    published: published_sum.clone(),
                    now: new_sum.clone(),
                });
            }
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_is_read_as_text_and_never_evaluated() {
        assert_eq!(
            field("pkgname=mdcat\npkgver=2.11.0\npkgrel=1\n", "pkgver").as_deref(),
            Some("2.11.0")
        );
        assert_eq!(
            field("  pkgver='1:4.16'\n", "pkgver").as_deref(),
            Some("1:4.16")
        );
        assert_eq!(field("pkgver=\"2.0\"\n", "pkgver").as_deref(), Some("2.0"));
        // The shell-computed form is shown as the text it is.
        assert_eq!(
            field("pkgver=$(git describe)\n", "pkgver").as_deref(),
            Some("$(git describe)")
        );
        // A function definition is not an assignment.
        assert_eq!(field("pkgver() {\n  echo 1\n}\n", "pkgver"), None);
        assert_eq!(field("pkgname=mdcat\n", "pkgver"), None);
        assert_eq!(field("pkgver=\n", "pkgver"), None);
    }

    #[test]
    fn a_version_needs_a_pkgver_and_defaults_the_pkgrel() {
        assert_eq!(
            version("pkgver=2.11.0\npkgrel=3\n").as_deref(),
            Some("2.11.0-3")
        );
        // makepkg's own default when the file omits it.
        assert_eq!(version("pkgver=2.11.0\n").as_deref(), Some("2.11.0-1"));
        assert_eq!(version("pkgname=x\n"), None);
    }

    #[test]
    fn arrays_are_read_across_lines_and_quote_styles() {
        let pkgbuild = "source=(\"a.tar.gz\"\n        'b.patch'\n        c-$pkgver.diff)\n";
        assert_eq!(
            array(pkgbuild, "source"),
            ["a.tar.gz", "b.patch", "c-$pkgver.diff"]
        );
        assert_eq!(array(pkgbuild, "sha256sums"), Vec::<String>::new());
        // An empty array is empty, not a missing one — both yield nothing.
        assert_eq!(array("source=()\n", "source"), Vec::<String>::new());
        // A comment inside the array is not an entry.
        assert_eq!(
            array("source=(a # the tarball\n        b)\n", "source"),
            ["a", "b"]
        );
        // An unterminated array is declined rather than guessed at.
        assert_eq!(array("source=(a\n", "source"), Vec::<String>::new());
    }

    /// `_sha256sums=(…)` is somebody's private variable, not the checksums.
    #[test]
    fn an_array_name_must_be_the_whole_name() {
        let pkgbuild = "_sha256sums=('decoy')\nsha256sums=('real')\n";
        assert_eq!(array(pkgbuild, "sha256sums"), ["real"]);
    }

    #[test]
    fn sums_are_paired_with_their_sources_and_skip_is_no_claim() {
        let pkgbuild = "\
source=(\"x-1.0.tar.gz::https://e.x/1.0.tar.gz\"
        \"git+https://e.x/x.git\")
sha256sums=('aaaa'
            'SKIP')
b2sums=('bbbb'
        'SKIP')
";
        let sums = sums_by_source(pkgbuild);
        assert_eq!(sums.len(), 1, "SKIP contributed a claim: {sums:?}");
        let entry = &sums["x-1.0.tar.gz::https://e.x/1.0.tar.gz"];
        assert_eq!(entry["sha256"], "aaaa");
        assert_eq!(entry["b2"], "bbbb");
    }

    #[test]
    fn per_architecture_arrays_are_read_too() {
        let pkgbuild = "\
source=('common.patch')
sha256sums=('cccc')
source_x86_64=('x86.bin')
sha256sums_x86_64=('dddd')
";
        let sums = sums_by_source(pkgbuild);
        assert_eq!(sums["common.patch"]["sha256"], "cccc");
        assert_eq!(sums["x86.bin"]["sha256"], "dddd");
    }

    /// THE alarm: an already-published version whose tarball now hashes
    /// differently. Someone or something rewrote the tag.
    #[test]
    fn a_changed_sum_for_a_published_source_is_an_alarm() {
        let published = "pkgver=1.0\nsource=('x-1.0.tar.gz')\nsha256sums=('aaaa')\n";
        let now = "pkgver=1.0\nsource=('x-1.0.tar.gz')\nsha256sums=('bbbb')\n";
        let changes = changed_sums(published, now);
        assert_eq!(
            changes,
            [SumChange {
                source: "x-1.0.tar.gz".into(),
                algo: "sha256".into(),
                published: "aaaa".into(),
                now: "bbbb".into(),
            }]
        );
        // Hex case is not a change.
        let upper = "pkgver=1.0\nsource=('x-1.0.tar.gz')\nsha256sums=('AAAA')\n";
        assert!(changed_sums(published, upper).is_empty());
    }

    /// Ordinary maintenance is not an alarm: a new patch, a source dropped, a
    /// source that was never checksummed. Only a *contradiction* alarms.
    #[test]
    fn ordinary_maintenance_is_not_an_alarm() {
        let published = "pkgver=1.0\nsource=('x-1.0.tar.gz')\nsha256sums=('aaaa')\n";

        // A patch inserted at the top shifts every index and changes nothing.
        let added = "pkgver=1.0\nsource=('fix.patch'\n        'x-1.0.tar.gz')\n\
                     sha256sums=('pppp'\n            'aaaa')\n";
        assert!(
            changed_sums(published, added).is_empty(),
            "insertion alarmed"
        );

        // A source that is gone entirely cannot contradict anything.
        let removed = "pkgver=1.0\nsource=('other.tar.gz')\nsha256sums=('zzzz')\n";
        assert!(changed_sums(published, removed).is_empty());

        // A newly-added algorithm is extra evidence, not a conflict.
        let extra = "pkgver=1.0\nsource=('x-1.0.tar.gz')\nsha256sums=('aaaa')\nb2sums=('bbbb')\n";
        assert!(changed_sums(published, extra).is_empty());

        // SKIP on either side is no claim to contradict.
        let skipped = "pkgver=1.0\nsource=('x-1.0.tar.gz')\nsha256sums=('SKIP')\n";
        assert!(changed_sums(published, skipped).is_empty());
        assert!(changed_sums(skipped, published).is_empty());

        // A first publish has nothing published to compare against.
        assert!(changed_sums("", published).is_empty());
    }
}
