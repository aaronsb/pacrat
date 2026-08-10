//! The tree digest: how pacrat says "these are the bytes that were graded".
//!
//! The hash itself is [`sha2`], deliberately. pacrat's entire argument is
//! that curated upstream code is more trustworthy than bespoke code — that
//! is what vendoring, reviewing and grading are *for* — and a hand-written
//! SHA-256 sitting in the part of pacrat that decides what to trust would
//! contradict the thesis however well it passed its vectors. RustCrypto's
//! implementation is audited, is what the ecosystem already depends on, and
//! keeps receiving fixes nobody here would have written. The ten crates it
//! brings are the price of that, and it is worth paying.
//!
//! What *is* ours is the construction below: which bytes go into the hash,
//! in what order, with what framing. That is a design decision no crate can
//! make, so it is the thing this module tests.
//!
//! Threat-model boundary: this digest detects mutation of bytes pacrat
//! hashed on both sides of the comparison — it computes the stored digest
//! and the checked digest itself, so an attacker never supplies either
//! half. It is NOT for verifying content against a digest that arrived
//! from elsewhere (a forge release, a mirror, a message); the moment one
//! side of the comparison is someone else's claim, that is a different
//! problem with different requirements — signatures, pinned checksums —
//! and this module is the wrong tool. `push` and `sync` will be tempted;
//! this sentence is here to say no.

use sha2::{Digest, Sha256};

/// Lowercase hex, the only form a digest is ever written or compared in.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// The digest of a package tree.
///
/// Each file contributes its relative path and its contents, both
/// length-prefixed. The prefixes are the point — without them a tree holding
/// `ab` + `c` would hash the same as one holding `a` + `bc`, and a rename
/// could be made invisible. Paths are `/`-joined and the caller supplies
/// them sorted, so the digest is a property of the tree's content and not of
/// the order the filesystem happened to enumerate it in.
///
/// The leading tag namespaces the construction: a future v2 that hashes
/// something else (modes, say) cannot collide with a v1 digest, and cached
/// gradings from before the change simply stop matching, which is the safe
/// direction.
///
/// `files` is `(relative path, contents)`.
pub fn tree_digest<'a, I>(files: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let mut h = Sha256::new();
    h.update(b"pacrat-tree/v1\0");
    for (path, content) in files {
        h.update((path.len() as u64).to_le_bytes());
        h.update(path.as_bytes());
        h.update((content.len() as u64).to_le_bytes());
        h.update(content);
    }
    hex(&h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Digests of two fixed trees, pinned by value.
    ///
    /// These are the load-bearing test in this module. They were computed
    /// independently (Python's `hashlib` over the same framing) and they did
    /// not change when the hash moved from a hand-written SHA-256 to
    /// [`sha2`] — which is what makes that swap invisible to every grade
    /// cache already on disk. If a refactor ever changes these numbers it
    /// has changed the cache format, and every stored grading silently stops
    /// matching its tree.
    #[test]
    fn known_tree_digests_are_pinned() {
        assert_eq!(
            tree_digest([("PKGBUILD", b"pkgname=mdcat\n".as_slice())]),
            "2102ef4ae61d9a59bf2ca0d0471d37c14b7b466b3fd02fb2deb9f0ab727fc2f2"
        );
        assert_eq!(
            tree_digest([
                ("PKGBUILD", b"pkgname=mdcat\n".as_slice()),
                ("mdcat.install", b"post_install() { :; }\n".as_slice()),
            ]),
            "b17e5cc0f1c4c46dd666466a9329ec4d836d5936c883fca78daf834ccd46f1bd"
        );
        // An empty tree still has a digest, and it is not the empty hash —
        // the tag is in there.
        assert_eq!(
            tree_digest([]),
            "0e370604d5392caeefefc05bd9a81baafed7ed1d3ad2895c2f6d352517ee8ccc"
        );
        assert_ne!(tree_digest([]), sha256_hex(b""));
    }

    /// The published SHA-256 vectors, kept as differential checks: they cost
    /// nothing and they pin that the bytes reach the hasher the way this
    /// module thinks they do — byte order, hex casing, no stray framing.
    /// FIPS 180-2 appendix B, plus the empty string.
    #[test]
    fn published_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            sha256_hex(
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmno\
                  ijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"
            ),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    #[test]
    fn the_one_million_a_vector() {
        let mut h = Sha256::new();
        for _ in 0..1000 {
            h.update([b'a'; 1000]);
        }
        assert_eq!(
            hex(&h.finalize()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// `tree_digest` streams file after file into one hasher rather than
    /// concatenating them, so how the input is chunked must not matter.
    /// Every split point across two block boundaries.
    #[test]
    fn chunking_never_changes_the_digest() {
        let data: Vec<u8> = (0u16..200).map(|i| (i % 251) as u8).collect();
        let want = sha256_hex(&data);
        for split in 0..data.len() {
            let mut h = Sha256::new();
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(hex(&h.finalize()), want, "split at {split}");
        }
        let mut h = Sha256::new();
        for byte in &data {
            h.update([*byte]);
        }
        assert_eq!(hex(&h.finalize()), want);
    }

    /// Lengths either side of the 55/56/64 padding boundaries.
    #[test]
    fn padding_boundaries() {
        for len in [0usize, 1, 54, 55, 56, 57, 63, 64, 65, 119, 120, 128] {
            let data = vec![b'x'; len];
            let mut h = Sha256::new();
            h.update(&data);
            let streamed = hex(&h.finalize());
            assert_eq!(streamed, sha256_hex(&data), "len {len}");
            assert_eq!(streamed.len(), 64);
        }
    }

    #[test]
    fn tree_digest_is_stable_and_content_addressed() {
        let a = tree_digest([("PKGBUILD", b"pkgname=x\n".as_slice())]);
        assert_eq!(a, tree_digest([("PKGBUILD", b"pkgname=x\n".as_slice())]));
        // One byte of the content.
        assert_ne!(a, tree_digest([("PKGBUILD", b"pkgname=y\n".as_slice())]));
        // The name, with identical content.
        assert_ne!(a, tree_digest([("PKGBUILT", b"pkgname=x\n".as_slice())]));
        // An added file.
        assert_ne!(
            a,
            tree_digest([
                ("PKGBUILD", b"pkgname=x\n".as_slice()),
                ("x.install", b"".as_slice()),
            ])
        );
        assert_eq!(tree_digest([]).len(), 64);
    }

    /// The length prefixes earn their place here: without them these two
    /// trees would feed the hasher an identical byte stream.
    #[test]
    fn field_boundaries_cannot_be_smuggled() {
        let left = tree_digest([("ab", b"c".as_slice())]);
        let right = tree_digest([("a", b"bc".as_slice())]);
        assert_ne!(left, right);

        // Splitting one file into two whose names and contents concatenate
        // to the same bytes.
        let one = tree_digest([("PKGBUILDx", b"yy".as_slice())]);
        let two = tree_digest([("PKGBUILD", b"".as_slice()), ("xyy", b"".as_slice())]);
        assert_ne!(one, two);
    }
}
