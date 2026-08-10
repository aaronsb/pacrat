//! `pacrat config` — list / get / set for `~/.config/pacrat/config.toml` —
//! and the one writer of that file. The setup interview saves through
//! [`save`] too, so every pacrat-written config has the same stamp, the same
//! shape, and the same guarantee: it was read back by `Config::from_toml`
//! before it touched disk, so a value `set` accepts is a value pacrat can
//! load (ADR-003).
//!
//! The file is rendered by hand rather than through a TOML serializer: the
//! output is small, the shape is fixed, and a comment header cannot come out
//! of a serializer. The cost is honesty about what a rewrite keeps — only
//! the keys pacrat knows — which the config module's own doc (pacrat-core)
//! accepts for a host-local, never-synced file.

use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use pacrat_core::config::{Config, Mode, Ui, SETTABLE};

use crate::ctx::Ctx;
use crate::out::visible_line;

/// `--ui` for `pacrat setup`, mirroring `update::ModeArg`'s shape.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum UiArg {
    /// Bare `pacrat` prints status
    Cli,
    /// Bare `pacrat` opens the TUI
    Tui,
}

impl From<UiArg> for Ui {
    fn from(ui: UiArg) -> Self {
        match ui {
            UiArg::Cli => Ui::Cli,
            UiArg::Tui => Ui::Tui,
        }
    }
}

#[derive(clap::Subcommand)]
pub enum Action {
    /// Every known key: effective value, and whether the file set it
    List,
    /// One key's effective value
    Get { key: String },
    /// Change one key (validated before anything is written)
    Set { key: String, value: String },
}

pub fn run(ctx: &Ctx, action: &Action) -> Result<(), String> {
    match action {
        Action::List => list(ctx),
        Action::Get { key } => {
            let value = get_value(&ctx.config, key)?;
            println!("{}", value.as_deref().unwrap_or("(unset)"));
            Ok(())
        }
        Action::Set { key, value } => {
            let mut config = ctx.config.clone();
            let said = set_value(&mut config, key, value)?;
            let path = save(&config)?;
            println!("set       {said}");
            println!("wrote     {}", path.display());
            Ok(())
        }
    }
}

fn list(ctx: &Ctx) -> Result<(), String> {
    let path = path()?;
    let in_file: Vec<&str> = match fs::read_to_string(&path) {
        Ok(text) => pacrat_core::config::set_in_file(&text)
            .map_err(|e| format!("{}: {e}", path.display()))?,
        Err(_) => Vec::new(),
    };

    println!("pacrat config — {}", path.display());
    if !path.is_file() {
        println!("(no file yet — every value below is a default; `pacrat setup` writes it)");
    }
    println!();
    for key in SETTABLE {
        let value = get_value(&ctx.config, key)?;
        println!(
            "{key:<20} {:<40} {}",
            visible_line(value.as_deref().unwrap_or("(unset)")).0,
            if in_file.contains(&key) {
                "file"
            } else {
                "default"
            }
        );
    }

    println!();
    if ctx.config.graders.is_empty() {
        println!("graders   none registered — `pacrat setup` offers known ones");
    } else {
        println!("graders   registered by `pacrat setup`; change them by editing the file");
        for g in &ctx.config.graders {
            // A grader's cmd is hand-editable text going to a terminal, so
            // it takes the same de-escaping walk every untrusted field does.
            println!(
                "  {:<12} timeout {}s   {}",
                visible_line(&g.name).0,
                g.timeout_s,
                visible_line(&g.cmd.join(" ")).0
            );
        }
    }
    Ok(())
}

/// One key's effective value, `None` for an optional key that is unset.
/// `Err` is an unknown key, and the message names the known ones.
fn get_value(config: &Config, key: &str) -> Result<Option<String>, String> {
    Ok(match key {
        "default_ui" => Some(config.default_ui.label().into()),
        "update_mode" => Some(config.update_mode.label().into()),
        "thresholds.warn_at" => Some(config.thresholds.warn_at.to_string()),
        "thresholds.block_at" => Some(config.thresholds.block_at.to_string()),
        "repo.name" => Some(config.repo.name.clone()),
        "repo.path" => Some(config.repo.path.clone()),
        "repo.server" => config.repo.server.clone(),
        _ => return Err(unknown_key(key)),
    })
}

/// Apply one `set` to an in-memory config. Pure — no file is touched — and
/// the returned phrase is what the verb reports. Whole-config validation is
/// [`save`]'s round-trip; what is checked here is only that the value parses
/// as the kind of thing the key holds.
fn set_value(config: &mut Config, key: &str, value: &str) -> Result<String, String> {
    match key {
        "default_ui" => {
            config.default_ui = match value {
                "cli" => Ui::Cli,
                "tui" => Ui::Tui,
                _ => {
                    return Err(format!(
                        "default_ui can be \"cli\" or \"tui\" — not {:?}",
                        visible_line(value).0
                    ))
                }
            };
        }
        "update_mode" => {
            config.update_mode = match value {
                "auto" => Mode::Auto,
                "semi" => Mode::Semi,
                "manual" => Mode::Manual,
                _ => {
                    return Err(format!(
                        "update_mode can be \"auto\", \"semi\" or \"manual\" — not {:?}",
                        visible_line(value).0
                    ))
                }
            };
        }
        "thresholds.warn_at" | "thresholds.block_at" => {
            let n: u8 = value.parse().map_err(|_| {
                format!(
                    "{key} is a grade number 0-255 — not {:?}",
                    visible_line(value).0
                )
            })?;
            if key == "thresholds.warn_at" {
                config.thresholds.warn_at = n;
            } else {
                config.thresholds.block_at = n;
            }
        }
        "repo.name" => config.repo.name = value.into(),
        "repo.path" => config.repo.path = value.into(),
        // The validator's own advice for an empty server is "omit the key
        // instead", so an empty value here is how you omit it.
        "repo.server" => {
            config.repo.server = if value.is_empty() {
                None
            } else {
                Some(value.into())
            };
            if value.is_empty() {
                return Ok("repo.server = (unset) — the repo is local-only again".into());
            }
        }
        _ => return Err(unknown_key(key)),
    }
    Ok(format!("{key} = {}", visible_line(value).0))
}

fn unknown_key(key: &str) -> String {
    format!(
        "unknown key {:?} — the keys are {}. Graders are registered by \
         `pacrat setup` and shown by `pacrat config list`, not set here.",
        visible_line(key).0,
        SETTABLE.join(", ")
    )
}

// ------------------------------------------------------------------ the file

/// Where the config lives. One spelling, shared with `Ctx::resolve`, so the
/// reader and the writer can never disagree about which file this is.
pub fn path() -> Result<PathBuf, String> {
    let base = match env::var_os("XDG_CONFIG_HOME") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env::var_os("HOME").ok_or("HOME is not set")?).join(".config"),
    };
    Ok(base.join("pacrat").join("config.toml"))
}

/// The whole file, from a `Config`. `written_at` is a parameter rather than
/// a clock call so the rendering stays testable as data.
pub fn render(config: &Config, written_at: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Written by pacrat on {written_at}.\n"));
    s.push_str("# `pacrat config list/get/set` reads and changes it; `pacrat setup` asks\n");
    s.push_str("# the first-run questions. Editing by hand works too, but the next time\n");
    s.push_str("# pacrat writes this file it keeps only the keys it knows.\n\n");
    s.push_str(&format!("default_ui = \"{}\"\n", config.default_ui.label()));
    s.push_str(&format!(
        "update_mode = \"{}\"\n\n",
        config.update_mode.label()
    ));
    s.push_str("[thresholds]\n");
    s.push_str(&format!("warn_at = {}\n", config.thresholds.warn_at));
    s.push_str(&format!("block_at = {}\n\n", config.thresholds.block_at));
    s.push_str("[repo]\n");
    s.push_str(&format!("name = {}\n", toml_str(&config.repo.name)));
    s.push_str(&format!("path = {}\n", toml_str(&config.repo.path)));
    if let Some(server) = &config.repo.server {
        s.push_str(&format!("server = {}\n", toml_str(server)));
    }
    for g in &config.graders {
        s.push_str("\n[[graders]]\n");
        s.push_str(&format!("name = {}\n", toml_str(&g.name)));
        s.push_str(&format!(
            "cmd = [{}]\n",
            g.cmd
                .iter()
                .map(|a| toml_str(a))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        s.push_str(&format!("timeout_s = {}\n", g.timeout_s));
        if let Some(scale) = g.scale {
            s.push_str(&format!(
                "scale = {{ min = {}, max = {} }}\n",
                scale.min, scale.max
            ));
        }
    }
    s
}

/// A TOML basic string. The repo fields are validated too tightly to need
/// this, but a grader's cmd is not — it can legally hold quotes, backslashes
/// and anything else exec would pass through — and a renderer that only
/// works for polite values is a renderer that corrupts the file the first
/// time a value is not.
fn toml_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render, prove it loads, then write — in that order, so nothing that
/// `Config::from_toml` would refuse can ever reach the disk. The write is a
/// sibling temp file and a rename, `Ctx::write_atomic`'s rule for the same
/// reason: a crash mid-write must not leave the host half-configured.
pub fn save(config: &Config) -> Result<PathBuf, String> {
    let text = render(config, &crate::out::now_rfc3339());
    Config::from_toml(&text)
        .map_err(|e| format!("refusing to write a config pacrat could not read back: {e}"))?;
    let path = path()?;
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".config.toml.{}.new", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()
    };
    write().map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pacrat_core::config::Grader;
    use pacrat_core::grading::Scale;

    /// The writer's real contract: whatever it renders, the load path reads
    /// back as the same config — graders, scale pins and all.
    #[test]
    fn render_round_trips_through_the_load_path() {
        let mut config = Config {
            default_ui: Ui::Tui,
            update_mode: Mode::Auto,
            ..Config::default()
        };
        config.thresholds.warn_at = 1;
        config.repo.server = Some("https://example.invalid/repo".into());
        config.graders.push(Grader {
            name: "yay-friend".into(),
            cmd: vec![
                "/opt/pacrat/contrib/graders/yay-friend-grade".into(),
                "--package".into(),
                "{package}".into(),
                "--tree".into(),
                "{tree}".into(),
                "--commit".into(),
                "{commit}".into(),
            ],
            timeout_s: 600,
            scale: Some(Scale { min: 0, max: 4 }),
        });

        let text = render(&config, "2026-08-10T12:00:00Z");
        assert!(text.starts_with("# Written by pacrat on 2026-08-10T12:00:00Z."));
        let read_back = Config::from_toml(&text).expect("pacrat wrote a file it cannot read");
        assert_eq!(read_back, config);
    }

    /// A grader argument is the one field that can hold TOML's own syntax;
    /// escaping is what keeps the file loadable.
    #[test]
    fn hostile_grader_arguments_survive_the_round_trip() {
        let mut config = Config::default();
        config.graders.push(Grader {
            name: "awkward".into(),
            cmd: vec![
                "/bin/grader".into(),
                "a \"quoted\" thing".into(),
                "back\\slash".into(),
                "{package}".into(),
            ],
            timeout_s: 30,
            scale: None,
        });
        let read_back = Config::from_toml(&render(&config, "now")).unwrap();
        assert_eq!(read_back.graders, config.graders);
    }

    // ---- get / set ----

    #[test]
    fn every_settable_key_gets_and_sets() {
        let mut config = Config::default();
        for (key, value) in [
            ("default_ui", "tui"),
            ("update_mode", "manual"),
            ("thresholds.warn_at", "1"),
            ("thresholds.block_at", "3"),
            ("repo.name", "fleet"),
            ("repo.path", "/srv/fleet/repo"),
            ("repo.server", "https://example.invalid/r"),
        ] {
            let said = set_value(&mut config, key, value).expect(key);
            assert!(said.contains(key), "{said}");
            assert_eq!(
                get_value(&config, key).unwrap().as_deref(),
                Some(value),
                "{key} did not read back"
            );
        }
        // And the rendered result of all seven still loads.
        assert!(Config::from_toml(&render(&config, "now")).is_ok());
    }

    #[test]
    fn an_unknown_key_is_refused_naming_the_known_ones() {
        let mut config = Config::default();
        let err = set_value(&mut config, "nonsense", "x").unwrap_err();
        for key in SETTABLE {
            assert!(err.contains(key), "{key} missing from: {err}");
        }
        assert!(err.contains("Graders"), "no word about graders: {err}");
        assert!(get_value(&config, "nonsense").is_err());
    }

    #[test]
    fn a_value_of_the_wrong_kind_is_refused_with_the_choices() {
        let mut config = Config::default();
        let err = set_value(&mut config, "default_ui", "gui").unwrap_err();
        assert!(err.contains("\"cli\" or \"tui\""), "{err}");
        let err = set_value(&mut config, "update_mode", "yolo").unwrap_err();
        assert!(err.contains("\"auto\""), "{err}");
        let err = set_value(&mut config, "thresholds.warn_at", "high").unwrap_err();
        assert!(err.contains("0-255"), "{err}");
        // Nothing half-applied: the config is still the default.
        assert_eq!(config, Config::default());
    }

    /// An empty value is how `repo.server` is unset — the validator refuses
    /// an empty *string* precisely so nothing else can mean this.
    #[test]
    fn an_empty_server_unsets_the_key() {
        let mut config = Config::default();
        set_value(&mut config, "repo.server", "https://x.invalid/r").unwrap();
        assert!(config.repo.server.is_some());
        let said = set_value(&mut config, "repo.server", "").unwrap();
        assert!(config.repo.server.is_none());
        assert!(said.contains("unset"), "{said}");
        assert_eq!(get_value(&config, "repo.server").unwrap(), None);
        // The rendered file simply has no server line.
        assert!(!render(&config, "now").contains("server"));
    }

    /// `set` refuses hostile values the same way the load path would — the
    /// round-trip in `save` is the enforcement, exercised here directly.
    #[test]
    fn a_value_the_load_path_would_refuse_never_renders_loadable() {
        let mut config = Config::default();
        // set_value itself accepts the string; the round-trip is the check.
        set_value(&mut config, "repo.name", "x\nSigLevel = Never").unwrap();
        assert!(
            Config::from_toml(&render(&config, "now")).is_err(),
            "a smuggled directive survived the round-trip"
        );
    }
}
