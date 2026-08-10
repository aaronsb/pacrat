//! pacman queries about what *exists* — the sync databases and the local db
//! — as opposed to `live`, which answers what is installed on this host.
//! Subprocess at the edges, pure parsers underneath; every call is printable
//! so the verbs can show the argv beside the results.

use std::collections::BTreeMap;
use std::process::Command;

use crate::out::shell_quote;

/// One result of `pacman -Ss`: a package as the sync databases see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncHit {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub description: String,
}

/// The command a search runs, written so it can be pasted back into a shell.
pub fn search_argv(term: &str) -> String {
    format!("pacman -Ss -- {}", shell_quote(term))
}

/// Ditto for the two record queries `info` runs.
pub fn show_argv(flag: &str, package: &str) -> String {
    format!("pacman {flag} -- {}", shell_quote(package))
}

/// Official-repo search. pacman exits 1 both for "no results" and for real
/// failures; only the latter says anything on stderr, so an empty, silent
/// exit 1 is an empty result set — an AUR-only term is not an error.
pub fn search(term: &str) -> Result<Vec<SyncHit>, String> {
    let out = Command::new("pacman")
        .args(["-Ss", "--", term])
        .output()
        .map_err(|e| format!("pacman: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        if !err.is_empty() {
            return Err(format!("{}: {err}", search_argv(term)));
        }
        return Ok(Vec::new());
    }
    Ok(parse_search(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `pacman -Ss` output: a `repo/name version [groups] [markers]` line
/// followed by its indented description.
pub fn parse_search(text: &str) -> Vec<SyncHit> {
    let mut hits: Vec<SyncHit> = Vec::new();
    for line in text.lines() {
        if line.starts_with(char::is_whitespace) {
            // The description. pacman keeps it on one line today; folding
            // continuations in means a wrapped one can never become a row.
            let part = line.trim();
            if let (Some(hit), false) = (hits.last_mut(), part.is_empty()) {
                if !hit.description.is_empty() {
                    hit.description.push(' ');
                }
                hit.description.push_str(part);
            }
            continue;
        }
        if let Some(hit) = parse_search_header(line) {
            hits.push(hit);
        }
    }
    hits
}

fn parse_search_header(line: &str) -> Option<SyncHit> {
    let (repo, rest) = line.split_once('/')?;
    if repo.is_empty() || repo.contains(char::is_whitespace) {
        return None;
    }
    let mut fields = rest.split_whitespace();
    let name = fields.next()?;
    let version = fields.next()?;
    // Whatever follows is group parens and `[installed…]` markers. Both are
    // dropped: custody is pacrat's answer, and it is a richer one.
    Some(SyncHit {
        repo: repo.to_string(),
        name: name.to_string(),
        version: version.to_string(),
        description: String::new(),
    })
}

/// `pacman -Si <pkg>` — the sync databases' full record, or None when no
/// repo carries the package.
pub fn sync_info(package: &str) -> Option<BTreeMap<String, String>> {
    show(&["-Si", "--", package])
}

/// `pacman -Qi <pkg>` — the local db's record, or None when it is not
/// installed on this host.
pub fn local_info(package: &str) -> Option<BTreeMap<String, String>> {
    show(&["-Qi", "--", package])
}

fn show(args: &[&str]) -> Option<BTreeMap<String, String>> {
    let out = Command::new("pacman").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let fields = parse_show(&String::from_utf8_lossy(&out.stdout));
    (!fields.is_empty()).then_some(fields)
}

/// Parse pacman's `Key : Value` record format (`-Si` / `-Qi`), keeping the
/// first block only — one query, one package.
pub fn parse_show(text: &str) -> BTreeMap<String, String> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut last: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            if !fields.is_empty() {
                break;
            }
            continue;
        }
        if line.starts_with(char::is_whitespace) {
            if let Some(value) = last.as_ref().and_then(|k| fields.get_mut(k)) {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        // Keys never contain a colon, so the first one is the separator.
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        fields.insert(key.clone(), value.trim().to_string());
        last = Some(key);
    }
    fields
}

/// A field worth printing: present, non-empty, and not pacman's "None".
pub fn field<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    fields
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty() && *v != "None")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from `pacman -Ss` on a live host: plain rows, an installed
    // marker, an out-of-date installed marker, and a group listing.
    const SS: &str = "\
extra/botsay 1.4.4-1
    Like cowsay, but with randomly generated ASCII robots and color support
extra/cowsay 3.8.4-1 [installed]
    Configurable talking cow (and a bit more)
core/device-mapper 2.03.42-1 [installed: 2.03.41-1]
    Device mapper userspace library and tools
extra/a2jmidid 9-6 (pro-audio)
    A daemon for exposing legacy ALSA sequencer applications in JACK MIDI system
extra/accessibility-inspector 26.04.3-1 (kde-applications kde-accessibility) [installed]
    Tool to inspect the accessibility of applications
";

    #[test]
    fn parse_search_reads_all_header_shapes() {
        let hits = parse_search(SS);
        assert_eq!(hits.len(), 5);
        assert_eq!(
            hits[0],
            SyncHit {
                repo: "extra".into(),
                name: "botsay".into(),
                version: "1.4.4-1".into(),
                description:
                    "Like cowsay, but with randomly generated ASCII robots and color support".into(),
            }
        );
        // Markers and groups are dropped, never mistaken for the version.
        assert_eq!(hits[1].name, "cowsay");
        assert_eq!(hits[1].version, "3.8.4-1");
        assert_eq!(hits[2].repo, "core");
        assert_eq!(hits[2].version, "2.03.42-1");
        assert_eq!(hits[3].version, "9-6");
        assert_eq!(hits[4].name, "accessibility-inspector");
        assert_eq!(hits[4].version, "26.04.3-1");
    }

    #[test]
    fn parse_search_folds_a_wrapped_description() {
        let hits = parse_search("extra/x 1-1\n    first half\n    second half\n");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].description, "first half second half");
    }

    #[test]
    fn parse_search_tolerates_junk() {
        assert!(parse_search("").is_empty());
        // No results: pacman prints nothing. Stray lines are not rows.
        assert!(parse_search("error: no targets specified\n").is_empty());
        assert!(parse_search("extra/lonely\n").is_empty()); // no version
    }

    // Captured from `pacman -Qi mdcat` (an installed foreign package).
    const QI: &str = "\
Name            : mdcat
Version         : 2.10.1-1
Description     : Sophisticated Markdown rendering for the terminal
URL             : https://github.com/BIRSAx2/mdcat
Groups          : None
Depends On      : gcc-libs  openssl  curl
Optional Deps   : less: for mdless [installed]
Install Date    : Tue 07 Jul 2026 11:01:31 PM CDT
Install Reason  : Explicitly installed
Validated By    : None
";

    #[test]
    fn parse_show_reads_fields() {
        let f = parse_show(QI);
        assert_eq!(field(&f, "Name"), Some("mdcat"));
        assert_eq!(field(&f, "URL"), Some("https://github.com/BIRSAx2/mdcat"));
        assert_eq!(
            field(&f, "Install Date"),
            Some("Tue 07 Jul 2026 11:01:31 PM CDT")
        );
        // A value's own colon does not confuse the split.
        assert_eq!(
            field(&f, "Optional Deps"),
            Some("less: for mdless [installed]")
        );
        // "None" is absence, not a value worth printing.
        assert_eq!(field(&f, "Groups"), None);
        assert_eq!(field(&f, "Missing"), None);
    }

    #[test]
    fn parse_show_folds_continuations_and_stops_after_one_block() {
        let text = "\
Name            : a
Licenses        : MIT  Apache-2.0
                  Unlicense

Name            : b
";
        let f = parse_show(text);
        assert_eq!(field(&f, "Name"), Some("a"));
        assert_eq!(field(&f, "Licenses"), Some("MIT  Apache-2.0 Unlicense"));
    }

    #[test]
    fn parse_show_empty() {
        assert!(parse_show("").is_empty());
        assert!(parse_show("\n\n").is_empty());
    }
}
