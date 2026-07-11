//! L2 precision counterfactuals (`docs/spec/schema.md` §8.3, spec M3 G2/G3/G5).
//!
//! Soundness proves L2 never masks a real token; precision proves it *does* mask
//! the phantom and type-mismatched tokens L2 exists to eliminate. Each case is
//! derived mechanically from a gold query by swapping **one** token — a real
//! property for a phantom, a real class for a non-existent one, a matching literal
//! for a type-mismatched one, a real column for an unemitted name — and asserts
//! that at the decision point the real token is still admissible while the swapped
//! token is cleared. The out-of-sample (OOS) cases run on the three held-out
//! schemas, so precision is shown to generalize to schemas no rule was authored
//! against (G5).
#![forbid(unsafe_code)]

#[path = "support/l2.rs"]
mod l2;
#[path = "support/lex.rs"]
mod lex;

use l2::{TokenVocab, lex, load_schema};
use purecard::{CompiledGrammar, DecoderSession};

/// Drive `prefix` (a valid partial query) through a schema-aware session for
/// `db_id`, then report, for each token in `probes`, whether it is admissible at
/// the resulting position. `probes` tokens are injected into the vocabulary so
/// they have ids even when absent from `prefix`.
fn admissible_after(db_id: &str, prefix: &str, probes: &[&[u8]]) -> Vec<bool> {
    let extras: Vec<Vec<u8>> = probes.iter().map(|p| p.to_vec()).collect();
    let vocab = TokenVocab::build(&[prefix], &extras);
    let grammar = CompiledGrammar::compile(vocab.vocab());
    let schema = load_schema(db_id);
    let mut session = DecoderSession::with_schema(&grammar, schema);
    for token in lex(prefix) {
        let id = vocab
            .id_of(&token)
            .unwrap_or_else(|| panic!("prefix token not in vocab: {:?}", bytes_str(&token)));
        session
            .accept_token(id)
            .unwrap_or_else(|err| panic!("prefix token rejected: {err}"));
    }
    let mask = session.allowed_mask();
    probes
        .iter()
        .map(|p| {
            let id = vocab.id_of(p).expect("probe token in vocab");
            mask.test(id)
        })
        .collect()
}

fn bytes_str(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Assert the real token stays admissible and the phantom is masked at a position.
fn assert_precision(db_id: &str, prefix: &str, real: &[u8], phantom: &[u8]) {
    let verdicts = admissible_after(db_id, prefix, &[real, phantom]);
    assert!(
        verdicts[0],
        "precision regression: real token {:?} was masked after prefix in {db_id}:\n  {prefix}",
        bytes_str(real)
    );
    assert!(
        !verdicts[1],
        "precision GAP: phantom token {:?} survived after prefix in {db_id}:\n  {prefix}",
        bytes_str(phantom)
    );
}

#[test]
fn n3_masks_a_phantom_source_class() {
    // A real class heads `.all()`; a non-existent path in the same namespace does
    // not — N3 clears it at the source position.
    assert_precision(
        "car_1",
        "|",
        b"spider::car_1::model::default::CarsData",
        b"spider::car_1::model::default::DoesNotExist",
    );
    // The store path is a legal source (arm-A), a phantom store is not.
    assert_precision("car_1", "|", b"spider::car_1::Db", b"spider::car_1::Nope");
}

#[test]
fn n1_masks_a_phantom_property_after_a_bound_var() {
    // `$x` is bound to CarsData; `cylinders` is a real property, `sallary` is not.
    let prefix = "|spider::car_1::model::default::CarsData.all()->filter(x|$x.";
    assert_precision("car_1", prefix, b"cylinders", b"sallary");
    // A sibling class's property is equally phantom on CarsData (`maker` is a
    // CarMakers/ModelList property, not a CarsData one).
    assert_precision("car_1", prefix, b"horsepower", b"maker");
}

#[test]
fn n2_masks_a_phantom_after_an_association_step() {
    // `$x.fk2DefaultCarMakers` advances ModelList → CarMakers; `fullName` is a
    // real CarMakers property, `cylinders` (a CarsData property) is not.
    let prefix =
        "|spider::car_1::model::default::ModelList.all()->filter(x|$x.fk2DefaultCarMakers.";
    assert_precision("car_1", prefix, b"fullName", b"cylinders");
}

#[test]
fn t1_masks_a_type_mismatched_comparison_operand() {
    // `cylinders` is Integer (numeric): a bare number is admissible, a
    // single-quoted string literal is masked.
    let numeric = "|spider::car_1::model::default::CarsData.all()->filter(x|$x.cylinders == ";
    assert_precision("car_1", numeric, b"4", b"'four'");
    // The `horsepower:String` lever (§6.2.2 declared-type caveat): a string
    // literal is admissible, a number literal is masked — the SQL-numeric column
    // is correctly constrained as String by the model.
    let string = "|spider::car_1::model::default::CarsData.all()->filter(x|$x.horsepower == ";
    assert_precision("car_1", string, b"'150'", b"150");
}

#[test]
fn n6_masks_an_unemitted_relation_column() {
    // After `project(...,['Name','Result'])` the relation columns are exactly
    // those names; a getInteger of an emitted name is admissible, of an unemitted
    // one is masked.
    let prefix = "|spider::battle_death::model::default::Battle.all()\
        ->project([x|$x.name, x|$x.result], ['Name', 'Result'])\
        ->filter(r|$r.getInteger(";
    assert_precision("battle_death", prefix, b"'Name'", b"'Ghost'");
}

#[test]
fn precision_generalizes_to_oos_held_out_schemas() {
    // world_1: Country is a real class; a phantom is masked at the source, and a
    // phantom property is masked after a bound var — on a schema no rule was
    // authored against (G5).
    assert_precision(
        "world_1",
        "|",
        b"spider::world_1::model::default::Country",
        b"spider::world_1::model::default::Nation",
    );
    let prefix = "|spider::world_1::model::default::Country.all()->filter(x|$x.";
    assert_precision("world_1", prefix, b"name", b"gdp");

    // dog_kennels: a phantom property after a bound var is masked.
    let dk = "|spider::dog_kennels::model::default::Professionals.all()->filter(x|$x.";
    assert_precision("dog_kennels", dk, b"lastName", b"salary");

    // student_transcripts_tracking: same, on the third held-out schema.
    let st = "|spider::student_transcripts_tracking::model::default::Transcripts.all()\
        ->filter(x|$x.";
    assert_precision(
        "student_transcripts_tracking",
        st,
        b"transcriptDate",
        b"nonexistent",
    );
}
