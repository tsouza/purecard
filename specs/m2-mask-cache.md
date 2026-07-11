# Spec: M2 — performance (per-step token mask + cache)

- **Status:** Draft (ready to implement)
- **Created:** 2026-07-10
- **Owner:** purecard engineer (tsouza)
- **Milestone:** M2 — PERFORMANCE. Depends on M0 (`Vocab`), M1 (byte-PDA + `DecoderSession`/`ByteRecognizer`). Design authority: `docs/spec/architecture.md` §4, §9; `docs/spec/testing.md` §8.5.

## Problem

The decoder must drive constrained generation against a model whose vocabulary is ~150k tokens. At **every** decode step the host needs a bitmask over that vocabulary marking which token ids keep the partial output on a path to a valid Pure query — i.e. which tokens, when their raw bytes are fed to the byte-PDA, leave it non-dead. This mask is consumed on the model's critical path (one call per generated token), so it must be **fast enough to never bottleneck the forward pass**: target **≤ a few hundred µs/token**.

The naive per-step computation is O(V·L̄): clone the PDA, replay every token's bytes, keep the survivors. At V=150k, L̄≈4 that is ~600k `step` calls per token — on the order of milliseconds, i.e. **over budget by ~10×**. M2 makes the per-step mask cheap while keeping it provably equal to the naive truth.

## Goals

Each goal maps to a concrete done-criterion.

- [ ] **G1 — correct `allowed_mask()`.** `session.allowed_mask() -> &BitMask` returns, for the current PDA state+stack, exactly the set of token ids whose bytes keep the PDA non-dead (byte-level, §4.4), with the EOS bit set iff `is_complete()`. *Done:* the oracle test (`mask_oracle.rs`) shows bit-equality against brute-force at every reachable walk-prefix state.
- [ ] **G2 — correct `accept_token()`.** `session.accept_token(id)` advances the PDA by that token's bytes, is legal for every id set in `allowed_mask()`, is rejected (leaving the session untouched) for every id cleared in it, and is byte-for-byte equivalent to folding `accept_byte` over `vocab.bytes(id)`. *Done:* property tests (§8.5) (a) and (b) green with committed seeds.
- [ ] **G3 — benchmarked latency.** A criterion benchmark measures `allowed_mask()` in isolation at ~150k synthetic tokens across shallow / deep-stack / identifier-position states and reports µs/token; the value is **≤ a few hundred µs/token** and the bench lane runs in CI. *Done:* `benches/allowed_mask.rs` exists, `just bench` runs it, CI asserts the floor, CodSpeed lane flipped live.
- [ ] **G4 — dep-light core.** `BitMask` is bespoke; core `[dependencies]` stays `⊆ {thiserror}`; `check-core-deplight` untouched. Only `[dev-dependencies]` grow (`proptest`, `criterion`). *Done:* `just ci` green including `check-core-deplight`.

## Non-goals

- **M3 schema narrowing (L2).** The mask exposes the intersection point (`mask.intersect(schema_terminals)`) but ships **no** schema. Explicitly forward-compatible, not implemented.
- **PyO3 / `ffi`.** No host binding surface added here.
- **A real model tokenizer/vocab.** None ships in the repo (as in M0/M1). Everything is computed against a host-supplied `Vocab`; tests use a synthetic byte-token vocab.
- **`from_spec` / EBNF grammar compilation (§5).** `CompiledGrammar::from_spec` is a thin stub returning the single fixed M1 PDA-backed grammar; real spec parsing is a later milestone.
- **Trie-shared cache build.** The prefix-trie build optimization is documented as a deferred lever (see Risks), not built.

## Design

### Spine decision

The per-step budget math (≥~600k `step` calls naive vs a ~2,344-word copy cached) means **brute-force per-step almost certainly misses the few-hundred-µs floor at V=150k**. The cache is therefore load-bearing, not speculative. But brute-force is *also* the natural test oracle and a permanently-correct reference. So we build in two grafted phases:

- **Phase A (correctness spine):** implement the brute-force `allowed_mask` + `accept_token`. Simple, obviously correct, retained forever as the `#[cfg(test)]` oracle.
- **Phase B (perf spine):** add the lazy per-`State` cache + runtime context-dependent flip, and pin it bit-equal to Phase A. The bench (G3) is the gate that Phase B is required and sufficient.

Chosen concrete design = **lazy per-`State` `OnceCell<BitMask>` cache** (from the `lazy-cache-bitset` design), with brute-force retained as oracle (from `minimal-correct`). The **trie** (from `trie-first`) is deferred — it only accelerates the one-time, amortized, off-critical-path build.

### BitMask (`src/mask.rs`, bespoke, ~50 lines)

```rust
/// Dense bitmask over token ids, `ceil(len/64)` words.
pub struct BitMask { words: Vec<u64>, len: usize }

impl BitMask {
    pub fn with_len(len: usize) -> Self { /* words = (len + 63) / 64 */ }
    pub fn set(&mut self, id: u32) { /* words[id/64] |= 1 << (id%64) */ }
    pub fn clear(&mut self, id: u32);
    pub fn test(&self, id: u32) -> bool;
    pub fn clear_all(&mut self);
    /// Word-wise `self &= other` — the M3 schema-narrowing hook.
    pub fn intersect(&mut self, other: &BitMask);
    /// Reuse an owned buffer: copy other's words in, no alloc.
    pub fn copy_from(&mut self, other: &BitMask);
    pub fn iter_ones(&self) -> impl Iterator<Item = u32> + '_;
}
```

`len = V + 1`; the top id `V` is the **EOS bit** (see Decisions D3). No `bitvec`/`roaring` — a word-wise newtype needs no crate and no vetting rubric.

### PDA additions (`src/grammar/pda.rs`)

`State` is already `Copy + Eq`; `advance` is no-mutate-on-dead; `State`/`Frame`/`Step`/`step` are already `pub`. Add:

```rust
impl State {
    /// Stable dense index for Vec-keyed caches. Extends the existing name() match.
    pub const fn index(self) -> usize { /* one arm per variant */ }
    pub const COUNT: usize = /* number of variants */;
}

impl Pda {
    pub fn state(&self) -> State { self.state }
    pub fn stack_top(&self) -> Option<Frame> { self.stack.last().copied() }

    /// Zero-heap-alloc admissibility probe: replay pure `step` over a reused
    /// scratch stack seeded from the live stack (or empty, for cache build).
    /// Returns (alive, consulted_ambient) — `consulted_ambient` is true iff a
    /// Pop was attempted while the local scratch was empty (a bare closer).
    pub fn probe(&self, bytes: &[u8], scratch: &mut Vec<Frame>) -> Probe { ... }
}

pub struct Probe { pub alive: bool, pub consulted_ambient: bool }
```

`probe` reuses one `scratch: Vec<Frame>` across all candidate tokens → no per-token heap `clone`. `consulted_ambient` is what falls out of the probe to classify a token as context-dependent for cache purposes.

### CompiledGrammar (`src/grammar/compiled.rs`)

```rust
pub struct CompiledGrammar {
    vocab: Vocab,
    cache: Vec<OnceCell<Cached>>,          // len == State::COUNT
    closer_candidates: Box<[u32]>,          // ids whose bytes contain any of )]}
}

/// Per-state memoized split (§4.2).
struct Cached {
    indep: BitMask,      // context-independent survivors from empty ambient stack
    deferred: Box<[u32]>, // ids that consulted ambient stack at this state
}

impl CompiledGrammar {
    pub fn compile(vocab: Vocab) -> Self { /* size cache to State::COUNT; precompute closer_candidates */ }
    pub fn from_spec(/* … */) -> Self { /* stub → compile over the fixed M1 PDA */ }
}
```

`cache` is interior-mutable (`OnceCell`) so `allowed_mask(&self)` fills the entry for a state on first visit (§4.5): only reached states pay the build.

### The mask computation

**Cache build (lazy, once per reached `state`, §4.2/4.5):**

```rust
fn build(state: State, vocab: &Vocab, candidates: &[u32]) -> Cached {
    let base = Pda::at(state);          // PDA pinned at `state`, empty stack
    let mut indep = BitMask::with_len(vocab.len() + 1);
    let mut deferred = Vec::new();
    let mut scratch = Vec::new();
    for id in 0..vocab.len() as u32 {
        let p = base.probe(vocab.bytes(id), &mut scratch);
        if p.consulted_ambient {
            deferred.push(id);          // stack-sensitive → resolve at runtime
        } else if p.alive {
            indep.set(id);              // state-only survivor → cache it
        }
    }
    Cached { indep, deferred: deferred.into() }
}
```

A token is context-dependent iff its byte path attempts a `Pop` on its own empty local scratch — i.e. it consults the ambient `stack_top` (the bare closers `)]}` and anything ending in an unmatched closer). `closer_candidates` is a cheap conservative pre-filter; the exact classification is the `consulted_ambient` flag from the probe (see Decision D5).

**Per step (§4.3, no allocation):**

```rust
pub fn allowed_mask(&self) -> &BitMask {
    let st = self.pda.state();
    let cached = self.grammar.cached(st);            // fills OnceCell on first visit
    let mask = &mut self.mask;                        // owned, reused buffer
    mask.copy_from(&cached.indep);                    // O(V/64) word copy
    let mut scratch = self.scratch.borrow_mut();      // reused Vec<Frame>
    for &id in &cached.deferred {                     // |deferred| ≪ V
        if self.pda.probe(self.grammar.vocab.bytes(id), &mut scratch).alive {
            mask.set(id);                              // survives against LIVE stack
        }
    }
    if self.pda.is_complete() { mask.set(EOS_ID) } else { mask.clear(EOS_ID) }
    // M3 hook (non-goal here): if let Some(t) = &self.schema { mask.intersect(t) }
    mask
}
```

The deferred re-probe runs against the **live** stack, so `step(state, stack_top, byte)` returns `Pop` only on a matched closer — this exactly enforces bracket matching against the real stack top. Everything else is a single word-wise copy.

### `accept_token` (§9.1, the §8.5 rollback property)

```rust
pub fn accept_token(&mut self, id: u32) -> Result<(), DecodeError> {
    if id == EOS_ID {
        return if self.pda.is_complete() { Ok(()) } else { Err(DecodeError::UnexpectedEos) };
    }
    let snapshot = (self.pda.state(), self.pda.stack_len());   // cheap: State is Copy
    for &b in self.grammar.vocab.bytes(id) {
        if self.accept_byte(b).is_err() {                      // reuses M1 deadness channel
            self.pda.rollback_to(snapshot);                    // token rejected → untouched
            return Err(DecodeError::InadmissibleToken { id });
        }
    }
    self.offset += self.grammar.vocab.bytes(id).len();
    Ok(())
}
```

Because M1 `advance` mutates on success, we snapshot `(state, stack.len())` and truncate/restore on any interior dead byte — a rejected token leaves the session exactly as it was (the §8.5 invariant that makes speculative masking sound).

### DecoderSession surface (`src/session.rs`)

`DecoderSession<'g>` gains `grammar: &'g CompiledGrammar`, one owned reusable `mask: BitMask`, and a reused `scratch: RefCell<Vec<Frame>>`. `new()` becomes `new(g: &'g CompiledGrammar)` (L1-only ⇒ `schema: None`). The M1 `ByteRecognizer` impl (`accept_byte`/`is_complete`/`reset`) is untouched; the three new methods (`allowed_mask`, `accept_token`) plus inherent re-exposed `is_complete`/`reset` sit beside it so callers need not import the trait. `reset` keeps the `mask` buffer (no re-alloc).

## API / contract impact

New public surface (architecture.md §9.1):

- `purecard::mask::BitMask` — bitset newtype + ops above.
- `purecard::grammar::CompiledGrammar` with `compile(Vocab)` and stub `from_spec`.
- `Pda::{state, stack_top, probe}`, `State::{index, COUNT}`, `Probe` — grammar-internal helpers, `pub` for `CompiledGrammar`/tests.
- `DecoderSession::{new(&CompiledGrammar), allowed_mask() -> &BitMask, accept_token(u32) -> Result<(), DecodeError>}` + inherent `is_complete`/`reset`.
- `DecodeError` gains `InadmissibleToken { id }` and `UnexpectedEos` variants (`thiserror`, lib-internal).

All new public items carry doc comments (`deny(missing_docs)`). **Core `[dependencies]` unchanged** (`⊆ {thiserror}`) → `check-core-deplight` allowlist untouched. `#![forbid(unsafe_code)]` holds (bitset is safe indexing).

## Testing plan

- **Correctness vs brute-force — `tests/mask_oracle.rs`.** At each state produced by replaying a `generate_walks()` prefix (guarantees reachability, §4.5 — never synthetic `State` literals), assert `session.allowed_mask()` is **bit-equal** to a reference mask built by cloning the PDA and probing every token's bytes byte-by-byte from the live state+stack. Pins `cache[state].indep ∪ runtime-deferred-flip == naive truth`, including the EOS bit ⇔ `is_complete()`. This kills "return cache without flip", "flip wrong bit", and "skip EOS" mutants.
- **Property tests (§8.5) — `tests/mask_properties.rs`** (`proptest`, fixed `rng_algorithm` + committed `proptest-regressions/`, matching the walker determinism rule §2). Generators: synthetic `Vocab` of 1–8-byte strings over `walker::ALPHABET` + EOS; reachable states via `generate_walks()` prefixes. Properties: **(a)** every id set in `allowed_mask()`, `accept_token`'d on a clone, returns `Ok` and leaves the PDA non-dead and non-panicking; **(b)** `accept_token(id)` ≡ folding `accept_byte` over `vocab.bytes(id)` — identical final state, stack, and `Err`/offset; **(c)** every id *cleared* in `allowed_mask()` is rejected by `accept_token` and leaves the session byte-identical (rollback).
- **Criterion benchmark — `benches/allowed_mask.rs`** (`harness = false`, `[[bench]]` in `Cargo.toml`; `just bench` already runs `cargo bench --workspace`). Build `CompiledGrammar` + `Vocab` once outside `iter`; `black_box` the session; measure `allowed_mask()` alone at ~150k synthetic tokens across three states: **shallow** (empty stack), **deep-stack** (maximizes context-dependent flips), **identifier-position** (dense admissible set). Report µs/token and assert the ≤ few-hundred-µs floor in CI. Wire the dormant lane: `benches/` presence + `cargo codspeed build/run` (ci.yml:280) is already scripted — flip repo var `CODSPEED_ENABLED=true` (ci.yml:262) for regression tracking.
- **Mutation surface.** Keep cache lookup, `build`, and the deferred flip as small pure functions so `cargo-mutants` targets them; the oracle test is their executioner. Coverage floor 70% + mutation floor hold or ratchet up — never down.
- **Walker extension.** Add `walker::token_walks(vocab)`: the clone-and-probe loop over token *ids* (`accept_token`) instead of bytes, yielding token-id sequences that feed the completeness lane at token granularity.

## Dependency vetting

- **`BitMask`** — hand-written `Vec<u64>` newtype. `bitvec`/`roaring` rejected: a ~50-line word-wise bitset needs no crate; adopting one would burn the vetting rubric and grow core deps for negative value. **Verdict: write our own** (keeps `check-core-deplight` `[dependencies] ⊆ {thiserror}`).
- **Byte-trie** — hand-written *if* built; but deferred (Risks). **Verdict: not now.**
- **`proptest`, `criterion`** — dev-only, so `check-core-deplight` (which gates `[dependencies]`) is untouched. Both must have their **current stable release looked up on crates.io at pin time** (§2 "latest stable, verified") — never from memory. Dependabot is the last-mile net, not the mechanism. **Verdict: adopt under `[dev-dependencies]`, version verified at implementation.**

## Risks & rollout

- **R1 — brute-force misses target (expected).** The math says per-step naive is ~ms. Mitigation: the cache is the spine, not an afterthought; brute-force is retained only as the oracle. The bench (G3) is the hard gate.
- **R2 — one-time build cost spikes latency on first visit to a state.** Each distinct reached `State` pays one O(V·L̄) build (~ms) on first `allowed_mask()`. Across a decode this amortizes (states are revisited many times) and is off the steady-state critical path, but the *first* token at a fresh state is slow. Mitigation: measure build cost in the bench; if painful, the **prefix-trie build** (`trie-first` design) makes the build sub-linear in Σtoken-bytes by sharing prefixes and pruning dead subtrees in one probe. Documented, deferred, YAGNI until the bench says otherwise.
- **R3 — conservative deferred set bloats runtime probes.** If we defer on "contains a closer" rather than the exact `consulted_ambient` flag, balanced tokens like `()` get needlessly re-probed each step. Mitigation: use the exact per-state `consulted_ambient` classification from the probe (D5); `closer_candidates` is only a compile-time pre-filter to avoid probing obviously-independent tokens during build.
- **R4 — `State::index`/`COUNT` drift.** Adding a `State` variant without extending `index()` breaks cache indexing. Mitigation (fix-the-system, §5): an exhaustive `match` in `index()` (no wildcard arm) makes a new variant a **compile error**, and a unit test asserts `index()` is a bijection into `0..COUNT`.
- **Rollout:** land behind the existing M1 API additively; no M1 behavior changes. Merge order per Implementation tasks — each task is independently green.

## Implementation tasks

1. **`src/mask.rs`** — `BitMask` + unit tests (`set`/`clear`/`test`/`intersect`/`copy_from`/`iter_ones`, EOS bit). Independently testable.
2. **`pda.rs` accessors** — `State::index()` (exhaustive match, no wildcard) + `State::COUNT`; `Pda::state`/`stack_top`; `Probe` + `Pda::probe` (scratch-reusing); bijection test for `index`.
3. **`src/grammar/compiled.rs`** — `CompiledGrammar::compile` (+ `from_spec` stub), `closer_candidates` precompute, `cache` sized to `State::COUNT`.
4. **`accept_token` + rollback** in `session.rs`; `DecodeError` variants; property test (b)+(c). Uses M1 `accept_byte`.
5. **Brute-force `allowed_mask`** (Phase A) as a `#[cfg(test)]` oracle helper; wire `tests/mask_oracle.rs` scaffold + `walker::token_walks`.
6. **Lazy cache `allowed_mask`** (Phase B): `OnceCell` build (`indep` + per-state `deferred`), per-step `copy_from` + deferred flip + EOS. Oracle test now asserts cache == brute-force (shallow + deep-stack states).
7. **Property tests** (a) + committed `proptest-regressions/` seed.
8. **`benches/allowed_mask.rs`** (criterion, `harness=false`) + `[[bench]]`; measure µs/token across three states; assert the floor in CI; flip `CODSPEED_ENABLED=true`.
9. **Docs** — update `docs/domain-model.md`/`docs/lessons.md` if the build-vs-cache tradeoff or the `consulted_ambient` classification taught us something; ADR if the trie-deferral decision warrants one.

## Decisions for the human

- **D1 — Trie build optimization: defer or build now?** *Recommend defer.* The trie only speeds the amortized, off-critical-path build; the per-step path (the actual budget) is identical. Build it only if R2 shows first-visit latency hurts. Cheap to add later behind the same `CompiledGrammar` façade.
- **D2 — Retain brute-force permanently?** *Recommend yes, as `#[cfg(test)]` oracle only* (not a runtime fallback). It is the correctness anchor for every cache mutant; keeping it out of the shipped path avoids dead runtime code.
- **D3 — EOS bit encoding.** *Recommend a reserved top id `V`* (mask `len = V+1`), independent of whatever id the host's `Vocab` assigns to its EOS token, so the mask has a canonical completeness bit. Alternative: use `Vocab`'s own eos id — rejected because it couples the mask layout to host tokenizer choices.
- **D4 — Benchmark CI hard-fail threshold.** *Recommend assert ≤ 300 µs/token* as the concrete reading of "a few hundred µs", plus CodSpeed for relative-regression tracking. Human sets the exact number; it is PROTECTED (ratchets tighter only).
- **D5 — Deferred classification: exact `consulted_ambient` vs conservative contains-closer.** *Recommend exact per-state `consulted_ambient`* from the probe (precise, minimizes runtime probes), with `closer_candidates` as a build-time pre-filter. Contains-closer alone is simpler but re-probes balanced tokens forever (R3).
