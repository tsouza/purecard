//! The shared token-replay soundness oracle for the BPE lanes.
//!
//! The shipped core has no model tokenizer (a host supplies one). The BPE lanes
//! feed a whole token-id stream through a [`DecoderSession`] and assert the
//! killer property directly on the *token* mask the host consumes: before each
//! [`accept_token`](DecoderSession::accept_token) the gold id must be admissible
//! ([`allowed_mask`](DecoderSession::allowed_mask)`.test(id)`), and at
//! end-of-stream the query is complete with the reserved EOS bit set.
//!
//! Both the synthetic-split Tier-1 lane and (once gated) the real-Qwen Tier-2
//! lane drive this one helper (constitution §4, DRY): the property is identical,
//! only the id stream differs.

use std::collections::HashMap;

use purecard::DecoderSession;

/// GPT-2's `bytes_to_unicode` keeps three byte ranges as their own code point and
/// remaps the rest; these are the exact range bounds from the reference
/// implementation. The Latin-1 span is split because `0xAD` (soft hyphen) is the one
/// byte in `0xA1..=0xFF` the reference drops from the identity set.
const LATIN1_PRINTABLE_LO: u8 = 0xA1;
const LATIN1_PRINTABLE_HI: u8 = 0xAC;
const LATIN1_SYMBOLS_LO: u8 = 0xAE;
const LATIN1_SYMBOLS_HI: u8 = 0xFF;
/// Bytes not kept as themselves are assigned code points starting just past the
/// 256-value byte range, so a remapped byte never collides with a kept one.
const REMAP_BASE: u32 = 256;
const BYTE_MAX: u16 = 255;

/// The inverse of GPT-2's `bytes_to_unicode`: map each byte-level-BPE token-string
/// char back to the raw byte the model actually emits (this also undoes the
/// `Ġ`→space and other whitespace conventions, since they live inside the byte
/// table). Every byte-level-BPE token string is composed only of chars in this
/// table. Qwen2.5-Coder and GPT-4's cl100k_base share the *same* byte-unicode
/// table (it is GPT-2's), so one decoder serves every byte-level tokenizer lane —
/// the real-Qwen oracle and the hermetic fused-precision replay both fold token
/// strings back to bytes through this one function (constitution §4, DRY).
#[allow(dead_code)]
pub fn gpt2_byte_decoder() -> HashMap<char, u8> {
    let mut bs: Vec<u8> = (b'!'..=b'~')
        .chain(LATIN1_PRINTABLE_LO..=LATIN1_PRINTABLE_HI)
        .chain(LATIN1_SYMBOLS_LO..=LATIN1_SYMBOLS_HI)
        .collect();
    let mut cs: Vec<u32> = bs.iter().map(|&b| u32::from(b)).collect();
    let mut n = 0u32;
    for b in 0u16..=BYTE_MAX {
        let byte = b as u8;
        if !bs.contains(&byte) {
            bs.push(byte);
            cs.push(REMAP_BASE + n);
            n += 1;
        }
    }
    bs.into_iter()
        .zip(cs)
        .map(|(b, c)| (char::from_u32(c).expect("valid scalar"), b))
        .collect()
}

/// The true emitted bytes of one byte-level-BPE token string, decoded through
/// [`gpt2_byte_decoder`]. A special token (`<|im_end|>`, FIM) is stored as a
/// literal ASCII string whose chars map to themselves, so its "bytes" are the
/// literal `<|...|>` — never valid Pure, so the byte-PDA rejects it and it is
/// inadmissible mid-query (M2), exactly as required.
#[allow(dead_code)]
pub fn true_bytes(tok: &str, dec: &HashMap<char, u8>) -> Vec<u8> {
    tok.chars()
        .map(|c| {
            *dec.get(&c).unwrap_or_else(|| {
                panic!("token char {c:?} is outside the byte-level table; cannot recover its true bytes")
            })
        })
        .collect()
}

/// Replay `ids` through `session`, asserting the gold token is admissible at
/// every step and that the stream completes with `eos` set.
///
/// Returns `Err` with the step and id at the first violation, so a lane can
/// attribute a masked or rejected chunk to its exact query. `eos` is the
/// reserved EOS bit (`grammar.vocab().len()`), passed in rather than re-derived
/// here so the caller owns the one `V + 1` convention.
#[allow(dead_code)]
pub fn replay_tokens(
    session: &mut DecoderSession<'_>,
    ids: &[u32],
    eos: u32,
) -> Result<(), String> {
    for (step, &id) in ids.iter().enumerate() {
        if !session.allowed_mask().test(id) {
            return Err(format!("masked gold token id={id} at step {step}"));
        }
        session
            .accept_token(id)
            .map_err(|err| format!("rejected gold token id={id} at step {step}: {err}"))?;
    }
    if !session.is_complete() {
        return Err("stream ended in a non-accepting state".to_owned());
    }
    if !session.allowed_mask().test(eos) {
        return Err("EOS bit cleared at completion".to_owned());
    }
    Ok(())
}
