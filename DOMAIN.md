# PureCard — Master Specification

**A Rust grammar/schema-constrained decoder for Legend Pure (a "PICARD-for-Pure" constrained-decoding library).**

- **OSS project name:** `PureCard` — *Pure* + *PICARD* lineage; reads as the "reference **card** of legal moves" for Pure generation.
- **Crate / repo:** `purecard` (internal Rust module name in this spec: `picard_pure`; the two names are interchangeable — the published crate is `purecard`).
- **Status:** Design, ready for implementation.
- **What this document is:** the *complete, self-contained* build spec for the constrained-decoder component. A fresh engineer (or a fresh Claude instance) with **only this file** — no repo access, no other design docs — can build PureCard end-to-end. All grammar rules, all schema-consistency rules, the masking algorithm, the API surface, and the build milestones are inlined here in full. The only external things the reader must fetch are (a) the *test corpus* of gold Pure queries and (b) a running *Legend engine* — both are data/services, not prose, and their locations are given in §8.

General Rust workspace conventions, CI, and agentic dev setup are out of scope; this document is laser-focused on the PICARD-to-Pure domain: what to build, the algorithm, the Pure grammar, the schema-consistency layer, the correctness oracle, the integration boundary, and the build milestones.

Project context in one line: an upstream project ("pure-lingua") trains an LLM to emit Legend Pure queries; at single-shot serving we want *guaranteed-valid* output in one forward pass (no compile-repair round-trip). PureCard provides that guarantee via constrained decoding.

---

## Table of contents

1. What PureCard is — the interface and the guarantee boundary
2. Scope and non-goals
3. Architecture (crate layout, PDA + semantic narrowing)
4. The masking algorithm (context-independent/dependent partition, mask cache, byte-level BPE alignment, latency target)
5. L1 — the emitted-Pure grammar (full EBNF, lexical rules, per-construct notes)
6. L2 — schema-consistency (Schema contract, scope tracker, N/T rules)
7. The L1↔L2 consistency-contract table
8. Correctness — the oracle-driven test strategy (soundness via gold corpus, completeness via the Legend compiler)
9. Public API (Rust + PyO3) and the integration boundary
10. Build milestones M0–M5
11. Risks and open questions
12. Roadmap position and build triggers
13. Test corpus — contents, provenance, location
14. Legend engine setup (for the completeness oracle) + CI

---

## 1. What PureCard is — the interface and the guarantee boundary

### 1.1 What PICARD is (background the reader will not have)

**PICARD** (Scholak, Schucher, Bahdanau — *"Parsing Incrementally for Constrained Auto-Regressive Decoding from Language Models,"* EMNLP 2021) is the original constrained decoder for text-to-SQL. Its central idea: an autoregressive language model, at each decode step, proposes a probability distribution over its whole vocabulary; PICARD sits **between the model's logits and the sampler** and rejects any next-token that would put the partial output on a path with no valid completion. The model's weights are **frozen** — PICARD is **inference-only** and **model-agnostic**: it does not fine-tune, it does not know the model's internals, it only reads the tokens generated so far and returns a decision about which next-tokens are still admissible.

The conceptual interface is a per-step logits transform:

```
mask(grammar_state, schema, logits) -> logits'
```

where `logits'` sets the logit of every inadmissible token to −∞ (so the sampler can never pick it), leaving admissible logits untouched. Output is valid **by construction**.

PICARD defines **three tiers** of checking, applied incrementally as text is generated:

1. **Lexical** — the emitted tokens form valid lexemes of the target language.
2. **Grammatical (syntactic)** — the partial output parses as the target grammar. *Schema-independent.*
3. **Schema-consistency** — the identifiers and types resolve against *this specific database's* schema: no phantom tables/columns, no type mismatches. *Per-database, context-sensitive.*

A hard problem PICARD solves is **BPE↔target-token misalignment**: the language model's subword (BPE) tokens do not align with the target language's lexical tokens — a single BPE token can straddle a keyword boundary, and a target keyword can span several BPE tokens. PICARD handles this by **incremental parsing**: it feeds generated text through the parser piece by piece and checks reachability of a valid parse. PureCard solves the same problem more simply, at the **byte level** (§4.4): it treats every model token as an opaque raw byte string and asks only whether feeding those bytes advances a byte-level automaton to a non-dead state, which sidesteps subword-boundary alignment entirely.

### 1.2 PureCard, mechanically

PureCard is a **logits mask generator** driven by an incremental recognizer for a restricted subset of **Legend Pure** (the functional query/modeling language of the FINOS Legend platform). At every decode step the model proposes a distribution over its vocabulary (~150k tokens); PureCard, given the tokens generated so far, returns a boolean bitmask over the vocabulary marking the tokens that keep the partial output on a path to a valid Pure query. The Python inference loop applies the mask (sets disallowed logits to −∞) before sampling. Output is valid by construction.

Two constraint levels are both in scope; a third is explicitly out of scope:

| Level                      | Guarantees                                                                                  | In scope                    |
| -------------------------- | ------------------------------------------------------------------------------------------- | --------------------------- |
| **L1 — syntactic**         | the output parses as (emitted-subset) Pure                                                  | ✅ core                      |
| **L2 — schema-consistent** | identifiers/types resolve against *this* model — no phantom classes/props, no type mismatch | ✅ overlay                   |
| L3 — faithful              | the query answers the question that was asked                                               | ❌ impossible at decode time |

### 1.3 The guarantee boundary (the single most important scoping fact)

PureCard guarantees **validity** (L1: the query parses) and **schema-consistency** (L2: the query *compiles against this model* — every identifier resolves and every operation type-checks). It does **NOT** and **CANNOT** guarantee **faithfulness** — that the query *means what was asked*.

The three levels form a strict containment hierarchy:

```
                 faithful  ⊂  schema-consistent  ⊂  syntactic
        (answers the Q)      (compiles on model)     (parses)
        L3 — out of scope    L2 — in scope           L1 — in scope
```

Read the containment right-to-left: every faithful query is schema-consistent, and every schema-consistent query is syntactic — but not vice versa. PureCard moves the output from "arbitrary text" into the *syntactic* set (L1) and, with a schema, into the *schema-consistent* set (L2). It **cannot** move it into the *faithful* set.

Why faithfulness is structurally unreachable at decode time: the mask sees the schema and the partial output string, but **never the question's intent**. Consider a database with a `Singer` class. Both

```pure
Singer.all()->filter(x|$x.country == 'France')
Singer.all()->filter(x|$x.name    == 'France')
```

are perfectly schema-consistent — `country` and `name` are both real String properties. L2 narrows `$x.` to the real member set `{singerId, name, country, songName, songReleaseYear, age, isMale}`; **every** member is a legal next-token, and L2 has no basis to prefer `country` over `name`. Only the model's own probability mass — shaped by training and by the in-context question — picks the faithful column. L2 only guarantees the model cannot pick a *non-existent* or *mistyped* column.

**False-confidence risk to state prominently.** Because L2 output *always compiles*, a downstream reader may over-trust it. A query can be 100% schema-consistent and 100% wrong (wrong column, wrong join, wrong aggregate). L2 narrows the *error surface* from {syntax errors ∪ phantom-reference errors ∪ type errors ∪ wrong-answer errors} down to {wrong-answer errors} — it does **not** shrink the wrong-answer class at all, and may enlarge it at the margin (see the over-constraint caveat in §11). Any evaluation must keep measuring execution-equivalence (faithfulness) with the constraint ON; a rise in "compiles but wrong" under L2 is the signal to watch.

---

## 2. Scope and non-goals

**In scope:** a single Rust crate that compiles the emitted-Pure grammar into a byte-level pushdown automaton, computes per-step logits masks efficiently, optionally narrows those masks with a schema-consistency overlay, and exposes the whole thing to Python over a thin PyO3 boundary — plus the oracle-driven test harness that proves it correct.

**Non-goals (keep the component small):**

- **Not** a full Pure parser/compiler. Only the *emitted subset* the trained model actually produces (class-anchored relation pipelines) needs to be recognized, and only far enough to mask next-tokens.
- **Not** faithfulness, ranking, or repair. It prunes invalid branches; it does not choose the right valid one.
- **Not** the training pipeline, the Python inference stack, tokenizer training, or general Rust project scaffolding. Only the decoder crate and its PyO3 boundary.
- **Not** trajectory constraint. The model emits full agentic trajectories (tool calls, reasoning, then the final query); PureCard constrains **only the final-query span** — the Python loop activates it when that span begins (integration assumption, §9).
- **Not** full Pure syntax. The grammar is a deliberate over-approximation of validity in a few places (§5.6); the Legend compiler oracle (§8) catches escapes and drives tightening. Do not gold-plate — keep it minimal to stay fast and sound.
- **Not** runtime data values. L2 never constrains literal *values* (only their *types*), because any type-valid literal compiles.

---

## 3. Architecture

### 3.1 Design stance: CFG skeleton (L1) + semantic narrowing (L2)

A pushdown automaton (PDA) handles Pure's context-free shape (the `->` pipeline, bracket matching, lambda structure). A thin type/scope tracker handles the context-sensitive parts — specifically, *which property is legal after `$x.`* depends on the class `$x` is bound to, which a pure CFG cannot express. This mirrors PICARD's lexical/grammatical/schema-consistency tiers, adapted to Pure: **L1 = the PDA over the emitted grammar; L2 = a typed-scope overlay that intersects L1's terminal set with the schema-legal set at exactly the identifier/type positions L1 defines.**

L2 never *widens* what L1 allows — it only narrows. `DecoderSession::new(grammar, None)` runs L1-only (pure syntactic guarantee, no schema needed) — useful before a schema is available and as a fast path.

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

---

## 5. L1 — the emitted-Pure grammar (syntactic constraint level)

L1 is the context-free grammar of the *emitted subset* of Legend Pure that the trained model actually produces — **class-anchored relation pipelines**. It makes the output *parse*; L2 (§6) makes the identifiers/types *resolve against a model*; L3 (faithfulness) is out of scope for both.

**Core principle (oracle-driven).** Every production below is derived from, and testable against, the thousands of execution-verified gold Pure queries the upstream pipeline already produced (see §8 for corpus locations). The verified corpus **is** the spec: a grammar that masks a token appearing in a gold query is a soundness bug. Do **not** invent productions the corpus does not exercise, and do **not** omit ones it does. The construct inventory in §5.7 is the empirical evidence (counts over ~1,791 verified gold queries at time of drafting).

### 5.1 Query envelope (two observed top-level forms)

The final-query span PureCard constrains is a Pure lambda. Two envelopes occur in the corpus:

```ebnf
query        = simpleQuery | blockQuery ;
simpleQuery  = "|" pipeline ;                          (* the common case: ~88% of gold *)
blockQuery   = "{|" { letBinding ";" } pipeline "}" ;  (* let-scoped block: ~12% of gold *)
letBinding   = "let" ident "=" pipeline ;              (* a named sub-pipeline, referenced as $ident *)
```

`blockQuery` binds one or more sub-pipelines with `let` and returns a final `pipeline`; a `let`-bound name is referenced later as a `$ident` row/scalar value (e.g. the `->at(0).getString('mps')` scalar-extraction pattern, §5.7). L2's scope machine (§6.4) enters each `pipeline` independently.

### 5.2 Pipeline and steps

```ebnf
pipeline   = source , { "->" step } ;
source     = classpath , ".all()" ;                    (* N3 position: classpath must be a real class *)
step       = filter | project | groupBy | olapGroupBy | restrict
           | sort | take | distinct ;
filter     = "filter" "(" lambda ")" ;
project    = "project" "(" "[" colLambda { "," colLambda } "]"
                       "," "[" strlit    { "," strlit    } "]" ")" ;
groupBy    = "groupBy" "(" "[" { keyLambda { "," keyLambda } } "]"   (* key list MAY be empty: [] *)
                       "," "[" agg       { "," agg       } "]"
                       "," "[" strlit    { "," strlit    } "]" ")" ;
olapGroupBy= "olapGroupBy" "(" "[" strlit { "," strlit } "]"          (* partition columns *)
                       "," sortSpec                                    (* window order, e.g. desc('MaxRevenue') *)
                       "," reduceLambda                                (* e.g. y|$y->rowNumber() *)
                       "," strlit ")" ;                                (* output column name *)
restrict   = "restrict" "(" "[" strlit { "," strlit } "]" ")" ;
sort       = "sort" "(" ( strlit "," sortdir | sortSpec { "," sortSpec } ) ")" ;  (* chainable multi-key *)
sortSpec   = ( "asc" | "desc" ) "(" strlit ")" ;       (* olap/sortBy helper form *)
sortdir    = "SortDirection.ASC" | "SortDirection.DESC" ;
take       = "take" "(" int ")" ;
distinct   = "distinct" "(" ")" ;
```

`groupBy` with an empty key list `[]` is the aggregate-over-all form (verified: the `count(*)` gold). `limit`/`extend` are **not** observed in the current corpus — omit them until a gold query exercises them (per §5, do not add unexercised productions; `take` is the emitted row-limiter).

### 5.3 Lambdas and expressions (the L2 narrowing surface)

```ebnf
lambda       = binderVar "|" boolExpr ;                (* filter predicate *)
colLambda    = binderVar "|" valueExpr ;               (* project column *)
keyLambda    = binderVar "|" valueExpr ;               (* groupBy key *)
mapLambda    = binderVar "|" valueExpr ;               (* agg map *)
reduceLambda = binderVar "|" reduceExpr ;              (* agg reduce, e.g. y|$y->sum() / ->count() *)
agg          = "agg" "(" mapLambda "," reduceLambda ")" ;

boolExpr   = cmp { ("&&" | "||") cmp }
           | "(" boolExpr ")" { ("&&" | "||") cmp } ;
cmp        = valueExpr cmpop valueExpr                 (* T1/T2/T6 operand-type + multiplicity position *)
           | valueExpr "->" boolPred                   (* T4/T6 predicate position *)
           | navExpr "->" "in" "(" pipeline "." ident ")" ;  (* subquery membership *)
cmpop      = "==" | "!=" | ">" | "<" | ">=" | "<=" ;

reduceExpr = refVar "->" reducer "(" ")" ;             (* T3 reducer-type position; body use is $-prefixed *)
reducer    = "count" | "sum" | "average" | "min" | "max" | "size" | "rowNumber" ;

boolPred   = ( "exists" | "contains" | "startsWith" | "endsWith"
             | "isEmpty" | "isNotEmpty" ) "(" [ predArg ] ")" ;
predArg    = lambda | valueExpr ;                      (* exists takes a lambda; contains/startsWith take a value *)

(* valueExpr is any scalar-valued expression usable as an operand, a projected column, or a key. *)
valueExpr  = term { arithop term } ;
arithop    = "+" | "-" | "*" | "/" ;
term       = navExpr { "->" collapse } { "->" fn ( "(" [ fnArgs ] ")" ) }
           | ifExpr | literal | colAccess | "(" valueExpr ")" ;
collapse   = "toOne" "(" ")" ;                         (* [0..1]/[*] -> [1]; the T6 collapse operator *)
fn         = "parseFloat" | "parseInteger" | "toString" | "toLower" | "toUpper"
           | "substring" | "year" | "at" | "cast" | "first" | "concatenate" ;
fnArgs     = valueExpr { "," valueExpr } ;
ifExpr     = "if" "(" boolExpr "," "|" valueExpr "," "|" valueExpr ")" ;  (* zero-arg then/else lambdas *)

navExpr    = refVar { "." ident } ;                    (* body use is $-prefixed; N1 (first ident) + N2 (chained idents) *)
colAccess  = refVar "." tdsGetter "(" strlit ")" ;     (* N6 relation-column access, e.g. $r.getInteger('cnt') *)
tdsGetter  = "getInteger" | "getFloat" | "getString" | "getBoolean" ;
```

**Note on `navExpr` = the whole L2 narrowing spine.** `navExpr = refVar { "." ident }` is intentionally one production covering *all* of: a plain property (`$x.name`), an association navigation (`$x.fk0DefaultContinents`), a qualified/derived property, and a chained navigation (`$x.fk0DefaultContinents.contId`). L1 cannot and must not distinguish them — that is exactly what L2's N1/N2/N5 narrow. The grammar's only job is to fix that a `.` after a `var` (or after a prior `ident`) is followed by an `ident`; L2 decides *which* ident.

### 5.4 Terminals and identifiers (lexis)

```ebnf
classpath  = ident { "::" ident } ;                    (* e.g. spider::car_1::model::default::Countries *)
binderVar  = ident ;                                   (* lambda HEADER only: the bare "x" in  x|...       *)
refVar     = "$" ident ;                               (* expression BODY only: the "$x" in  $x.name       *)
literal    = strlit | number | boollit ;
strlit     = "'" { schar | "''" } "'" ;                (* SINGLE quotes only; embedded quote doubled ''   *)
number     = [ "-" ] digit { digit } [ "." digit { digit } ] ;
boollit    = "true" | "false" ;
int        = digit { digit } ;
ident      = alpha { alnum | "_" } ;                   (* camelCase props, PascalCase classes, snake cols *)
schar      = <any character except a single quote> ;
alpha      = "a".."z" | "A".."Z" ;
alnum      = alpha | digit ;
digit      = "0".."9" ;
```

### 5.5 Verified lexical quirks (corpus-confirmed)

- **Single-quote strings only.** Double quotes never appear; an embedded quote is written `''` (15 gold queries exercise the doubling). A grammar admitting `"..."` is a compile-unsound over-approximation — keep `strlit` single-quote-only.
- **`SortDirection.ASC` / `SortDirection.DESC`** are the only enum-shaped literals in the pilot corpus (368 occurrences), and they occur **only inside `sort`** (via `sortdir`), never as a comparison operand. They are a *Pure builtin*, not a schema enumeration, so they are **not** an L2 N4/N5 position — L1 fixes their `EnumPath "." IDENT` shape as a fixed terminal in `sortdir`, and L2 does not narrow them. **Schema-enum comparison** (`$x.status == SomeEnum.ACTIVE`, an `EnumRef` property vs an enum value) is what L2's N4/T5 target; it does **not** occur in the current Spider-derived corpus, so per §5 the emitted grammar carries **no** enum-literal *operand* production yet. That operand production (`enumLit = classpath "." ident`, feeding `term`) is **reserved**: add it on the first gold query that compares an enum, at which point L2's N4/T5 narrow its RHS. See §7 (the N4/T5 contract rows) and §6.5 N4 / §6.6 T5, which mark the same rules forward-looking.
- **`binderVar` vs `refVar`.** The lambda *header* names the variable bare (`x|`); every *use* in the body is `$`-prefixed (`$x.`). L1 keeps them distinct so a stray bare `x.name` or `$x|` is rejected; L2 binds the header name and resolves `$`-uses against it (§6.4, transition S2).

### 5.6 Deliberate over-approximations (oracle-driven tightening)

The grammar over-approximates validity where a CFG cannot cheaply enforce a constraint the compiler oracle already catches. Do **not** tighten these speculatively; tighten only where §8 differential compile testing finds a real invalid escape:

- **Projected-column-count == name-count.** `project`/`groupBy`/`olapGroupBy` do not enforce that the lambda-list length equals the name-list length. The compiler catches a mismatch.
- **Arithmetic/`if` type coherence.** `valueExpr` allows any `arithop` between any two `term`s; L2's type rules (T1–T2) and the compiler reject numeric/string mixing.
- **Collapse necessity.** L1 allows `navExpr` scalar comparisons without a `->toOne()`; whether a `[0..1]`/`[*]` navigation *must* be collapsed first is L2's T6, not L1's.
- **Predicate arity.** `boolPred` arguments are loosely typed (`predArg`); the exact arg shape per predicate (lambda vs value) is left to L2/compiler.

### 5.7 Observed construct inventory (the empirical spec)

Counts over the ~1,791 verified gold Pure queries at drafting time. Every construct here MUST parse; anything absent here is *not yet* in the grammar (add on first gold occurrence).

| Construct | Count | Grammar production |
|---|---:|---|
| `filter` | 1894 | `filter` |
| `project` | 1015 | `project` |
| `groupBy` | 847 | `groupBy` (empty-key form included) |
| `restrict` | 590 | `restrict` |
| `->count()` | 582 | `reducer` |
| `sort` | 372 | `sort` / `sortdir` / `sortSpec` |
| `take` | 295 | `take` |
| `distinct` | 237 | `distinct` |
| `->max()` / `->min()` | 211 / 64 | `reducer` |
| `->toOne()` | 206 | `collapse` (T6 collapse operator) |
| `isNotEmpty` / `isEmpty` | 187 / 147 | `boolPred` |
| `->average()` / `->sum()` | 142 / 140 | `reducer` |
| `getInteger`/`getFloat`/`getString` | 310 / 25 / 11 | `colAccess` / `tdsGetter` (N6 relation-column access) |
| `parseFloat` / `parseInteger` | 59 / 1 | `fn` |
| `concatenate` | 55 | `fn` |
| `->exists(...)` | 54 | `boolPred` (to-many collapse; T6) |
| `->size()` | 51 | `reducer` |
| `->in(subquery.col)` | 47 | `cmp` subquery-membership form |
| `->contains(...)` | 42 | `boolPred` (String, T4) |
| `map` | 31 | (nav/collection map — treat as `fn`) |
| `->year()` | 20 | `fn` (temporal → numeric) |
| `toLower` / `toString` / `startsWith` | 11 / 10 / 10 | `fn` / `boolPred` |
| `if(...)` | present | `ifExpr` |
| `olapGroupBy` + `rowNumber` | 6 | `olapGroupBy` / `reducer` |
| `asc()` / `desc()` sort helpers | 12 / 4 | `sortSpec` |
| `substring` / `at` / `first` / `cast` | 4 / 8 / 6 / 2 | `fn` |
| `let ... = ...` block form | 214 | `blockQuery` / `letBinding` |
| `&&` / `\|\|` boolean connectives | 1180 / 52 | `boolExpr` |
| `==` `!=` `>` `<` `>=` `<=` | (all present) | `cmpop` |

---

## 6. L2 — schema-consistency (the schema-aware constraint level)

L2 is the semantic overlay that L1 cannot express. Given (a) the emitted-Pure L1 grammar and (b) a `Schema` for the target database, L2 defines the additional per-position constraints that keep a partial query referencing only **real, correctly-typed model elements**. It narrows at exactly the positions L1's §7 consistency-contract table enumerates; it never *widens* what L1 allows.

**Core principle (oracle-driven).** Every rule below is derived from, and testable against, the execution-verified gold corpus **and its schemas**. A rule that masks a token appearing in a gold query for that schema is a soundness bug. Do not invent constraints the corpus does not exercise.

### 6.1 Why L1 cannot do this (the context-sensitivity)

A context-free grammar can enforce that `$x.` is followed by *an identifier*. It cannot enforce that the identifier is one of `{id, maker, fullName, country}` **because that set depends on the class `$x` is bound to**, which depends on the `.all()` source and any intervening association navigation — a context-sensitive fact. L2 threads a small **typed scope** through the parse and, at exactly the identifier and operator positions, intersects L1's terminal set with the schema-legal set for the current scope.

### 6.2 The `Schema` data-contract

The minimal per-database structure a schema-aware decoder consults. It is populated **host-side** (never by the decoder — the decoder never calls Legend) from the PureModelContextData (PMCD) or, equivalently, the MCP reflection tools (§6.3), then handed to the decoder at session init. All names are the **autogen model identifiers** (camelCase properties, PascalCase class simple-names, fully-qualified `spider::db::model::default::Class` paths) exactly as they appear in the ctx brief and gold queries — never the underlying SQL table/column names.

#### 6.2.1 Structure

```
Schema {
  db_id: string
  classes:      Map<ClassPath, ClassInfo>          // keyed by fully-qualified path
  associations: List<AssociationSpec>              // navigability derived, see 6.2.3
  enums:        Map<EnumPath, List<EnumValue>>     // enumeration path -> its literal values
}

ClassInfo {
  path:                 ClassPath                  // e.g. spider::car_1::model::default::CarMakers
  simple_name:          string                     // "CarMakers" (the .all() head the model emits)
  properties:           List<PropertySpec>         // stored/regular properties, declared order
  qualified_properties: List<QualifiedPropertySpec>// derived properties (0..* per class)
  super_types:          List<ClassPath>            // inherited members resolve transitively
}

PropertySpec {
  name:         string                             // "horsepower"
  type:         PropType
  multiplicity: Multiplicity
}

PropType =
  | Primitive(PrimName)     // one of the Pure primitives, 6.2.2
  | ClassRef(ClassPath)     // a complex/class-typed property (navigation continues)
  | EnumRef(EnumPath)       // an enumeration-typed property

Multiplicity { lower: u32, upper: u32 | UNBOUNDED }   // "1"->(1,1) "0..1"->(0,1) "1..*"->(1,UNBOUNDED)

AssociationSpec {
  path: AssociationPath
  ends: [AssociationEnd; 2]                         // exactly two ends (well-formed assoc)
}
AssociationEnd {
  property_name: string                             // "fk0DefaultContinents"
  target_class:  ClassPath                          // Continents
  multiplicity:  Multiplicity                       // [1]
}
// NOTE (Pure semantics, verified): an end's property is navigable FROM the class at the OTHER end
// and yields target_class[multiplicity]. See 6.2.3.

QualifiedPropertySpec {
  name:               string                        // "doubled"
  return_type:        PropType                       // its declared return type
  return_multiplicity: Multiplicity
  // parameter list exists in the PMCD but is not needed for identifier narrowing; a decoder MAY
  // ignore args and treat a qualified property as a nav step yielding return_type (MVP), or narrow
  // its argument positions later. Args are rare in the emitted subset.
}

EnumValue = string                                  // the enum literal, e.g. "ACTIVE"
```

`PropType`'s three-way split (`Primitive` | `ClassRef` | `EnumRef`) is load-bearing: the type determines whether a `.` after this property **continues navigation** (`ClassRef`), **terminates at a value** (`Primitive`), or **narrows a comparison RHS to enum values** (`EnumRef`). A flat `type: str` is insufficient; a decoder MUST split it.

#### 6.2.2 The primitive type set (from the autogen models)

`PrimName ∈ { Integer, Float, Decimal, Number, String, Boolean, Date, StrictDate, DateTime }`. For the **type rules** (§6.5) primitives collapse into type *classes*:

- **numeric** = { Integer, Float, Decimal, Number } — comparable with `< > <= >=` and number literals; aggregatable with `sum`/`avg`.
- **string** = { String } — comparable with `== !=` and single-quoted literals; string predicates.
- **boolean** = { Boolean } — comparable with `== !=` and `true`/`false` only.
- **temporal** = { Date, StrictDate, DateTime } — comparable with `< > <= >=` and date literals.

(The autogen pilot models are numeric/String/Boolean-heavy; temporal appears in other Spider DBs. Enums are rare in the Spider-derived corpus but MUST be supported for general PMCDs.)

**Declared-type caveat (verified).** Some SQL numeric columns are declared `String` in the autogen model (e.g. car_1's `horsepower`/`mpg`, a TEXT-affinity artifact). `PropType` MUST reflect the **model's declared** type, not the SQL intent: a String-typed numeric column is correctly constrained by L2 as **String**. The model, not the SQL, is L2's ground truth.

#### 6.2.3 Association navigability (the subtle rule)

An `AssociationSpec` with ends `[e0, e1]` yields **two directed navigations**:

- from `e0.target_class`, the property **`e1.property_name`** is navigable and yields `e1.target_class` with `e1.multiplicity`;
- from `e1.target_class`, the property **`e0.property_name`** is navigable and yields `e0.target_class` with `e0.multiplicity`.

Concretely, `fk_0 = { fk0DefaultCountries: Countries[1..*], fk0DefaultContinents: Continents[1] }` means: **from a `Countries`** you may navigate `.fk0DefaultContinents` → `Continents[1]`, and **from a `Continents`** you may navigate `.fk0DefaultCountries` → `Countries[1..*]`. This is exactly what the gold query `Countries.all()->filter(x|$x.continent == $x.fk0DefaultContinents.contId)` does. Getting the direction backwards is a soundness bug (it would mask `fk0DefaultContinents` on `Countries`). A decoder therefore precomputes, per class, its **navigable set** = { each opposite-end property }.

#### 6.2.4 Provenance — how the contract is fed

The decoder never calls Legend; the host builds `Schema` once, at session init, from either source (they are the same PMCD, different access paths). The MCP reflection tools live in the upstream project's `mcp_server` (tool names below are stable API):

| Contract field                                     | MCP tool                                                                                | PMCD field                                                         |
| -------------------------------------------------- | --------------------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| `classes[*].properties` (name, type, multiplicity) | `legend_describe_class` → `properties[]` (`name`, `type`, `lower_bound`, `upper_bound`) | class `properties[].genericType.rawType.fullPath` + `multiplicity` |
| `classes[*].super_types`                           | `legend_describe_class` → `super_types[]`                                               | `superTypes[].path`                                                |
| `classes[*].qualified_properties`                  | `legend_get_derivations` → `derivations[]` (`name`, `return_type`)                      | `qualifiedProperties[]` (`returnGenericType`)                      |
| `associations` (ends, targets, multiplicities)     | `legend_get_associations` → `associations[].properties[]` + `other_end_class`           | `Association.properties[]`                                         |
| `enums`                                            | `legend_list_enums` → `enums[]` (`path`, `values`)                                      | `Enumeration.values[]`                                             |

`legend_describe_class` returns `type` as a full path string; the host classifies it into `PropType`: if it is a primitive path → `Primitive`; if it resolves to a `class` element → `ClassRef`; if to an `enumeration` element → `EnumRef`. Milestoning `target_stereotypes` are ignored by L2 (they affect *arguments*, not name/type resolution).

### 6.3 The `Schema` construction is host-side

How the PMCD / MCP tools are queried to *populate* the contract is host-side. This spec defines the contract's *shape and semantics*, not the extraction, and the decoder ingests `Schema` from JSON at session init (`Schema::from_json`, §9).

### 6.4 The scope-tracking state machine

L2 maintains a small **scope stack**. The top-of-stack `Scope` determines narrowing. A `Scope` is one of:

- `ClassScope(class_path, var_name?, multiplicity)` — a row is a single instance of `class_path` (multiplicity tracks whether we are on a to-one or to-many path);
- `RelationScope(columns: List<ColName>)` — the pipeline has become a TDS/relation (after `project`/`groupBy`); rows are named columns, not class instances.

#### 6.4.1 Transitions

1. **Source (S1).** On `ClassPath.all()`, the class must exist in `Schema.classes` (rule N3). Set the pipeline scope to `ClassScope(ClassPath, var=None, mult=(1,1))`.
2. **Lambda entry (S2).** On entering a lambda `var | …` (inside `filter`, and inside each `colLambda`/`keyLambda`/`mapLambda`), bind `var` to the *current pipeline element type*: push `ClassScope(current_class, var, (1,1))`. The bound var is the only in-scope row variable inside the lambda body.
3. **Navigation entry (S3).** On `$var.` where `$var` is the bound var, the next identifier is narrowed (N1). After it is consumed:
   - if it is a `Primitive`/`EnumRef` property → the nav expression's *resolved type* is that primitive/enum; navigation cannot continue (a further `.` is illegal, N-terminal).
   - if it is a `ClassRef` property or an **association navigation** → advance the nav scope's class to the target class and multiply multiplicities (rule S-mult); a further `.` now narrows to the *target* class's members (N2). This is the chained navigation `$x.fk2DefaultCarMakers.fullName`.
   - if it is a **qualified property** → the resolved type is its `return_type`; if that is a `ClassRef`, navigation may continue from the returned class (MVP: treat like a ClassRef step).
4. **Lambda exit.** On the lambda's closing boundary, pop the lambda `ClassScope`; the pipeline scope is unchanged by `filter` (filter does not change the element type).
5. **project / restrict / olapGroupBy.** `project([colLambdas], [names])`, `restrict([names])`, and `olapGroupBy([partCols], sortSpec, reduceLambda, 'outName')` change the pipeline scope to `RelationScope(names)` — the emitted `names` string-literals (for `olapGroupBy`, the partition columns plus `'outName'`) become the column universe. After this point, class-property narrowing no longer applies; `sort('col', …)` column references and the TDS-column accessors `$r.getInteger('col')` / `getFloat` / `getString` / `getBoolean` (rule N6) and further `restrict` names must be members of the current `RelationScope`. (The `getX('col')` accessor is the post-aggregate HAVING-style read — e.g. `->filter(r|$r.getInteger('cnt') >= 2)` — and is a first-class N6 position; its `strlit` arg is the `colAccess` production in §5.3.)
6. **groupBy.** `groupBy([keyLambdas], [aggs], [names])` also yields `RelationScope(names)`, where `names` are the group-key + aggregate output names. Inside each `keyLambda` and each `agg`'s `mapLambda` the scope is still `ClassScope(source_class, var)` (the lambdas run over the pre-group rows) — so their bodies narrow against the source class, exactly as gold `groupBy([x|$x.fk2DefaultCarMakers.fullName, …], [agg(x|$x.modelId, y|$y->count())], […])`.
7. **agg reduce lambda.** Inside `agg(mapLambda, reduceLambda)` the `reduceLambda` var (`$y`) is bound to the *collection of mapped values*; its element type = the `mapLambda`'s resolved type. This is where aggregation type rules (T3) fire: `$y->sum()` is legal only if that element type is numeric.
8. **sort / take / limit / distinct.** Do not change scope type. `sort` references a column name (N6); `take`/`limit` take an int; `distinct` takes nothing.

#### 6.4.2 Worked example (DB: `car_1`)

Gold query (verified):

```pure
|spider::car_1::model::default::Countries.all()
  ->filter(x|$x.continent == $x.fk0DefaultContinents.contId)
  ->groupBy([x|$x.fk0DefaultContinents.contId, x|$x.fk0DefaultContinents.continent],
            [agg(x|$x.countryId, y|$y->count())],
            ['ContId','Continent','count'])
```

Position-by-position scope + narrowing:

| Position | Scope before | L2 action |
|---|---|---|
| `spider::…::Countries` (source) | — | N3: must be a real class path; it is. Set `ClassScope(Countries,(1,1))`. |
| `.all()` | ClassScope(Countries) | pipeline element type = `Countries`. |
| `filter(x\|` | ClassScope(Countries) | S2: bind `x`→`Countries`. |
| `$x.` → `continent` | ClassScope(Countries, x) | N1: `continent` ∈ Countries.properties `{countryId, countryName, continent}` ✓; type `Integer[0..1]` (numeric). |
| `==` | resolved LHS numeric | T1/T6: LHS is `[0..1]` scalar numeric → RHS must be numeric-typed. |
| `$x.fk0DefaultContinents` | ClassScope(Countries, x) | N1+N5: `fk0DefaultContinents` is the navigable end of `fk_0` **from Countries** ✓ → advance to `Continents[1]` (S-mult keeps scalar). |
| `.contId` | (nav) Continents | N2: `contId` ∈ Continents.properties `{contId, continent}` ✓; type `Integer` (numeric) → RHS type-matches LHS. Comparison legal. |
| `groupBy([x\|$x.fk0DefaultContinents.contId, …]` | ClassScope(Countries) per keyLambda | S2 rebinds `x`→Countries; nav narrows as above; both keys resolve. |
| `agg(x\|$x.countryId,` | ClassScope(Countries, x) | mapLambda: `countryId` ∈ Countries ✓ → numeric. |
| `y\|$y->count()` | reduce over numeric collection | T3: `count` legal on any collection ✓. |
| `['ContId','Continent','count']` | → RelationScope | scope becomes `RelationScope({ContId,Continent,count})`; any following `sort`/`restrict` narrows against these names. |

A **counterfactual** the overlay must reject: `Countries.all()->filter(x|$x.maker == 'Ford')` — masked at `maker`, because `maker` is not a Countries property (it is a CarMakers property); and even if a `makerName` existed, `== 'Ford'` on a numeric FK column would be masked by T1. Both are *phantom / type-mismatch* errors L2 exists to eliminate.

### 6.5 Narrowing rules (identifier positions) — N1–N6

Each rule = "at this position, intersect L1's terminal set with this schema-legal set." All sets are computed from `Schema` and the current `Scope`.

- **N1 — property/first-navigation narrowing.** At `$var.<IDENT>` where `$var` is bound to `ClassScope(C)`: legal `<IDENT>` = `C.properties[*].name` ∪ `C.qualified_properties[*].name` ∪ `navigable(C)` (the opposite-end property names of every association touching `C`, per §6.2.3) ∪ the same three sets for every class in `C.super_types` (transitively). Nothing else.
- **N2 — chained-navigation narrowing.** After a ClassRef/association step advanced the scope to target class `T`, a further `.<IDENT>` narrows to `T`'s member set (N1 computed for `T`).
- **N3 — source-class narrowing.** At the `classpath` before `.all()`, the fully-qualified path must be a key of `Schema.classes`. (Phantom-class prevention; catches `test::DoesNotExist.all()`.)
- **N4 — enum-value narrowing.** When a nav expression resolves to `EnumRef(E)` and is compared (`== / !=`), the RHS enum literal `E.value` (or `EnumPath.value` form) is narrowed to `Schema.enums[E]`. Nothing outside that enum's declared values. **Forward-looking / not in the current corpus:** no gold query in the Spider-derived corpus compares a schema enum, so the emitted L1 grammar carries **no** enum-literal *operand* production yet (§5.5, §7 N4 mark this reserved). N4 becomes active the moment L1 adds that operand (`enumLit = classpath "." ident`) on the first gold enum comparison. (`SortDirection.ASC/DESC`, the only enum-shaped literal in the corpus, is a Pure builtin inside `sort` — **not** a schema enum and **not** an N4 position.)
- **N5 — association navigability direction.** A navigation property is legal from `C` only if it is the *opposite* end of an association whose other end targets `C` (§6.2.3). This prevents emitting a navigation from the wrong side of the association.
- **N6 — relation-column narrowing.** In `RelationScope(cols)`, every reference to an emitted column name must be a member of `cols` (the names emitted by the preceding `project`/`groupBy`/`olapGroupBy`). Four reference positions occur in the corpus and are all narrowed: (a) a `sort('<COL>', …)` / `asc('<COL>')` / `desc('<COL>')` column string; (b) any `restrict([...])` or later `project` name-reference; (c) the **TDS-column accessor** `$r.get{Integer,Float,String,Boolean}('<COL>')` — the post-aggregate HAVING read (`->filter(r|$r.getInteger('cnt') >= 2)`), which is the single most common relation-column reference (340+ gold occurrences); and (d) the trailing column `<IDENT>` in the `->in(subquery.<IDENT>)` membership form (47 gold), narrowed against the **subquery pipeline's own terminal `RelationScope`** — the subquery is entered as an independent scope (§6.4), so its projected column universe, not the outer pipeline's, is the legal set. This keeps post-projection column references real. (Weaker than N1–N5: column names are string-literals, so this is enforced only where the model references a *previously emitted* name; it is the relation-side analogue of property narrowing. The `getX` accessor additionally fixes the *type* of the read — `getInteger` on a numeric column — which L2 MAY check against the aggregate's output type, but the compiler oracle also catches a `getString` on a numeric column, so this is an optional tightening.)

### 6.6 Type rules (operator / operand / reducer positions) — T1–T7

- **T1 — comparison operand-type compatibility.** At `navExpr cmpop operand`, the `operand`'s literal type must match the navExpr's resolved type class (§6.2.2): string prop ↔ single-quoted literal; numeric prop ↔ number literal; boolean prop ↔ `true`/`false`; temporal prop ↔ date literal. (Also admits `navExpr cmpop navExpr` when both resolved types share a type class — e.g. the gold `$x.continent == $x.fk0DefaultContinents.contId`, numeric ↔ numeric.)
- **T2 — ordered-comparator restriction.** `< > <= >=` are legal only when the resolved type is **numeric or temporal**; `== !=` additionally legal for string/boolean/enum. (Masks `boolProp > 3`.)
- **T3 — aggregation-reducer type rule.** In `agg(mapLambda, reduceLambda)`: `->sum()` and `->average()` legal only if the mapLambda's resolved element type is **numeric**; `->min()`/`->max()` legal on numeric or temporal (ordered); `->count()` legal on any collection. (The gold corpus uses exactly `count/average/min/max/sum`.)
- **T4 — string-predicate type rule.** `->startsWith(…)`, `->endsWith(…)`, `->contains(…)`, `->toLower()`/`->toUpper()` legal only when the receiver's resolved type is **String**.
- **T5 — enum-comparison type rule.** A nav expression resolving to `EnumRef(E)` may be compared only against a value of enum `E` (pairs with N4); comparing it to a string/number literal is masked. **Forward-looking**, exactly like N4: inert until L1 adds the reserved `enumLit` operand on the first gold enum comparison (§7 N4/T5).
- **T6 — multiplicity / collapse rule.** A scalar comparison (`navExpr cmpop operand`), a scalar string/temporal `fn`, or scalar arithmetic requires the navExpr's resolved multiplicity to be **to-one** (`upper == 1`). A navigation whose resolved multiplicity is `[0..1]` or that crosses a to-many association end (e.g. from `Continents` via `fk0DefaultCountries` → `Countries[1..*]`) yields a *non-scalar*; using it scalar-wise is illegal — it must be **collapsed to `[1]` first**. The corpus-attested collapse operators are, in order of frequency: **`->toOne()`** (206 gold occurrences — the canonical `[0..1] → [1]` collapse, e.g. `$x.note->toOne()->contains('East')` and `$x.balance->toOne() + …`), an **aggregate** (`->sum()`/`->count()`/… inside `agg`), or an **existence predicate** (`->exists(lambda)` / `->isEmpty()` / `->isNotEmpty()`, which consume a to-many collection and return a scalar Boolean). L2 treats a `navExpr` immediately followed by any of these as scalar at the enclosing operator position. A scalar comparison applied to an *un-collapsed* `[0..1]`/`[*]` navExpr is masked. (Optional-to-one `[0..1]` FK navigations DO occur in the pilot corpus and are collapsed with `->toOne()`; strictly-to-one `[1]` ends need no collapse.)
- **T7 — projection/key lambda return-shape.** `colLambda`/`keyLambda` bodies must resolve to a **scalar** (`upper == 1`) primitive/enum value (a TDS column is scalar); a body left at a class or a to-many collection is masked. (Prevents `project([x|$x.fk0DefaultCountries], …)` — projecting a whole to-many navigation instead of one of its columns.)

### 6.7 Rule count

6 scope-transition rules (S1/source, S2/lambda-bind, S3/nav-advance, plus project/groupBy/agg/sort re-typing consolidated) + **6 narrowing rules (N1–N6)** + **7 type rules (T1–T7)** = **13 narrowing/type constraint rules**, over the scope state machine of §6.4. The 13 N/T rules are the maskable, per-position constraints an implementation enforces; the scope machine is the state they read.

---

## 7. The L1↔L2 consistency-contract table

L1 and L2 share a **single position vocabulary**: every place L2 narrows must be a specific, unambiguous grammar position L1 defines, and every L1 identifier/literal position that L2 references must exist in the grammar. The table below is the cross-check spine — L1 productions and L2 narrowing positions MUST stay in lockstep. A drift on either side is a bug.

| L2 rule (§6)                                           | L1 position (§5)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| ------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **N3** source-class narrowing                          | `source = classpath ".all()"` — the `classpath` before `.all()` (§5.2, §5.4)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| **N1** property / first-navigation                     | `navExpr = refVar { "." ident }` — the **first** `ident` after `$var .` (§5.3)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| **N2** chained navigation                              | `navExpr` — each **subsequent** `ident` after a `.` (§5.3)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| **N4** enum-value narrowing                            | **Reserved / forward-looking (not in current corpus).** Targets the RHS `EnumPath "." IDENT` of a schema-enum comparison. The emitted grammar carries **no** enum-literal *operand* production today (per §5 + §5.5); it is added (`enumLit = classpath "." ident`, feeding `term`) on the first gold enum comparison, at which point this row narrows its RHS. (`SortDirection.ASC/DESC` in `sort` is a Pure builtin, **not** an N4 position — §5.5.)                                                                                                                                                             |
| **N5** association navigability direction              | same `ident` position as N1/N2 (L1 does not distinguish assoc from prop)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| **N6** relation-column narrowing                       | the **reference** positions narrowed against a `RelationScope`: the `strlit` of `sort`/`asc`/`desc`, of `restrict`, of a later `project` name-reference, the `strlit` argument of `colAccess` (`$r.getInteger('col')`), **and** the trailing `ident` of the `->in(pipeline "." ident)` subquery-membership form (narrowed against the subquery pipeline's OWN terminal `RelationScope` — L2 enters each pipeline independently), §5.2–§5.3. (The `project`/`groupBy`/`olapGroupBy` name-lists *emit/define* the column universe — they establish the scope, they are not themselves narrowed against a prior one.) |
| **T1/T2** comparison operand type & ordered-comparator | `cmp = valueExpr cmpop valueExpr` — the `cmpop` + operand positions (§5.3)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| **T3** aggregation-reducer type                        | `reduceExpr = refVar "->" reducer "()"` — the `reducer` position (§5.3)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| **T4** string-predicate / string-transform type        | two L1 positions: `valueExpr "->" boolPred` for the predicates `contains`/`startsWith`/`endsWith`, **and** `valueExpr "->" fn` for the transforms `toLower`/`toUpper` (which are `fn`, not `boolPred`, in §5.3) (§5.3)                                                                                                                                                                                                                                                                                                                                                                                             |
| **T5** enum-comparison type                            | **Reserved / forward-looking**, pairs with N4 at the same (not-yet-emitted) enum-comparison RHS                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| **T6** multiplicity / collapse                         | the `collapse` (`->toOne()`), `boolPred` `exists`/`isEmpty`/`isNotEmpty`, and `agg` positions that turn a `[0..1]`/`[*]` `navExpr` into a scalar before a scalar `cmp` (§5.3)                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| **T7** projection/key lambda return-shape              | the `valueExpr` body of `colLambda`/`keyLambda` must be scalar (§5.3)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |

**Two contract points L1 explicitly provides for L2:**

1. **The relation-column access form.** `colAccess = refVar "." tdsGetter "(" strlit ")"` (e.g. `$r.getInteger('cnt')`) is the post-`groupBy`/`olapGroupBy` HAVING-style column read (`getInteger` alone 310, all four `getX` ≈ 340+ gold occurrences — matching §6.5 N6). Its `strlit` is an **N6 position** — it references a name emitted by the preceding `project`/`groupBy`, and L2 narrows it to the current `RelationScope(cols)`. Without this production L1 could not even reach the position, so the two levels would silently disagree.
2. **The `->toOne()` collapse operator.** `collapse` is the primary mechanism by which a `[0..1]` navigation becomes a `[1]` scalar so a scalar `cmp`/`fn`/arithmetic is legal (206 gold occurrences); it is one of the T6 collapse operators (alongside `exists` and the aggregates). L2's T6 references it by name.

---

## 8. Correctness — the oracle-driven test strategy (most important section)

A constrained decoder fails *silently and catastrophically*: a **soundness** bug masks valid tokens (the model can never produce correct queries); a **completeness** bug lets the model down a dead end. Both are mechanically testable here because the project owns a ground-truth oracle and a large verified corpus. This is the crux of the whole component — build it test-first.

### 8.1 Soundness — never mask a valid continuation (the killer test)

The repo has **thousands of execution-verified gold Pure queries** (~1,791+ at drafting time). Where they live and how to obtain the test corpus:

- **`data/phase2/armC_*.jsonl`** — verified armC gold queries (JSONL). The Pure query is in the `pure_text` / `final_query` field of each record.
- **`data/pilot/armC2_results_*.jsonl`** — pilot armC2 results (JSONL); same `pure_text` / `final_query` fields.
- **`data/phase2/navC_train.jsonl`** — navigation-heavy training queries.
- (also present: `data/phase2/armA_*.jsonl`.)

Each record's `pure_text` (equivalently `final_query`) field holds a single execution-verified gold Pure query string — extract those to form the soundness test set. The upstream project accesses these via `uv run` Python tooling; for the decoder's Rust tests, read the JSONL directly and pull the query field.

**The soundness test.** For each gold query: tokenize with the target model's tokenizer, replay through the decoder, and **assert at every step that the actual next token is in `allowed_mask()`**. Any gold token masked = soundness bug. This corpus *is* the L1 test spec, and, with schemas attached, the L2 test spec. For L2, replay against the query's matching `Schema` (built from the DB's ctx brief / MCP reflection) and assert no N/T rule masks a token that actually appears — this catches navigability-direction, inheritance, and multiplicity mistakes mechanically.

### 8.2 Completeness — no dead ends (differential compile test)

Generate under constraint (random accepting walks over the PDA, or model-driven walks), then **compile every result via the real Legend engine**. The engine runs at:

```
http://localhost:6300/api           (self-hosted docker stack, engine 4.113.0)
compile endpoint:  /pure/v1/compilation/lambdaReturnType
```

Target: **100% of constrained generations compile.** Any compile failure = a grammar/overlay gap; tighten the grammar there (oracle-driven, never speculative). This reuses the project's existing engine client and applies the same execution-verification philosophy to the decoder.

### 8.3 Schema-consistency verification (L2)

Constrained generation against schema `S` must never reference a non-`S` identifier or a type-illegal operation. Verify against the compiler's name/type resolution on the `S` model: assert **zero** phantom-identifier / type-mismatch compile errors under L2, using the same `/pure/v1/compilation/lambdaReturnType` oracle.

### 8.4 Differential fuzzing

Random accepting walks over the PDA → all must compile; feed adversarial near-miss prefixes to check masks reject exactly the invalid next-tokens.

### 8.5 Property tests

Using `proptest`: `accept_token` after any token in `allowed_mask()` never panics and never dead-ends before an accepting state is reachable.

### 8.6 Corpus-derivation invariant

Any production/rule a gold query violates is *wrong* and must be relaxed to admit the corpus; any construct the corpus lacks stays out until a gold query adds it. The verified queries, not intuition, bound the grammar and the rules.

### 8.7 CI gate (non-negotiable)

**100% gold-corpus soundness + 100% constrained-generation compile rate on a held-out schema set.** These are mechanical and non-negotiable gates for the component.

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

`#[cfg(feature="python")]` — the *only* Python-facing surface; keep it thin:

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

- The host provides the vocabulary as **raw byte strings per token id**, handling the tokenizer's metaspace / leading-space conventions (byte-BPE vs SentencePiece) *before* handing bytes over. Getting this exactly right is a soundness prerequisite; the decoder treats tokens as opaque byte strings.
- The host builds `Schema` from the PMCD / MCP tools and passes it (as JSON) at session init.
- The host **activates constraint only over the final-query span** of a trajectory (a mode switch), not over tool calls or reasoning text.
- The host owns sampling; PureCard only masks.
- Concrete loop: create a `Session` with the query's schema at the moment the final-query span begins; each step, `&`-mask the logits; sample; `accept_token`; stop when `is_complete()` and EOS is sampled.

---

## 10. Build milestones (M0–M5)

- **M0 — skeleton + oracle harness.** Crate, `Vocab` ingestion, byte-PDA infrastructure, and the §8.1 soundness harness wired to the gold corpus + the §8.2 differential compile test against the live Legend engine. Test-first: the corpus and compiler are the spec.
  - *Done when:* the harness can replay a gold query through a stub decoder and can POST a query to `/pure/v1/compilation/lambdaReturnType` and read the result.

- **M1 — L1 grammar.** Implement §5; pass **100% gold-corpus soundness** and **100% constrained-walk compile rate**. No perf work yet.
  - *Done when:* every gold query replays with zero masked-gold-token failures, and random accepting PDA walks all compile.

- **M2 — performance.** Context-independent per-state mask cache (§4); hit the per-token latency target (≤ a few hundred µs/token); benchmark.
  - *Done when:* mask generation is off the critical path against the model's forward pass, with a benchmark to prove it.

- **M3 — L2 schema overlay.** Scope/type tracker + schema-narrowed terminals (§6); pass schema soundness on the gold corpus with real schemas + **zero phantom/type-mismatch** under L2.
  - *Done when:* §8.1 (L2 mode) and §8.3 both green on a held-out schema set.

- **M4 — PyO3 boundary.** §9 bindings + a reference Python harness driving a real small model to produce compilable Pure end-to-end under constraint.
  - *Done when:* a Python loop generates constrained Pure from a real model and it compiles.

- **M5 — hardening.** Tokenizer self-check (round-trip a sample of gold queries through tokenize→bytes→decoder at startup), incomplete-generation handling, error recovery, fuzzing, final benchmarks.

**Definition of done:** L1 shippable after M2 (guaranteed-syntactic single-shot Pure); L2 shippable after M3 (guaranteed schema-consistent); the CI gate in §8.7 green on a held-out schema set.

---

## 11. Risks and open questions

- **Grammar drift.** The emitted subset co-evolves with the trained model; a query shape the model emits but the grammar rejects is a soundness failure. *Mitigation:* the gold-corpus soundness test (§8.1) runs against the *current* model's outputs and fails loudly on drift; treat the grammar spec as versioned alongside model checkpoints.
- **`map`'s argument grammar (L1-internal gap).** `map` (~31 gold, §5.7) is currently listed as "treat as `fn`", but it takes a lambda argument that `fnArgs` (`valueExpr { "," valueExpr }`) cannot accept — an unresolved L1-internal completeness gap. Whether `map`'s argument production should admit a lambda (vs a value) is left open pending a corpus decision; the production is deliberately not guessed here.
- **L2 context-dependent set size.** If schema narrowing touches too many token positions, the runtime (non-cached) fraction grows and perf degrades. *Mitigation:* narrow only at identifier/type positions; cache per-(state, class-scope) identifier masks (§4.5).
- **Tokenizer exactness.** Any mismatch between the host's byte representation of tokens and the model's actual tokenization breaks soundness invisibly. *Mitigation:* a startup self-check that round-trips a sample of gold queries through tokenize→bytes→decoder (M5).
- **Possible redundancy.** The agentic schema-exploration path (the model calls `legend_describe_class` / `legend_get_associations` *before* writing the query) may already suppress name hallucination enough that L2's marginal value is small; L1 (cheap) is the safe first target, L2 is gated on measured post-training schema-reference error. Build L1 fully; build L2 only when the measurement justifies it — but this doc specs both so the agent can proceed straight to L2 if the trigger is already met.
- **Over-constraint vs faithfulness.** Masking can force a valid-but-wrong token the model would not otherwise pick: if the model was about to emit a phantom name that (after repair) it would have corrected toward the *faithful* name, hard-masking may instead push it to a *different real-but-wrong* name. L2 trades "compiles never" for "compiles always, sometimes wrongly." This is inherent to constrained decoding and out of scope to solve here; flag it so host-side evaluation watches for faithfulness regressions when the constraint is enabled.
- **False confidence (restated from §1.3).** Because L2 output always compiles, downstream readers may over-trust it. Keep measuring execution-equivalence with the constraint ON; a rise in "compiles but wrong" under L2 is the signal to watch.

---

## 12. Roadmap position and build triggers

PureCard is an **inference-time serving optimization**, not an urgent-blocking dependency. It exists to deliver *guaranteed-valid Pure in a single forward pass* at serving time, removing the compile-repair round-trip. Its place in the roadmap:

**Build gate.** PureCard is gated on the conjunction of:

1. a trained model exists (that emits Pure), AND
2. single-shot serving is committed (as opposed to compile-and-repair loops), AND
3. measured schema-reference errors are still material after the cheap L1 version.

**Build order.**

- **Build L1 first.** It is cheap, schema-independent, and delivers the syntactic guarantee with no schema plumbing. Ship after M2.
- **Escalate to L2 only if** name-hallucination (phantom-identifier / type-mismatch) errors *specifically* dominate the residual error after L1 + agentic schema-exploration. Measure before over-building — L2 may be partly redundant with the model's own agentic schema-exploration, so its marginal value must be demonstrated, not assumed. This spec exists so that, *if* the trigger fires, the L2 rules are ready to implement — not as a mandate to build L2 unconditionally.

**One-line placement.** PureCard is the durable Rust serving kernel that turns a trained Pure-emitting model's final-query span from "probably valid" into "valid by construction (L1), and — when the measurement justifies it — schema-consistent by construction (L2)," while never claiming to make it *faithful*.

---

## 13. Test corpus — contents, provenance, location

The oracle-driven test strategy of §8 needs two concrete inputs: a large set of execution-verified gold Pure queries (the **soundness** oracle) and per-database schemas (the **L2** test inputs). Both are already assembled and ship **inside the PureCard workspace** under `corpus/` (committed to the PureCard repo). A fresh Claude on a fresh machine needs nothing but this checkout to run the entire soundness backbone; the corpus is self-contained and engine-free. This section documents exactly what is in `corpus/`, where it came from, and how to extend it.

### 13.1 `corpus/gold_queries.jsonl` — the soundness oracle

**5,034 unique, execution-verified gold Pure query strings** spanning **161 databases**. This is the SOUNDNESS oracle of §8.1: replay every gold query through the L1 decoder and assert at every step that the actual next token is in `allowed_mask()`; any gold token the mask would forbid is a grammar (soundness) bug. It is simultaneously the **empirical basis the L1 grammar (§5) was derived from** — the verified corpus *is* the spec (§5, §8.6), and this file is that corpus in shippable form.

**Soundness testing over this file is FULLY OFFLINE — no Legend engine required.** It needs only the gold query text + the grammar + the model tokenizer's byte representation of tokens (§9). This is the whole point: the core correctness backbone runs in any CI with zero infrastructure.

Provenance: distilled from the upstream **pure-lingua** project's Phase-2 output — `data/phase2/armA_*.jsonl` + `data/phase2/armC_*.jsonl`, keeping only `accepted=true` (execution-verified) records and de-duplicating query strings. The full `data/phase2/` directory is **231 MB** (not GitHub-committable); this distillation is **4.8 MB** and is committed to the PureCard repo.

Line schema (JSONL, one gold query per line):

```json
{ "db_id": "car_1",
  "source_id": "...",
  "arm": "A",                       // "A" = relational / tableToTDS idiom
                                    // "C" = class-navigation idiom
  "constructs": ["join", "group_by", "agg"],
  "pure_text": "|spider::car_1::model::default::Countries.all()->..." }
```

`pure_text` holds the single execution-verified gold Pure lambda string — the exact field the §8.1 replay reads. `arm` records which of the two emitted idioms produced it (see §5.2 / §5.7): **A = relational** (`tableToTDS`-style), **C = class-navigation** (the `.all()->filter(...)` class-anchored pipelines §5 is written around). Arm split: **A = 4,639, C = 395.**

**Construct coverage** (so the reader knows what the grammar is exercised against — these are the SQL-level constructs behind the gold queries, complementing the emitted-Pure inventory of §5.7):

| Construct  | Count | Construct       | Count |
| ---------- | ----: | --------------- | ----: |
| agg        | 2364  | limit           | 692   |
| join       | 2136  | having          | 297   |
| group_by   | 1155  | scalar_subquery | 225   |
| order_by   | 1054  | not_in_subquery | 164   |
| multi_join | 822   | intersect       | 156   |
| distinct   | 712   | except          | 124   |

### 13.2 `corpus/schemas/*.md` — the L2 (schema-consistency) test inputs

**8 database schema context files** — the 5 pilot DBs plus 3 out-of-sample (OOS) DBs:

- Pilot: `concert_singer`, `pets_1`, `battle_death`, `car_1`, `employee_hire_evaluation`
- OOS: `dog_kennels`, `student_transcripts_tracking`, `world_1`

These are the **L2 test inputs** (§6, §8.1 L2-mode, §8.3): the `Schema` data-contract (§6.2) is populated **from these files** (host-side, never by the decoder), then a gold query for that DB is replayed under L2 asserting no N/T rule masks a token that actually appears. This is what mechanically catches navigability-direction (§6.2.3), inheritance, and multiplicity mistakes. The pilot set backs M3 schema-soundness; the 3 OOS DBs are the **held-out schema set** the §8.7 CI gate and M3 done-criterion refer to.

**File format** (from the `concert_singer` example). Each file is Markdown with two load-bearing blocks:

1. An **`## Execution coordinates`** block — `project_id`, `workspace`, `database_path`, the autogen mapping/runtime paths, and the fully-qualified `classes:` and `associations:` lists. Only the class/property/association **structure** feeds L2; the coordinate paths matter to the completeness oracle (§14) when it needs a live model.

2. A **`## Pure model`** block — the autogen Pure grammar text: each `Class …::default::<Name> { prop: <Type>[<mult>]; … }` and each `Association …::fk_N { <endProp>: <TargetClass>[<mult>]; … }`. This is the direct source for the `Schema` contract: classes → `{prop → (type, multiplicity)}`, associations → the two directed navigations of §6.2.3. Example (abbreviated):

```pure
Class spider::concert_singer::model::default::Singer
{
  singerId: Integer[1];
  name: String[0..1];
  country: String[0..1];
  age: Integer[0..1];
  isMale: Boolean[0..1];
}
Association spider::concert_singer::model::fk_1
{
  fk1DefaultSingerInConcert: spider::concert_singer::model::default::SingerInConcert[1..*];
  fk1DefaultSinger:          spider::concert_singer::model::default::Singer[1];
}
```

(Most files also carry a `## Glossary` block mapping question vocabulary → model identifiers; L2 does **not** consume it — it is question-side, not schema-structure.)

**Stale-workspace caveat.** The `workspace:` id in the `## Execution coordinates` block (e.g. `concert-singer-1783544672`) is **ephemeral/throwaway** — fs-SDLC workspaces are disposable (§14.3), and the id will not exist on a fresh stack. Only the class / property / association **STRUCTURE** matters for L2. Never key anything off the workspace id; if the completeness oracle needs a live model, regenerate the workspace (§14).

### 13.3 Where it lives, and regenerating/extending

|              | pure-lingua source repo                                   | PureCard workspace                                         |
| ------------ | --------------------------------------------------------- | ---------------------------------------------------------- |
| Gold queries | `data/phase2/armA_*.jsonl` + `armC_*.jsonl` (231 MB, raw) | `corpus/gold_queries.jsonl` (4.8 MB, distilled, committed) |
| Schemas      | `data/pilot/armC_ctx_<db>.md` (+ OOS ctx briefs)          | `corpus/schemas/<db>.md` (committed)                       |
| Legend stack | `infra/legend-stack/`                                     | `corpus/legend-stack/` (§14)                               |

**The shipped `corpus/` is sufficient for M0–M3** (M1 L1 soundness, M2 perf, M3 L2 overlay) with no upstream access. To **regenerate or extend** the corpus — more schemas, more query shapes, new constructs the grammar does not yet exercise — the reader needs the full **pure-lingua repo + its Legend stack** (the datagen pipeline that produced `data/phase2/` and the ctx briefs). That is out of scope for building PureCard; note it only so a future maintainer knows the upstream provenance path exists. For the decoder itself, the committed corpus is the complete test spec.

---

## 14. Legend engine setup (for the completeness oracle) + CI

The **soundness** half of §8 is offline (§13.1). The **completeness** half (§8.2 — *do constrained generations actually compile?* — and §8.3 — *does L2 output resolve on the real model?*) needs a **live Legend engine**. This section documents that engine, taken verbatim from the real infra files (`infra/legend-stack/docker-compose.yml`, `engine-config.yml`, `sdlc-config.yml`) and the Gate-0 probe findings (`docs/probes/gate0-findings.md`) — not invented. The stack ships to the PureCard workspace under `corpus/legend-stack/`.

### 14.1 The stack

`docker compose` with two pinned, anonymous-auth (no GitLab, no Mongo) services, both `platform: linux/amd64`:

| Service         | Image                                            | Port | Health endpoint           |
| --------------- | ------------------------------------------------ | ---: | ------------------------- |
| `legend-engine` | `finos/legend-engine-server-http-server:4.113.0` | 6300 | `GET /api/server/v1/info` |
| `legend-sdlc`   | `finos/legend-sdlc-server-fs:0.195.0`            | 6100 | `GET /api/info`           |

The engine runs `org.finos.legend.engine.server.Server server /config/engine-config.yml`; the SDLC runs `org.finos.legend.sdlc.server.startup.LegendSDLCServerFS server /config/sdlc-config.yml` (filesystem backend, entities under `/data/sdlc`). Both configs use `AnonymousClient` (`deployment.mode: TEST_IGNORE_FUNCTION_MATCH`; `pac4j.bypassPaths: ["/api/server/v1/info"]`). Total image footprint ≈ **1.7 GB**.

Bring-up (from `corpus/legend-stack/`):

```bash
docker compose -f corpus/legend-stack/docker-compose.yml up -d

# health-wait (compose sets engine start_period 60s, sdlc 30s):
curl -sf http://localhost:6300/api/server/v1/info   # engine ready
curl -sf http://localhost:6100/api/info             # sdlc ready
```

The compose file already declares matching healthchecks (engine: `curl -sf http://localhost:6300/api/server/v1/info`, 60s start / 10s interval / 10 retries; sdlc: `curl -sf http://localhost:6100/api/info`, 30s start). A CI job should poll those two endpoints until 200 before running completeness tests.

### 14.2 The endpoints the completeness oracle uses

Compiling a candidate Pure lambda is a **two-call** sequence on the engine (both from `gate0-findings.md`; the `lambdaReturnType` compile call is the same oracle §8.2 already names):

1. **`POST /pure/v1/grammar/grammarToJson/lambda`** — body is the Pure lambda **text**; returns the lambda as **protocol JSON** (the `grammarToJson` family; per Gate-0, elements carry `package`+`name`, not a `path`).
2. **`POST /pure/v1/compilation/lambdaReturnType`** — body `{ "lambda": <protocol-json-from-step-1>, "model": <PMCD> }`; on success returns the lambda's **return type** (e.g. `TabularDataSet` for a projected pipeline — the Gate-0 end-to-end probe confirmed this), and on failure returns a **compile error**. A returned type == compiles == completeness satisfied for that generation; an error == a grammar/overlay gap to tighten (oracle-driven, never speculative — §8.2).

The `model` is the **PMCD** (PureModelContextData) for the DB — the same model structure the schema files (§13.2) describe, either regenerated into the fs-SDLC workspace or supplied inline. For **L2** verification (§8.3) the model is the specific DB's PMCD, and a phantom-identifier / type-mismatch generation surfaces as a `lambdaReturnType` compile error.

### 14.3 Key quirks that will bite (compilation-relevant subset)

From `gate0-findings.md` + the stack. Keep to what affects *compiling lambdas* (not the full datagen pipeline):

- **`table` is a reserved SQL-grammar word.** In any relational store text it must be quoted: `"table" => '...'`. Relevant if you (re)generate a store/model rather than using a shipped PMCD.
- **fs-SDLC entity access.** `/entityPaths` 500s on empty workspaces — use `/entities` instead; entities are pushed via `POST .../workspaces/{ws}/entities` with `{message, entities:[{path, classifierPath, content}], replace:true}` (compose `package::name` for the `path`). fs-SDLC workspace **DELETE is broken** (jgit ref lingers) — always use **fresh throwaway workspace names** (this is why the schema files' `workspace:` ids are ephemeral, §13.2). Also verify the PMCD roundtrip after push (`GET .../pureModelContextData` count == pushed count): fs-SDLC **silently drops** elements its bundled protocol can't deserialize.
- **DuckDB is a dead end on stock images — H2 is the store.** The stock engine image lacks the DuckDB execution connector and the SDLC drops DuckDB connections in PMCD conversion. Both are closed facts; do not retry. H2 (`LocalH2`) is the proven store. This only matters if you regenerate models with a relational connection; for pure lambda *compilation* against a supplied PMCD it is moot.
- **Images are amd64.** On Apple Silicon they run under Rosetta/QEMU emulation (works, slower). The intended **Ubuntu host is native x86**, so no emulation there — the stack runs natively on the target machine.

### 14.4 CI guidance (the reader must decide — here is the reasoning)

Two test classes with very different infrastructure cost:

| Test class                                                                                                      | What it needs                                                                                    | CI stance                                                                                                                                                                                                                                           |
| --------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Soundness** — replay 5,034 gold queries through L1 (§8.1); L2 replay against `corpus/schemas/` (§8.1 L2-mode) | **Nothing** — just the committed `corpus/` + the model tokenizer bytes. Fully offline, hermetic. | **Run in EVERY CI run.** Zero infra. This is the core correctness backbone.                                                                                                                                                                         |
| **Completeness** — constrained generations must compile (§8.2); L2 resolves on the real model (§8.3)            | **A live Legend engine** — two amd64 images, ≈ 1.7 GB, docker-compose up + health-wait.          | **Separate engine-backed job.** Either (a) spin the compose up in a dedicated CI job on an **x86 runner** (feasible; document the health-wait of §14.1), or (b) gate it as **opt-in / nightly / local-only** to keep the main CI fast and hermetic. |

**Recommendation:** run **offline soundness in every CI run**; run **completeness as a separate engine-backed job — nightly or on-demand** — on an x86 runner. State plainly to the reader: **the core correctness backbone (soundness replay of all 5,034 gold queries) needs NO engine**, so PureCard is CI-testable out of the box with only the committed corpus; the Legend engine is required **only** for the completeness half, and that half can be deferred to a nightly/on-demand job without weakening the always-on soundness gate. (The §8.7 CI gate remains the target — 100% gold soundness always, 100% constrained-generation compile rate on the completeness job.)

---

## Appendix B — Prior art / references (for the implementer)

- **PICARD** (Scholak, Schucher, Bahdanau, *"Parsing Incrementally for Constrained Auto-Regressive Decoding from Language Models,"* EMNLP 2021) — the original SQL constrained decoder; incremental parsing + lexical/grammatical/schema-consistency tiers. PureCard is its Pure analogue.
- **xgrammar** — Rust-cored grammar-constrained decoding; the context-independent/context-dependent token-mask partition and per-state caching (§4) follow its approach.
- **llama.cpp GBNF** and **Outlines** — grammar/regex-constrained decoding designs; useful references for byte-level automaton masking (§4.4).
- **Legend / Pure** — the FINOS Legend platform; the compile oracle is engine 4.113.0 at `http://localhost:6300/api`, endpoint `/pure/v1/compilation/lambdaReturnType` (§8.2).
