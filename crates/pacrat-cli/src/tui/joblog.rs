//! What this pacrat has done, and what it is doing right now.
//!
//! ADR-001 decision 3 settles that jobs run in-process only: no daemon, no
//! queue that outlives the process, headless warmth is `pacrat update` on a
//! timer. So this is not a job *scheduler* — there is nothing to schedule.
//! It is the record the jobs screen renders (mockup §7), and it holds two
//! things:
//!
//! * **Operations**, each pushed as *running* before the work starts and
//!   closed when it ends. Pushed first because the frame that shows a
//!   fetch in progress is drawn before the fetch is asked for — the shell's
//!   draw-then-work order is the whole of pacrat's loading machinery, and an
//!   operation recorded afterwards would only ever appear finished.
//! * **The chatter those operations produced**, verbatim, from
//!   [`crate::out`]'s capture. Those lines are where the argv is: `git.rs`
//!   prints `run  git clone -- …` before every call it makes, and with the
//!   terminal owned by ratatui the only place that line can honestly land is
//!   here. "Every external call is shown verbatim" (mockup §7) is this
//!   buffer.
//!
//! Both are bounded. A session left open for a week must not turn a record
//! of what happened into a reason the machine runs out of memory, and the
//! oldest lines are the ones nobody is going to scroll back to.

use std::time::{Duration, Instant};

/// The most log lines kept. Twenty thousand is far more than a session
/// produces and small enough to be uninteresting as an allocation.
const LINE_CAP: usize = 20_000;

/// The most operations kept.
const JOB_CAP: usize = 500;

/// How an operation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Running,
    /// Finished, with the one line worth putting beside it.
    Done(String),
    /// Did not finish. The text is the error as the verb worded it — pacrat
    /// does not paraphrase its own failures into something friendlier.
    Failed(String),
}

/// A handle to one operation.
///
/// A wrapper over a counter rather than a position in the vector, and the
/// difference is a bug that was here: handles used to be indices, and
/// `start` evicts from the front once the cap is reached, so every
/// outstanding handle silently shifted by one. An operation still in flight
/// across an eviction would have closed *the job after it* — writing one
/// operation's outcome onto another's row, which is precisely the lie a log
/// exists not to tell. Ids are never reused, so a stale handle finds nothing
/// instead of finding somebody else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handle(u64);

pub struct Job {
    id: Handle,
    /// What was asked for, in the words the screen shows.
    pub what: String,
    pub started: Instant,
    pub took: Option<Duration>,
    pub outcome: Outcome,
}

impl Job {
    pub fn running(&self) -> bool {
        self.outcome == Outcome::Running
    }
}

#[derive(Default)]
pub struct Log {
    jobs: Vec<Job>,
    lines: Vec<String>,
    /// The next id. Monotonic and never reused; see [`Handle`].
    next: u64,
}

impl Log {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an operation about to start, and return its handle.
    pub fn start(&mut self, what: impl Into<String>) -> Handle {
        if self.jobs.len() >= JOB_CAP {
            self.jobs.remove(0);
        }
        let id = Handle(self.next);
        self.next += 1;
        self.jobs.push(Job {
            id,
            what: what.into(),
            started: Instant::now(),
            took: None,
            outcome: Outcome::Running,
        });
        id
    }

    /// Close an operation, and take whatever it said on the way.
    ///
    /// Looked up by id rather than by position, and a handle that finds
    /// nothing closes nothing: an operation can be evicted by the cap while
    /// it is still in flight, and neither a panic nor somebody else's row
    /// getting this one's outcome is an acceptable way to discover that.
    pub fn finish(&mut self, handle: Handle, result: Result<String, String>) {
        self.absorb();
        let Some(job) = self.jobs.iter_mut().find(|job| job.id == handle) else {
            return;
        };
        job.took = Some(job.started.elapsed());
        job.outcome = match result {
            Ok(note) => Outcome::Done(note),
            Err(e) => Outcome::Failed(e),
        };
    }

    /// Put a line of pacrat's own into the log.
    ///
    /// For the things that are worth a reader seeing and are nobody's
    /// command — a defensive repaint that did not work, say. It goes through
    /// the same buffer as the captured chatter so the order is the order
    /// things happened.
    pub fn note(&mut self, line: impl Into<String>) {
        self.absorb();
        self.lines.push(line.into());
    }

    /// Pull whatever the CLI verbs have said since the last time.
    pub fn absorb(&mut self) {
        self.lines.extend(crate::out::capture_take());
        let over = self.lines.len().saturating_sub(LINE_CAP);
        if over > 0 {
            self.lines.drain(..over);
        }
    }

    /// Newest last, as they were said.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Newest first: the jobs screen leads with what is happening now.
    pub fn jobs(&self) -> impl Iterator<Item = &Job> {
        self.jobs.iter().rev()
    }

    pub fn running(&self) -> usize {
        self.jobs.iter().filter(|j| j.running()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty() && self.lines.is_empty()
    }
}

/// `01:12`, the mockup's elapsed column.
pub fn elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_operation_is_running_until_it_is_closed() {
        let mut log = Log::new();
        let handle = log.start("review mdcat");
        assert_eq!(log.running(), 1);
        assert!(log.jobs().next().unwrap().running());

        log.finish(handle, Ok("2 files changed".into()));
        assert_eq!(log.running(), 0);
        let job = log.jobs().next().unwrap();
        assert_eq!(job.outcome, Outcome::Done("2 files changed".into()));
        assert!(job.took.is_some());
    }

    /// A failure keeps the verb's own words. A jobs screen that showed
    /// "failed" and nothing else would send the reader to a terminal to find
    /// out what pacrat already knew.
    #[test]
    fn a_failure_keeps_what_went_wrong() {
        let mut log = Log::new();
        let handle = log.start("review yay");
        log.finish(handle, Err("git clone timed out after 120s".into()));
        assert_eq!(
            log.jobs().next().unwrap().outcome,
            Outcome::Failed("git clone timed out after 120s".into())
        );
    }

    /// Newest first, because the question the screen answers is "what is
    /// pacrat running right now".
    #[test]
    fn the_newest_operation_is_the_first_one_shown() {
        let mut log = Log::new();
        log.start("first");
        log.start("second");
        let order: Vec<&str> = log.jobs().map(|j| j.what.as_str()).collect();
        assert_eq!(order, ["second", "first"]);
    }

    /// A session left open for a week is a record, not a leak.
    #[test]
    fn both_buffers_are_bounded() {
        let mut log = Log::new();
        for i in 0..JOB_CAP + 50 {
            log.start(format!("job {i}"));
        }
        assert_eq!(log.jobs().count(), JOB_CAP);
        // The oldest went, and the newest is still the newest.
        assert_eq!(
            log.jobs().next().unwrap().what,
            format!("job {}", JOB_CAP + 49)
        );

        log.lines = (0..LINE_CAP + 100).map(|i| i.to_string()).collect();
        log.absorb();
        assert_eq!(log.lines().len(), LINE_CAP);
        assert_eq!(
            log.lines()[0],
            "100",
            "the oldest lines are the ones that go"
        );
    }

    /// A handle whose job has already been pushed out by the cap closes
    /// nothing rather than panicking **or closing somebody else's**.
    ///
    /// The second half is the one that matters and the one the old version
    /// of this test only claimed. Handles were vector indices; eviction
    /// shifts every index down by one, so an operation in flight across an
    /// eviction closed the row after it — one operation's outcome written
    /// onto another's name. The assertions below are about that: after
    /// closing a stale handle, everything still in the log must be untouched.
    #[test]
    fn closing_a_job_that_aged_out_closes_nothing_at_all() {
        let mut log = Log::new();
        let stale = log.start("early");
        for i in 0..JOB_CAP + 10 {
            log.start(format!("later {i}"));
        }
        assert!(
            log.jobs().all(|j| j.what != "early"),
            "the job under test was supposed to be evicted"
        );

        log.finish(stale, Ok("done".into()));
        assert_eq!(
            log.running(),
            JOB_CAP,
            "a stale handle closed a job it did not own"
        );
        assert!(
            log.jobs().all(|j| j.outcome == Outcome::Running),
            "somebody else's row took the stale handle's outcome"
        );
    }

    /// The live case of the same thing: a handle held across an eviction
    /// still closes its own job, and only its own.
    #[test]
    fn a_handle_survives_eviction_of_the_jobs_around_it() {
        let mut log = Log::new();
        // Fill to the cap, then take a handle to the newest — it will still
        // be present after further starts push the front out.
        for i in 0..JOB_CAP {
            log.start(format!("filler {i}"));
        }
        let mine = log.start("mine");
        for i in 0..10 {
            log.start(format!("after {i}"));
        }

        log.finish(mine, Ok("finished".into()));
        let closed: Vec<&Job> = log.jobs().filter(|j| !j.running()).collect();
        assert_eq!(closed.len(), 1, "more than one job was closed");
        assert_eq!(closed[0].what, "mine");
        assert_eq!(closed[0].outcome, Outcome::Done("finished".into()));
    }

    #[test]
    fn elapsed_reads_as_the_mockups_clock() {
        assert_eq!(elapsed(Duration::from_secs(72)), "01:12");
        assert_eq!(elapsed(Duration::from_secs(0)), "00:00");
        assert_eq!(elapsed(Duration::from_secs(277)), "04:37");
    }
}
