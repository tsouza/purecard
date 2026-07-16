# Spec: L2 BPE-prefix soundness fix (adversarial-review B1/B2 + majors)

- **Status:** Draft — ready to implement
- **Created:** 2026-07-11
- **Owner:** purecard engineer (Thiago)
- **Review source:** `scratchpad/adversarial-review.md` (PureCARD @ `be2a052`) — 2 blockers (B1, B2), 5 majors (M1–M5), 5 minors.
- **Spec refs:** `docs/spec/schema.md` §6.5–§6.6 (N1/N2/N3/N6, T1), §9.3 (host EOS/specials), §4.5 (per-scope mask cache). Constitution §1 (pure, dep-light, forbid-unsafe), §2 (cache/mirror CI fetches, no shell), §3 (no skips), §4 (DRY, library-before-writing).

---

## Problem

**B1 — L2 narrowing is whole-token exact-match; it destroys soundness under real byte-level BPE (soundness-DESTROYING).**
`src/schema/narrow.rs::fill` iterates the whole vocab and keeps a token id **only if its whole bytes, run through `classify()` into a single `Lexeme`, exactly satisfy the rule**: `Member(class)` iff `Lexeme::Ident(text) && members.contains(text)`; `SourceIdent` iff `schema.is_source(text)`; `Column` iff `Lexeme::Str(content) && cols.contains(content)`. Qwen2.5-Coder is byte-level BPE: a schema identifier `countryName` arrives as sub-tokens `country` + `Name`; a classpath `spider::car_1::…::CarMakers` as many; the N6 column string `'MaxRevenue'` as `'` + `Max` + `Revenue` + `'`. The model must first emit the **leading** sub-token (`country`, `sp`/`spider`, `'`). `classify` makes that `Ident("country")` / `Str("")` — not a full legal name — so `fill` clears the bit and `mask.intersect(&narrow_buf)` removes it from the L1 mask. **The gold sub-token is masked, so the model cannot even begin the identifier.** Fatal for N3 (source class), N1/N2 (member/nav after `.`), N6 (relation-column string). Compounding: L2 only fires at the *anchor* state (`AfterDot`/`ExpectSource`/opening quote) and does nothing while inside an identifier (`InIdent`/`InSourceIdent`/`InStrLit`), so even a continuation sub-token gets no help.

**M3 (folded here, not independent) — the scope tracker mis-scopes multi-subword identifiers.** `ScopeTracker::observe(bytes, pre_state, schema)` (`src/schema/scope.rs:203`) is called once per accepted **token** and immediately `classify`es those bytes as a whole lexeme, then `resolve_member(text)`. Under BPE `observe` fires on `country` (a sub-token), `resolve_member("country")` returns `None`, `nav_cursor`/`last_resolved` are cleared, and the next sub-token `Name` navigates from a lost scope — N2/N5/T1 silently degrade. The fix shares B1's accumulation machinery.

**B2 — the bug is invisible because every L2 test embeds B1's false assumption.** Every L2 lane tokenizes gold with `tests/support/lex.rs::lex()` (via `TokenVocab::build`), which yields **one clean lexeme per token** — so every identifier is a single token and whole-token exact-match trivially passes. There is **no real tokenizer anywhere in the repo** (`grep -ri 'qwen|tokenizer|bpe|hf-hub'` returns nothing). The suite therefore measured *lexeme-level* soundness against a proxy, never *real-token-level* soundness against Qwen — the only property pure-lingua depends on. A green CI is false assurance.

**M1 — the self-check re-segments with greedy longest-match, not real BPE** (`src/selfcheck.rs::longest_match`), so it cannot catch the tokenization drift it exists to catch: it can pass on bytes Qwen splits differently.

**M4 (perf) — `fill` rescans the full ~152k vocab on every masked step** at every narrowing position (`narrow.rs:103`), violating §4.5's mandate to cache per-(state, class-scope) masks.

---

## Goals

- [ ] A synthetic BPE-split reproducer (`tests/bpe_split_soundness.rs`) **reds on today's L2** (leading sub-token `country`/`Max`/`'` masked) and **greens** after the fix — committed red first.
- [ ] L1+L2 **real-token soundness** verified: `replay_tokens` asserts `allowed_mask().test(gold_id)` at every step, plus `is_complete()` + EOS at end, for both the synthetic split vocab (always-on) and the real Qwen slice (gated, network-free fixture).
- [ ] **L1 stays 5034** — `soundness_replay` byte-replay is untouched; the schema-`None` path never constructs a trie.
- [ ] Core stays **dep-light + `forbid(unsafe_code)`**: `[dependencies]` remains `{ thiserror, serde, serde_json }`; `just check-core-deplight` passes.
- [ ] Tokenizer support (`tokenizers`, `hf-hub`) is **test-only + feature-gated**; no `#[test]` fetches from the network on the PR critical path.
- [ ] M3 mis-scope fixed (multi-chunk `observe` accumulates before resolving); M4 rescan removed (cache); M1 self-check consumes host token ids; M2 EOS/specials contract documented + asserted; M5 API drift recorded as ADR.

---

## Non-goals

- **L3** (semantic/type-inference beyond the shipped N1/N2/N3/N6/T1 levers) — out of scope.
- **Changing L1 semantics.** The byte-PDA, mask cache, and `accept_token` rollback are believed sound and must not regress. The only L1-touching change is *exposing* an existing boundary predicate (`is_ident_tail`) and a new pure `const fn` classifier — no transition changes.
- **Shipping the real-Qwen full-vocab lane as a blocking PR gate** before it is reproducible. The blocking gate is the committed fixture *slice* + the synthetic reproducer; the full-vocab fetch is nightly, `actions/cache`-backed.
- **Turning L2 ON at serving** as part of this change — see Rollout. L1 ships first.

---

## Design — B2 tier-1 synthetic BPE reproducer (the failing oracle FIRST)

Land this before any fix so the red is real and attributable to B1.

**Shared oracle — new `tests/support/bpe.rs`** (DRY, constitution §4; reused by both tiers and the M1 gold lane):

```rust
pub fn replay_tokens(session: &mut DecoderSession, ids: &[u32], eos: u32) {
    for (step, &id) in ids.iter().enumerate() {
        assert!(session.allowed_mask().test(id), "masked gold token id={id} step={step}");
        session.accept_token(id).unwrap_or_else(|e| panic!("reject id={id} step={step}: {e}"));
    }
    assert!(session.is_complete(), "stream incomplete");
    assert!(session.allowed_mask().test(eos), "EOS not set at completion");
}
```

**Tier 1 — `tests/bpe_split_soundness.rs` (always-on, network-free).** Deliberately fragment identifiers so a real BPE split is simulated with no tokenizer dependency:

- Lex each gold query with the existing `lex()`.
- For each **identifier / classpath** lexeme, split its bytes into 2–3 chunks at fixed offsets (thirds). `::` and `.` stay their own tokens. For the **column string** `'MaxRevenue'`, split `'` / `Max` / `Revenue` / `'`. Keep keywords, operators, numbers, punctuation whole.
- Dedup all chunks into `BTreeMap<Vec<u8>, u32>` (mirroring `TokenVocab::build`); `eos = ids.len()`; build `Vocab::from_byte_tokens`. The gold id-stream is the ordered chunk ids.
- Run `replay_tokens` for **L1** (`DecoderSession::new`, all arm-A + arm-C) — **stays green** (L1 byte-liveness keeps partial idents alive) — and for **L1+L2** (`with_schema` via `l2::load_schema`, the 8 fixtures / 13 arm-C in-scope queries) — **REDS today** on the leading chunk, **greens only after** the B1 narrower + accumulating tracker land. This same multi-chunk `observe` proves M3.

This is the network-free regression that anchors the whole change.

---

## Design — B1 prefix-aware narrower + M3 accumulator + M4 cache (the fix)

**One shared invariant:** L2 stops reasoning over whole classified lexemes and reasons over *reachable byte-prefixes*. One accumulator — the bytes emitted since the L2 anchor — is walked through one schema-legal **trie**. The narrower reads its cursor node; the tracker advances it; the cache is keyed on it. All three land together.

**Files:** `src/grammar/pda.rs`, new `src/schema/trie.rs`, `src/schema/narrow.rs`, `src/schema/scope.rs`, `src/session.rs`.

### (1) The trie — `src/schema/trie.rs` (core, pure `std`, no new dep)

Built from `Schema` only. Entries are the **raw bytes the model emits**: member names (`Member`), source classpaths + `let` (`SourceIdent`), quote-doubled `'…'` column strings (`Column`) — **byte-exact** (this kills the `from_utf8_lossy` minor at the same time).

```rust
pub(crate) struct Trie { nodes: Vec<Node> }        // node 0 = root
struct Node { next: Vec<(u8, u32)>, terminal: bool } // sorted, binary-searched — dense-safe, no 256-array blowup
pub(crate) enum Walk { Stay(u32), Complete, Diverge }

pub(crate) fn walk(t: &Trie, mut n: u32, bytes: &[u8]) -> Walk {
    for &b in bytes {
        // Try to DESCEND first: a byte that continues into a longer legal name
        // (e.g. `country` is terminal, yet `countryName` descends on `N`) must not
        // be treated as an overshoot. Only a byte with no child ends the walk.
        match t.child(n, b) {
            Some(c) => n = c,
            None => {
                return if t.is_terminal(n) && !is_ident_tail(b) {
                    Walk::Complete // a name ended at a legal boundary byte
                } else {
                    Walk::Diverge  // dead end, or an ident-tail byte past a name end
                };
            }
        }
    }
    Walk::Stay(n)
}
```

**Keep-test:** keep `T` iff `walk(cursor, T) != Diverge`. `is_ident_tail` is the byte-PDA's own boundary predicate (make it `pub(crate)` in `pda.rs`), so the overshoot tail past a name end is left to L1's `intersect` to re-vet — `L2 ⊆ L1` still holds. A single-token whole identifier walks `root → … → terminal` = `Stay`/`Complete`, kept: the old whole-lexeme path is subsumed, no special case.

### (2) `fill_trie` replaces `fill` — `src/schema/narrow.rs`

`narrow_into` gains a **cursor node** (from the tracker). `fill` becomes:

```rust
fn fill_trie(dst, vocab, eos, trie, cursor, kind) {
    dst.clear_all();
    for id in 0..vocab.len() as u32 {
        let bytes = vocab.bytes(id).unwrap_or(&[]);
        // Only identifier/string *candidate* tokens are subject to the rule.
        // Structural tokens (`.`, `(`, whitespace, keywords) are not candidates —
        // the rule never constrains them, so they pass through kept; walking them
        // through the trie would wrongly Diverge-clear them.
        let keep = !is_candidate(bytes, kind, cursor.mid_lexeme())
            || !matches!(walk(trie, cursor.node, bytes), Walk::Diverge);
        if keep {
            dst.set(id);
        }
    }
    dst.set(eos);  // §4.3 — EOS always kept
}
```

`SourceIdent`, `Member`, `Column` route through `fill_trie`. `ReValue(T1)` keeps its `keeps_operand` literal-class test **unchanged** (BPE does not break it — the leading `'`/digit still classifies correctly; T1-temporal deferral stays inert, noted in `docs/lessons.md`).

### (3) The M3 accumulator — `src/schema/scope.rs`

Expose a pure boundary classifier on `State` in `pda.rs`:

```rust
pub(crate) enum LexKind { Ident, Number, Str, Date }
impl State {
    pub(crate) const fn lexeme_kind(self) -> Option<LexKind> {
        match self {
            InIdent | InSourceIdent | InBinder | SourceColon | SourceColon2 => Some(Ident), // `::` continues one classpath
            SawNumSign | InNumberInt | NeedFracDigit | InNumberFrac => Some(Number),
            SawPercent | InDateLit => Some(Date),
            InStrLit { .. } => Some(Str),
            _ => None,
        }
    }
}
```

`ScopeTracker` gains `narrow_cursor: Option<(NarrowKey, u32 /*node*/)>` and `LexAcc { buf: Vec<u8> }`. `observe` stops calling whole-token `classify`; it **walks `bytes` through the existing `step`** from `pre_state`, tracking `lexeme_kind`:

- **entering** a lexeme (`None → Some`): open `buf` with the byte.
- **staying** (`Some(k) → Some(k)`): push.
- **closing** (`Some(k) → other`): **flush** — hand the whole `buf` to the existing `classify`/`resolve_member`/`on_ident`/T1-arming keyed on the stored anchor, then clear and re-dispatch the current byte as the next lexeme's opener/structural byte (`on_dot`/`on_arrow`). Because `buf` persists **across `observe` calls**, `country` + `Name` accumulate; `resolve_member` finally sees `countryName` — M3 fixed.

When an anchor fires, `narrow_cursor = Some((key, root))`; each accepted sub-token advances it by `walk` (`Complete` clears it). `position()` returns the constraining rule when the cursor is `Some` **and** the PDA is at the anchor *or* at `InIdent`/`InSourceIdent`/`InStrLit` — so narrowing persists across sub-tokens, firing *inside* the identifier. B1's narrower reads the same `buf` as the emitted prefix.

### (4) M4 cache — `src/session.rs` owns a `NarrowCache`

```rust
enum NarrowKey { Source, Member(String), ReValue(TypeClass), Column }
struct NarrowCache {
    source: OnceCell<BitMask>,
    members: HashMap<String, BitMask>,     // O(#classes), each built once
    revalue: HashMap<TypeClass, BitMask>,
    columns: /* nonstr_base OnceCell + per-name id lists, OR-in incrementally */,
}
```

`narrow_into` computes `NarrowKey`; **hit** → `mask.intersect(cached)` with no scan; **miss** → one `fill_trie` scan, stored. The anchor case (cursor = root) is the common path and warms after first use. Columns grow monotonically (OR-in newly-emitted column ids, never rescanned). Mid-identifier cursor states (cursor ≠ root) fall back to a live `fill_trie` walk — rare, bounded by identifier length.

**No-regression argument:** schema-`None` never constructs a trie or cache → L1 byte-replay (5034) is byte-for-byte untouched. Non-ident tokens (`.`, `(`) diverge at root → cleared exactly as before. EOS always set. `L2 ⊆ L1` remains structural.

---

## Design — B2 tier-2 real-Qwen gated lane + M1 self-check on real token-ids

**Tier 2 — new `tests/qwen_soundness.rs`, `#[cfg(feature = "qwen-oracle")]`.** Load `tests/fixtures/qwen/tokenizer.slice.json` (committed; the arm-C-relevant token bytes + specials only — so CI is network-free) via the `tokenizers` dev-dep; encode each gold `pure_text`; feed real ids through `replay_tokens`. `Vocab` needs the **true emitted bytes**, so undo GPT-2 byte-level→unicode and the `Ġ` metaspace with the inverted `bytes_to_unicode` decoder:

```rust
fn byte_decoder() -> HashMap<char, u8> {
    let mut bs: Vec<u32> = (0x21..=0x7e).chain(0xa1..=0xac).chain(0xae..=0xff).collect();
    let mut cs = bs.clone();
    let mut n = 0u32;
    for b in 0u32..256 { if !bs.contains(&b) { bs.push(b); cs.push(256 + n); n += 1; } }
    bs.iter().zip(cs).map(|(&b, c)| (char::from_u32(c).unwrap(), b as u8)).collect()
}
// Ġ (U+0120) → 0x20 space falls out automatically.
```

Build `Vocab::from_byte_tokens` in `get_vocab` id-order; map real EOS `<|im_end|>` = 151645 → the reserved EOS bit; dead-byte other specials. The fixture path is overridable by env `PURECARD_QWEN_TOKENIZER`, so the **nightly full lane** points it at the `actions/cache`-restored full `tokenizer.json` — no `#[test]` ever fetches.

**M1 — `self_check` over real ids (`src/selfcheck.rs`).** Delete `longest_match`, `Unsegmentable`, `SMOKE`, `self_check_smoke`. New signature `pub fn self_check(grammar: &CompiledGrammar, samples: &[&[u32]]) -> Result<(), SelfCheckError>` where each sample is a **host-produced token-id stream** (host tokenizes; PureCARD only verifies admissibility). Per sample: fresh session, assert `allowed_mask().test(id)` else `DeadEnd { query_index, step, id }` → `accept_token(id)` → `is_complete()` else `Incomplete`. Drop `pos` → `step` (token index). It is thin over `replay_tokens`' contract. New `tests/selfcheck_gold.rs` tokenizes gold (Tier-1 split vocab always; Tier-2 slice under feature) and runs `self_check` for both `new` and `with_schema`, wired as a **mandatory** `just selfcheck-gold` gate.

---

## Design — M2 EOS/specials contract; M5 spec-drift note; minors

**M2 — Qwen EOS/specials contract.** The reserved EOS bit is synthetic index `V` (`mask_len = V+1`); Qwen's real stop `<|im_end|>` = 151645 is **in-vocab**.

- **Document** in `ffi.rs` (Session doc) + a `docs/spec/` §9.3 note: the host maps its real EOS id ↔ the reserved bit `V`; when PureCARD sets bit `V` the host may sample its real EOS; the host must never forward the real EOS id into `accept_token`; specials must be excluded from `vocab_bytes` or given bytes the PDA treats as dead inside the query span.
- **Assert** (unit + the gold lane): (a) the reserved bit is set **iff** `is_complete()`; (b) every declared special id (`im_start`, `im_end`, `endoftext`, FIM `fim_*`, `repo_name`) is **inadmissible mid-query** at every step.

**M5 — API drift (spec §9.1).** The code's `new(g)` + `with_schema(g, schema)` + `allowed_mask(&mut self)` is the better shape (reused buffer needs `&mut`; the split gives clean L1/L1+L2 parity) than the spec's single `new(g, Option<Schema>)`/`&self`. **No code change** — record the intentional deviation + rationale as a new ADR in `docs/decisions/` and in `docs/domain-model.md`; flag the pure-research repo to update §9.1.

**Minors (fold in where cheap):**

- **N6 byte-exact column compare** — subsumed by the byte-trie (§ trie entries are raw bytes); remove the `from_utf8_lossy` in unquote.
- **map-lambda L1 gap** — extend `fnArgs` to accept a lambda production so `map(x|$x.f)` doesn't dead-end; add a gold `map`-lambda fixture. The Tier-1 lane surfaces it. *Judge fold-vs-branch in the PR* (constitution §6) — recommend a **separate** small PR since it touches L1 grammar, not L2.
- **T1 temporal prefix** — inert now; note in `docs/lessons.md` that the `%`-date path needs the same B1 treatment when temporal comparisons enter the corpus.

---

## Testing & CI

**Hermetic / always-on (blocking `just ci`):**

- `tests/bpe_split_soundness.rs` — Tier 1 L1 (arm-A + arm-C) green, L1+L2 (8 fixtures / 13 arm-C) red→green. Network-free.
- `tests/selfcheck_gold.rs` — M1 over Tier-1 split vocab, `new` + `with_schema`.
- `tests/qwen_soundness.rs` under `--features qwen-oracle` — **committed slice fixture**, network-free, on a required job.
- `src/schema/trie.rs` unit tests — prefix / overshoot-`Complete` / divergence.
- M2 unit + gold assertions — EOS-iff-complete; specials inadmissible mid-query.

**Nightly / manual (never on the PR critical path):**

- Full-vocab Qwen lane under a `qwen-oracle-full` feature: a Bun `.mjs` orchestrator (`just qwen-oracle`) fetches `tokenizer.json` into `actions/cache` keyed on the pinned model revision (constitution §2 — cache/mirror; no bare `curl`), then runs the same feature with `PURECARD_QWEN_TOKENIZER` pointed at the restored file.

**No-regression checks:**

- `soundness_replay` (5034 gold byte-replay) unchanged and green.
- `mask_oracle`, `l2_soundness`, `l2_precision` unchanged and green after the fix.
- `just check-core-deplight` passes (`tokenizers`/`hf-hub` never enter `[dependencies]`).
- `#![forbid(unsafe_code)]` holds (trie is pure `std`).

---

## Dependency vetting

- **`tokenizers`** (HF): **optional dev-dep only**, behind `[features] qwen-oracle = ["dep:tokenizers"]`. Never in `[dependencies]`; the core's `{ thiserror, serde, serde_json }` allowlist and `check-core-deplight` are untouched. Used only to *decode* a committed fixture in `tests/` — the core still ships no model tokenizer (host supplies `Vocab`). Run the `dependency-vetting` skill and record the note + `cargo-deny`/`cargo-vet` entry; pin the **current** stable via `cargo add tokenizers` (constitution §2 "latest stable, verified"), not from memory.
- **`hf-hub`**: **not compiled into any `#[test]`** — used only by the nightly Bun `.mjs` fetch step, or as a dev-dep behind `qwen-oracle-full`. Same vetting + pin discipline.
- Core `[dependencies]` **unchanged**. The prefix trie is bespoke pure `std` (no new pin): "library before writing" is satisfied because no lockfile crate provides a byte-trie tuned to this walk, and a new pin would drag weight into the pure core for a ~40-line structure — the bespoke code owns its edge cases via the trie unit tests.

---

## Implementation tasks (ordered, oracle-first, each independently testable)

1. **`tests/support/bpe.rs::replay_tokens`** — the shared oracle. Unit-exercise against the existing lexeme vocab (green).
2. **Tier-1 `tests/bpe_split_soundness.rs`** — split vocab; L1 green, **L1+L2 committed RED**. Proves B1/B2 before any fix.
3. **`State::lexeme_kind` + `pub(crate) is_ident_tail`** in `pda.rs` (State-bijection test stays green).
4. **`src/schema/trie.rs`** — `Trie`/`Node`/`Walk`/`walk` + unit tests (prefix / overshoot / divergence).
5. **`fill_trie` + cursor param** in `narrow.rs::narrow_into` (N3/N1/N2/N6 route through it; T1 unchanged; drop `from_utf8_lossy`).
6. **`LexAcc` byte-walk `observe` + `narrow_cursor` + `position()` persistence** in `scope.rs`; thread cursor from `session.rs::allowed_mask`. → **Tier-1 L1+L2 goes GREEN** (proves B1 + M3).
7. **`NarrowCache`** in `session.rs` (M4) — hits after warmup; columns incremental.
8. **M1** — rewrite `self_check` to consume `&[&[u32]]`; delete greedy/`SMOKE`; add `tests/selfcheck_gold.rs`; wire `just selfcheck-gold` into `just ci`.
9. **Tier-2** — commit `tokenizer.slice.json`; add `qwen-oracle` feature + `tokenizers` dev-dep (vetted, pinned); `tests/qwen_soundness.rs`; required job under `--features qwen-oracle`. → **Tier-2 slice GREEN.**
10. **M2** — document EOS/specials in `ffi.rs` + `docs/spec/` §9.3; add the EOS-iff-complete + specials-inadmissible assertions.
11. **M5** — ADR in `docs/decisions/` + `docs/domain-model.md`; flag pure-research §9.1.
12. **Nightly full lane** — `qwen-oracle-full` feature, `just qwen-oracle`, Bun `.mjs` + `actions/cache` workflow. Update `docs/lessons.md` (BPE-prefix lesson, T1-temporal note).
13. **Minor (separate PR, recommended):** map-lambda `fnArgs` production + gold fixture — judge fold-vs-branch in that PR body.

---

## Risks & rollout

- **L1 ships first.** L1-only (`DecoderSession::new`) is unchanged by this work; once Tier-1 L1 + the Qwen L1 slice are green, L1 is safe for RL rollouts. The adversarial review agrees L1 is sound-in-practice; the new lanes convert "very likely" to "verified."
- **L2 stays OFF at serving until the real lane is green.** `with_schema` must not be enabled at serving until both Tier-1 L1+L2 and the Qwen-slice L1+L2 lanes are green (and ideally one nightly full-vocab pass). This is a rollout gate, documented in the ADR and `docs/lessons.md`.
- **Fixture drift risk:** the committed `tokenizer.slice.json` must match the pinned Qwen revision. Mitigation: the nightly full-vocab lane re-derives against the real `tokenizer.json`, so a slice that drifts from upstream fails nightly, not silently.
- **Cursor-state cache misses** (mid-identifier, cursor ≠ root) fall back to a live `fill_trie` walk — bounded and rare; the common anchor=root path is cached. If profiling shows it hot, memoize per (key, node); not needed for correctness.
- **`unsendable` / GIL** for PyO3 batched rollouts is a host concern, unchanged here (one `Session` per stream).

---

## Decisions for the human (recommend)

1. **Blocking gate = committed Qwen *slice* + synthetic Tier-1; full vocab is nightly.** — *Recommend yes.* Keeps CI hermetic and fast (constitution §2) while still exercising real Qwen bytes on the arm-C surface.
2. **Ship the map-lambda L1 grammar fix as a separate PR, not folded here.** — *Recommend yes* (fold-vs-branch, constitution §6): it touches L1 grammar, orthogonal to the L2 soundness fix; folding would muddy the "L1 unchanged" guarantee this PR rests on.
3. **`tokenizers` as an optional dev-dep behind `qwen-oracle`; `hf-hub` nightly-only.** — *Recommend yes*; needs a `dependency-vetting` note + current-stable pin before merge.
4. **Record the §9.1 API split as code-wins (ADR), update pure-research rather than the code.** — *Recommend yes*; the `&mut` + split is the better contract for L1/L1+L2 parity.
5. **Keep T1-temporal deferred (inert) this change; note the follow-up.** — *Recommend yes*; no temporal comparisons in the corpus yet, and the `%`-date path needs the same B1 treatment when they arrive.
