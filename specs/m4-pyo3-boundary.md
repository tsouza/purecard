# Spec: M4 — PyO3 boundary + maturin wheel

- **Status:** Draft (ready for `/spec plan`)
- **Created:** 2026-07-11
- **Owner:** Thiago (`tsouza`)

## Problem

PureCard is the Rust half of a Rust/Python split. The decoder core (M0–M3) is a pure library. Python must drive the decoder inside its per-token sampling loop, and there is no Python-facing surface yet. M4 exposes it via a **thin** PyO3 binding — marshaling only, no decode logic — packaged as a maturin `abi3` wheel. The FFI crux: `DecoderSession<'g>` borrows a `CompiledGrammar` (`src/session.rs:39`, the *only* borrowed field), and a Python object cannot hold a Rust lifetime. The pure core, default build, and `check-core-deplight` must stay untouched behind a non-default `python` feature.

## Decisions (adjudicated)

- **Ownership:** kill the lifetime with a `Borrow` type param on the core session (`DecoderSession<G = &'static CompiledGrammar>`), so the pyclass owns an `Arc<CompiledGrammar>` and a fully-`'static` session. **No `self_cell`/`ouroboros`, no `unsafe` in our tree, one fewer vetted dep.** `std` gives `Borrow<T> for &T` and `Arc<T>: Borrow<T>`, and the default type param keeps every M0–M3 `new(&g)` caller source-compatible.
- **Mask marshaling:** packed little-endian `PyBytes` via a new pure `BitMask::pack_le_bytes_into(&mut Vec<u8>)` (reused buffer, no per-step alloc). **Reject the numpy crate** — ~19 KB is negligible, numpy is heavy/pyo3-coupled and thickens the binding.
- **forbid(unsafe_code):** kept crate-wide, proven by a `cargo check --features python` gate; sibling-crate `#[allow]` is a pre-designed fallback only.
- **Errors:** `create_exception!(purecard, PureCardError, PyValueError)` + `impl From<DecodeError/SchemaError> for PyErr` carrying `Display`.

## Ground-truth corrections baked in

The three input designs assumed some APIs that the real M3 core doesn't have — I verified and corrected:

- `Vocab::from_byte_tokens` and `CompiledGrammar::{compile,from_spec}` are **infallible** → `compile_grammar` returns `Grammar` directly, no `map_err`/`VocabError`. (`from_spec` currently *ignores* `spec` and compiles the fixed byte-PDA.)
- `DecodeError` is currently a single `DeadState` variant — mapping the whole enum via `Display` future-proofs §9.2's added variants.
- `BitMask{words:Vec<u64>,len}` has no byte view; a `&[u8]` reinterpret needs `unsafe`, so I specified copy-into-reused-`Vec<u8>` instead of the `pack_le_bytes(&self)->&[u8]` two designs proposed.
- deplight (`core_dependency_entries` in `xtask/src/tasks.rs`) line-scans `[dependencies]` and handles both inline and `[dependencies.<name>]` sub-table spellings plus `package=` renames — the optional-skip fix must cover **both** spellings and must not be spoofable by a commented `# optional = true` (reuse `strip_toml_comment`).

The full spec file also contains: **Goals** (checkboxed, mapped to the done-criterion — thin binding works + wheel builds + hermetic pytest green; model+engine e2e as an explicitly deferred nightly criterion), **Non-goals** (M5 hardening, the training/inference stack, HELD PyPI auto-publish), concrete **Rust `ffi.rs` + Python sketches**, **API/contract impact** table, the **maturin wheel lane** (`PyO3/maturin-action@v1.51.0`, abi3-py39, tag-gated held `upload`), **Cargo/pyproject/mise** snippets, the honest **testing plan** (Rust unit under coverage/mutants → `check-ffi` → hermetic synthetic-vocab pytest → gold-corpus replay through the binding → deferred model+engine e2e), **dependency vetting** verdicts (pyo3 adopt/optional; maturin adopt as build-tool-not-Cargo-dep; numpy + self_cell rejected), **risks & rollout**, an **11-step ordered implementation plan** (green after each), and **5 decisions for the human** with recommendations.

The one item genuinely needing your sign-off: **decision #2** — the ownership choice edits the pure core's `DecoderSession` signature (adds a generic param). I recommend it over `self_cell`, but it touches the core, so it's flagged as a human decision, with `self_cell` as the fallback.

Note: all version pins (pyo3 0.29.0, maturin 1.14.1, maturin-action v1.51.0) carry an explicit "re-verify via `cargo add`/releases at implementation time" per constitution §2, since the finding-time lookups may have gone stale.
