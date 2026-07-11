//! The L2 narrowing rules (`docs/spec/schema.md` §6.5–§6.6): given an
//! [`L2Position`] and the [`Schema`], build the **schema-legal** [`BitMask`] over
//! the model vocabulary that the per-step mask is intersected with.
//!
//! The mask is built so L2 **only clears bits, never sets** them: every token the
//! rule does not specifically constrain is *kept* (its bit set), and the reserved
//! EOS bit is always kept so a complete query stays completable (§4.3 guard).
//! Intersecting such a mask can only remove admissible tokens — the structural
//! `L2 ⊆ L1` guarantee (§6, G4) is a property of the operation, not merely a test.
//!
//! Only the shipped rules build a constraining mask (N3, N1/N2, N6, T1). Every
//! other position returns [`None`] — the mask passes through unchanged.

use std::collections::HashSet;

use crate::mask::BitMask;
use crate::schema::model::{Schema, TypeClass};
use crate::schema::scope::{L2Position, Lexeme, classify};
use crate::vocab::Vocab;

/// The `let` binder keyword, legal at a block-statement source position alongside
/// a real pipeline source (§5.4). N3 admits it so a block query's `let` is not
/// mistaken for a phantom class.
const LET_KEYWORD: &str = "let";

/// Refill the caller's reused `dst` buffer with the schema-legal set for `pos`,
/// returning `true` when a constraint applied (the caller intersects `dst` into
/// the L1 mask) or `false` when the position carries no L2 constraint (the L1
/// mask passes through untouched).
///
/// `dst` is the session's own buffer, sized to the grammar's
/// [`mask_len`](crate::grammar::compiled::CompiledGrammar::mask_len) — so
/// narrowing allocates no per-step mask (§4.3). Both the mask length and
/// `eos_bit` flow in from `compiled.rs`, the single source of the `V + 1` / EOS
/// convention, rather than being re-derived from `vocab.len()` here (DRY: the
/// session buffer and this legal set can never disagree on length). `columns` is
/// the N6 legal column set (the tracker's emitted-string superset); it is ignored
/// for other rules.
pub(crate) fn narrow_into(
    dst: &mut BitMask,
    schema: &Schema,
    pos: &L2Position,
    columns: &[String],
    vocab: &Vocab,
    eos_bit: u32,
) -> bool {
    match pos {
        L2Position::None => false,
        L2Position::ReValue(TypeClass::Boolean | TypeClass::Temporal) => {
            // Boolean/temporal operand narrowing is deferred (§6.6 T1 ships only
            // the string/numeric levers) — keep the L1 mask unchanged.
            false
        }
        L2Position::SourceIdent => {
            fill(dst, vocab, eos_bit, |lex| match lex {
                // A source position also legally holds the `let` binder keyword (a
                // block-statement start) — it is grammar, not a phantom class.
                Lexeme::Ident(text) => schema.is_source(text) || text == LET_KEYWORD,
                _ => true,
            });
            true
        }
        L2Position::Member(class) => {
            let member_names = schema.member_names(class);
            let members: HashSet<&str> = member_names.iter().map(String::as_str).collect();
            fill(dst, vocab, eos_bit, |lex| match lex {
                Lexeme::Ident(text) => members.contains(text.as_str()),
                _ => true,
            });
            true
        }
        L2Position::ReValue(tc) => {
            let masked_by = *tc;
            fill(dst, vocab, eos_bit, |lex| keeps_operand(lex, masked_by));
            true
        }
        L2Position::Column => {
            let cols: HashSet<&str> = columns.iter().map(String::as_str).collect();
            fill(dst, vocab, eos_bit, |lex| match lex {
                Lexeme::Str(content) => cols.contains(content.as_str()),
                _ => true,
            });
            true
        }
    }
}

/// Whether an operand token is kept under a T1 constraint with LHS class `lhs`
/// (§6.6 T1). A literal of a *disjoint* class is cleared; everything else — a
/// matching literal, an identifier, a `$var` navExpr, a delimiter — is kept, so
/// only a genuine type mismatch is masked.
fn keeps_operand(lex: &Lexeme, lhs: TypeClass) -> bool {
    match lex {
        Lexeme::Str(_) => matches!(lhs, TypeClass::Str),
        Lexeme::Number => matches!(lhs, TypeClass::Numeric),
        Lexeme::Date => matches!(lhs, TypeClass::Temporal),
        _ => true,
    }
}

/// Refill `dst` in place — no allocation — keeping every `vocab` token for which
/// `keep` holds (given its classified [`Lexeme`]), plus the reserved `eos_bit`.
fn fill(dst: &mut BitMask, vocab: &Vocab, eos_bit: u32, keep: impl Fn(&Lexeme) -> bool) {
    dst.clear_all();
    for id in 0..vocab.len() as u32 {
        let bytes = vocab.bytes(id).unwrap_or(&[]);
        if keep(&classify(bytes)) {
            dst.set(id);
        }
    }
    // The EOS bit is always kept: L2 must never make a complete query
    // uncompletable (§4.3). The L1 mask decides whether it is actually set.
    dst.set(eos_bit);
}

#[cfg(test)]
mod tests {
    use super::narrow_into;
    use crate::grammar::compiled::CompiledGrammar;
    use crate::mask::BitMask;
    use crate::schema::model::{Schema, TypeClass};
    use crate::schema::scope::L2Position;
    use crate::vocab::Vocab;

    const SAMPLE: &str = r#"{
      "db_id": "d", "db_path": "spider::d::Db",
      "classes": { "A": { "simple_name": "A",
        "properties": [{"name": "n", "type": {"kind": "primitive", "name": "Integer"}, "mult": {"lower": 1, "upper": 1}}] } },
      "associations": [], "enums": {}
    }"#;

    fn schema() -> Schema {
        Schema::from_json(SAMPLE).expect("parses")
    }

    /// A vocabulary whose tokens span every lexeme class the rules distinguish.
    fn vocab(tokens: &[&[u8]]) -> Vocab {
        let owned: Vec<Vec<u8>> = tokens.iter().map(|t| t.to_vec()).collect();
        Vocab::from_byte_tokens(owned, tokens.len() as u32)
    }

    fn bit(mask: &BitMask, id: u32) -> bool {
        mask.test(id)
    }

    /// Narrow a fresh buffer for `pos` over `v`, routing the mask length and EOS
    /// bit through the grammar's single source (`compiled.rs`) exactly as the
    /// session does. Returns whether a constraint applied and the filled buffer.
    fn run(pos: &L2Position, cols: &[String], v: Vocab) -> (bool, BitMask) {
        let grammar = CompiledGrammar::compile(v);
        let mut mask = BitMask::with_len(grammar.mask_len());
        let applied = narrow_into(
            &mut mask,
            &schema(),
            pos,
            cols,
            grammar.vocab(),
            grammar.eos_bit(),
        );
        (applied, mask)
    }

    #[test]
    fn none_position_yields_no_mask() {
        assert!(!run(&L2Position::None, &[], vocab(&[b"x"])).0);
    }

    #[test]
    fn deferred_operand_classes_pass_through() {
        // Boolean/temporal operand narrowing is deferred → no constraint (pass-through).
        assert!(
            !run(
                &L2Position::ReValue(TypeClass::Boolean),
                &[],
                vocab(&[b"x"])
            )
            .0
        );
        assert!(
            !run(
                &L2Position::ReValue(TypeClass::Temporal),
                &[],
                vocab(&[b"x"])
            )
            .0
        );
    }

    #[test]
    fn source_ident_keeps_classes_the_store_and_let_masks_phantoms() {
        // ids: 0 real class, 1 store, 2 `let`, 3 phantom, 4 a non-identifier `(`.
        let v = vocab(&[b"A", b"spider::d::Db", b"let", b"Nope", b"("]);
        let eos = v.len() as u32;
        let (applied, mask) = run(&L2Position::SourceIdent, &[], v);
        assert!(applied);
        assert!(bit(&mask, 0) && bit(&mask, 1) && bit(&mask, 2));
        assert!(!bit(&mask, 3), "a phantom class is masked");
        assert!(bit(&mask, 4), "a non-identifier token is never touched");
        assert!(bit(&mask, eos), "EOS is always kept");
    }

    #[test]
    fn member_masks_a_non_member_ident_but_keeps_structure() {
        let v = vocab(&[b"n", b"phantom", b"."]);
        let (applied, mask) = run(&L2Position::Member("A".to_owned()), &[], v);
        assert!(applied);
        assert!(bit(&mask, 0), "a real member survives");
        assert!(!bit(&mask, 1), "a phantom member is masked");
        assert!(bit(&mask, 2), "a non-identifier token is kept");
    }

    #[test]
    fn revalue_masks_the_disjoint_literal_class_only() {
        // ids: 0 string lit, 1 number lit, 2 date lit, 3 an ident (navExpr operand).
        let (applied_n, numeric) = run(
            &L2Position::ReValue(TypeClass::Numeric),
            &[],
            vocab(&[b"'x'", b"5", b"%2018-01-01", b"foo"]),
        );
        assert!(applied_n);
        assert!(
            !bit(&numeric, 0),
            "a string literal is masked for a numeric LHS"
        );
        assert!(bit(&numeric, 1), "a number literal matches");
        assert!(
            !bit(&numeric, 2),
            "a date literal is masked for a numeric LHS"
        );
        assert!(
            bit(&numeric, 3),
            "a navExpr operand is never masked by type"
        );
        let (applied_s, string) = run(
            &L2Position::ReValue(TypeClass::Str),
            &[],
            vocab(&[b"'x'", b"5", b"%2018-01-01", b"foo"]),
        );
        assert!(applied_s);
        assert!(bit(&string, 0), "a string literal matches");
        assert!(
            !bit(&string, 1),
            "a number literal is masked for a string LHS"
        );
    }

    #[test]
    fn column_keeps_emitted_names_and_masks_the_rest() {
        let v = vocab(&[b"'cnt'", b"'ghost'", b"getInteger"]);
        let cols = ["cnt".to_owned()];
        let (applied, mask) = run(&L2Position::Column, &cols, v);
        assert!(applied);
        assert!(bit(&mask, 0), "an emitted column survives");
        assert!(!bit(&mask, 1), "an unemitted column string is masked");
        assert!(bit(&mask, 2), "a non-string token is kept");
    }
}
