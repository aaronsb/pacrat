//! Hold-to-confirm: the TUI's half of ADR-001 decision 2.
//!
//! The CLI's friction for overriding a BLOCK is authoring a justification —
//! `--override-block --reason "…"`. That friction does not survive being
//! moved to a keybinding: `o` is one byte, and one byte is what a paste, a
//! macro, a stuck key or a `yes o |` supplies. The ADR's answer is that the
//! TUI's affordance must require *holding* a key until a progress bar fills,
//! and it is specific about why: "a single keypress does nothing, so no
//! script or paste can trip it".
//!
//! This is the state machine behind that bar. It is a plain struct over
//! `Instant`s with no terminal in it, because the property it exists to have
//! — that a burst of input cannot satisfy it — is exactly the kind of claim
//! that has to be testable without a keyboard.
//!
//! ## What "a human held a key" is made of
//!
//! A terminal in the mode pacrat runs in does not report key *releases*.
//! Holding a key produces auto-repeat *presses* at whatever rate the
//! terminal repeats, after an initial delay. So "held" is reconstructed from
//! three conditions that a run of press events must satisfy together, and
//! each one closes a different door:
//!
//! 1. **Real elapsed time.** The first and last press of the run must be
//!    [`HOLD`] apart *by the clock*. A thousand buffered bytes that arrive
//!    in one read and are dispatched in microseconds form a run microseconds
//!    long, however many of them there are. Counting events alone would fall
//!    to exactly that; counting time is what makes the count irrelevant.
//! 2. **No gap.** Consecutive presses must be within [`GAP`]. Letting go
//!    stops the auto-repeat, the gap opens, and the run is discarded — which
//!    is what makes releasing early *reset* rather than pause.
//! 3. **Not faster than a keyboard.** Consecutive presses must be at least
//!    [`FLOOR`] apart. See below: this is the condition that actually closes
//!    the paste, and it was added after watching a paste nearly get through.
//! 4. **More than one press.** [`MIN_EVENTS`] is implied by 1 and 2 (three
//!    seconds cannot be crossed in gaps of 600ms in fewer than six steps)
//!    and is stated anyway, because "a single keypress does nothing" is the
//!    sentence in the ADR and it should be a line in the code rather than an
//!    emergent property of two constants somebody may later tune.
//!
//! The gap is generous — 600ms — because the *initial* auto-repeat delay is
//! a per-terminal setting that is commonly 500ms, and a lock that a normal
//! keyboard cannot open is a lock that gets replaced with a keybinding. A
//! terminal with auto-repeat off is why the overlay tells the reader they
//! may tap the key instead of holding it: tapping faster than every 600ms
//! satisfies the same conditions, and it is the same deliberate three
//! seconds of a human's attention either way.
//!
//! ## Why elapsed time alone was not enough
//!
//! The first version of this had conditions 1, 2 and 4, and the reasoning
//! above about bursts is *wrong in one case* — the case that matters. A
//! terminal's input buffer holds only a few kilobytes, so a paste of forty
//! thousand spaces is not delivered in one read: the application drains it
//! over many turns of its event loop, and each turn costs a render. At a
//! millisecond a frame, forty thousand events take forty seconds of real
//! wall-clock time, arriving steadily and well inside the gap. Every one of
//! conditions 1, 2 and 4 is satisfied by a long enough paste, because the
//! *application itself* is what spreads it over real time.
//!
//! Driving a paste of four thousand spaces at the bar filled it a third of
//! the way. That is what [`FLOOR`] is for, and it is a sharper signal than
//! duration: a keyboard's fastest auto-repeat is around forty a second, and
//! a drained buffer arrives as fast as the loop can turn. Eight milliseconds
//! sits in the gap between "no keyboard is this fast" and "no render is this
//! slow", and a press that beats it does not shorten the run — it *restarts*
//! it, so a flood never accumulates however long it lasts.
//!
//! The call site adds a third check of the same kind, because two cheap
//! independent tests beat one clever one: it asks the terminal whether more
//! input is *already waiting* behind this keystroke. A person holding a key
//! has produced exactly one event and the next is 30ms away; a paste always
//! has thousands queued behind it.
//!
//! ## What this does not do
//!
//! It does not decide whether the caller is a human. Nothing can. It makes
//! the cheap ways of not being one fail, and it is paired at the call site
//! with a check that stdin is a terminal — a hold satisfied by something
//! that is not a tty is not a hold, however well-timed.

use std::time::{Duration, Instant};

/// How long the key must be held. Three seconds is long enough to be a
/// decision and short enough that somebody who means it will finish.
pub const HOLD: Duration = Duration::from_secs(3);

/// The longest silence a single hold may contain before it is a new one.
pub const GAP: Duration = Duration::from_millis(600);

/// The shortest interval between two presses that a keyboard could have
/// produced.
///
/// Below this, the events are being drained from a buffer rather than typed:
/// the fastest auto-repeat a terminal offers is around forty a second (25ms),
/// and a paste arrives as fast as the event loop can turn, which is a render
/// — well under a millisecond on any screen this draws. Eight milliseconds is
/// the middle of that gap. See the module note for the paste that made this
/// necessary.
pub const FLOOR: Duration = Duration::from_millis(8);

/// The fewest presses a satisfied hold can be made of. See the module note:
/// implied by `HOLD` and `GAP`, said out loud because it is the requirement.
pub const MIN_EVENTS: usize = 6;

/// One run of presses: when it began, when it was last fed, how many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    started: Instant,
    last: Instant,
    events: usize,
}

/// The hold, as far as it has got.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hold {
    run: Option<Run>,
}

impl Hold {
    pub fn new() -> Self {
        Self::default()
    }

    /// The key went down (or repeated). Returns whether the hold is now
    /// satisfied.
    ///
    /// `flooded` is the caller's answer to "was there already more input
    /// waiting behind this one" — true means a buffer is being drained, and
    /// no amount of that is a person holding a key.
    ///
    /// The answer is computed here, at the press, rather than being read off
    /// later by whoever asks: the question is "was this key held until
    /// now", and `now` is this event's own timestamp. A frame that happened
    /// to be drawn a second after the last press must not be able to answer
    /// yes on its behalf.
    pub fn press(&mut self, now: Instant, flooded: bool) -> bool {
        let fresh = Run {
            started: now,
            last: now,
            events: 1,
        };
        self.run = Some(match self.run {
            // Faster than a keyboard, or arriving with a queue behind it:
            // this is a buffer being drained. It *restarts* the run rather
            // than merely being dropped, so a flood that lasts a minute
            // accumulates nothing — the alternative was a paste that filled
            // the bar a third of the way in four seconds.
            _ if flooded => fresh,
            Some(run) if now.duration_since(run.last) < FLOOR => fresh,
            // Within the gap: the same hold, continuing.
            Some(run) if now.duration_since(run.last) <= GAP => Run {
                started: run.started,
                last: now,
                events: run.events + 1,
            },
            // A gap, or nothing yet: this press is the start of a hold, and
            // whatever came before it is not part of this one.
            _ => fresh,
        });
        self.satisfied()
    }

    /// A frame passed with no press. Discards a run that has gone quiet, so
    /// the bar the reader is watching empties when they let go.
    pub fn tick(&mut self, now: Instant) {
        if self
            .run
            .is_some_and(|run| now.duration_since(run.last) > GAP)
        {
            self.run = None;
        }
    }

    /// Is the hold complete?
    ///
    /// All three conditions, as one expression, so there is no path that
    /// satisfies two of them and is treated as done.
    pub fn satisfied(&self) -> bool {
        self.run.is_some_and(|run| {
            run.events >= MIN_EVENTS && run.last.duration_since(run.started) >= HOLD
        })
    }

    /// How full the bar is, 0.0 to 1.0.
    ///
    /// Measured from the last *press*, not from now: progress is something
    /// the reader makes by holding the key, so a bar that kept creeping
    /// while nothing was pressed would be promising something the hold will
    /// not honour. It stops where they stopped, until [`Hold::tick`] empties
    /// it.
    pub fn filled(&self) -> f64 {
        self.run.map_or(0.0, |run| {
            let held = run.last.duration_since(run.started).as_secs_f64();
            (held / HOLD.as_secs_f64()).clamp(0.0, 1.0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hold driven at a fixed rate, the way auto-repeat drives one.
    fn held(every: Duration, presses: usize) -> (Hold, Instant) {
        feed(every, presses, false)
    }

    fn feed(every: Duration, presses: usize, flooded: bool) -> (Hold, Instant) {
        let start = Instant::now();
        let mut hold = Hold::new();
        for step in 0..presses {
            hold.press(start + every * step as u32, flooded);
        }
        (hold, start + every * presses.saturating_sub(1) as u32)
    }

    /// The ordinary case: a key held down, repeating at 30ms, for the three
    /// seconds the bar asks for.
    #[test]
    fn a_key_held_for_the_whole_duration_satisfies_it() {
        let (hold, _) = held(Duration::from_millis(30), 101);
        assert!(hold.satisfied());
        assert_eq!(hold.filled(), 1.0);
    }

    /// The requirement in the ADR's own words: a single keypress does
    /// nothing. Neither does a short one.
    #[test]
    fn one_press_does_nothing_and_neither_does_a_brief_run() {
        let mut hold = Hold::new();
        let now = Instant::now();
        assert!(!hold.press(now, false));
        assert!(!hold.satisfied());
        assert_eq!(hold.filled(), 0.0);

        // Two seconds of a three-second hold is not two-thirds of a
        // permission.
        let (partial, _) = held(Duration::from_millis(30), 67);
        assert!(!partial.satisfied());
        assert!(partial.filled() > 0.6 && partial.filled() < 1.0);
    }

    /// **The pipe.** A thousand buffered events delivered in a burst is the
    /// shape of every way of not being a human that costs nothing: a paste,
    /// `yes o |`, a macro, an autorepeat storm replayed out of a file. They
    /// all arrive with no time between them, and no number of them is a
    /// hold.
    #[test]
    fn a_burst_of_events_is_never_a_hold_however_many_there_are() {
        let now = Instant::now();
        let mut hold = Hold::new();
        for _ in 0..10_000 {
            assert!(!hold.press(now, false), "a burst satisfied the hold");
        }
        assert!(!hold.satisfied());
        assert_eq!(hold.filled(), 0.0, "and it filled nothing");

        // Nor does a burst that takes a realistic few milliseconds to
        // dispatch rather than literally zero.
        let (fast, _) = held(Duration::from_micros(50), 10_000);
        assert!(!fast.satisfied());
    }

    /// **The paste that nearly worked.** A terminal's input buffer is a few
    /// kilobytes, so a very large paste is not one read — the application
    /// drains it over thousands of turns of its event loop, each costing a
    /// render, and that spreads it across real wall-clock time. Elapsed
    /// time, the gap and the event count are *all* satisfied by a long
    /// enough paste; only the rate floor is not.
    ///
    /// Forty thousand events at a millisecond each is forty seconds of
    /// clock, more than ten times the hold.
    #[test]
    fn a_paste_drained_over_real_time_still_never_fills_it() {
        let (drained, _) = held(Duration::from_millis(1), 40_000);
        assert!(
            !drained.satisfied(),
            "a paste spread over 40 seconds by the event loop satisfied the hold"
        );
        assert_eq!(drained.filled(), 0.0, "and it must accumulate nothing");

        // Just under the floor is still a machine; just over it is a
        // plausible — if implausibly fast — keyboard.
        let (under, _) = held(FLOOR - Duration::from_millis(1), 4_000);
        assert!(!under.satisfied());
        let (over, _) = held(FLOOR + Duration::from_millis(2), 400);
        assert!(over.satisfied(), "a fast auto-repeat must still work");
    }

    /// The call site's independent check: a keystroke with more input
    /// already queued behind it is part of a flood, whatever its timing
    /// looks like. It restarts the run rather than being ignored, so a flood
    /// cannot accumulate between the events that happen to look well-spaced.
    #[test]
    fn input_with_a_queue_behind_it_is_never_a_hold() {
        let (flooded, _) = feed(Duration::from_millis(30), 200, true);
        assert!(!flooded.satisfied());
        assert_eq!(flooded.filled(), 0.0);

        // And a genuine hold interrupted by one flooded event starts over
        // rather than carrying its progress across.
        let start = Instant::now();
        let mut hold = Hold::new();
        for step in 0..90 {
            hold.press(start + Duration::from_millis(30) * step, false);
        }
        assert!(hold.filled() > 0.8);
        hold.press(start + Duration::from_millis(30 * 90), true);
        assert_eq!(hold.filled(), 0.0);
    }

    /// Releasing resets. The gap is what a release looks like from here —
    /// the repeat stops — and a hold that could be assembled out of two
    /// separate presses minutes apart would not be a hold.
    #[test]
    fn letting_go_discards_the_run_rather_than_pausing_it() {
        let start = Instant::now();
        let mut hold = Hold::new();
        // Nearly there…
        for step in 0..90 {
            hold.press(start + Duration::from_millis(30) * step, false);
        }
        assert!(hold.filled() > 0.8);

        // …and let go. The next press is a new hold, not the last one
        // resumed, so it starts from nothing however long ago the first was.
        let after = start + Duration::from_secs(60);
        assert!(!hold.press(after, false));
        assert_eq!(hold.filled(), 0.0, "the old run was carried over");
        assert!(!hold.satisfied());
    }

    /// The bar the reader is watching has to empty when they stop, or it is
    /// telling them they have made progress they have not.
    #[test]
    fn a_run_that_goes_quiet_is_dropped_by_the_frame_clock() {
        let start = Instant::now();
        let mut hold = Hold::new();
        hold.press(start, false);
        hold.press(start + Duration::from_millis(30), false);
        assert!(hold.filled() > 0.0);

        hold.tick(start + Duration::from_millis(40));
        assert!(
            hold.filled() > 0.0,
            "a frame inside the gap changes nothing"
        );
        hold.tick(start + GAP + Duration::from_millis(100));
        assert_eq!(hold.filled(), 0.0);
        assert!(!hold.satisfied());
    }

    /// Tapping is holding, for a terminal whose auto-repeat is off. The
    /// three conditions are about elapsed time and continuity, not about
    /// which mechanism produced the events — and a lock a normal keyboard
    /// cannot open is a lock somebody removes.
    #[test]
    fn tapping_inside_the_gap_works_and_tapping_slower_does_not() {
        // Ten taps, 350ms apart: 3.15 seconds, no gap over 600ms.
        let (tapped, _) = held(Duration::from_millis(350), 10);
        assert!(tapped.satisfied());

        // The same ten taps at a rate a person could not mistake for
        // holding: each one starts over.
        let (slow, _) = held(Duration::from_millis(900), 10);
        assert!(!slow.satisfied());
        assert_eq!(slow.filled(), 0.0);
    }

    /// The event floor, tested where the timing alone would pass it: two
    /// presses three seconds apart cross `HOLD` but are not a hold, and only
    /// `MIN_EVENTS` says so.
    ///
    /// It cannot be reached through `press` — a three-second silence is a
    /// gap, so the second press starts a new run — which is the belt-and-
    /// braces the constant exists to be. Constructed directly, because the
    /// question is what `satisfied` requires, not what `press` can build.
    #[test]
    fn crossing_the_duration_in_too_few_presses_is_not_a_hold() {
        let start = Instant::now();
        let sparse = Hold {
            run: Some(Run {
                started: start,
                last: start + HOLD + Duration::from_millis(1),
                events: MIN_EVENTS - 1,
            }),
        };
        assert!(!sparse.satisfied(), "{MIN_EVENTS} presses is the floor");

        let enough = Hold {
            run: Some(Run {
                events: MIN_EVENTS,
                ..sparse.run.unwrap()
            }),
        };
        assert!(enough.satisfied());
    }

    /// Dismissing the affordance and re-opening it starts from empty, which
    /// is why the caller builds a new one rather than keeping one around: a
    /// hold left half-made from last time would be three seconds of
    /// somebody's attention spent on a different question.
    #[test]
    fn a_fresh_hold_is_empty() {
        let fresh = Hold::new();
        assert!(!fresh.satisfied());
        assert_eq!(fresh.filled(), 0.0);
    }
}
