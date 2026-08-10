//! The AUR RPC (v5), reached by shelling out to curl. The suite does not
//! link an HTTP client for this — dotfiles-cli's self-update fetches the
//! same way — which keeps the dependency tree small and, more to the point,
//! keeps the network call printable like every other command pacrat runs.
//!
//! The RPC is best-effort by design: a host can be offline and the AUR can
//! be down, and neither should stop pacrat from answering about the repos
//! and the store. Callers get a `Result` and are expected to degrade.

use std::process::Command;

use serde::Deserialize;

use crate::out::shell_quote;

const BASE: &str = "https://aur.archlinux.org/rpc/v5";
const TIMEOUT: &str = "10";

/// One AUR package as the RPC describes it. Only the fields pacrat shows
/// are named; the RPC's other keys are ignored, so a schema addition
/// upstream is not a breaking change here.
#[derive(Debug, Clone, Deserialize)]
pub struct AurPkg {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Description")]
    pub description: Option<String>,
    #[serde(rename = "URL")]
    pub url: Option<String>,
    #[serde(rename = "Maintainer")]
    pub maintainer: Option<String>,
    #[serde(rename = "NumVotes")]
    pub votes: Option<u64>,
    #[serde(rename = "Popularity")]
    pub popularity: Option<f64>,
    #[serde(rename = "LastModified")]
    pub last_modified: Option<i64>,
    #[serde(rename = "OutOfDate")]
    pub out_of_date: Option<i64>,
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    results: Vec<AurPkg>,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    error: Option<String>,
}

pub fn search_url(term: &str) -> String {
    format!("{BASE}/search?arg={}", encode(term))
}

pub fn info_url(package: &str) -> String {
    format!("{BASE}/info?arg[]={}", encode(package))
}

/// Longest URL a batched `info` may build. The AUR rejects requests over
/// roughly 8k, and a shorter ceiling costs nothing: one extra round trip per
/// ~150 packages, against the alternative of a whole batch coming back 414.
const MAX_URL: usize = 4000;

/// The `info` URLs covering `packages` — a multiinfo query per chunk.
///
/// Split out from the fetch so a caller can print every argv before the first
/// one runs (ADR-001), which is the whole reason this returns URLs rather
/// than doing the work.
pub fn info_urls(packages: &[String]) -> Vec<String> {
    let mut urls = Vec::new();
    let mut url = String::new();
    for package in packages {
        let arg = format!("&arg[]={}", encode(package));
        if url.is_empty() {
            url = format!("{BASE}/info?arg[]={}", encode(package));
            continue;
        }
        if url.len() + arg.len() > MAX_URL {
            urls.push(std::mem::take(&mut url));
            url = format!("{BASE}/info?arg[]={}", encode(package));
            continue;
        }
        url.push_str(&arg);
    }
    if !url.is_empty() {
        urls.push(url);
    }
    urls
}

/// Records for many packages at once. Names the AUR does not have are simply
/// absent from the reply — the RPC does not error on them — so callers must
/// treat a missing name as "not on the AUR", not as a failed lookup.
///
/// All-or-nothing across chunks: a partial answer here would look exactly
/// like "these packages are not on the AUR", and silently under-reporting
/// drift is the one failure this verb must not have.
pub fn info_many(packages: &[String]) -> Result<Vec<AurPkg>, String> {
    let mut all = Vec::new();
    for url in info_urls(packages) {
        all.extend(fetch(&url)?);
    }
    Ok(all)
}

/// The exact command a fetch runs, written so it can be pasted back into a
/// shell — the URL carries `?`, `&` and `[]`, all of which a shell would eat.
pub fn argv(url: &str) -> String {
    format!("curl -fsS --max-time {TIMEOUT} {}", shell_quote(url))
}

/// Search the AUR by name and description.
pub fn search(term: &str) -> Result<Vec<AurPkg>, String> {
    fetch(&search_url(term))
}

/// Full record for one package; an empty vec means the AUR does not have it
/// (an error means we could not ask).
pub fn info(package: &str) -> Result<Vec<AurPkg>, String> {
    fetch(&info_url(package))
}

fn fetch(url: &str) -> Result<Vec<AurPkg>, String> {
    let out = Command::new("curl")
        .args(["-fsS", "--max-time", TIMEOUT, url])
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        return Err(if err.is_empty() {
            format!("curl exited {}", out.status)
        } else {
            err.to_string()
        });
    }
    let reply: Reply =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("unparseable RPC reply: {e}"))?;
    if reply.kind == "error" {
        return Err(reply.error.unwrap_or_else(|| "RPC error".into()));
    }
    Ok(reply.results)
}

/// Percent-encode a query-string value. The search term is user input going
/// straight into a URL, so nothing outside the unreserved set survives.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_passes_unreserved_and_escapes_the_rest() {
        assert_eq!(encode("pacseek"), "pacseek");
        assert_eq!(encode("python3.12_x-y~z"), "python3.12_x-y~z");
        assert_eq!(encode("a b&arg[]=c"), "a%20b%26arg%5B%5D%3Dc");
        // Non-ASCII goes out as UTF-8 bytes, one escape each.
        assert_eq!(encode("é"), "%C3%A9");
    }

    #[test]
    fn urls_are_query_form() {
        assert_eq!(
            search_url("a b"),
            "https://aur.archlinux.org/rpc/v5/search?arg=a%20b"
        );
        assert_eq!(
            info_url("mdcat"),
            "https://aur.archlinux.org/rpc/v5/info?arg[]=mdcat"
        );
    }

    #[test]
    fn a_batch_that_fits_is_one_url() {
        assert!(info_urls(&[]).is_empty());
        assert_eq!(
            info_urls(&["mdcat".into()]),
            ["https://aur.archlinux.org/rpc/v5/info?arg[]=mdcat"]
        );
        assert_eq!(
            info_urls(&["mdcat".into(), "a b".into()]),
            ["https://aur.archlinux.org/rpc/v5/info?arg[]=mdcat&arg[]=a%20b"]
        );
    }

    #[test]
    fn a_batch_too_long_for_one_url_is_split_and_complete() {
        let packages: Vec<String> = (0..500).map(|i| format!("package-name-{i}")).collect();
        let urls = info_urls(&packages);
        assert!(urls.len() > 1, "500 names should not fit one URL");
        for url in &urls {
            assert!(url.len() <= MAX_URL, "{} chars", url.len());
            assert!(url.starts_with("https://aur.archlinux.org/rpc/v5/info?arg[]="));
        }
        // Every name, once, in order: a split that drops or duplicates a
        // package reads as "not on the AUR" downstream, or asks twice.
        let carried: Vec<&str> = urls
            .iter()
            .flat_map(|u| u.split("arg[]=").skip(1))
            .map(|arg| arg.trim_end_matches('&'))
            .collect();
        assert_eq!(carried, packages);
    }
}
