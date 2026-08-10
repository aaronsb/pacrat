//! `~/.config/pacrat/config.toml` — host-local preferences. Every field has
//! a default so a missing file is a valid config. Unknown keys are ignored
//! (forward compatibility across pacrat versions on different hosts).
//!
//! `[repo]` is validated rather than escaped. Its values are substituted
//! into two grammars pacrat does not own — pacman.conf, and a shell script
//! that runs as root out of a pacman hook — so a value that would need
//! escaping is a configuration error. Rejecting a small, well-defined set
//! is auditable; getting escaping right in two grammars forever is not.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::grading::Scale;
use crate::Thresholds;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Ui {
    /// Default until the TUI lands; bare `pacrat` prints status.
    #[default]
    Cli,
    Tui,
}

/// How much of the update loop runs without being asked.
///
/// ADR-001 describes the loop's three transitions — grade, decide, build —
/// as gates, each auto | verbose | manual, and bundles them into presets.
/// This is the preset, as one word, because the gates are not independent in
/// practice: a host that wants to be asked about a decision wants to be
/// asked before the build that follows it, and three orthogonal knobs would
/// let a user configure combinations that mean nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Never ask. The timer's mode: a clean PROCEED is adopted and built,
    /// and everything else holds for a human.
    Auto,
    /// Ask when the answer is not clean — the default, and the one that
    /// fits a person running `pacrat update` after coffee.
    #[default]
    Semi,
    /// Ask about everything, including a PROCEED, and once more before the
    /// build.
    Manual,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Semi => "semi",
            Mode::Manual => "manual",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The local pacman repo pacrat builds into and serves from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Repo {
    /// Section name in pacman.conf.
    pub name: String,
    /// Where the repo db and built packages live on this host.
    pub path: String,
    /// Optional shared-serving URL (phase 2): any URL pacman can fetch from —
    /// a forge's release artifacts (GitHub, Gitea, Codeberg, …), a plain
    /// https server, or file://. Deliberately forge-agnostic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

impl Default for Repo {
    fn default() -> Self {
        Self {
            name: "dotfiles-aur".into(),
            path: "/var/cache/pacrat/repo".into(),
            server: None,
        }
    }
}

impl Repo {
    /// Reject anything that could not be written literally into pacman.conf
    /// and into a single-quoted shell word. Callers may then substitute
    /// these values directly; see the module note on validate-don't-escape.
    pub fn validate(&self) -> Result<(), String> {
        // pacman's own section-name grammar. A newline here would smuggle
        // directives into pacman.conf ("x\nSigLevel = Never"); the rest of
        // the excluded set has no meaning in a section name anyway.
        if self.name.is_empty() {
            return Err("repo.name is empty".into());
        }
        if let Some(bad) = self
            .name
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || "_.@+-".contains(*c)))
        {
            return Err(format!(
                "repo.name {:?} contains {bad:?}: pacman repository names are [A-Za-z0-9_.@+-]+",
                self.name
            ));
        }

        if !Path::new(&self.path).is_absolute() {
            return Err(format!(
                "repo.path {:?} must be absolute — it is written into pacman.conf and into \
                 a root-run hook script, where a relative path has no fixed meaning",
                self.path
            ));
        }
        // Quotes and backslash could end or escape the single-quoted shell
        // word the path is substituted into; `$` and backticks would expand
        // if that word is ever double-quoted; control characters (newline
        // included) break both pacman.conf and the printed commands.
        if let Some(bad) = self
            .path
            .chars()
            .find(|c| c.is_control() || matches!(c, '\'' | '"' | '`' | '\\' | '$'))
        {
            return Err(format!(
                "repo.path {:?} contains {bad:?}, which pacrat will not substitute into \
                 pacman.conf or into the root-run guard script",
                self.path
            ));
        }

        // The server URL reaches pacman.conf on the same line grammar, so a
        // newline there is the same directive-smuggling hole as in the name.
        if let Some(url) = &self.server {
            if url.is_empty() {
                return Err("repo.server is empty — omit the key instead".into());
            }
            if let Some(bad) = url.chars().find(|c| c.is_control() || c.is_whitespace()) {
                return Err(format!(
                    "repo.server {url:?} contains {bad:?}: a Server line is one unbroken URL"
                ));
            }
        }
        Ok(())
    }
}

/// The placeholders a grader's argv template may use, in the order the
/// error message lists them.
pub const PLACEHOLDERS: [&str; 3] = ["package", "tree", "commit"];

/// The grader name pacrat keeps for itself: a human's own judgement,
/// recorded by `pacrat grade --grade N --note …`.
pub const MANUAL: &str = "manual";

/// An external grader: a program pacrat runs to get a `pacrat-grade/v1`
/// report on stdout.
///
/// `cmd` is an **argv template**, never a command line. Each element is
/// substituted independently and handed straight to exec, so a value
/// containing spaces, quotes or `;` stays exactly one argument — there is no
/// shell anywhere on this path and therefore nothing to escape for. The
/// placeholders are checked here rather than at substitution time so a typo
/// in the config is an error at load, not a grader that silently receives
/// the literal text `{treee}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grader {
    /// Names the grader in output and in the cache file. Restricted to a
    /// filename-safe alphabet because it becomes a path component.
    pub name: String,
    pub cmd: Vec<String>,
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    /// The scale this grader is expected to answer on.
    ///
    /// Optional, and worth setting for anything whose output format could
    /// change under you. pacrat maps a foreign scale onto its own 0-4, so a
    /// grader that silently switched from 0-4 to 0-100 would keep producing
    /// plausible verdicts — every grade would just quietly become four times
    /// less alarming. Pinning turns that into an UNGRADED with a reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Scale>,
}

/// Five minutes: an LLM-backed grader reading a PKGBUILD is slow, and a
/// timeout that fires on a working grader turns every run into an
/// unnecessary hold.
fn default_timeout_s() -> u64 {
    300
}

impl Grader {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("a grader has no name".into());
        }
        if self.name == MANUAL {
            return Err(format!(
                "grader {MANUAL:?} is built in (`pacrat grade --grade N --note …`) \
                 and cannot be configured as a program"
            ));
        }
        // The name becomes `<commit>.<name>.json` in the grade cache. An
        // allowlist keeps a config typo from writing outside that directory.
        if self.name.starts_with('.') || self.name.starts_with('-') {
            return Err(format!(
                "grader name {:?} may not begin with '.' or '-'",
                self.name
            ));
        }
        // A failure record is `<commit>.<name>.failed.json`. A grader
        // actually named `x.failed` would write its *gradings* to the path
        // that holds grader `x`'s *failures* — two different kinds of file
        // at one name, which the reader cannot tell apart.
        if self.name == "failed" || self.name.ends_with(".failed") {
            return Err(format!(
                "grader name {:?} collides with the grade cache's failure records \
                 (`<commit>.<grader>.failed.json`)",
                self.name
            ));
        }
        if let Some(bad) = self
            .name
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || "_.+-".contains(*c)))
        {
            return Err(format!(
                "grader name {:?} contains {bad:?}: names are [A-Za-z0-9_.+-]+ because \
                 the name is a filename in the grade cache",
                self.name
            ));
        }

        let Some(program) = self.cmd.first() else {
            return Err(format!("grader {:?} has an empty cmd", self.name));
        };
        if program.is_empty() {
            return Err(format!("grader {:?} has an empty program name", self.name));
        }
        // The program is the grader. Taking it from the subject would let a
        // package name choose what runs.
        if program.contains('{') {
            return Err(format!(
                "grader {:?}: the program {program:?} may not contain a placeholder — \
                 pacrat substitutes into arguments, not into what it executes",
                self.name
            ));
        }
        for arg in &self.cmd {
            check_placeholders(arg).map_err(|e| format!("grader {:?}: {e}", self.name))?;
        }

        if self.timeout_s == 0 {
            return Err(format!(
                "grader {:?} has timeout_s = 0 — it would be killed before it ran",
                self.name
            ));
        }
        if let Some(scale) = self.scale {
            if scale.min >= scale.max {
                return Err(format!(
                    "grader {:?} pins scale {}-{}, which spans nothing",
                    self.name, scale.min, scale.max
                ));
            }
        }
        Ok(())
    }

    /// Check a report's declared scale against the pin, if there is one.
    /// The error is the message a reviewer sees instead of a grade.
    pub fn check_scale(&self, declared: Scale) -> Result<(), String> {
        match self.scale {
            Some(pinned) if pinned != declared => Err(format!(
                "declared scale {}-{}, pinned {}-{} — a grader whose scale moved is \
                 not one whose grades can be compared to the old ones",
                declared.min, declared.max, pinned.min, pinned.max
            )),
            _ => Ok(()),
        }
    }

    /// The argv for one subject. Single-pass: a substituted value that
    /// happens to read like a placeholder is never substituted again.
    pub fn argv(&self, package: &str, tree: &str, commit: &str) -> Vec<String> {
        let values = [("package", package), ("tree", tree), ("commit", commit)];
        self.cmd.iter().map(|a| substitute(a, &values)).collect()
    }
}

fn check_placeholders(arg: &str) -> Result<(), String> {
    let mut rest = arg;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(format!("{arg:?} has an unclosed '{{'"));
        };
        let name = &after[..close];
        if !PLACEHOLDERS.contains(&name) {
            return Err(format!(
                "{arg:?} uses unknown placeholder {{{name}}} — pacrat substitutes {}",
                PLACEHOLDERS.map(|p| format!("{{{p}}}")).join(", ")
            ));
        }
        rest = &after[close + 1..];
    }
    if rest.contains('}') {
        return Err(format!("{arg:?} has a '}}' with no '{{'"));
    }
    Ok(())
}

/// Replace every known `{name}` with its value, left to right, consuming the
/// input as it goes. Unknown placeholders are left literal — `validate`
/// already rejected them, and an unvalidated template should show the user
/// its own text rather than quietly dropping it.
fn substitute(arg: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(arg.len());
    let mut rest = arg;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else { break };
        let name = &after[..close];
        match values.iter().find(|(k, _)| *k == name) {
            Some((_, value)) => {
                out.push_str(&rest[..open]);
                out.push_str(value);
            }
            None => out.push_str(&rest[..open + 1 + close + 1]),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub default_ui: Ui,
    /// What `pacrat update` does when nobody passed `--mode`.
    pub update_mode: Mode,
    pub thresholds: Thresholds,
    pub repo: Repo,
    /// External graders, run in the order configured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graders: Vec<Grader>,
}

impl Config {
    /// Parse and validate. Validation lives here, not at the use site, so
    /// every consumer of a `Config` holds one that is already safe to
    /// substitute — there is no unvalidated path through this type.
    pub fn from_toml(s: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(s).map_err(|e| e.to_string())?;
        config.repo.validate()?;
        config.validate_graders()?;
        Ok(config)
    }

    /// Names must be unique: they are the cache filename, so two graders
    /// sharing one would overwrite each other's gradings.
    fn validate_graders(&self) -> Result<(), String> {
        let mut seen: Vec<&str> = Vec::new();
        for grader in &self.graders {
            grader.validate()?;
            if seen.contains(&grader.name.as_str()) {
                return Err(format!(
                    "two graders are named {:?} — the name is the grade cache's key, \
                     so they would overwrite each other",
                    grader.name
                ));
            }
            seen.push(&grader.name);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_defaults() {
        let c = Config::from_toml("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.repo.name, "dotfiles-aur");
        assert_eq!(c.thresholds.warn_at, 2);
        assert_eq!(c.thresholds.block_at, 4);
        // Semi, not auto: the default posture asks rather than acts, and a
        // host that wants the timer's silence says so.
        assert_eq!(c.update_mode, Mode::Semi);
    }

    #[test]
    fn the_update_mode_is_a_preset_word() {
        for (word, mode) in [
            ("auto", Mode::Auto),
            ("semi", Mode::Semi),
            ("manual", Mode::Manual),
        ] {
            let c = Config::from_toml(&format!("update_mode = {word:?}\n")).unwrap();
            assert_eq!(c.update_mode, mode);
            assert_eq!(mode.to_string(), word);
        }
        // A mode this pacrat does not have is a config error, not a silent
        // fallback to whichever default happens to be least surprising.
        assert!(Config::from_toml("update_mode = \"yolo\"\n").is_err());
    }

    #[test]
    fn partial_override_and_unknown_keys() {
        let c = Config::from_toml(
            r#"
default_ui = "tui"
future_knob = true

[thresholds]
warn_at = 1

[repo]
name = "fleet"
server = "https://codeberg.org/aaronsb/fleet-repo/releases/latest/download"
"#,
        )
        .unwrap();
        assert_eq!(c.default_ui, Ui::Tui);
        assert_eq!(c.thresholds.warn_at, 1);
        assert_eq!(c.thresholds.block_at, 4); // untouched default
        assert_eq!(c.repo.name, "fleet");
        assert!(c
            .repo
            .server
            .as_deref()
            .unwrap()
            .starts_with("https://codeberg.org"));
    }

    /// The default must survive its own validator, or every host with no
    /// config file fails to start.
    #[test]
    fn default_repo_is_valid() {
        assert_eq!(Repo::default().validate(), Ok(()));
    }

    #[test]
    fn hostile_repo_names_are_rejected() {
        // Each of these reaches pacman.conf as a section header and the
        // guard script as a shell literal.
        let hostile = [
            "x\nSigLevel = Never",           // F2: smuggled pacman directive
            "x'; rm -rf /tmp/pwned; echo '", // F1: breaks out of '...'
            "x\"y",
            "x`id`",
            "x$(id)",
            "with space",
            "x]\n[core",
            "",
        ];
        for name in hostile {
            let repo = Repo {
                name: name.into(),
                ..Repo::default()
            };
            assert!(
                repo.validate().is_err(),
                "repo.name {name:?} should have been rejected"
            );
        }
    }

    #[test]
    fn hostile_repo_paths_are_rejected() {
        let hostile = [
            "/var/cache/it's", // F1: the reviewer's apostrophe
            "relative/path",
            "/var/cache\nSigLevel = Never",
            "/var/cache/$(id)",
            "/var/cache/`id`",
            "/var/cache/back\\slash",
            "/var/cache/quote\"here",
            "/var/cache/bell\u{7}",
        ];
        for path in hostile {
            let repo = Repo {
                path: path.into(),
                ..Repo::default()
            };
            assert!(
                repo.validate().is_err(),
                "repo.path {path:?} should have been rejected"
            );
        }
    }

    #[test]
    fn hostile_server_urls_are_rejected() {
        for url in ["https://x/\nSigLevel = Never", "https://x y", ""] {
            let repo = Repo {
                server: Some(url.into()),
                ..Repo::default()
            };
            assert!(
                repo.validate().is_err(),
                "repo.server {url:?} should have been rejected"
            );
        }
    }

    /// A path with a space is legal on disk and is *not* an injection: it
    /// stays allowed, and the CLI quotes it when it renders shell.
    #[test]
    fn unusual_but_harmless_values_are_allowed() {
        let repo = Repo {
            name: "dotfiles-aur.v2+1".into(),
            path: "/var/cache/my repo".into(),
            server: Some("https://example.invalid/r".into()),
        };
        assert_eq!(repo.validate(), Ok(()));
    }

    #[test]
    fn a_hostile_config_file_fails_to_parse() {
        let err = Config::from_toml("[repo]\nname = \"x\\nSigLevel = Never\"\n").unwrap_err();
        assert!(err.contains("repo.name"), "unhelpful error: {err}");
    }

    // ---- graders ----

    fn grader() -> Grader {
        Grader {
            name: "yay-friend".into(),
            cmd: vec![
                "yay-friend".into(),
                "--format".into(),
                "pacrat".into(),
                "--tree".into(),
                "{tree}".into(),
                "{package}".into(),
            ],
            timeout_s: 300,
            scale: None,
        }
    }

    #[test]
    fn graders_are_read_from_the_config_with_a_default_timeout() {
        let c = Config::from_toml(
            r#"
[[graders]]
name = "yay-friend"
cmd = ["yay-friend", "--format", "pacrat", "--tree", "{tree}", "{package}"]

[[graders]]
name = "shellcheck-pkgbuild"
cmd = ["/usr/local/bin/pkgb-grade", "{tree}/PKGBUILD", "--commit={commit}"]
timeout_s = 30
"#,
        )
        .unwrap();
        assert_eq!(c.graders.len(), 2);
        assert_eq!(c.graders[0].timeout_s, 300);
        assert_eq!(c.graders[1].timeout_s, 30);
        // No graders is the default, and an empty list stays empty.
        assert!(Config::from_toml("").unwrap().graders.is_empty());
    }

    #[test]
    fn grader_names_must_be_usable_as_a_filename() {
        for name in ["", ".hidden", "-x", "a/b", "a b", "..", "a\nb", "naïve"] {
            let g = Grader {
                name: name.into(),
                ..grader()
            };
            assert!(g.validate().is_err(), "name {name:?} should be rejected");
        }
        for name in ["yay-friend", "pkgb.v2", "a_b+c", "grader3"] {
            let g = Grader {
                name: name.into(),
                ..grader()
            };
            assert_eq!(g.validate(), Ok(()), "name {name:?} should be accepted");
        }
    }

    /// A grading and a failure record are different kinds of file; a name
    /// that makes one look like the other is a config error.
    #[test]
    fn grader_names_may_not_collide_with_failure_records() {
        for name in ["failed", "yay-friend.failed", "x.failed"] {
            let g = Grader {
                name: name.into(),
                ..grader()
            };
            let err = g
                .validate()
                .unwrap_err_or_else(|| panic!("name {name:?} should be rejected"));
            assert!(err.contains("failure records"), "unhelpful error: {err}");
        }
        // Not a collision: the suffix has to be the whole trailing segment.
        for name in ["failedx", "x-failed", "failure"] {
            let g = Grader {
                name: name.into(),
                ..grader()
            };
            assert_eq!(g.validate(), Ok(()), "name {name:?} should be accepted");
        }
    }

    #[test]
    fn a_pinned_scale_is_read_and_enforced() {
        let c = Config::from_toml(
            r#"
[[graders]]
name = "yf"
cmd = ["yf"]
scale = { min = 0, max = 4 }
"#,
        )
        .unwrap();
        let g = &c.graders[0];
        assert_eq!(g.scale, Some(Scale { min: 0, max: 4 }));
        assert_eq!(g.check_scale(Scale { min: 0, max: 4 }), Ok(()));

        let err = g.check_scale(Scale { min: 0, max: 100 }).unwrap_err();
        assert!(err.contains("declared scale 0-100"), "unhelpful: {err}");
        assert!(err.contains("pinned 0-4"), "unhelpful: {err}");

        // No pin means no opinion — the default, and what every grader that
        // predates this option keeps doing.
        let unpinned = grader();
        assert_eq!(unpinned.scale, None);
        assert_eq!(unpinned.check_scale(Scale { min: 0, max: 100 }), Ok(()));
    }

    #[test]
    fn a_pinned_scale_that_spans_nothing_is_rejected() {
        let g = Grader {
            scale: Some(Scale { min: 4, max: 4 }),
            ..grader()
        };
        assert!(g.validate().is_err());
    }

    #[test]
    fn manual_is_reserved_for_the_human() {
        let g = Grader {
            name: MANUAL.into(),
            ..grader()
        };
        let err = g.validate().unwrap_err();
        assert!(err.contains("built in"), "unhelpful error: {err}");
    }

    #[test]
    fn duplicate_grader_names_are_rejected() {
        let err = Config::from_toml(
            r#"
[[graders]]
name = "yf"
cmd = ["a"]

[[graders]]
name = "yf"
cmd = ["b"]
"#,
        )
        .unwrap_err();
        assert!(err.contains("overwrite"), "unhelpful error: {err}");
    }

    #[test]
    fn an_empty_or_placeholder_program_is_rejected() {
        for cmd in [vec![], vec!["".into()], vec!["{package}".into()]] {
            let g = Grader { cmd, ..grader() };
            assert!(g.validate().is_err(), "cmd {:?} should be rejected", g.cmd);
        }
    }

    #[test]
    fn unknown_placeholders_are_a_config_error() {
        for arg in ["{treee}", "{}", "{Package}", "--x={sha}", "{tree", "tree}"] {
            let g = Grader {
                cmd: vec!["yf".into(), arg.into()],
                ..grader()
            };
            let err = g
                .validate()
                .unwrap_err_or_else(|| panic!("{arg:?} should be rejected"));
            assert!(
                err.contains("placeholder") || err.contains('{') || err.contains('}'),
                "unhelpful error for {arg:?}: {err}"
            );
        }
        // The known three, in every position, are fine.
        let g = Grader {
            cmd: vec![
                "yf".into(),
                "{package}@{commit}".into(),
                "{tree}/PKGBUILD".into(),
            ],
            ..grader()
        };
        assert_eq!(g.validate(), Ok(()));
    }

    #[test]
    fn a_zero_timeout_is_rejected() {
        let g = Grader {
            timeout_s: 0,
            ..grader()
        };
        assert!(g.validate().is_err());
    }

    #[test]
    fn substitution_is_per_argument_and_total() {
        let argv = grader().argv("mdcat", "/store/aur/packages/mdcat", "3f9c21ab");
        assert_eq!(
            argv,
            [
                "yay-friend",
                "--format",
                "pacrat",
                "--tree",
                "/store/aur/packages/mdcat",
                "mdcat"
            ]
        );
    }

    /// The whole point of an argv template: nothing a value contains can
    /// become syntax, because there is no syntax left to become. A value
    /// with spaces, semicolons and quotes stays exactly one argv element.
    #[test]
    fn a_hostile_value_stays_one_argument() {
        let g = Grader {
            cmd: vec!["yf".into(), "{package}".into(), "--tree={tree}".into()],
            ..grader()
        };
        let hostile = "a; rm -rf ~ #'\"$(id)`id`\n";
        let argv = g.argv(hostile, "/store/my tree", "3f9c21ab");
        assert_eq!(
            argv.len(),
            3,
            "a value split into extra arguments: {argv:?}"
        );
        assert_eq!(argv[1], hostile, "the value was altered on the way to exec");
        assert_eq!(argv[2], "--tree=/store/my tree");
    }

    /// Substitution happens once. A tree path containing `{commit}` is
    /// data, not another template.
    #[test]
    fn a_substituted_value_is_not_substituted_again() {
        let g = Grader {
            cmd: vec!["yf".into(), "{tree}".into(), "{package}".into()],
            ..grader()
        };
        let argv = g.argv("{tree}", "/store/{commit}/x", "3f9c21ab");
        assert_eq!(argv[1], "/store/{commit}/x");
        assert_eq!(argv[2], "{tree}");
    }

    #[test]
    fn an_unvalidated_template_keeps_its_unknown_placeholders_literal() {
        let g = Grader {
            cmd: vec!["yf".into(), "{sha}-{package}".into(), "{tree".into()],
            ..grader()
        };
        let argv = g.argv("mdcat", "/t", "3f9c21ab");
        assert_eq!(argv[1], "{sha}-mdcat");
        assert_eq!(argv[2], "{tree");
    }

    trait UnwrapErrOrElse {
        fn unwrap_err_or_else(self, f: impl FnOnce() -> String) -> String;
    }
    impl UnwrapErrOrElse for Result<(), String> {
        fn unwrap_err_or_else(self, f: impl FnOnce() -> String) -> String {
            match self {
                Ok(()) => panic!("{}", f()),
                Err(e) => e,
            }
        }
    }
}
