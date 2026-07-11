//! A dense bitmask over token ids — the per-step allowed-token set (§4).
//!
//! [`BitMask`] is a bespoke `Vec<u64>` bitset: `ceil(len / 64)` words, bit `id`
//! living in word `id / 64` at position `id % 64`. It is deliberately *not* a
//! `bitvec`/`roaring` dependency — a word-wise newtype is a few dozen lines, needs
//! no vetting rubric, and keeps the published core's `[dependencies]` at
//! `⊆ { thiserror }` (constitution §1, `check-core-deplight`).
//!
//! The mask spans `V + 1` bits over a `V`-token vocabulary: bit `V` is the
//! reserved **EOS** bit (architecture Decision D3), a canonical completeness
//! signal independent of whatever id the host's tokenizer assigns its own EOS
//! token. [`intersect`](BitMask::intersect) is the forward-compatible hook the M3
//! schema overlay narrows through; [`copy_from`](BitMask::copy_from) refills an
//! owned buffer from a cached mask with no allocation, which is what keeps the
//! per-step path alloc-free (§4.3).

/// The number of bits in one backing word.
const WORD_BITS: u32 = 64;

/// A dense bitmask over token ids `0..len`, packed into `ceil(len / 64)` `u64`
/// words.
///
/// Ids at or beyond `len` (the length fixed at construction) are **out of
/// range**: [`set`](BitMask::set) and [`clear`](BitMask::clear) treat them as a
/// no-op and [`test`](BitMask::test) reports them absent, so a stray id can
/// never index past the backing words or flip a padding bit — and, because no
/// path can set an id `>= len`, [`iter_ones`](BitMask::iter_ones) never yields
/// one. The intended `len` is `vocab.len() + 1`, so the top bit is the reserved
/// EOS position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitMask {
    words: Vec<u64>,
    len: usize,
}

/// Word index and in-word bit position for token `id`.
const fn locate(id: u32) -> (usize, u32) {
    ((id / WORD_BITS) as usize, id % WORD_BITS)
}

impl BitMask {
    /// An all-zero mask with room for ids `0..len`.
    #[must_use]
    pub fn with_len(len: usize) -> Self {
        let words = len.div_ceil(WORD_BITS as usize);
        Self {
            words: vec![0; words],
            len,
        }
    }

    /// The number of ids this mask spans (`V + 1`, EOS bit included).
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the mask spans no ids at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Set the bit for `id` (mark the token admissible). An id `>= len` is out
    /// of range and ignored (see the type contract).
    pub fn set(&mut self, id: u32) {
        if id as usize >= self.len {
            return;
        }
        let (word, bit) = locate(id);
        self.words[word] |= 1u64 << bit;
    }

    /// Clear the bit for `id` (mark the token inadmissible). An id `>= len` is
    /// out of range and ignored (see the type contract).
    pub fn clear(&mut self, id: u32) {
        if id as usize >= self.len {
            return;
        }
        let (word, bit) = locate(id);
        self.words[word] &= !(1u64 << bit);
    }

    /// Whether the bit for `id` is set. An id `>= len` is out of range and
    /// reported absent (see the type contract).
    #[must_use]
    pub fn test(&self, id: u32) -> bool {
        if id as usize >= self.len {
            return false;
        }
        let (word, bit) = locate(id);
        (self.words[word] >> bit) & 1 == 1
    }

    /// Clear every bit, keeping the length and the backing allocation.
    pub fn clear_all(&mut self) {
        self.words.iter_mut().for_each(|word| *word = 0);
    }

    /// Word-wise `self &= other` — the M3 schema-narrowing hook (§4.3). Both
    /// masks must share a length (hence a word count); the loop intersects the
    /// overlap.
    pub fn intersect(&mut self, other: &BitMask) {
        for (word, &mask) in self.words.iter_mut().zip(&other.words) {
            *word &= mask;
        }
    }

    /// Overwrite `self` with `other`'s bits by copying words in place — no
    /// allocation, the reuse that keeps the per-step mask build alloc-free
    /// (§4.3). Both masks are meant to share a length; a mismatch copies only the
    /// overlapping words rather than panicking, so a public misuse degrades
    /// gracefully instead of aborting (constitution §1 — no library panic).
    pub fn copy_from(&mut self, other: &BitMask) {
        for (dst, &src) in self.words.iter_mut().zip(&other.words) {
            *dst = src;
        }
    }

    /// Iterate the ids of every set bit, ascending.
    pub fn iter_ones(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(word, &bits)| {
            let base = (word as u32) * WORD_BITS;
            OnesInWord { bits }.map(move |bit| base + bit)
        })
    }
}

/// Yields the positions of the set bits in a single word, lowest first.
struct OnesInWord {
    bits: u64,
}

impl Iterator for OnesInWord {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.bits == 0 {
            return None;
        }
        let bit = self.bits.trailing_zeros();
        // Clear the lowest set bit.
        self.bits &= self.bits - 1;
        Some(bit)
    }
}

#[cfg(test)]
mod tests {
    use super::{BitMask, WORD_BITS};

    /// A host-scale vocabulary size — its EOS bit at id `V` (mask length `V + 1`)
    /// sits far past the first backing word, exercising the word-index math.
    const HOST_SCALE_VOCAB_LEN: u32 = 150_000;

    #[test]
    fn with_len_rounds_up_to_whole_words() {
        assert!(BitMask::with_len(0).is_empty());
        // 65 bits need two 64-bit words; every bit starts clear.
        let mask = BitMask::with_len(65);
        assert_eq!(mask.len(), 65);
        assert!((0..65).all(|id| !mask.test(id)));
    }

    #[test]
    fn is_empty_is_exactly_a_zero_length_mask() {
        // A zero-length mask spans no ids; any positive length does not.
        assert!(BitMask::with_len(0).is_empty());
        assert!(!BitMask::with_len(1).is_empty());
        assert!(!BitMask::with_len(WORD_BITS as usize).is_empty());
    }

    #[test]
    fn clear_targets_the_exact_bit_across_a_word() {
        // Clearing a bit at a non-zero in-word position must clear *that* bit and
        // leave its word-mates set — the shift direction in `clear` is
        // load-bearing (a `>>` would no-op every bit but position 0).
        let mut mask = BitMask::with_len(WORD_BITS as usize);
        for id in 0..5u32 {
            mask.set(id);
        }
        mask.clear(3);
        assert!(!mask.test(3));
        assert!(mask.test(0) && mask.test(1) && mask.test(2) && mask.test(4));
    }

    #[test]
    fn out_of_range_ids_are_inert_never_panicking() {
        // One backing word: valid ids are `0..64`. An id at or past `len` must be
        // a no-op / absent, never an out-of-bounds index into the words.
        let mut mask = BitMask::with_len(WORD_BITS as usize);
        mask.set(WORD_BITS); // == len
        mask.set(WORD_BITS + 100); // well past len, past the only word
        assert!(!mask.test(WORD_BITS));
        assert!(!mask.test(WORD_BITS + 100));
        assert_eq!(mask.iter_ones().count(), 0);
        // A valid id in the same word still round-trips; out-of-range clears — at
        // the `== len` boundary and well past it — leave it untouched, never
        // indexing past the backing word.
        mask.set(WORD_BITS - 1);
        mask.clear(WORD_BITS); // == len
        mask.clear(WORD_BITS + 5);
        assert!(mask.test(WORD_BITS - 1));
    }

    #[test]
    fn set_test_and_clear_round_trip_across_a_word_boundary() {
        let mut mask = BitMask::with_len(130);
        for &id in &[0u32, 63, 64, 65, 129] {
            assert!(!mask.test(id));
            mask.set(id);
            assert!(mask.test(id));
        }
        mask.clear(64);
        assert!(!mask.test(64));
        // Clearing one bit leaves its word-mates alone.
        assert!(mask.test(63) && mask.test(65));
    }

    #[test]
    fn clear_all_zeroes_every_bit() {
        let mut mask = BitMask::with_len(200);
        for id in [1, 64, 199] {
            mask.set(id);
        }
        mask.clear_all();
        assert_eq!(mask.iter_ones().count(), 0);
    }

    #[test]
    fn intersect_keeps_only_the_common_bits() {
        let mut a = BitMask::with_len(128);
        let mut b = BitMask::with_len(128);
        for id in [1, 5, 70, 100] {
            a.set(id);
        }
        for id in [5, 70, 127] {
            b.set(id);
        }
        a.intersect(&b);
        assert_eq!(a.iter_ones().collect::<Vec<_>>(), vec![5, 70]);
    }

    #[test]
    fn copy_from_overwrites_the_whole_mask() {
        let mut dst = BitMask::with_len(128);
        dst.set(3);
        dst.set(64);
        let mut src = BitMask::with_len(128);
        src.set(9);
        src.set(120);
        dst.copy_from(&src);
        assert_eq!(dst, src);
        assert_eq!(dst.iter_ones().collect::<Vec<_>>(), vec![9, 120]);
    }

    #[test]
    fn iter_ones_yields_set_ids_ascending() {
        let mut mask = BitMask::with_len(200);
        let ids = [0u32, 7, 63, 64, 128, 199];
        for &id in &ids {
            mask.set(id);
        }
        assert_eq!(mask.iter_ones().collect::<Vec<_>>(), ids.to_vec());
    }

    #[test]
    fn the_reserved_top_bit_is_addressable() {
        // len = V + 1: the EOS bit lives at id V, the highest addressable id.
        let v = HOST_SCALE_VOCAB_LEN;
        let mut mask = BitMask::with_len(v as usize + 1);
        assert!(!mask.test(v));
        mask.set(v);
        assert!(mask.test(v));
        assert_eq!(mask.iter_ones().last(), Some(v));
    }
}
