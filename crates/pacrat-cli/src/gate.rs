//! The setup gate (ADR-003): a host that is not on the serving model runs
//! `pacrat about` and `pacrat setup`, and nothing else. Every other verb —
//! the read-only ones and the TUI included — answers with one line naming
//! what is missing, and exits 1. The gate asks about the *system* (the repo,
//! the pacman.conf section, the guard), not about the config file: the
//! system is what makes acting safe.
//!
//! `PACRAT_SETUP_GATE=off` skips the check and announces itself on every run
//! it affects — the same posture as the guard's `PACRAT_BYPASS`: this is
//! accident resistance for a human, not access control, and a script that
//! states its intent in the environment has stated it. The demo sandbox and
//! the integration tests are that script.
//!
//! The decision is pure — setup state plus environment in, one of three
//! answers out — so it is testable without reading this machine's
//! /etc/pacman.conf.

use std::io::IsTerminal;

use crate::setup::State;

/// What the gate says about this run.
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    /// The host is set up; run normally and say nothing.
    Open,
    /// Not set up, but `PACRAT_SETUP_GATE=off`: run, announcing the bypass
    /// first. The line is dimmed at a terminal so it reads as an aside.
    Bypassed { announce: String },
    /// Not set up and no bypass: the one-line refusal. Exit 1.
    Refused { line: String },
}

/// State plus environment in, answer out. `env` is `PACRAT_SETUP_GATE` as
/// found (or not found); only the value `off` — any letter case — opens the
/// gate, so a typo refuses rather than quietly bypassing.
pub fn decide(state: &State, env: Option<&str>) -> Gate {
    if state.complete() {
        return Gate::Open;
    }
    if env.is_some_and(|v| v.eq_ignore_ascii_case("off")) {
        return Gate::Bypassed {
            announce: "setup gate off (PACRAT_SETUP_GATE) — this host is not fully set up".into(),
        };
    }
    Gate::Refused {
        line: format!(
            "this host is not set up — missing {}. Run `pacrat setup` to finish.",
            state.missing().join(" + ")
        ),
    }
}

/// Print the bypass announcement, dimmed when someone is looking at it.
/// stderr, because stdout may belong to `--json` or to a pipe.
pub fn announce(line: &str) {
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[2m{line}\x1b[0m");
    } else {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `State` built from data, never from this machine's /etc.
    fn state(repo_dir: bool, repo_db: bool, conf: bool, hook: bool, script: bool) -> State {
        State {
            repo_dir,
            repo_db,
            conf_section: conf,
            hook,
            script,
        }
    }

    #[test]
    fn a_set_up_host_is_open_and_silent_even_with_the_env_set() {
        let complete = state(true, true, true, true, true);
        assert_eq!(decide(&complete, None), Gate::Open);
        // The announce line exists to mark runs the gate would have refused;
        // a set-up host is not one, so the env changes nothing.
        assert_eq!(decide(&complete, Some("off")), Gate::Open);
    }

    #[test]
    fn an_unfinished_host_is_refused_with_the_missing_pieces_named() {
        let Gate::Refused { line } = decide(&state(true, true, false, false, true), None) else {
            panic!("an incomplete host must be refused");
        };
        assert!(line.contains("pacman.conf"), "{line}");
        assert!(line.contains("guard"), "{line}");
        assert!(!line.contains("repo +"), "repo is present: {line}");
        assert!(
            line.contains("pacrat setup"),
            "no pointer at the fix: {line}"
        );
    }

    #[test]
    fn a_fresh_machine_is_refused_naming_everything() {
        let Gate::Refused { line } = decide(&state(false, false, false, false, false), None) else {
            panic!("an untouched host must be refused");
        };
        assert!(line.contains("repo + pacman.conf + guard"), "{line}");
    }

    #[test]
    fn off_bypasses_and_announces() {
        let incomplete = state(false, false, false, false, false);
        for value in ["off", "OFF", "Off"] {
            let Gate::Bypassed { announce } = decide(&incomplete, Some(value)) else {
                panic!("{value:?} should bypass");
            };
            assert!(announce.contains("PACRAT_SETUP_GATE"), "{announce}");
            assert!(announce.contains("not fully set up"), "{announce}");
        }
    }

    /// Only `off` opens the gate. `1`, `true`, an empty value — anything a
    /// hand might type meaning something else — still refuses.
    #[test]
    fn any_other_value_still_refuses() {
        let incomplete = state(false, false, false, false, false);
        for value in ["1", "true", "on", "no", ""] {
            assert!(
                matches!(decide(&incomplete, Some(value)), Gate::Refused { .. }),
                "{value:?} must not open the gate"
            );
        }
    }
}
