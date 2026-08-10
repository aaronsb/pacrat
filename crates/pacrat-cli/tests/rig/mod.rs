//! The TUI scenario rig (ADR-005): the real binary, a real pty, assertions
//! over the cell grid a real terminal would show.
//!
//! ## The two-layer authority split
//!
//! The fast in-process tests — keymap dispatch, viewport arithmetic, screen
//! state over ratatui's `TestBackend` — stay where they are and stay the
//! authority on their own logic. What none of them can see is the seam the
//! first human hit twice in one sitting: focus, geometry, and what the
//! terminal actually renders when a key is pressed. That seam is this
//! layer's. Scenarios here run `pacrat tui` under a pseudo-terminal at a
//! chosen geometry, inside the demo-grade sandbox ([`fixture`]), write keys
//! to the pty, feed the byte stream to a vt100 state machine, and assert on
//! the resulting grid — text, attributes, cursor. Pass/fail authority for
//! "does the key do what the screen advertises" lives here and nowhere
//! else.
//!
//! ## The record is dual
//!
//! Every checkpoint can [`snapshot`](Rig::snapshot) the grid to a
//! plain-text golden beside the scenarios (geometry-keyed, diffed in
//! review, re-blessed with `RIG_BLESS=1`), and every run emits an asciinema
//! v2 cast into `target/rig-casts/` — the golden answers "did it change",
//! the cast answers "what did a human watching it see" when a diff needs
//! judging.
//!
//! ## Waiting is by condition, never by sleep
//!
//! [`wait_for`](Rig::wait_for) polls the grid for a pattern under a hard
//! deadline, and it is the only wait the rig offers. The deadline is
//! deliberately generous — a slow CI runner is late, not wrong — and a
//! scenario that would need a fixed delay to pass is a bug in the scenario.

pub mod fixture;

use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use fixture::Fixture;

/// How long a condition may take before the scenario is declared failed.
/// Generous on purpose: everything the fixture answers is local, so the
/// only thing this bounds is a CI runner having a bad day, and a timeout
/// that fires early is a flake the rig itself introduced.
pub const DEADLINE: Duration = Duration::from_secs(20);

/// A live TUI under the rig's hands: the pty, the parsed screen, the cast.
pub struct Rig {
    name: String,
    cols: u16,
    rows: u16,
    parser: vt100::Parser,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    /// Raw pty output, ferried out of the blocking reader thread.
    rx: mpsc::Receiver<Vec<u8>>,
    cast: Cast,
}

/// Spawn `pacrat tui` at the given geometry inside the fixture's sandbox.
/// `name` keys the cast file and every golden this rig writes.
pub fn spawn(name: &str, (cols, rows): (u16, u16), fixture: &Fixture) -> Rig {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("rig: opening the pty");

    // The workspace's own binary — cargo builds it for integration tests
    // and names it in the environment, so the rig can never quietly test
    // whatever stale `pacrat` happens to be on PATH.
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_pacrat"));
    cmd.arg("tui");
    // Cleared, then rebuilt from the fixture alone: the `env -i` rule. An
    // inherited DOTFILES_DIR answering for the sandbox is exactly the
    // accident this line exists to prevent.
    cmd.env_clear();
    for (key, value) in fixture.env() {
        cmd.env(key, value);
    }
    cmd.cwd(&fixture.root);

    let child = pair
        .slave
        .spawn_command(cmd)
        .expect("rig: spawning pacrat in the pty");
    // The slave side belongs to the child now; holding our copy open would
    // keep the pty alive after the child dies and the reader would never
    // see EOF.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("rig: cloning the pty reader");
    let writer = pair.master.take_writer().expect("rig: taking the writer");

    // The pty read blocks, so it lives on its own thread and the scenario
    // thread drains a channel. The thread ends itself on EOF or error —
    // both mean the child is gone — so nobody joins it.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
            }
        }
    });

    Rig {
        name: name.to_string(),
        cols,
        rows,
        parser: vt100::Parser::new(rows, cols, 0),
        master: pair.master,
        child,
        writer,
        rx,
        cast: Cast::new(name, cols, rows),
    }
}

impl Rig {
    /// Type text into the pty, byte for byte — a search term, a digit key.
    pub fn send(&mut self, text: &str) {
        self.writer
            .write_all(text.as_bytes())
            .expect("rig: writing to the pty");
        self.writer.flush().expect("rig: flushing the pty");
    }

    /// Press named keys, space-separated: `press("2 tab enter")`. Single
    /// characters pass through as themselves, so `press("/ r q")` works;
    /// use [`send`](Rig::send) for text that contains a literal space.
    pub fn press(&mut self, keys: &str) {
        for key in keys.split_whitespace() {
            let bytes: &[u8] = match key {
                "enter" => b"\r",
                "tab" => b"\t",
                "backtab" => b"\x1b[Z",
                "esc" => b"\x1b",
                "space" => b" ",
                "up" => b"\x1b[A",
                "down" => b"\x1b[B",
                "right" => b"\x1b[C",
                "left" => b"\x1b[D",
                _ => {
                    assert_eq!(
                        key.chars().count(),
                        1,
                        "rig: unknown key name {key:?} — name it in press(), or send() it"
                    );
                    self.send(key);
                    continue;
                }
            };
            self.writer.write_all(bytes).expect("rig: writing a key");
            self.writer.flush().expect("rig: flushing the pty");
        }
    }

    /// Resize the pty mid-scenario — the child gets a real SIGWINCH and the
    /// redraw path runs for real. The parser follows, and the cast records
    /// the moment so a replay shows the same jump.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("rig: resizing the pty");
        self.parser.screen_mut().set_size(rows, cols);
        self.cols = cols;
        self.rows = rows;
        self.cast.resize(cols, rows);
    }

    /// Wait until `pattern` appears anywhere on the grid. The only wait the
    /// rig offers, with [`wait_until`](Rig::wait_until) as its general
    /// form; there is deliberately no sleep.
    pub fn wait_for(&mut self, pattern: &str) {
        let what = format!("the text {pattern:?}");
        self.wait_until(&what, |screen| screen.contents().contains(pattern));
    }

    /// Wait until the grid satisfies `pred`, under the hard deadline. On
    /// expiry the panic carries the final grid and the cast path, which
    /// together are the failure report a human can act on.
    pub fn wait_until(&mut self, what: &str, pred: impl Fn(&vt100::Screen) -> bool) {
        let end = Instant::now() + DEADLINE;
        loop {
            self.pump();
            if pred(self.parser.screen()) {
                return;
            }
            if Instant::now() >= end {
                self.fail(&format!("waited {DEADLINE:?} for {what} and it never came"));
            }
            // Block briefly on the channel rather than spinning; the wait
            // is still by condition, this is only how the bytes arrive.
            match self.rx.recv_timeout(Duration::from_millis(25)) {
                Ok(bytes) => self.ingest(&bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // The child is gone. Its last bytes may still satisfy
                    // the condition; anything else is a crashed scenario.
                    self.pump();
                    if pred(self.parser.screen()) {
                        return;
                    }
                    self.fail(&format!("the TUI exited while waiting for {what}"));
                }
            }
        }
    }

    /// Compare the grid against the golden beside the scenarios, keyed by
    /// name and geometry; `RIG_BLESS=1` rewrites it instead. The golden is
    /// the diff surface a deliberate visual change shows up on in review.
    pub fn snapshot(&mut self, checkpoint: &str) {
        self.pump();
        let got = self.grid();
        let path = goldens().join(format!(
            "{}_{checkpoint}_{}x{}.txt",
            self.name, self.cols, self.rows
        ));
        if std::env::var_os("RIG_BLESS").is_some() {
            fs::create_dir_all(goldens()).expect("rig: creating the goldens directory");
            fs::write(&path, &got).expect("rig: blessing the golden");
            return;
        }
        let want = fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!(
                "rig: no golden at {} — bless it with RIG_BLESS=1",
                path.display()
            )
        });
        if want != got {
            self.fail(&format!(
                "the grid drifted from {}\n{}",
                path.display(),
                diff(&want, &got)
            ));
        }
    }

    /// The grid as plain text: every row, right-trimmed, one per line. What
    /// the goldens hold and what failure reports print.
    pub fn grid(&mut self) -> String {
        self.pump();
        let mut out = String::new();
        for row in self.parser.screen().rows(0, self.cols) {
            out.push_str(row.trim_end());
            out.push('\n');
        }
        out
    }

    /// Drain everything the pty has already produced into the parser and
    /// the cast. Non-blocking; the waits do the blocking.
    fn pump(&mut self) {
        while let Ok(bytes) = self.rx.try_recv() {
            self.ingest(&bytes);
        }
    }

    fn ingest(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.cast.output(bytes);
    }

    /// Every rig failure carries the same evidence: what went wrong, the
    /// grid as it stood, and where the replayable record landed.
    fn fail(&mut self, why: &str) -> ! {
        let grid = self.grid();
        panic!(
            "rig [{} {}x{}]: {why}\n--- screen ---\n{grid}--- cast: {} ---",
            self.name,
            self.cols,
            self.rows,
            self.cast.path.display()
        );
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // Kill the child and keep the terminal as it is: the pty is the
        // scenario's terminal, not the test runner's, and there is nothing
        // of ours to restore. The wait reaps the zombie; the reader thread
        // ends itself on the EOF the kill causes.
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.pump();
    }
}

/// Where the goldens live: beside the scenarios, in the repository, because
/// they are reviewed like any other code change.
fn goldens() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/goldens")
}

/// A first-mismatch diff, both sides in full. The goldens are a screenful,
/// so printing everything costs little and saves a round trip to a file.
fn diff(want: &str, got: &str) -> String {
    let mut out = String::new();
    for (index, (w, g)) in want.lines().zip(got.lines()).enumerate() {
        if w != g {
            let _ = writeln!(out, "first difference at line {}:", index + 1);
            let _ = writeln!(out, "  golden: {w:?}");
            let _ = writeln!(out, "  screen: {g:?}");
            break;
        }
    }
    let _ = writeln!(out, "--- golden ---\n{want}--- screen ---\n{got}");
    out
}

/// The asciinema v2 record of one run: a JSON-lines header, then timed
/// output events, written as they arrive so a crashed scenario still leaves
/// its evidence. Kept under `target/rig-casts/`, uploaded by CI on failure.
struct Cast {
    path: PathBuf,
    file: Option<fs::File>,
    started: Instant,
}

impl Cast {
    fn new(name: &str, cols: u16, rows: u16) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/rig-casts");
        fs::create_dir_all(&dir).expect("rig: creating target/rig-casts");
        let path = dir.join(format!("{name}_{cols}x{rows}.cast"));
        let mut file = fs::File::create(&path).expect("rig: creating the cast file");
        let header = serde_json::json!({
            "version": 2,
            "width": cols,
            "height": rows,
            "timestamp": SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "title": name,
            "env": { "TERM": "xterm-256color" },
        });
        let _ = writeln!(file, "{header}");
        Self {
            path,
            file: Some(file),
            started: Instant::now(),
        }
    }

    fn output(&mut self, bytes: &[u8]) {
        // Lossy on purpose: a cast is JSON, and the one thing an invalid
        // UTF-8 boundary may cost is a replacement glyph in the replay.
        self.event("o", &String::from_utf8_lossy(bytes));
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.event("r", &format!("{cols}x{rows}"));
    }

    fn event(&mut self, code: &str, data: &str) {
        let t = self.started.elapsed().as_secs_f64();
        if let Some(file) = self.file.as_mut() {
            let line = serde_json::json!([t, code, data]);
            let _ = writeln!(file, "{line}");
        }
    }
}
