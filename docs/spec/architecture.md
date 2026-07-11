# PureCard Spec — Architecture

_[Spec index](README.md) · [domain model](../domain-model.md)_

## 3. Architecture

### 3.1 Design stance: CFG skeleton (L1) + semantic narrowing (L2)

A pushdown automaton (PDA) handles Pure's context-free shape (the `->` pipeline, bracket matching, lambda structure). A thin type/scope tracker handles the context-sensitive parts — specifically, _which property is legal after `$x.`_ depends on the class `$x` is bound to, which a pure CFG cannot express. This mirrors PICARD's lexical/grammatical/schema-consistency tiers, adapted to Pure: **L1 = the PDA over the emitted grammar; L2 = a typed-scope overlay that intersects L1's terminal set with the schema-legal set at exactly the identifier/type positions L1 defines.**

L2 never _widens_ what L1 allows — it only narrows. `DecoderSession::new(grammar, None)` runs L1-only (pure syntactic guarantee, no schema needed) — useful before a schema is available and as a fast path.

### 3.2 Crate layout

Single Rust crate, `picard_pure` (published as `purecard`), with an optional PyO3 feature exposing bindings. Internal modules:

```
picard_pure/
  grammar/        L1: emitted-Pure grammar -> byte-level pushdown automaton (PDA)
    spec.rs         EBNF-ish grammar definition (the emitted subset, §5)
    pda.rs          compiled pushdown automaton (states, stack symbols, byte transitions)
    build.rs        grammar-spec -> PDA compiler
  vocab.rs        model vocabulary as raw byte strings per token id; token trie
  mask/
    cache.rs        context-independent per-state token-mask cache (the perf core, §4)
    engine.rs       per-step mask = cache[state] ∩ runtime(context-dependent) ∩ schema-narrow(L2)
  schema/          L2: schema-consistency overlay
    model.rs        Schema { classes -> {prop -> type} }, passed from Python at session init
    scope.rs        lambda scope / type environment tracker (what class is the row var bound to)
    narrow.rs       at identifier/type positions, restrict terminals to the schema-legal set
  session.rs      DecoderSession: state + stack + scope; accept_token / allowed_mask / is_complete
  ffi.rs          #[cfg(feature="python")] PyO3 bindings (§9)
  testing/        soundness/differential harness hooks (§8)
```

### 3.3 Core data flow (per generation)

```
Python (inference loop)                 Rust (picard_pure)
─────────────────────────               ───────────────────
build Schema from PMCD/MCP  ──init──▶    DecoderSession::new(compiled_grammar, Some(schema))
loop each decode step:
  logits = model.forward(...)
  mask   = session.allowed_mask()  ◀──   BitMask over vocab (cached + runtime + schema-narrowed)
  logits[!mask] = -inf
  tok    = sample(logits)
  session.accept_token(tok)        ──▶   advance PDA state + stack + scope; err if illegal
  if session.is_complete() and tok==EOS: break
```

- `allowed_mask` is called every step over the full vocab (~150k tokens) — it must be cheap (§4).
- `accept_token` advances the recognizer (PDA state + stack + scope), erroring if the token is illegal.
- `is_complete` is true when the PDA is in an accepting state (a syntactically — and, under L2, schema- — complete query), so the loop knows EOS is legal.

---

## 4. The masking algorithm (performance core)

Naive per-token PDA replay at every step over a 150k vocab is far too slow. PureCard follows the **xgrammar-style split** into context-independent (cacheable) and context-dependent (runtime) token sets, with a per-state mask cache.

### 4.1 Compile once

1. **Compile** the grammar to a byte-level PDA once (per grammar). Preprocess the model vocabulary into a **byte trie** (each token id → its raw byte string).

### 4.2 Partition the vocabulary per PDA state

1. Partition vocabulary tokens, per PDA state, into two classes:
   - **context-independent**: acceptance depends only on the current state, not on the stack contents (the vast majority — keywords, identifier characters, literals). Precompute a **per-state token bitmask cache**.
   - **context-dependent**: acceptance depends on the stack (e.g. a closing `)` / `]` is legal only if the matching opener is on top of the stack). This is a small set; check it at runtime by consulting the stack.

### 4.3 Per step

1. Compute the mask as:

   ```
   mask = cache[state]                         # cached context-independent bitmask
   mask = flip_context_dependent(mask, stack)  # small runtime stack check
   if L2 active and state is an identifier/type position:
       mask = mask ∩ schema_legal_terminals(scope)   # §6 narrowing
   return mask
   ```

   The context-dependent flip touches only the small set of stack-sensitive terminals. The L2 intersection applies **only** at identifier/type positions (§7 table), keeping the runtime fraction small.

### 4.4 Byte-level detokenization (BPE↔Pure alignment, solved)

1. Detokenization is **byte-level**, so subword boundaries never need special alignment: a candidate token is admissible iff **feeding its raw bytes advances the byte-PDA to a non-dead state**. This sidesteps the BPE/Pure-token misalignment that PICARD handled with explicit incremental parsing (§1.1). The decoder treats every model token as an opaque byte string; the host is responsible for supplying the correct raw bytes per token id (§9).

### 4.5 Latency target and cache construction

Target: **mask generation ≤ a few hundred µs/token**, so it is never the bottleneck against the model's ms-scale forward pass. The per-state cache is what makes this hold. Build it **lazily** — memoize each state's mask the first time that state is reached — to avoid precomputing masks for unreachable states.

For L2, additionally **cache per-(state, class-scope) identifier masks**: the set of schema-legal identifiers after `$x.` depends only on the class `$x` is bound to, so it can be memoized per (position, class) pair rather than recomputed every step.

### 4.6 Shipped M5 baseline (the locked performance record)

The criterion suite (`benches/allowed_mask.rs`) locks the shipped per-step baseline. The intended regression guard is CodSpeed (the `bench` job — deterministic *instruction count*, walltime-independent, so it reproduces faithfully in CI), but it is **opt-in, not yet an enforced merge check**: the `bench` job is gated behind `vars.CODSPEED_ENABLED == 'true'`, so until the CodSpeed app is installed and that variable is set, it posts perf deltas without blocking a PR. Once enabled, CodSpeed's instruction-count delta is what fails a PR. Recommended CodSpeed threshold at first-lock: **±10 %** instruction count, ratcheted tighter over time (a PROTECTED gate only tightens).

The families and the *relative* cost each establishes (no absolute figures are quoted here: there is no gate asserting a hand-copied number against the bench output, so only the shape is stated — the bench itself holds the measurements):

- **`allowed_mask`** — steady-state per step, and the cheapest at shallow and identifier positions. The deep-stack worst case (nested open frames, maximal context-dependent re-probe) is the costliest per-step path, but stays inside the **≤ a few hundred µs/token** design target (§4.5) and is dominated by the model's forward pass.
- **`accept_token`** — a whole-token advance is cheap: a byte-fold through a PDA clone.
- **`cache_win`** — the M2 partition cache: a warm step (word-wise copy) is dramatically cheaper than a cold first-visit build (which probes the whole ~150k-token vocabulary). This is why the lazy per-state cache is load-bearing, not an optimization.
- **`l2_overhead`** — the schema-narrowing block at an identifier position adds a small constant over the L1 mask (the `intersect` plus the scope-legal set build); L2 ⊆ L1 by construction, so it only ever narrows.

---

## 9. Public API (Rust + PyO3) and integration boundary

### 9.1 Rust core

```rust
pub struct Vocab { /* token id -> raw bytes; byte trie */ }
impl Vocab { pub fn from_byte_tokens(tokens: Vec<Vec<u8>>, eos: u32) -> Self; }

pub struct PureGrammar { /* parsed spec */ }
impl PureGrammar {
    pub fn from_spec(spec: &str) -> Result<Self, GrammarError>;   // §5 EBNF
    pub fn compile(&self, vocab: &Vocab) -> CompiledGrammar;      // build PDA + lazy caches
}

pub struct Schema { /* §6.2 */ }
impl Schema { pub fn from_json(s: &str) -> Result<Self, SchemaError>; }

pub struct DecoderSession<'g> { /* state, stack, scope, &CompiledGrammar */ }
impl<'g> DecoderSession<'g> {
    pub fn new(g: &'g CompiledGrammar, schema: Option<Schema>) -> Self;
    pub fn allowed_mask(&self) -> &BitMask;      // over vocab; EOS bit set iff is_complete()
    pub fn accept_token(&mut self, id: u32) -> Result<(), DecodeError>;
    pub fn is_complete(&self) -> bool;
    pub fn reset(&mut self);                      // reuse allocation across generations
}
```

### 9.2 PyO3 boundary

`#[cfg(feature="python")]` — the _only_ Python-facing surface; keep it thin:

```python
# picard_pure / purecard (compiled extension)
g    = compile_grammar(spec_str, vocab_bytes, eos_id)     # once per (model, grammar)
sess = Session(g, schema_json_or_None)                    # once per generation
mask = sess.allowed_mask()        # -> np.ndarray[bool] or packed bits, len == vocab
sess.accept_token(tok_id)         # advance; raises on illegal token
sess.is_complete()                # bool
```

### 9.3 Integration boundary (host code lives elsewhere, stated so the API is right)

PureCard is the **Rust half of a Python/Rust split**. Python owns training, datagen, and orchestration (it is ecosystem-bound: MLX, HuggingFace, tokenizers); Rust owns the durable, performance- and correctness-critical serving kernels. PureCard exposes itself via PyO3 to a Python inference loop and constrains **only the final-query span** of an agentic trajectory (not the whole trajectory).

Host-side contract for the inference loop (out of scope to build here):

- The host provides the vocabulary as **raw byte strings per token id**, handling the tokenizer's metaspace / leading-space conventions (byte-BPE vs SentencePiece) _before_ handing bytes over. Getting this exactly right is a soundness prerequisite; the decoder treats tokens as opaque byte strings.
- The host builds `Schema` from the PMCD / MCP tools and passes it (as JSON) at session init.
- The host **activates constraint only over the final-query span** of a trajectory (a mode switch), not over tool calls or reasoning text.
- The host owns sampling; PureCard only masks.
- Concrete loop: create a `Session` with the query's schema at the moment the final-query span begins; each step, `&`-mask the logits; sample; `accept_token`; stop when `is_complete()` and EOS is sampled.
