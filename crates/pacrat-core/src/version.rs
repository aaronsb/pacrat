//! Arch version comparison — alpm's `alpm_pkg_vercmp`, reimplemented.
//!
//! pacrat has to answer "is the AUR's version newer than the one installed
//! here?" for every tracked package it reports on. A string compare gets that
//! wrong the first time it meets `1.9` and `1.10`, and the `vercmp(8)` binary
//! would mean one subprocess per package — each of which ADR-001 would
//! require pacrat to print, burying the answer under its own bookkeeping.
//!
//! So the algorithm lives here, in the pure crate, where it is one function
//! call and can be tested against captured `vercmp` output. It is a faithful
//! port of pacman's `lib/libalpm/version.c` (itself rpm's `rpmvercmp`), and
//! deliberately keeps that code's shape — including the parts that read
//! oddly — because the value of this module is agreeing with pacman, not
//! being elegant. Two consequences worth knowing before reading further:
//!
//! * A missing pkgrel is not zero, it is *unasked*: `1.0` and `1.0-1` compare
//!   equal, because pacman only compares releases when both sides carry one.
//! * Separators do not have identities. `1.0_1`, `1.0.1` and `1.0-1`'s version
//!   part all segment the same way, so the first two compare equal.

use std::cmp::Ordering;

/// Compare two `[epoch:]pkgver[-pkgrel]` strings the way pacman does.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (epoch_a, ver_a, rel_a) = parse_evr(a);
    let (epoch_b, ver_b, rel_b) = parse_evr(b);

    // Epochs go through the same segment comparison rather than an integer
    // parse: alpm does it that way, so a nonsense epoch sorts consistently
    // here and there instead of both silently becoming 0.
    match rpmvercmp(epoch_a, epoch_b) {
        Ordering::Equal => {}
        other => return other,
    }
    match rpmvercmp(ver_a, ver_b) {
        Ordering::Equal => {}
        other => return other,
    }
    match (rel_a, rel_b) {
        (Some(x), Some(y)) => rpmvercmp(x, y),
        // One side did not state a release, so neither side's release is
        // evidence of anything.
        _ => Ordering::Equal,
    }
}

/// True when `candidate` is a strictly newer version than `installed`.
pub fn is_newer(installed: &str, candidate: &str) -> bool {
    vercmp(installed, candidate) == Ordering::Less
}

/// Split `[epoch:]pkgver[-pkgrel]`.
///
/// An epoch is only an epoch when a run of digits is followed by a colon —
/// `https://…` has a colon and no epoch, and this is the rule that says so.
///
/// The release is everything past the *last* hyphen (alpm uses `strrchr`),
/// which matters because a pkgver may legally contain one: `1.0-1-2` is
/// version `1.0-1` at release `2`, not version `1.0` at release `1-2`. Split
/// at the first hyphen instead and `1.0-1-2` compares *equal* to `1.0`, since
/// the versions match and one side's release then goes unexamined.
fn parse_evr(s: &str) -> (&str, &str, Option<&str>) {
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    let (epoch, rest) = match s.as_bytes().get(digits) {
        Some(b':') => (&s[..digits], &s[digits + 1..]),
        _ => ("0", s),
    };
    // `:1.0` — a colon with nothing in front of it — is epoch zero.
    let epoch = if epoch.is_empty() { "0" } else { epoch };
    match rest.rfind('-') {
        Some(i) => (epoch, &rest[..i], Some(&rest[i + 1..])),
        None => (epoch, rest, None),
    }
}

/// rpm's segment-wise comparison: walk both strings as alternating runs of
/// digits and letters, separators ignored except for how many of them there
/// were, numeric runs beating alphabetic ones.
fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (x, y) = (a.as_bytes(), b.as_bytes());
    // `one`/`two` in the C: the start of the segment under comparison.
    let (mut one, mut two) = (0usize, 0usize);
    // `ptr1`/`ptr2`: one past its end, and the previous segment's start on
    // entry to each iteration — which is how the separator run gets measured.
    let (mut end_x, mut end_y) = (0usize, 0usize);

    while one < x.len() && two < y.len() {
        while one < x.len() && !x[one].is_ascii_alphanumeric() {
            one += 1;
        }
        while two < y.len() && !y[two].is_ascii_alphanumeric() {
            two += 1;
        }
        if one >= x.len() || two >= y.len() {
            break;
        }
        // Unequal separator runs decide it outright: `1..2` is newer than
        // `1.2`. Odd, but it is what pacman does.
        if one - end_x != two - end_y {
            return (one - end_x).cmp(&(two - end_y));
        }
        end_x = one;
        end_y = two;

        let numeric = x[end_x].is_ascii_digit();
        if numeric {
            while end_x < x.len() && x[end_x].is_ascii_digit() {
                end_x += 1;
            }
            while end_y < y.len() && y[end_y].is_ascii_digit() {
                end_y += 1;
            }
        } else {
            while end_x < x.len() && x[end_x].is_ascii_alphabetic() {
                end_x += 1;
            }
            while end_y < y.len() && y[end_y].is_ascii_alphabetic() {
                end_y += 1;
            }
        }

        let mut seg_x = &x[one..end_x];
        let mut seg_y = &y[two..end_y];
        // `seg_x` cannot be empty — we know its first byte is alphanumeric.
        // `seg_y` empty means the two sides are different types here, and a
        // number is always newer than a letter.
        if seg_y.is_empty() {
            return if numeric {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        if numeric {
            // Leading zeros carry no magnitude, so strip them and let digit
            // count stand in for value — no integer parse, no overflow.
            while let Some((b'0', tail)) = seg_x.split_first() {
                seg_x = tail;
            }
            while let Some((b'0', tail)) = seg_y.split_first() {
                seg_y = tail;
            }
            match seg_x.len().cmp(&seg_y.len()) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        match seg_x.cmp(seg_y) {
            Ordering::Equal => {}
            other => return other,
        }
        one = end_x;
        two = end_y;
    }

    if one >= x.len() && two >= y.len() {
        // Every segment matched; only the separators differed.
        return Ordering::Equal;
    }
    // The tail-breaker. A leftover alphabetic run loses to nothing at all —
    // that is what makes `1.0rc1` older than `1.0` — while a leftover numeric
    // run wins.
    let x_alpha = x.get(one).is_some_and(u8::is_ascii_alphabetic);
    let y_alpha = y.get(two).is_some_and(u8::is_ascii_alphabetic);
    if (one >= x.len() && !y_alpha) || x_alpha {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case captured from `vercmp` on a live Arch host, so a divergence
    /// from pacman shows up as a failing test rather than a wrong answer in
    /// the updates table.
    #[test]
    fn agrees_with_pacmans_vercmp() {
        let cases: &[(&str, &str, i32)] = &[
            ("1.0", "1.1", -1),
            ("1.0-1", "1.0-2", -1),
            ("1:1.0", "2.0", 1),
            ("1.0", "1.0", 0),
            ("1.0rc1", "1.0", -1),
            ("1.0.1", "1.0", 1),
            ("2.10.1-1", "2.11.0-1", -1),
            ("1.0a", "1.0", -1),
            ("1.0a", "1.0b", -1),
            ("20240101", "20231231", 1),
            ("1.0.0", "1.0", 1),
            ("r10.abc", "r9.abc", 1),
            ("1.0_1", "1.0.1", 0),
            // A stated release is not compared against an unstated one.
            ("1.0", "1.0-1", 0),
            ("1:1.0-1", "1:1.0-2", -1),
            // A pkgver containing a hyphen: the split is at the *last* one,
            // so these are version `1.0-1` release `2`. Splitting at the
            // first would make the first case compare equal.
            ("1.0-1-2", "1.0", 1),
            ("1.0-1-2", "1.0-2", 1),
            ("1.0-1-2", "1.0-1-3", -1),
            ("1.0-1-2", "1.0-1-1", 1),
            ("1.0-2", "1.0-1-2", -1),
            ("1:1.0-1-2", "1:1.0-3", 1),
            ("0.10.15", "0.10.16", -1),
            ("4.15", "4.16", -1),
            ("1.0alpha", "1.0", -1),
            ("1.0", "1.0.a", -1),
            ("a", "b", -1),
            ("1", "1.0", -1),
        ];
        for (a, b, want) in cases {
            let got = match vercmp(a, b) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            };
            assert_eq!(got, *want, "vercmp({a:?}, {b:?})");
            // Antisymmetry: pacman's answer reversed is the reverse answer.
            let back = match vercmp(b, a) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            };
            assert_eq!(back, -*want, "vercmp({b:?}, {a:?})");
        }
    }

    #[test]
    fn leading_zeros_do_not_change_magnitude() {
        assert_eq!(vercmp("1.007", "1.7"), Ordering::Equal);
        assert_eq!(vercmp("1.0007", "1.008"), Ordering::Less);
        // Digit count, not integer width: neither side has to fit in a u64.
        let big = "1.99999999999999999999999999999999";
        let bigger = "1.999999999999999999999999999999999";
        assert_eq!(vercmp(big, bigger), Ordering::Less);
    }

    #[test]
    fn epochs_outrank_everything_after_them() {
        assert_eq!(vercmp("1:0.1", "9.9.9"), Ordering::Greater);
        assert_eq!(vercmp("10:1.0", "9:1.0"), Ordering::Greater);
        // A colon that is not an epoch separator stays part of the version.
        assert_eq!(vercmp("a:1", "a:1"), Ordering::Equal);
        assert_eq!(vercmp(":1.0", "1.0"), Ordering::Equal);
    }

    #[test]
    fn empty_and_degenerate_input_does_not_panic() {
        assert_eq!(vercmp("", ""), Ordering::Equal);
        assert_eq!(vercmp("", "1"), Ordering::Less);
        assert_eq!(vercmp("1", ""), Ordering::Greater);
        assert_eq!(vercmp("...", "..."), Ordering::Equal);
        // Non-ASCII bytes are not alphanumeric, so they read as separators —
        // and `é` being two bytes makes the separator run longer, which is
        // itself how pacman decides. Captured, not reasoned about.
        assert_eq!(vercmp("1.é.2", "1.2"), Ordering::Greater);
    }

    /// Differential test against the real thing, over generated input.
    ///
    /// Ignored by default: it shells out to pacman's `vercmp(8)`, which CI
    /// does not have and which this crate is otherwise pure without. Run it
    /// on an Arch box after touching anything above —
    /// `cargo test -p pacrat-core -- --ignored` — because the captured
    /// battery only proves agreement on cases someone thought to write down,
    /// and the interesting disagreements are the ones nobody predicted.
    #[test]
    #[ignore = "needs pacman's vercmp(8); run manually on Arch"]
    fn agrees_with_the_real_vercmp_over_generated_input() {
        use std::process::Command;

        // A deterministic LCG: a failure has to be reproducible to be worth
        // reporting, and this crate takes no dependencies for a test.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) as usize
        };
        // The alphabet is the one that makes rpmvercmp branch: digits, letters,
        // every separator it treats specially, and the epoch and release marks.
        let atoms = [
            "0", "1", "2", "9", "10", "07", "a", "b", "rc", "alpha", ".", ".", "-", "_", "+", ":",
            "~",
        ];

        let mut cases = Vec::new();
        for _ in 0..4000 {
            let mut build = || {
                let n = 1 + next() % 7;
                (0..n)
                    .map(|_| atoms[next() % atoms.len()])
                    .collect::<String>()
            };
            cases.push((build(), build()));
        }

        for (a, b) in cases {
            let out = Command::new("vercmp")
                .args([&a, &b])
                .output()
                .expect("vercmp(8) must be installed to run this test");
            let want: i32 = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("vercmp {a:?} {b:?} said nothing"));
            let got = match vercmp(&a, &b) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            };
            assert_eq!(got, want, "vercmp({a:?}, {b:?})");
        }
    }

    #[test]
    fn is_newer_is_the_display_decision() {
        assert!(is_newer("2.10.1-1", "2.11.0-1"));
        // Equal, and older, are both "nothing to report".
        assert!(!is_newer("2.11.0-1", "2.11.0-1"));
        assert!(!is_newer("2.11.0-1", "2.10.1-1"));
        // The AUR quotes a pkgrel the local db also has; only rel differs.
        assert!(is_newer("1.3.0-1", "1.3.0-2"));
        // A local build of the same pkgver with no rel stated upstream is
        // not an update, and must not be reported as one.
        assert!(!is_newer("1.3.0-2", "1.3.0"));
    }
}
