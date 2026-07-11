//! The byte-level pushdown automaton for the emitted-Pure grammar (§5).
//!
//! This is the live automaton: an explicit, hand-written state machine, not a
//! compiled EBNF table (see `specs/m1-l1-grammar.md`, *Design*). [`step`] is a
//! **pure** transition function — `(state, stack_top, byte) -> `[`Step`] — with no
//! I/O, allocation, or hidden state; [`Pda`] is the thin mutable driver that
//! applies each [`Step`] to a state field and a [`Frame`] stack.
//!
//! ## Shape of the recognizer
//!
//! The grammar is a *pipeline of terms*: `source ( "->" step )*`, where a term is
//! an identifier / classpath, a literal, a `$`-var navigation, a lambda, a list,
//! or a parenthesised sub-expression. Rather than one state per named production,
//! the automaton lexes byte-by-byte around two hub states — [`State::ExpectValue`]
//! (at the start of a term) and [`State::AfterValue`] (having just completed one)
//! — and defers all delimiter nesting to the [`Frame`] stack. This keeps the
//! machine an over-approximation of §5 (which §5.6 explicitly sanctions: L1 admits
//! more than the compiler accepts) while enforcing the load-bearing syntactic
//! invariants byte-exactly: a query must open with `|` or `{|`, brackets must
//! balance against the matching opener, string literals close on an un-doubled
//! quote, and `$`/`.`/`->`/`:` each demand the token that may follow them.
//!
//! Multi-byte operators (`->`, `::`, `==`, `&&`, `||` vs. the lambda `|`, …) are
//! recognised by a "saw first byte" state that consults the *next* byte and, when
//! the second byte does not complete the operator, **delegates** it back into the
//! hub state it belongs to by re-invoking [`step`]. That delegation is what keeps
//! the machine a true byte-at-a-time recogniser without any look-ahead buffer.

use crate::grammar::DeadState;

/// A recognizer state: a position in the byte-level parse of an emitted-Pure
/// query.
///
/// The two hubs are [`ExpectValue`](State::ExpectValue) (the machine is about to
/// read a fresh term) and [`AfterValue`](State::AfterValue) (it has just finished
/// one and expects an operator, separator, or closer). Every other variant is a
/// transient lexical position: inside an identifier, number, string, or date
/// literal, or one byte into a multi-byte operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Before the first byte: only `|` (a simple query) or `{` (a block query)
    /// may open the stream.
    Start,
    /// At the start of a term (after `(`, `[`, `{`, a `,`, a `;`, or an operator).
    ExpectValue,
    /// Having just completed a term; an operator, separator, call, or closer may
    /// follow.
    AfterValue,
    /// Inside an identifier or classpath segment (`[A-Za-z_][A-Za-z0-9_]*`).
    InIdent,
    /// Inside the integer part of a number literal.
    InNumberInt,
    /// Inside the fractional part of a number literal, after the `.`.
    InNumberFrac,
    /// Inside a single-quoted string literal.
    ///
    /// `escaped` is `true` when the previous byte was a `'` whose role — closing
    /// quote or the first half of a doubled `''` — is decided by the current
    /// byte (§5.5 quote doubling).
    InStrLit {
        /// Whether a pending `'` is awaiting its disambiguating byte.
        escaped: bool,
    },
    /// Inside a `%`-prefixed date/time literal (`%2018-03-17T07:13:53`).
    InDateLit,
    /// Just consumed `$`; a `refVar` identifier must follow.
    AfterDollar,
    /// Just consumed `.`; a property / getter / `all` identifier must follow.
    AfterDot,
    /// Just consumed `->`; a step / method / reducer identifier must follow.
    AfterArrow,
    /// Just consumed a `:` or `::`; a classpath identifier must follow.
    AfterColon,
    /// Just consumed `-`; a `>` completes `->`, anything else is arithmetic minus.
    SawDash,
    /// Just consumed `|`; a second `|` is boolean `||`, anything else is the
    /// lambda-binder pipe and starts the body.
    SawPipe,
    /// Just consumed `=`; an optional second `=` completes `==` (vs. `let x =`).
    SawEq,
    /// Just consumed `!`; a `=` must follow to complete `!=`.
    SawBang,
    /// Just consumed `>`; an optional `=` completes `>=`.
    SawGt,
    /// Just consumed `<`; an optional `=` completes `<=`.
    SawLt,
    /// Just consumed `&`; a second `&` must follow to complete `&&`.
    SawAmp,
}

impl State {
    /// A stable name for this state, used in [`DecodeError::DeadState`] so a
    /// soundness failure names the exact production position that rejected a byte
    /// (`specs/m1-l1-grammar.md`, G4).
    ///
    /// [`DecodeError::DeadState`]: crate::DecodeError::DeadState
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            State::Start => "Start",
            State::ExpectValue => "ExpectValue",
            State::AfterValue => "AfterValue",
            State::InIdent => "InIdent",
            State::InNumberInt => "InNumberInt",
            State::InNumberFrac => "InNumberFrac",
            State::InStrLit { escaped: false } => "InStrLit",
            State::InStrLit { escaped: true } => "InStrLit(pendingQuote)",
            State::InDateLit => "InDateLit",
            State::AfterDollar => "AfterDollar",
            State::AfterDot => "AfterDot",
            State::AfterArrow => "AfterArrow",
            State::AfterColon => "AfterColon",
            State::SawDash => "SawDash",
            State::SawPipe => "SawPipe",
            State::SawEq => "SawEq",
            State::SawBang => "SawBang",
            State::SawGt => "SawGt",
            State::SawLt => "SawLt",
            State::SawAmp => "SawAmp",
        }
    }
}

/// A stack frame: an open delimiter awaiting its match.
///
/// The frame kind makes bracket matching **context-dependent** (§4.2): a `)`
/// closes only a [`Paren`](Frame::Paren), a `]` only a [`Bracket`](Frame::Bracket),
/// and a `}` only a [`Brace`](Frame::Brace); any other pairing is a dead state.
/// The three delimiter kinds are the whole stack alphabet — pipeline `->` chains
/// and lambda bodies need no resume marker because the [`State::ExpectValue`] /
/// [`State::AfterValue`] hubs already encode "what may come next" without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// An open `(` — a call's argument list or a parenthesised expression.
    Paren,
    /// An open `[` — a list literal or a `[mult]` multiplicity bracket.
    Bracket,
    /// An open `{` — a block query (`{|…}`) or a `join` brace lambda.
    Brace,
}

impl Frame {
    /// A stable name for this frame, used in [`DecodeError::DeadState`]'s
    /// `stack_top` field.
    ///
    /// [`DecodeError::DeadState`]: crate::DecodeError::DeadState
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Frame::Paren => "Paren",
            Frame::Bracket => "Bracket",
            Frame::Brace => "Brace",
        }
    }
}

/// The outcome of feeding one byte to [`step`].
///
/// [`Pop`](Step::Pop) is only ever returned when the byte's closer matches the
/// current `stack_top`, so [`Pda`] can pop unconditionally; a mismatched or
/// missing opener yields [`Dead`](Step::Dead) instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Stay within the current frame; move to the given state.
    Next(State),
    /// Open a new delimiter: push the frame, move to the given state.
    Push(Frame, State),
    /// Close the current (matched) delimiter: pop the stack, move to the state.
    Pop(State),
    /// No valid continuation: the byte is rejected.
    Dead,
}

/// A single space, tab, newline, or carriage return: the inter-token whitespace
/// skipped between — never inside — tokens.
const WS: &[u8; 4] = b" \t\n\r";

fn is_ws(byte: u8) -> bool {
    WS.contains(&byte)
}

const fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_ident_tail(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The bytes that may appear inside a `%`-prefixed date/time literal: digits and
/// the `-`, `T`, `:` separators (`%2018-03-17T07:13:53`).
const fn is_date_char(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'-' | b'T' | b':')
}

/// Close `top` if `byte` is its matching closer, else [`Step::Dead`].
///
/// The one place delimiter matching is decided; both hubs route their `)`/`]`/`}`
/// here so the context-dependent pop lives in a single spot.
const fn close(top: Option<Frame>, byte: u8) -> Step {
    match (top, byte) {
        (Some(Frame::Paren), b')') | (Some(Frame::Bracket), b']') | (Some(Frame::Brace), b'}') => {
            Step::Pop(State::AfterValue)
        }
        _ => Step::Dead,
    }
}

/// The pure transition function: given the current `state`, the `stack_top`
/// frame (if any), and the next `byte`, return the [`Step`] to take.
///
/// Pure and total — the same inputs always yield the same [`Step`], with no side
/// effects. Multi-byte operators are handled by delegating an already-consumed
/// first byte's continuation back into the hub state it belongs to (a tail call
/// to `step` itself), which is why this reads a stream one byte at a time with no
/// look-ahead.
#[must_use]
pub fn step(state: State, stack_top: Option<Frame>, byte: u8) -> Step {
    match state {
        State::Start => match byte {
            b if is_ws(b) => Step::Next(State::Start),
            b'|' => Step::Next(State::ExpectValue),
            b'{' => Step::Push(Frame::Brace, State::ExpectValue),
            _ => Step::Dead,
        },

        State::ExpectValue => match byte {
            b if is_ws(b) => Step::Next(State::ExpectValue),
            b if is_ident_start(b) => Step::Next(State::InIdent),
            b if b.is_ascii_digit() => Step::Next(State::InNumberInt),
            b'-' => Step::Next(State::InNumberInt),
            b'\'' => Step::Next(State::InStrLit { escaped: false }),
            b'%' => Step::Next(State::InDateLit),
            b'$' => Step::Next(State::AfterDollar),
            b'(' => Step::Push(Frame::Paren, State::ExpectValue),
            b'[' => Step::Push(Frame::Bracket, State::ExpectValue),
            b'{' => Step::Push(Frame::Brace, State::ExpectValue),
            // A bare `|` opens a zero-arg lambda body (`if(c, |x, |y)`) or the
            // top-level pipeline right after `{|`.
            b'|' => Step::Next(State::ExpectValue),
            // A `!` in value position is the unary boolean-NOT prefix
            // (`&& !$s.name->in(…)`); the operand follows.
            b'!' => Step::Next(State::ExpectValue),
            // A lone `*` is the `[*]` multiplicity token.
            b'*' => Step::Next(State::AfterValue),
            b')' | b']' | b'}' => close(stack_top, byte),
            _ => Step::Dead,
        },

        State::AfterValue => match byte {
            b if is_ws(b) => Step::Next(State::AfterValue),
            // A fresh identifier abutting a term across whitespace: the `let name`
            // binder in a block query is the corpus witness.
            b if is_ident_start(b) => Step::Next(State::InIdent),
            b'-' => Step::Next(State::SawDash),
            b'>' => Step::Next(State::SawGt),
            b'<' => Step::Next(State::SawLt),
            b'=' => Step::Next(State::SawEq),
            b'!' => Step::Next(State::SawBang),
            b'&' => Step::Next(State::SawAmp),
            b'|' => Step::Next(State::SawPipe),
            b'+' | b'*' | b'/' => Step::Next(State::ExpectValue),
            b'.' => Step::Next(State::AfterDot),
            b':' => Step::Next(State::AfterColon),
            b'(' => Step::Push(Frame::Paren, State::ExpectValue),
            b'[' => Step::Push(Frame::Bracket, State::ExpectValue),
            b',' if stack_top.is_some() => Step::Next(State::ExpectValue),
            b';' if stack_top == Some(Frame::Brace) => Step::Next(State::ExpectValue),
            b')' | b']' | b'}' => close(stack_top, byte),
            _ => Step::Dead,
        },

        State::InIdent => {
            if is_ident_tail(byte) {
                Step::Next(State::InIdent)
            } else {
                step(State::AfterValue, stack_top, byte)
            }
        }

        State::InNumberInt => match byte {
            b if b.is_ascii_digit() => Step::Next(State::InNumberInt),
            b'.' => Step::Next(State::InNumberFrac),
            _ => step(State::AfterValue, stack_top, byte),
        },

        State::InNumberFrac => {
            if byte.is_ascii_digit() {
                Step::Next(State::InNumberFrac)
            } else {
                step(State::AfterValue, stack_top, byte)
            }
        }

        State::InStrLit { escaped } => {
            if escaped {
                // The previous byte was a `'`. A second `'` is a doubled quote
                // (stay in the body); anything else means the string already
                // closed, so re-dispatch this byte from `AfterValue`.
                if byte == b'\'' {
                    Step::Next(State::InStrLit { escaped: false })
                } else {
                    step(State::AfterValue, stack_top, byte)
                }
            } else if byte == b'\'' {
                Step::Next(State::InStrLit { escaped: true })
            } else {
                Step::Next(State::InStrLit { escaped: false })
            }
        }

        State::InDateLit => {
            if is_date_char(byte) {
                Step::Next(State::InDateLit)
            } else {
                step(State::AfterValue, stack_top, byte)
            }
        }

        State::AfterDollar => {
            if is_ident_start(byte) {
                Step::Next(State::InIdent)
            } else {
                Step::Dead
            }
        }

        State::AfterDot => match byte {
            b if is_ws(b) => Step::Next(State::AfterDot),
            b if is_ident_start(b) => Step::Next(State::InIdent),
            _ => Step::Dead,
        },

        State::AfterArrow => match byte {
            b if is_ws(b) => Step::Next(State::AfterArrow),
            b if is_ident_start(b) => Step::Next(State::InIdent),
            _ => Step::Dead,
        },

        State::AfterColon => match byte {
            b if is_ws(b) => Step::Next(State::AfterColon),
            b':' => Step::Next(State::AfterColon),
            b if is_ident_start(b) => Step::Next(State::InIdent),
            _ => Step::Dead,
        },

        // `-` → `->` (arrow) or arithmetic minus (delegate the byte as a value).
        State::SawDash => {
            if byte == b'>' {
                Step::Next(State::AfterArrow)
            } else {
                step(State::ExpectValue, stack_top, byte)
            }
        }

        // `|` → `||` (boolean OR) or the lambda pipe whose body starts here.
        State::SawPipe => {
            if byte == b'|' {
                Step::Next(State::ExpectValue)
            } else {
                step(State::ExpectValue, stack_top, byte)
            }
        }

        // `=` → `==` (comparison) or a single `let x =` assignment; either way the
        // right-hand side is a fresh value.
        State::SawEq => {
            if byte == b'=' {
                Step::Next(State::ExpectValue)
            } else {
                step(State::ExpectValue, stack_top, byte)
            }
        }

        State::SawBang => {
            if byte == b'=' {
                Step::Next(State::ExpectValue)
            } else {
                Step::Dead
            }
        }

        State::SawGt => {
            if byte == b'=' {
                Step::Next(State::ExpectValue)
            } else {
                step(State::ExpectValue, stack_top, byte)
            }
        }

        State::SawLt => {
            if byte == b'=' {
                Step::Next(State::ExpectValue)
            } else {
                step(State::ExpectValue, stack_top, byte)
            }
        }

        State::SawAmp => {
            if byte == b'&' {
                Step::Next(State::ExpectValue)
            } else {
                Step::Dead
            }
        }
    }
}

/// The mutable driver over [`step`]: a current [`State`] and a [`Frame`] stack.
///
/// [`Pda`] owns no offset counter and reports no errors of its own — that is the
/// job of the [`DecoderSession`](crate::DecoderSession) that wraps it. It only
/// applies each [`Step`] and answers whether the stream so far is in an accepting
/// configuration.
#[derive(Debug, Clone)]
pub struct Pda {
    state: State,
    stack: Vec<Frame>,
}

impl Default for Pda {
    fn default() -> Self {
        Self::new()
    }
}

impl Pda {
    /// A fresh automaton positioned at [`State::Start`] with an empty stack.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: State::Start,
            stack: Vec::new(),
        }
    }

    /// Feed one `byte`, advancing the state and stack.
    ///
    /// # Errors
    /// Returns [`DeadState`] — the automaton's `state` name and `stack_top` name
    /// at the point of rejection — when `byte` has no valid continuation. The
    /// automaton is left unchanged on error, so a caller may inspect it.
    pub fn advance(&mut self, byte: u8) -> Result<(), DeadState> {
        let top = self.stack.last().copied();
        match step(self.state, top, byte) {
            Step::Next(next) => {
                self.state = next;
                Ok(())
            }
            Step::Push(frame, next) => {
                self.stack.push(frame);
                self.state = next;
                Ok(())
            }
            Step::Pop(next) => {
                // `step` returns `Pop` only when the closer matched `top`, so the
                // stack is non-empty here.
                self.stack.pop();
                self.state = next;
                Ok(())
            }
            Step::Dead => Err(DeadState {
                state: self.state.name(),
                stack_top: top.map_or("none", Frame::name),
            }),
        }
    }

    /// Whether the stream so far is a complete query: every delimiter closed and
    /// the last token finished ([`State::AfterValue`]).
    #[must_use]
    pub fn is_accepting(&self) -> bool {
        self.stack.is_empty() && self.state == State::AfterValue
    }

    /// Reset to the initial configuration, retaining the stack's allocation
    /// (§9.1) for reuse across streams.
    pub fn reset(&mut self) {
        self.state = State::Start;
        self.stack.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{Frame, Pda, State, Step, WS, is_date_char, is_ident_start, is_ident_tail, step};

    /// Drive `bytes` through a fresh [`Pda`], returning it (or the first dead
    /// state) so a test can assert on the terminal configuration.
    fn run(bytes: &[u8]) -> Result<Pda, (usize, &'static str, &'static str)> {
        let mut pda = Pda::new();
        for (offset, &byte) in bytes.iter().enumerate() {
            if let Err(dead) = pda.advance(byte) {
                return Err((offset, dead.state, dead.stack_top));
            }
        }
        Ok(pda)
    }

    fn accepts(text: &str) -> bool {
        matches!(run(text.as_bytes()), Ok(pda) if pda.is_accepting())
    }

    fn dies(text: &str) -> bool {
        run(text.as_bytes()).is_err()
    }

    #[test]
    fn ws_constant_is_the_four_inter_token_spaces() {
        assert_eq!(WS, b" \t\n\r");
    }

    #[test]
    fn char_class_helpers_agree_with_grammar() {
        assert!(is_ident_start(b'a') && is_ident_start(b'_') && is_ident_start(b'Z'));
        assert!(!is_ident_start(b'0') && !is_ident_start(b'$'));
        assert!(is_ident_tail(b'0') && is_ident_tail(b'z') && is_ident_tail(b'_'));
        assert!(!is_ident_tail(b'-'));
        assert!(
            is_date_char(b'0') && is_date_char(b'-') && is_date_char(b'T') && is_date_char(b':')
        );
        assert!(!is_date_char(b'Z'));
    }

    #[test]
    fn start_admits_only_pipe_or_brace() {
        assert!(matches!(
            step(State::Start, None, b'|'),
            Step::Next(State::ExpectValue)
        ));
        assert!(matches!(
            step(State::Start, None, b'{'),
            Step::Push(Frame::Brace, State::ExpectValue)
        ));
        assert!(matches!(step(State::Start, None, b'x'), Step::Dead));
        assert!(matches!(step(State::Start, None, b'('), Step::Dead));
    }

    #[test]
    fn empty_stream_is_not_accepting() {
        assert!(!Pda::new().is_accepting());
        assert!(!accepts(""));
    }

    #[test]
    fn arm_c_source_and_project_accepts() {
        assert!(accepts("|X.all()->project([x|$x.name], ['n'])"));
    }

    #[test]
    fn arm_a_envelope_accepts() {
        assert!(accepts(
            "|db::Db->tableReference('default', 'T')->tableToTDS()->limit(5)"
        ));
    }

    #[test]
    fn bracket_context_dependence_rejects_crossed_closers() {
        // `(` opened, `]` cannot close a Paren.
        assert!(dies("|X.all()->take(2]"));
        // `[` opened, `)` cannot close a Bracket.
        assert!(dies("|X.all()->project([x|$x.n)"));
        // A closer with an empty stack is dead.
        assert!(dies("|X.all())"));
    }

    #[test]
    fn matched_nested_brackets_accept() {
        assert!(accepts(
            "|X.all()->groupBy([], [agg(x|$x.v, y|$y->sum())], ['s'])"
        ));
    }

    #[test]
    fn string_quote_doubling_is_consumed_in_body() {
        // The doubled `''` is one embedded quote, not a close-then-reopen.
        assert!(accepts("|X.all()->filter(x|$x.name == 'O''Brien')"));
        // An un-doubled closing quote ends the string; the trailing `)` closes.
        assert!(accepts("|X.all()->restrict('Rank')"));
    }

    #[test]
    fn parens_inside_a_string_do_not_touch_the_stack() {
        // `'COUNT()'` must not push/pop Paren frames.
        assert!(accepts(
            "|db::Db->tableReference('default', 'T')->tableToTDS()\
             ->groupBy([], agg('COUNT()', row: meta::pure::tds::TDSRow[1]|$row, \
             y: meta::pure::tds::TDSRow[*]|$y->count()))"
        ));
    }

    #[test]
    fn whitespace_is_skipped_between_tokens_only() {
        assert!(accepts("|X.all()\n  ->filter( x | $x.age > 18 )"));
        // …but a token is never split: a space inside a number literal leaves a
        // stray digit that `AfterValue` cannot resume.
        assert!(dies("|X.all()->take(1 0)"));
    }

    #[test]
    fn empty_key_group_by_accepts() {
        assert!(accepts(
            "|X.all()->groupBy([], [agg(x|$x.v, y|$y->count())], ['c'])"
        ));
    }

    #[test]
    fn typed_multiplicity_binder_accepts_one_and_star() {
        assert!(accepts(
            "|db::Db->tableReference('default','T')->tableToTDS()\
             ->filter(row: meta::pure::tds::TDSRow[1]|$row.getInteger('c') == 1)"
        ));
        assert!(accepts(
            "|db::Db->tableReference('default','T')->tableToTDS()\
             ->groupBy([], agg('C', row: meta::pure::tds::TDSRow[1]|$row, \
             y: meta::pure::tds::TDSRow[*]|$y->count()))"
        ));
    }

    #[test]
    fn brace_multi_binder_join_accepts() {
        assert!(accepts(
            "|a::Db->tableReference('default','A')->tableToTDS()->join(\
             a::Db->tableReference('default','B')->tableToTDS(), \
             meta::relational::metamodel::join::JoinType.INNER, \
             {r1: meta::pure::tds::TDSRow[1], r2: meta::pure::tds::TDSRow[1]|\
             $r1.getInteger('x') == $r2.getInteger('y')})"
        ));
    }

    #[test]
    fn dollar_requires_an_identifier() {
        assert!(dies("|X.all()->filter(x|$)"));
        assert!(dies("|X.all()->filter(x|$5 > 1)"));
    }

    #[test]
    fn or_operator_is_distinct_from_the_lambda_pipe() {
        // First `|` is the binder pipe, `||` is boolean OR.
        assert!(accepts("|X.all()->filter(x|($x.a == 1) || ($x.b == 2))"));
    }

    #[test]
    fn bang_is_both_the_not_prefix_and_the_ne_operator() {
        // Unary NOT in value position (after `&&`).
        assert!(accepts(
            "|X.all()->filter(s|($s.a == 0) && !$s.name->in($xs))"
        ));
        // Binary `!=` in operator position (after a value).
        assert!(accepts("|X.all()->filter(x|$x.a != 1)"));
        // A lone `!` not completing `!=` in operator position is dead.
        assert!(dies("|X.all()->filter(x|$x.a ! 1)"));
    }

    #[test]
    fn block_query_with_let_binding_accepts() {
        assert!(accepts(
            "{|let m = X.all().pop->max(); Y.all()->filter(b|$b.v == $m)\
             ->project([x|$x.c], ['c']);}"
        ));
    }

    #[test]
    fn date_literal_operand_accepts() {
        assert!(accepts(
            "|db::Db->tableReference('default','T')->tableToTDS()\
             ->filter(r: meta::pure::tds::TDSRow[1]|$r.getDateTime('d') < %2018-03-17T07:13:53)"
        ));
    }

    #[test]
    fn unterminated_string_or_open_paren_is_not_accepting() {
        assert!(!accepts("|X.all()->restrict('Rank"));
        assert!(!accepts("|X.all()->take(2"));
    }

    #[test]
    fn reset_returns_to_the_initial_configuration() {
        let mut pda = Pda::new();
        for &byte in b"|X.all()->take(2)" {
            pda.advance(byte).expect("live");
        }
        assert!(pda.is_accepting());
        pda.reset();
        assert!(!pda.is_accepting());
        assert!(pda.advance(b'x').is_err());
    }

    #[test]
    fn start_skips_leading_whitespace_before_the_opener() {
        assert!(accepts(" \n\t|X.all()->take(1)"));
    }

    #[test]
    fn top_level_separators_require_an_open_frame() {
        // A `,` or `;` is legal only inside a frame; with an empty stack it dies.
        assert!(dies("|X.all(),"));
        assert!(dies("|X.all();"));
    }

    #[test]
    fn dot_arrow_colon_skip_whitespace_then_demand_an_identifier() {
        // Whitespace after `.` / `->` is skipped, then an identifier is required.
        assert!(accepts("|X. all()->take(1)"));
        assert!(accepts("|X.all()->  take(1)"));
        // …and a non-identifier byte in that position is a dead state.
        assert!(dies("|X.all().5"));
        assert!(dies("|X.all()->5"));
        assert!(dies("|X::5"));
    }

    #[test]
    fn a_dead_state_names_its_state_and_top_frame() {
        // `.` then a digit dies in `AfterDot`, with the enclosing stack empty.
        assert_eq!(
            run(b"|X.all().5").expect_err("dies"),
            (9, "AfterDot", "none")
        );
        // A `,` with nothing to separate dies in `ExpectValue` under a `Paren`.
        assert_eq!(
            run(b"|X.all(,").expect_err("dies"),
            (7, "ExpectValue", "Paren")
        );
        // An unmatched closer dies with an empty stack (`none`).
        assert_eq!(
            run(b"|X.all())").expect_err("dies"),
            (8, "AfterValue", "none")
        );
    }
}
