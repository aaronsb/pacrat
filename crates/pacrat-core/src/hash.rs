//! SHA-256, and the tree digest pacrat builds out of it.
//!
//! **Why this is here rather than a crate.** pacrat needs exactly one hash,
//! of a few kilobytes of PKGBUILD, to answer one question: are these the
//! bytes that were graded? `sha2` would bring six crates (digest,
//! block-buffer, crypto-common, generic-array, typenum, cfg-if) into a core
//! that ADR-001 keeps deliberately thin. SHA-256 is a fully specified
//! function with published test vectors, so an implementation that
//! reproduces them *is* SHA-256 — the risk of hand-rolling is a subtly wrong
//! implementation, and that risk is bought off by the vectors below: the
//! three FIPS 180-2 examples, the one-million-'a' vector, and a streaming
//! test that chunks input at every offset across the block boundary.
//!
//! Reach for a crate the moment pacrat needs a second primitive. One
//! well-tested function is maintainable; a homegrown crypto library is not.

use std::fmt::Write as _;

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const INIT: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// A streaming SHA-256. Streaming matters: the tree digest feeds file after
/// file through one hasher rather than concatenating them in memory.
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    /// Bytes currently in `buf`; always < 64, because a full block is
    /// compressed immediately.
    buffered: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            h: INIT,
            buf: [0; 64],
            buffered: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);

        if self.buffered > 0 {
            let take = (64 - self.buffered).min(data.len());
            self.buf[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 64 {
                let block = self.buf;
                compress(&mut self.h, &block);
                self.buffered = 0;
            }
        }
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            compress(&mut self.h, block.try_into().expect("64 bytes"));
            data = rest;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buffered = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        // `buffered` is < 64, so there is always room for the 0x80 byte.
        self.buf[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > 56 {
            self.buf[self.buffered..].fill(0);
            let block = self.buf;
            compress(&mut self.h, &block);
            self.buffered = 0;
        }
        self.buf[self.buffered..56].fill(0);
        self.buf[56..].copy_from_slice(&bits.to_be_bytes());
        let block = self.buf;
        compress(&mut self.h, &block);

        let mut out = [0u8; 32];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.h) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

fn compress(h: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for (word, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes(chunk.try_into().expect("4 bytes"));
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);

        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (slot, add) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *slot = slot.wrapping_add(add);
    }
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finish())
}

/// The digest of a package tree: what "these are the bytes that were graded"
/// means concretely.
///
/// Each file contributes its relative path and its contents, both
/// length-prefixed. The prefixes are the point — without them a tree holding
/// `ab` + `c` would hash the same as one holding `a` + `bc`, and renaming a
/// file could be made invisible. Paths are `/`-joined and the caller supplies
/// them sorted, so the digest is a property of the tree's content and not of
/// the order the filesystem happened to enumerate it in.
///
/// `files` is `(relative path, contents)`.
pub fn tree_digest<'a, I>(files: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let mut h = Sha256::new();
    h.update(b"pacrat-tree/v1\0");
    for (path, content) in files {
        h.update(&(path.len() as u64).to_le_bytes());
        h.update(path.as_bytes());
        h.update(&(content.len() as u64).to_le_bytes());
        h.update(content);
    }
    hex(&h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-2 appendix B, plus the empty string. These are what make a
    /// hand-rolled implementation defensible: matching them is what it means
    /// to be SHA-256.
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
        // 56 bytes: the padding case where the length does not fit in the
        // final block and a second block is required.
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
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&h.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// Streaming must not depend on how the caller chunks its input. Every
    /// split point across two block boundaries is exercised, which is where
    /// a buffering bug would hide.
    #[test]
    fn chunking_never_changes_the_digest() {
        let data: Vec<u8> = (0u16..200).map(|i| (i % 251) as u8).collect();
        let want = sha256_hex(&data);
        for split in 0..data.len() {
            let mut h = Sha256::new();
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(hex(&h.finish()), want, "split at {split}");
        }
        // And in many small pieces.
        let mut h = Sha256::new();
        for byte in &data {
            h.update(&[*byte]);
        }
        assert_eq!(hex(&h.finish()), want);
    }

    /// Lengths either side of the 55/56/64 padding boundaries.
    #[test]
    fn padding_boundaries() {
        for len in [0usize, 1, 54, 55, 56, 57, 63, 64, 65, 119, 120, 128] {
            let data = vec![b'x'; len];
            let mut h = Sha256::new();
            h.update(&data);
            let streamed = hex(&h.finish());
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
