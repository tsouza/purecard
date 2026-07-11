//! Seeded accepting-walk generator for the byte-PDA (`specs/m1-l1-grammar.md` T8,
//! G3; `docs/spec/testing.md` §8.2).
//!
//! The recognizer is `Clone` and leaves itself unchanged on a dead byte
//! ([`Pda::advance`] doc), so the *language* it accepts can be sampled by
//! clone-and-probe: at each position, try every byte of a representative alphabet
//! on a clone, keep the ones the automaton accepts, and pick one with a seeded
//! PRNG. Growing then closing (openers are disabled once a per-walk length target
//! is reached) drives every walk to an accepting configuration in bounded steps.
//!
//! The generator is fully deterministic — committed seeds, a committed
//! SplitMix64 — so a walk set is reproducible in CI (constitution §2, no
//! local-only state) and identical across runs. Its output feeds two lanes: the
//! hermetic `completeness_walks` self-test (every walk must stream through the
//! shipped [`DecoderSession`]) and the opt-in `legend` engine lane.

use purecard::Pda;

/// The representative byte alphabet the walk probes at each position. It is not
/// all 256 bytes: one or two witnesses per character class (identifier, digit,
/// each delimiter, each operator, string/date/var sigils) reach every arm of the
/// transition function while keeping generated walks short and legible. Extra
/// letters would only lengthen identifiers without exercising a new transition.
const ALPHABET: &[u8] = b"abXY1_ |{}()[].,;:$%'-><=!&+*/";

/// Number of accepting walks a full generation produces.
pub const WALK_COUNT: usize = 64;

/// The base seed; walk `i` derives from `BASE_SEED` advanced past every seed a
/// prior walk consumed, so the set is one deterministic stream, not 64 correlated
/// low seeds.
const BASE_SEED: u64 = 0x5075_7265_4361_7264; // "PureCard" as ASCII bytes.

/// Shortest accepted walk kept (a bare `|X ` is length 3); below this a walk is
/// too trivial to be worth an engine round-trip.
const MIN_LEN: usize = 4;

/// The per-walk growth target is drawn from `[GROW_MIN, GROW_MAX)`; until the walk
/// reaches it, openers are allowed and the walk explores. Past it, openers are
/// disabled and the walk closes toward an accepting state.
const GROW_MIN: u64 = 6;
const GROW_MAX: u64 = 44;

/// Hard cap on emitted bytes per attempt — a safety bound so a pathological walk
/// terminates rather than spins. Comfortably above `GROW_MAX` plus the deepest
/// close-out any reachable stack needs.
const HARD_CAP: usize = 400;

/// Weight added, in the closing phase, to a byte whose result is an accepting
/// configuration — biases each closing step toward finishing the walk.
const ACCEPT_BONUS: u32 = 10;

/// SplitMix64 — a tiny, well-known, fully specified PRNG. Committed here rather
/// than pulling a `rand` dependency for a deterministic test generator: the whole
/// state is one `u64`, so a seed pins the exact stream (constitution §4 — bespoke
/// over a dependency where the bespoke code is trivial and total).
struct SplitMix64 {
    state: u64,
}

/// SplitMix64's golden-ratio increment and two avalanche multipliers (Steele et
/// al., 2014) — named, not magic (constitution §4).
const SPLITMIX_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
const SPLITMIX_MIX_A: u64 = 0xBF58_476D_1CE4_E5B9;
const SPLITMIX_MIX_B: u64 = 0x94D0_49BB_1331_11EB;

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(SPLITMIX_GAMMA);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(SPLITMIX_MIX_A);
        z = (z ^ (z >> 27)).wrapping_mul(SPLITMIX_MIX_B);
        z ^ (z >> 31)
    }

    /// A uniform `u64` in `[0, bound)`; `bound` is always a small positive alphabet
    /// or range size here, so a modulo bias is negligible.
    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

const fn is_opener(byte: u8) -> bool {
    matches!(byte, b'(' | b'[' | b'{')
}

const fn is_closer(byte: u8) -> bool {
    matches!(byte, b')' | b']' | b'}')
}

const fn is_wordish(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The sampling weight of an enabled `byte`, given whether the walk is still
/// growing and whether taking the byte lands in an accepting state.
fn weight(byte: u8, growing: bool, leads_accept: bool) -> u32 {
    let base = if growing {
        // Growth: modestly favour structure over ever-longer identifiers.
        if is_opener(byte) {
            3
        } else if is_wordish(byte) {
            4
        } else {
            3
        }
    } else {
        // Closing: openers are forbidden (they never shrink the stack); closers
        // and token-enders are favoured so the walk terminates.
        if is_opener(byte) {
            0
        } else if is_closer(byte) {
            12
        } else {
            2
        }
    };
    if !growing && leads_accept {
        base + ACCEPT_BONUS
    } else {
        base
    }
}

/// Pick an enabled byte by weight. `cands` is non-empty with at least one positive
/// weight whenever this is called.
fn weighted_pick(cands: &[(u8, u32)], rng: &mut SplitMix64) -> u8 {
    let total: u32 = cands.iter().map(|&(_, w)| w).sum();
    let mut target = rng.below(u64::from(total)) as u32;
    for &(byte, w) in cands {
        if target < w {
            return byte;
        }
        target -= w;
    }
    // Unreachable: weights sum to `total` and `target < total`. Fall back to the
    // last candidate rather than panic.
    cands[cands.len() - 1].0
}

/// Attempt one accepting walk from `seed`. Returns the byte string and the number
/// of PRNG draws it consumed (so the next walk can start past this stream), or
/// `None` if the attempt did not reach an accepting state within [`HARD_CAP`].
fn attempt(seed: u64) -> (Option<Vec<u8>>, u64) {
    let mut rng = SplitMix64::new(seed);
    let mut draws = 0u64;
    let grow_target = {
        draws += 1;
        GROW_MIN + rng.below(GROW_MAX - GROW_MIN)
    };
    let mut pda = Pda::new();
    let mut out: Vec<u8> = Vec::new();

    for _ in 0..HARD_CAP {
        let growing = (out.len() as u64) < grow_target;
        if !growing && pda.is_accepting() && out.len() >= MIN_LEN {
            return (Some(out), draws);
        }
        let mut cands: Vec<(u8, u32)> = Vec::new();
        for &byte in ALPHABET {
            let mut probe = pda.clone();
            if probe.advance(byte).is_ok() {
                let w = weight(byte, growing, probe.is_accepting());
                if w > 0 {
                    cands.push((byte, w));
                }
            }
        }
        if cands.is_empty() {
            return if pda.is_accepting() && out.len() >= MIN_LEN {
                (Some(out), draws)
            } else {
                (None, draws)
            };
        }
        draws += 1;
        let byte = weighted_pick(&cands, &mut rng);
        // The byte was chosen from probed-live candidates, so advance cannot fail.
        if pda.advance(byte).is_err() {
            return (None, draws);
        }
        out.push(byte);
    }
    if pda.is_accepting() && out.len() >= MIN_LEN {
        (Some(out), draws)
    } else {
        (None, draws)
    }
}

/// Generate [`WALK_COUNT`] deterministic accepting walks. Seeds advance past every
/// draw a prior walk consumed (and past a failed attempt), so the whole set is one
/// reproducible SplitMix64 stream.
#[must_use]
pub fn generate_walks() -> Vec<Vec<u8>> {
    let mut walks = Vec::with_capacity(WALK_COUNT);
    let mut seed = BASE_SEED;
    // Bound total attempts so a bug can never loop forever; in practice the
    // biased close-out lands an accepting walk on nearly every seed.
    let max_attempts = WALK_COUNT * 64;
    for _ in 0..max_attempts {
        if walks.len() == WALK_COUNT {
            break;
        }
        let (walk, draws) = attempt(seed);
        seed = seed.wrapping_add(draws.max(1)).wrapping_add(SPLITMIX_GAMMA);
        if let Some(bytes) = walk {
            walks.push(bytes);
        }
    }
    walks
}
