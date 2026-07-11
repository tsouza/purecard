//! `allowed_mask` latency benchmark (`docs/spec/architecture.md` §4, G3).
//!
//! The mask is consumed on the model's critical path — one call per generated
//! token over a ~150k vocabulary — so it must never bottleneck the forward pass:
//! the target is **≤ a few hundred µs/token** (Decision D4). This measures
//! `allowed_mask` alone, with the `CompiledGrammar` and `Vocab` built once outside
//! the timing loop and the per-state cache pre-warmed, at three representative
//! configurations:
//!
//! - **shallow** — an empty stack right after the source (dense admissible set);
//! - **deep-stack** — nested open frames, maximizing context-dependent flips;
//! - **identifier-position** — just after a `->`, an identifier-dense position.
#![forbid(unsafe_code)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use purecard::{ByteRecognizer, CompiledGrammar, DecoderSession};

#[path = "../tests/support/synth.rs"]
mod synth;
use synth::synthetic_vocab;

/// A production-scale synthetic vocabulary — the ~150k tokens the budget math
/// (§4) is stated against.
const VOCAB_SIZE: usize = 150_000;

/// Byte prefixes that drive the session to each measured configuration.
const SHALLOW_PREFIX: &[u8] = b"|X.all()";
const DEEP_PREFIX: &[u8] = b"|X.all()->groupBy([agg(x|$x.v,";
const IDENT_PREFIX: &[u8] = b"|X.all()->";

fn drive<'g>(grammar: &'g CompiledGrammar, prefix: &[u8]) -> DecoderSession<'g> {
    let mut session = DecoderSession::new(grammar);
    for &byte in prefix {
        session
            .accept_byte(byte)
            .expect("benchmark prefix must be live");
    }
    // Pre-warm the lazy per-state cache so the loop measures the steady-state
    // per-step cost, not the one-time first-visit build (Risk R2).
    let _ = session.allowed_mask();
    session
}

fn bench_allowed_mask(c: &mut Criterion) {
    let grammar = CompiledGrammar::compile(synthetic_vocab(VOCAB_SIZE));

    let mut group = c.benchmark_group("allowed_mask");
    for (name, prefix) in [
        ("shallow", SHALLOW_PREFIX),
        ("deep_stack", DEEP_PREFIX),
        ("identifier_position", IDENT_PREFIX),
    ] {
        let mut session = drive(&grammar, prefix);
        group.bench_function(name, |b| {
            b.iter(|| {
                let mask = black_box(&mut session).allowed_mask();
                black_box(mask.len())
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_allowed_mask);
criterion_main!(benches);
