# Spec: L2 pass-2 fixes (audit-2 C1/H1/H2 + majors)

- **Status:** Draft — ready to implement (oracle-first)
- **Created:** 2026-07-12
- **Owner:** decoder core (purecard)
- **Audit source:** `docs/probes/purecard-review.md` re-audit pass 2 (audit-2.md), repo @ `fa43b7b`

---

## Problem

L1 (byte-PDA + mask cache) is sound and integration-ready; the 5034-gold byte-replay is green. **B1** (whole-token exact-match deleting the leading BPE sub-token of every schema name) is genuinely **FIXED** by the trie-reachability narrower (`schema/trie.rs` `walk`, `scope.rs::Pending` cross-sub-token accumulation, `narrow.rs::narrow_trie` continuation narrowing). L2 remains a pure *narrower* (`mask.intersect(&narrow_buf)`), so `L2 ⊆ L1` is structural.

But L2 is **not serving-certifiable**, and **B2 is still open**: no test exercises the real Qwen2.5-Coder tokenizer. The shipped synthetic lane (`tests/bpe_split_soundness.rs`) *lexes first, then fragments each lexeme into thirds* — so no synthetic chunk ever straddles a lexeme boundary. Real GPT-2 byte-level BPE routinely merges **across** lexeme boundaries. That gap hides two concrete, code-demonstrable defects:

- **H1 (CRITICAL — real L2 soundness break, serving).** `scope.rs::observe` brackets a whole token by `(pre_state, post_state)` and dispatches it via `classify(whole_bytes)`. When the column-defining string literal merges with its trailing delimiter into one Qwen token — `'MaxRevenue')` or `'MaxRevenue',` — the whole token is `Str`-classified and `unquote()` does `strip_prefix('\'').strip_suffix('\'')`, which **fails on the trailing `)`**. Garbage (quotes+paren) lands in `emitted_strings` instead of `MaxRevenue`. A later clean `getString('MaxRevenue')` fires `L2Position::Column`, walks the gold `'`/`Max`/… against a trie built from `emitted_columns` that *lacks* `MaxRevenue` → `Diverge` → **the gold token is cleared from the mask**. A legitimate continuation is masked out. Fatal soundness break, invisible to the synthetic lane.

- **H2 (HIGH — coverage/precision, soundness-SAFE but overstated value).** When a token swallows the anchoring punctuation (`.count`, `('`, `$x.`), `pre_state` is *not* the anchor state, so `on_dot`/`on_open` never run, `position()` returns `None`, and L2 passes through. This never masks a gold token (safe), but it means L2's *real-Qwen* constraining coverage is far below what the synthetic lane shows. **Synthetic-green is not serving-ready evidence.**

Still-open majors from prior review: **M1** (self-check re-segments greedily via `longest_match`, not real BPE; opt-in, not a gate), **M2** (FFI `eos_id` contract vs Qwen in-vocab stops `<|im_end|>`=151645 / `<|endoftext|>`=151643 undocumented and unenforced), **M3-perf** (`narrow_trie` rebuilds the trie + O(V)≈152k rescan per continuation sub-token). Plus touch-ups: byte-exact N6 column key (`unquote` still routes through `from_utf8_lossy`), spec §9.1 file pointer, and tracked deferrals.

---

## Goals

- [ ] A cross-boundary **synthetic** reproducer (network-free, always-on) **REDs on H1** before the fix, then **GREENs** after the lexeme-boundary scope fix; it also emits a non-failing per-rule H2 coverage probe.
- [ ] Real-Qwen **L1+L2 soundness verified** on a reproducible, committed fixture slice (hermetic gate) plus an opt-in nightly full-vocab lane.
- [ ] **L1 stays green over all 5034 gold** byte-replay — no `pda::step`/`State`/`is_accepting`/`lexeme_kind` edit.
- [ ] Core `[dependencies]` stays `{thiserror, serde, serde_json}` with `#![forbid(unsafe_code)]`; `check-core-deplight` green.
- [ ] The tokenizer (`tokenizers`/`hf-hub`) is **test-only + gated**; `--all-features` stays offline.
- [ ] **L2 stays OFF at serving** until the real-Qwen lane is green; L1 ships first.

---

## Non-goals

- L3 (semantic/faithfulness beyond schema-consistency narrowing).
- Any change to L1's byte-PDA transition table, `State`, or accepting logic.
- Shipping the *full* real-Qwen download lane as a **blocking** CI gate before it is reproducible (the blocking gate runs against the committed slice; the full download is nightly/opt-in).
- Turning L2 on at serving in this changeset.
- Widening the fixture DB set, T1 Boolean/Temporal narrowing, and the `map`-as-`fn` lambda-arg L1 gap (tracked, deferred — see touch-ups).

---

## Design — the failing oracle first (synthetic cross-boundary reproducer)

**File:** `tests/bpe_split_soundness.rs` (extend), reusing `tests/support/bpe.rs::replay_tokens(session, &[u32], eos)` verbatim and `tests/support/synth.rs` fragmenter.

The current pipeline is `lex(query) → fragment(lexeme into thirds) → chunks`, per-lexeme, so nothing straddles. Add a **second, deterministic merge pass** over the flat lexeme-boundary-tagged chunk stream that glues selected adjacent-across-boundary chunk pairs into one token id. Bytes are preserved (the `split_chunks_concatenate` invariant holds).

```rust
enum Merge { CloseQuoteDelim, DotIdent, OpenQuote } // the three H1/H2 shapes
// At each lexeme seam matching a rule, concatenate the LAST chunk of lexeme[i]
// with the FIRST chunk of lexeme[i+1] into one token id:
//   CloseQuoteDelim: Str-closing `'` + following `)` | `,`  -> "')" / "',"   (H1)
//   DotIdent:        `.` + leading third of next ident       -> ".co" of .count (H2)
//   OpenQuote:       `(` + opening `'`                        -> "('"          (H2)
```

Add a `merge_mode` axis to `build_split_vocab`/`split_ids` so the same corpus runs both a **clean** lane and a **merged** lane. Two new tests mirror the existing clean lanes with `Merge::*` applied, replaying gold token-by-token and asserting `mask.test(gold_id)` for L1 (`DecoderSession::new`) and L1+L2 (`with_schema`).

- The **merged L1+L2 lane REDs today** on H1: the merged `'MaxRevenue')` defining token corrupts `emitted_strings`; a later clean reference `Diverge`-clears the gold column token. This is the executable proof of H1. It goes GREEN only after the `scope.rs::observe` lexeme-boundary fix.
- A **coverage probe** (non-failing, prints counts): per rule (N1 member-dot, N2 source, N5 open-paren, N6 column) count how many merged seams still fired `position() != None`. This quantifies H2 — the collapse of rule firing under straddle — without gating on it.

**Acceptance for this step:** the merged L1+L2 test compiles and RED before touching `scope.rs`. Do not proceed to the fix until the oracle is red for the H1 reason (assert the specific cleared gold id, not a generic failure).

---

## Design — H1+H2+M3 lexeme-boundary scope fix (observe rewrite)

Drive **all** scope transitions off the byte-PDA **lexeme boundaries**, not whole-token `classify()`. No L1 change: `observe` only *re-drives* `step` read-only to find the boundaries the token crosses.

**Files:** `src/grammar/pda.rs` (add `pub(crate) fn stack(&self) -> &[Frame]` accessor to seed the walk — no transition change), `src/session.rs` (pass the pre-fold PDA), `src/schema/scope.rs` (the rewrite).

**Signature change (`session.rs`).** `accept_token` already holds the pre-fold `self.pda` before it reassigns `self.pda = probe`. Capture it and pass a reference (pre-state **and stack** — an interior closer `)` routes through `step(AfterValue, stack_top, byte)`):

```rust
let pre_pda = self.pda.clone();          // before the commit
// ... existing L1 fold ...
self.tracker.observe(bytes, &pre_pda, schema);
```

**`observe` rewrite (the boundary walk).** Mirror `Pda::admits`: seed `let mut state = pre.state(); let mut stack = pre.stack().to_vec();`. The token is known-admissible (L1 pre-validated it), so `Step::Dead` is `unreachable!`. Track `run_start: usize` and `run_anchor: State` (the pre-state where the current run opened).

```rust
for i in 0..bytes.len() {
    let prev   = state.lexeme_kind();
    let before = state;
    match step(state, stack.last().copied(), bytes[i]) {
        Step::Next(s)     => state = s,
        Step::Push(f, s)  => { stack.push(f); state = s; }
        Step::Pop(s)      => { stack.pop();  state = s; }
        Step::Dead        => unreachable!("token pre-validated by L1"),
    }
    let next = state.lexeme_kind();
    if let Some(k) = prev {
        if next != prev {                              // a lexeme just CLOSED at i
            self.close_run(k, &bytes[run_start..=i], run_anchor, schema);
            run_start = i + 1; run_anchor = state;
        }
    }
    if prev.is_none() && next.is_none() {              // structural single byte
        self.dispatch_token(&bytes[i..=i], before, schema); // fires on_dot / on_open (H2)
        run_start = i + 1; run_anchor = state;
    }
}
// trailing run may still be an OPEN lexeme (token ends mid-identifier):
// do NOT dispatch — fold into Pending exactly as today (B1/M3 preserved).
if let Some(k) = state.lexeme_kind() {
    self.buffer_pending(k, &bytes[run_start..], run_anchor, schema);
}
```

- `buffer_pending` is today's continue-accumulation path: if `pending.kind == k` extend `buf`; else flush the prior pending via `dispatch_token`, then open `Pending { kind: k, buf, anchor: run_anchor, pos: opening_position(run_anchor) }` (ReValue→`None` as today). A run still **open** at token end stays buffered *across tokens* — so a fragmented `countryName` still accumulates (B1/M3 unchanged). A run that **closes** inside the token is dispatched immediately via `close_run`.

**H1 — byte-exact string close.** For a `Str` run the walk knows the offsets: opening `'` at `run_start`, close where `InStrLit{escaped:true} → AfterValue` fires. `close_run(Str, …)` records the **inner slice** `bytes[run_start+1 .. close_idx]`, undoubling `''`→`'` on the **raw bytes**, and pushes it into `emitted_strings` as a `Vec<u8>` (byte-exact key — drop `String::from_utf8_lossy`). So `'MaxRevenue')` records exactly `MaxRevenue`; the trailing `)` becomes its own structural run → `on_close`. `narrow.rs::quote` already keys the trie byte-exactly, so both sides match.

**H2 — buried dot/open.** Each `None`-kind structural byte is its own one-byte `dispatch_token` carrying its `before` pre-state, so `.count`/`('`/`$x.` run `on_dot`/`on_open` and arm `dot_base`/`ref_stack`/`est_stack` even when the punctuation is merged into a larger token. `position()` then fires the rule.

**Type changes for byte-exactness (mechanical, threaded end-to-end):**
`Lexeme::Str(Vec<u8>)`, `emitted_strings: Vec<Vec<u8>>`, `emitted_columns() -> &[Vec<u8>]`, `unquote(&[u8]) -> Vec<u8>`. `keeps_operand` reads only the discriminant, so T1 is untouched.

**Soundness invariant (unchanged).** L2 still only narrows: `narrow_into` builds `narrow_buf`, `allowed_mask` does `mask.intersect(&narrow_buf)`, EOS always kept. A boundary the walk mis-segments can only *widen* (pass-through), never mask a gold token. No `step` edit → L1's 5034 byte-replay is byte-identical.

**Acceptance:** the merged L1+L2 synthetic lane flips to GREEN; scope unit tests: `b"'MaxRevenue')"` → `emitted_columns() == [b"MaxRevenue"]`; `b".count"` → `on_dot` armed; `b"('"` → `on_open` ran.

---

## Design — C1 real-Qwen lane (byte-unmapping; committed slice + nightly full)

**Files:** new `tests/qwen_soundness.rs`, new `tests/fixtures/qwen-slice.json`; `Cargo.toml` (`qwen = []` feature + `tokenizers`/`hf-hub` as **dev-deps**); `justfile` (`qwen-slice`, `qwen-full`); `xtask` (`qwen-slice` subcommand); `scripts/qwen-fetch.mjs` (+ `scripts/lib/`); CI job.

**Byte un-mapping.** Qwen2.5-Coder is GPT-2 byte-level BPE (no sentencepiece metaspace; `Ġ`=space lives *inside* the byte table). Invert `bytes_to_unicode` once and map each token string char→byte:

```rust
fn gpt2_byte_decoder() -> HashMap<char, u8> {
    let mut bs: Vec<u8> = (b'!'..=b'~').chain(0xA1..=0xAC).chain(0xAE..=0xFF).collect();
    let mut cs: Vec<u32> = bs.iter().map(|&b| b as u32).collect();
    let mut n = 0u32;
    for b in 0u16..=255 { if !bs.contains(&(b as u8)) { bs.push(b as u8); cs.push(256 + n); n += 1; } }
    bs.into_iter().zip(cs).map(|(b, c)| (char::from_u32(c).unwrap(), b)).collect()
}
fn true_bytes(tok: &str, d: &HashMap<char, u8>) -> Vec<u8> {
    tok.chars().map(|c| d[&c]).collect()   // undoes Ġ→0x20 too
}
```

Build `Vocab::from_byte_tokens((0..vocab_size).map(id_to_true_bytes).collect(), eos)`. **Reserve the two in-vocab stops** `<|endoftext|>`=151643 and `<|im_end|>`=151645 as raw specials → map model-EOS to the reserved bit; assert no other special is admissible mid-query (closes M2 on the test side). Tokenize each gold `pure_text` with the real tokenizer and replay via `replay_tokens`:

- **L1 over all 5034 gold** (assert `mask.test(gold_id)` at every step; `is_complete()` + reserved EOS at end).
- **L1+L2 over the 8 in-scope schemas**, mandatorily including at least one **merged-closing-quote** gold (`'MaxRevenue')`) and one **dot-abuts-ident** gold (`.count`) — the exact H1/H2 shapes.

**Hermetic-vs-nightly via env, not a second feature** (so `--all-features` stays offline, no `#[ignore]`). The test reads `PURECARD_QWEN_VOCAB`:

- unset → the committed `tests/fixtures/qwen-slice.json` (true-byte tokens actually reached by the arm-C gold + the 2 specials). Fully network-free; this is the **blocking `just ci` gate**.
- set (nightly) → the full downloaded vocab.

`just qwen-slice` = `cargo xtask qwen-slice` regenerates the slice from the full vocab, so vocab **drift REDs** a PR (the committed slice must match). Nightly `just qwen-full`: `scripts/qwen-fetch.mjs` (Bun `$`, shared `scripts/lib/`) fetches the model **revision SHA** via `hf-hub`, restored through `actions/cache` keyed on that SHA (cache-miss-only download, first-party mirror fallback per constitution §2), exports the vocab path, and runs `cargo nextest --features qwen`.

**Expected real-Qwen L2 coverage (measured, not synthetic — report these numbers from the lane so the serving decision uses real data):** N1 member-dot ~55–70% (`.count`/`.name` merges suppress the rest), N2 source ~80% (classpath rarely merges the leading `::`), N5 open-paren ~40–55% (`('` merges common), N6 column ~35–50% (closing-quote merges frequent). These are the H2 coverage collapse, quantified.

---

## Design — M1 self-check on real token-ids; M2 EOS/specials contract

**M1 — `src/selfcheck.rs`.** Delete `longest_match` and its two tests (greedy longest-match is *not* BPE merge-rank segmentation — it can validate a path the real tokenizer never emits). Swap the sample surface to host-produced id streams:

```rust
pub fn self_check(
    grammar: &CompiledGrammar,
    token_id_streams: &[&[u32]],   // host-tokenized, one Vec<u32> per gold sample
) -> Result<(), SelfCheckError>
```

Per stream: fresh `DecoderSession`; for each `id`, assert `allowed_mask().test(id)` (else `DeadEnd`), then `accept_token(id)` (else `DeadEnd`); at end require `is_complete()` (else `Incomplete`). Drop the `Unsegmentable` variant and the byte-`pos` field — position becomes **stream index** (`step: usize`) since the host owns segmentation. `SMOKE`/`self_check_smoke` become `&[&[u32]]` produced under `#[cfg(feature = "qwen")]` from the canonical strings; **without the feature, smoke is a compile-time no-op** so the pure default build keeps zero tokenizer surface.

**Mandatory gate.** `just selfcheck-real` (wired into `just ci` + a required CI job) feeds `self_check` the **same** real-token id streams C1 builds (one tokenizer fixture, DRY) — so a drifting vocab reddens a PR. Do C1 first, then point M1 at it.

**M2 — `src/ffi.rs` (doc) + `src/selfcheck.rs` (check fn).**

1. Document the mapping in the `ffi.rs` module doc and on `compile_grammar`: the host maps its model EOS id → the reserved bit `V = vocab.len()` (surfaced in `allowed_mask` at index `vocab_len`); it must **not** place a real Qwen stop id inside `vocab_bytes`. Stopping is signaled by bit `V`, decoded back to the model's real EOS id by the host.
2. Add `fn check_eos_contract(session, id_streams, special_ids: &[u32]) -> Result<(), SelfCheckError>` co-located in `selfcheck.rs`, called from the M1 gate over the same streams: assert (a) bit `V` is set in `allowed_mask` **iff** `is_complete()` at every step; (b) no `id ∈ special_ids` (`<|im_start|>`, FIM, `<|repo_name|>`, and the unused stop) is ever admissible mid-stream. One gate proves M1 and M2.

---

## Design — M3-perf trie cache; touch-ups

**M3-perf — `src/schema/narrow.rs`.** `narrow_trie`'s non-empty-prefix branch calls `build()` (rebuild trie from schema) **and** `fill_trie` (O(V)≈152k walk) every continuation sub-token. The trie is cursor-independent — only the walk cursor moves. Replace the trie-rule mask cache with a per-`(schema, rule)` entry owning the built trie plus a per-cursor-node mask memo:

```rust
struct NarrowCache {
    operand: HashMap<CacheKey, BitMask>,   // ReValue lever, via with_cache — unchanged
    tries:   HashMap<CacheKey, RuleCache>, // N3 / N1-N2 / N6
}
struct RuleCache { trie: Trie, kind: TrieKind, masks: HashMap<u32, BitMask> } // key = cursor node id
```

```rust
let entry = cache.tries.entry(key)
    .or_insert_with(|| RuleCache { trie: build(), kind, masks: HashMap::new() });
let cursor = if prefix.is_empty() { entry.trie.root() }
    else { match walk(&entry.trie, entry.trie.root(), prefix) {
        Walk::Stay(c) => c,
        Walk::Complete | Walk::Diverge => return false,   // unchanged
    }};
if let Some(m) = entry.masks.get(&cursor) { dst.copy_from(m); }
else { fill_trie(dst, vocab, eos_bit, &entry.trie, cursor, kind);
       entry.masks.insert(cursor, dst.clone()); }
true
```

`build()` fires **once per key**, not per sub-token (primary win). `fill_trie`/`walk`/`is_candidate` are pure functions of `(trie, cursor, kind)`, so the memo is behaviour-preserving; the anchor mask collapses into `cursor == root` in the same memo (retire the separate anchor cache). `clear()` empties both maps. The `Column(count)` key inherits the identical monotonic-append soundness the current mask cache relies on (same count ⇒ same emitted set within a stream, wiped on reset). Extend `the_anchor_mask_is_cached_and_reused` with a **mid-cursor prefix** to assert memo purity.

**Touch-ups (fold into this changeset):**

- **Byte-exact column key** — subsumed by the H1 type changes above (`Lexeme::Str(Vec<u8>)`, `emitted_strings: Vec<Vec<u8>>`, `unquote(&[u8]) -> Vec<u8>`, `quote(&[u8]) -> Vec<u8>`, `narrow_into(columns: &[Vec<u8>])`). Removes the `from_utf8_lossy` `�` desync on the N6 emitted-set side.
- **Spec §9.1 (doc, no code fix)** — the API prose lives in `docs/spec/architecture.md` (not `schema.md`, which has no §9.1) and already matches the shipped `new(g)` / `with_schema(g, schema)` / `allowed_mask(&mut self)`. No drift; **note the audit's file pointer correction in the PR description**.

**Tracked deferrals (branch, not now — justify in PR per constitution §6):**

- T1 Boolean/Temporal narrowing + temporal date-literal prefix treatment (needs the same B1 prefix path).
- `map`-as-`fn` lambda-argument L1 gap (needs the C1 real-tokenizer corpus decision).
- Widening the 8-schema / ~269-gold L2 fixture surface from pure-lingua's 161-DB corpus (after C1 lands).

---

## Testing & CI

- **Oracle-first ordering:** the merged synthetic L1+L2 lane must be RED (for the specific H1 cleared-gold reason) before `scope.rs` is touched, then GREEN after.
- **Hermetic (blocking) gate:** `tests/qwen_soundness.rs` with `PURECARD_QWEN_VOCAB` unset runs against committed `tests/fixtures/qwen-slice.json` — network-free, in `just ci` and a required job. `just selfcheck-real` (M1+M2) runs over the same slice-derived id streams.
- **Nightly (opt-in) full lane:** `just qwen-full` via `scripts/qwen-fetch.mjs`, `hf-hub` pinned to the revision SHA, `actions/cache` keyed on that SHA, first-party mirror fallback — cache-miss-only download.
- **Drift gate:** `cargo xtask qwen-slice` regenerates the committed slice from the full vocab; a stale slice REDs the PR.
- **No-regression:** no `pda::step`/`State`/`is_accepting`/`lexeme_kind` edit → L1 5034 byte-replay identical; `replay_tokens` reused unchanged; anti-drift gates (stale-lint, check-doc-facts, doctests) unaffected (selfcheck change is internal, ffi is doc-only).
- **Mutation:** the mutation floor (0 missed) must hold on the new `observe` boundary walk, `close_run`, `buffer_pending`, and the `RuleCache` memo — add targeted unit assertions so no introduced branch survives mutation.

---

## Dependency vetting

- Core `[dependencies]` **unchanged** = `{thiserror, serde, serde_json}`; `#![forbid(unsafe_code)]`; `check-core-deplight` stays green. The boundary walk is pure Rust over existing PDA primitives.
- `tokenizers` and `hf-hub` are added as **dev-dependencies only**, gated behind an empty `qwen = []` feature (mirroring the shipped `legend = []` idiom). Both are widely-used, first-party HuggingFace crates — pin the **current stable** release verified from crates.io at implementation time (constitution §2 "latest stable, verified"; do not carry a version from memory). Run the `dependency-vetting` skill for each before adding.
- `--all-features` stays offline because the full-vocab path is **env-gated** (`PURECARD_QWEN_VOCAB`), not feature-gated — no `#[ignore]`, no network in the default/all-features run.

---

## Implementation tasks (ordered, oracle-first, each independently testable)

1. **Oracle.** Add the `Merge`/`merge_mode` cross-boundary pass + two merged tests + the H2 coverage probe to `tests/bpe_split_soundness.rs`. Confirm merged L1+L2 **REDs** on H1 (assert the specific cleared gold id). *(test-only)*
2. **PDA accessor.** `Pda::stack()` (read-only) in `src/grammar/pda.rs`. *(no transition change)*
3. **Session wiring.** Capture `pre_pda` and pass `&pre_pda` to `observe` in `src/session.rs`.
4. **Byte-exact types.** `Lexeme::Str(Vec<u8>)`, `emitted_strings: Vec<Vec<u8>>`, `unquote(&[u8])`, `emitted_columns()->&[Vec<u8>]` in `scope.rs`; `quote(&[u8])`, `narrow_into(columns: &[Vec<u8>])` in `narrow.rs`.
5. **observe rewrite.** Boundary walk + `close_run` (byte-exact string close, H1) + `buffer_pending` (trailing open lexeme, B1/M3 preserved) + one-byte structural `dispatch_token` (H2). Oracle from step 1 flips **GREEN**; add scope unit tests. Confirm L1 5034 replay unchanged.
6. **M3-perf cache.** `RuleCache` mid-cursor memo in `narrow.rs`; collapse anchor cache; extend `the_anchor_mask_is_cached_and_reused` with a mid-cursor prefix.
7. **C1 real-Qwen lane.** `qwen` feature + dev-deps; `gpt2_byte_decoder`/`true_bytes`; `tests/qwen_soundness.rs` (env-gated slice/full); committed `tests/fixtures/qwen-slice.json`; `xtask qwen-slice`; `scripts/qwen-fetch.mjs` + cache; `just qwen-slice`/`qwen-full`; hermetic CI job. Report measured N1/N2/N5/N6 coverage numbers.
8. **M1 self-check rewrite.** `self_check(&CompiledGrammar, &[&[u32]])`; delete `longest_match` + 2 tests + `Unsegmentable`; `SMOKE`→`&[&[u32]]` under `#[cfg(feature="qwen")]`; `just selfcheck-real` in `just ci` + required job, pointing at C1's streams.
9. **M2 contract.** `check_eos_contract` in `selfcheck.rs` (called from the M1 gate); `ffi.rs` module + `compile_grammar` doc for the EOS/reserved-bit mapping and specials guard.
10. **Touch-ups + PR notes.** Confirm `docs/spec/architecture.md` §9.1 accuracy (correct the audit's `schema.md` pointer in the PR); record T1/`map`-lambda/fixture-widening as tracked deferrals with fold-vs-branch justification (constitution §6).

---

## Risks & rollout

- **L2 stays OFF at serving** until the real-Qwen L1+L2 lane is green with acceptable measured coverage. **L1 ships first** into RL rollouts (it is sound today).
- The blocking gate runs against the **committed slice**, not a live download — so CI never depends on HuggingFace availability (constitution §2 cache/mirror). The full lane is nightly/opt-in.
- H2 is soundness-safe (pass-through), so shipping L2 with sub-100% real coverage never masks a gold token — but the **coverage numbers from C1**, not synthetic-green, gate the serving decision.
- The `observe` rewrite is the only load-bearing core change; it is contained to L2's read-only re-drive of `step`, cannot alter L1, and is guarded by the merged oracle, the real-Qwen lane, and the mutation floor.
- Type churn (`String`→`Vec<u8>`) is mechanical and compiler-enforced across `scope.rs`/`narrow.rs`; `keeps_operand` reads only the discriminant, so T1 behaviour is preserved.

---

## Decisions for the human (recommend)

1. **Blocking gate = committed slice; full download = nightly opt-in.** *Recommend adopt* — satisfies constitution §2 (reproducible, cache-or-mirror, no bare CI download) while still catching vocab drift via `xtask qwen-slice`.
2. **Hermetic-vs-nightly split via `PURECARD_QWEN_VOCAB` env, not a second Cargo feature.** *Recommend adopt* — keeps `--all-features` offline and avoids `#[ignore]` (constitution §3 no-skip).
3. **Delete `longest_match` and re-base self-check on host id streams.** *Recommend adopt* — greedy longest-match validates paths real BPE never emits; the host owns segmentation.
4. **Reserve `<|im_end|>`/`<|endoftext|>` as specials; map model-EOS → reserved bit `V`; forbid real stop ids in `vocab_bytes`.** *Recommend adopt and document at the FFI boundary* (M2).
5. **Ship L1 now; hold L2 serving until C1 green.** *Recommend adopt* — the honest read of B2; L1 is certifiable today, L2 is not.
