//! ADR-005's first scenarios: the two defects that reached a human, the
//! focus chip, and the selection language ADR-002 promises — driven through
//! a real pty by the rig and asserted on the grid a real terminal would
//! show. The rig itself, and the sandbox everything runs in, are in
//! [`rig`]; the goldens beside these tests are the diff surface a
//! deliberate visual change shows up on in review.

mod rig;

use rig::fixture::Fixture;
use rig::Rig;

/// The tint a marked row carries — `theme::SELECT`, as a terminal sees it.
const SELECT_TINT: vt100::Color = vt100::Color::Idx(60);

/// Every inverse-video run on the region rule rows — the rows whose first
/// cell is the `├` seam tee. The focused title chip is the only inverse
/// ink that belongs on a rule; the cursor row's reversal lives on body
/// rows and never trips this.
fn chips(screen: &vt100::Screen) -> Vec<String> {
    let (rows, cols) = screen.size();
    let mut found = Vec::new();
    for row in 0..rows {
        let rule = screen
            .cell(row, 0)
            .is_some_and(|cell| cell.contents() == "├");
        if !rule {
            continue;
        }
        let mut run = String::new();
        for col in 0..cols {
            let cell = screen.cell(row, col).expect("a cell inside the grid");
            if cell.inverse() {
                run.push_str(cell.contents());
            } else if !run.is_empty() {
                found.push(std::mem::take(&mut run));
            }
        }
        if !run.is_empty() {
            found.push(run);
        }
    }
    found
}

/// Wait until exactly one title chip is inverse and it names `title`.
fn expect_chip(rig: &mut Rig, title: &str) {
    rig.wait_until(&format!("one focus chip, on {title:?}"), |screen| {
        let found = chips(screen);
        found.len() == 1 && found[0].contains(title)
    });
}

/// The row that names `package` carries the selection tint behind it —
/// the half of ADR-002's "glyph *and* tint" the plain-text grid cannot
/// carry.
fn expect_tint(rig: &mut Rig, package: &str) {
    rig.wait_until(
        &format!("the tint behind the marked {package} row"),
        |screen| {
            let (rows, cols) = screen.size();
            (0..rows).any(|row| {
                let text: String = (0..cols)
                    .filter_map(|col| screen.cell(row, col))
                    .map(vt100::Cell::contents)
                    .collect();
                text.contains(package)
                    && (0..cols).any(|col| {
                        screen
                            .cell(row, col)
                            .is_some_and(|cell| cell.bgcolor() == SELECT_TINT)
                    })
            })
        },
    );
}

/// Spawn the TUI and see the overview through its first load — every
/// scenario's first step, split out so they cannot disagree about what
/// "started" means.
fn started(name: &str, geometry: (u16, u16), fixture: &Fixture) -> Rig {
    let mut rig = rig::spawn(name, geometry, fixture);
    // The drift title only lands once the overview's queries — all of them
    // answered by the fixture's shims — have come back.
    rig.wait_for("drift on north");
    rig
}

/// Get to the browse screen with the fixture's search on it: `/`, the
/// term, enter — the prompt overlay driven by keys, the way a hand does it.
fn searched(rig: &mut Rig) {
    rig.press("2");
    rig.wait_for("nothing searched for yet");
    rig.press("/");
    rig.wait_for("search the official repos and the AUR");
    rig.send("rat");
    rig.press("enter");
    // Two rows from the sync dbs, one from the AUR — both worlds answered.
    rig.wait_for("results — 3 across both worlds");
}

/// Defect one, as a scenario: search on browse, then arrows. The rows are
/// what the reader asked for, so the cursor must land on them (the
/// focus-follows-results fix) and the detail pane must follow every step.
#[test]
fn browse_search_then_arrows_walk_rows_and_the_detail_follows() {
    let fixture = Fixture::new("browse-search");
    let mut rig = started("browse-search", (80, 24), &fixture);
    searched(&mut rig);

    // The fix under test: a search that left focus on the query pane
    // produced a screen where Up and Down visibly did nothing.
    expect_chip(&mut rig, "results");
    rig.wait_for("rat-hole — what pacrat already knows");

    // Arrows walk, the detail follows, and walking back works too.
    rig.press("down");
    rig.wait_for("rat-trap — what pacrat already knows");
    rig.press("down");
    rig.wait_for("rat-stash — what pacrat already knows");
    rig.press("up");
    rig.wait_for("rat-trap — what pacrat already knows");
    rig.snapshot("walked");

    // A mid-scenario resize: SIGWINCH and the redraw path, for real. The
    // frame's corner landing in the new last column is the proof the app
    // saw the new geometry rather than repainting the old one.
    rig.resize(100, 30);
    rig.wait_until("the frame redrawn at 100 columns", |screen| {
        screen
            .cell(0, 99)
            .is_some_and(|cell| cell.contents() == "┐")
    });
    rig.wait_for("rat-trap — what pacrat already knows");
}

/// Defect two, as a scenario: Tab around every screen. Exactly one title
/// renders as the inverse-video chip, and it moves where Tab says —
/// through every region, wrapping back to the first.
#[test]
fn tab_cycles_one_visible_focus_chip_on_every_screen() {
    let fixture = Fixture::new("tabs");
    let mut rig = started("tabs", (80, 24), &fixture);

    // Each screen: the key that reaches it, a marker that only renders
    // once its load has come back, and its region titles in Tab order
    // (dynamic titles listed by their stable part).
    let screens: &[(&str, &str, &[&str])] = &[
        (
            "1",
            "drift on north",
            &[
                "store & ledger",
                "attention",
                "tracked lists",
                "drift on north",
            ],
        ),
        (
            "2",
            "nothing searched for yet",
            &["search", "results", "detail"],
        ),
        (
            "3",
            "pending —",
            &["pending", "upstream", "diff", "grading"],
        ),
        ("4", "matrix — 5 packages", &["matrix", "legend", "bat"]),
        (
            "5",
            "nothing waiting to publish",
            &["this session", "log", "publish queue"],
        ),
        ("6", "flow —", &["flow", "policy", "graders", "sources"]),
        ("7", "the petit chef", &["the petit chef"]),
    ];
    for (key, loaded, titles) in screens {
        rig.press(key);
        rig.wait_for(loaded);
        // Focus starts on the first region, and one full cycle of Tab
        // visits every title and wraps home — a chip that ever went
        // missing, or doubled, fails the wait.
        expect_chip(&mut rig, titles[0]);
        for step in 1..=titles.len() {
            rig.press("tab");
            expect_chip(&mut rig, titles[step % titles.len()]);
        }
    }
}

/// The focus policy: enter on a browse row fills the detail pane, and
/// focus stays on the results — the reader is mid-walk, and the arrows
/// must keep walking.
#[test]
fn browse_enter_fills_the_detail_and_focus_stays_on_the_results() {
    let fixture = Fixture::new("browse-enter");
    let mut rig = started("browse-enter", (80, 24), &fixture);
    searched(&mut rig);

    rig.press("enter");
    // The deep detail: the three sources `pacrat info` merges, answered by
    // the pacman and curl shims.
    rig.wait_for("rat-hole — everything pacrat knows");
    rig.wait_for("https://example.invalid/rat-hole");
    rig.snapshot("detail");

    // The pane filled and focus did not move: the chip is still on the
    // results, and the next arrow still walks the rows.
    expect_chip(&mut rig, "results");
    rig.press("down");
    rig.wait_for("rat-trap — what pacrat already knows");
}

/// ADR-002's selection language, end to end, at one geometry: marks by
/// space and by the whole-screen trio, both signals on a marked row, marks
/// surviving a reload that reorders the rows, and the apply flow behind
/// `x` and `A` — the confirm naming the count, the write, the cleared
/// marks.
fn hosts_selection_at(name: &str, geometry: (u16, u16)) {
    let fixture = Fixture::new(name);
    let mut rig = started(name, geometry, &fixture);
    rig.press("4");
    rig.wait_for("matrix — 5 packages");

    // Mark two rows by name: cowsay (the ✗ row) and zoxide (a + row).
    // Both signals must show — the • glyph in the grid, the tint behind it.
    rig.press("down space");
    rig.wait_for("•cowsay");
    expect_tint(&mut rig, "cowsay");
    rig.press("G space");
    rig.wait_for("•zoxide");

    // A reload that reorders the rows: a new unmanaged package sorts
    // first, shifting every index. The marks are names, not indices, so
    // both must still be on the same packages afterwards.
    fixture.install("native", "apple\nbat\nfd\nripgrep\nzoxide\n");
    rig.press("r");
    rig.wait_for("matrix — 6 packages");
    rig.wait_for("•cowsay");
    // The cursor survives a reload by index, not by name, so it now sits a
    // row above zoxide — and on a 15-row terminal zoxide is below the
    // fold. Walking to the bottom is the scenario's job, not the grid's.
    rig.press("G");
    rig.wait_for("•zoxide");
    rig.snapshot("marked");

    // The whole-screen trio: none, then walk to cowsay and mark it alone.
    rig.press("-");
    rig.wait_until("the marks cleared", |screen| {
        !screen.contents().contains('•')
    });
    rig.press("g down down space");
    rig.wait_for("•cowsay");

    // `x` over the mark: the confirm overlay names the count, and esc
    // backs out without applying.
    rig.press("x");
    rig.wait_for("Untrack 1 package");
    rig.press("esc");
    rig.wait_until("the confirm closed", |screen| {
        !screen.contents().contains("Untrack 1 package")
    });

    // `A` over two + rows: the confirm names the count, `y` applies
    // through the CLI verb's writer, and the marks do not survive their
    // own apply.
    rig.press("space"); // unmark cowsay — add would refuse a ✗ row
    rig.press("G space"); // zoxide
    rig.press("g down space"); // bat
    rig.press("A");
    rig.wait_for("Add 2 package");
    rig.press("y");
    rig.wait_for("added 2 package(s)");
    rig.wait_until("the marks cleared by the apply", |screen| {
        !screen.contents().contains('•')
    });
    // Focus never left the rows: the next keystroke adjusts marks or
    // confirms, and both live on the matrix (the focus policy).
    expect_chip(&mut rig, "matrix");
    rig.snapshot("applied");
}

/// The hosts selection language at the ADR's two named geometries.
#[test]
fn hosts_selection_language_at_80x24() {
    hosts_selection_at("hosts-marks", (80, 24));
}

#[test]
fn hosts_selection_language_at_40x15() {
    hosts_selection_at("hosts-marks-narrow", (40, 15));
}
