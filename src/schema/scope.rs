//! The L2 scope-tracking state machine (`docs/spec/schema.md` §6.4).
//!
//! L1's byte-PDA surfaces only the *lexical* anchor (`AfterDot`, `ExpectSource`,
//! a comparison operator state); it cannot know which class a `$var` is bound to,
//! the class a navigation has reached, or whether the pipeline has become a
//! relation — the context-sensitive facts §6.1 forbids a PDA from carrying. The
//! [`ScopeTracker`] threads that small typed scope through the parse **in
//! lockstep** with the byte-PDA (advanced token-by-token from
//! [`DecoderSession`](crate::DecoderSession)), and at each identifier/operand
//! position yields an [`L2Position`] the narrower keys on.
//!
//! It never widens: an unresolved or unknown scope yields
//! [`L2Position::None`] (pass-through), so a position the tracker cannot type is
//! left exactly as L1 allowed it.

use std::collections::HashMap;

use crate::grammar::pda::{Frame, LexKind, Pda, State, Step, is_ident_tail, step};
use crate::schema::model::{Resolved, Schema, TypeClass};

/// Whether `a`, `b` begin one of the two-byte operators the grammar recognises
/// (`-> == != <= >= && ||`). A structural-gap walk munches these whole so an
/// operator never fragments into mis-classified single bytes (`>` alone reads as
/// a comparison).
const fn is_two_byte_op(a: u8, b: u8) -> bool {
    matches!(
        (a, b),
        (b'-', b'>')
            | (b'=', b'=')
            | (b'!', b'=')
            | (b'<', b'=')
            | (b'>', b'=')
            | (b'&', b'&')
            | (b'|', b'|')
    )
}

/// Whether `byte` may start an identifier — the block-query `let` keyword and a
/// classpath source both begin here, the only alphabetic runs a structural gap
/// can hold.
const fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

/// A lexical token, classified from its raw bytes — the granularity the tracker
/// and narrower reason over. Whole identifiers/classpaths, string/number/date
/// literals, and the operators that drive scope transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lexeme {
    /// Inter-token whitespace (skipped by the scope machine).
    Ws,
    /// An identifier or `::`-joined classpath; carries its text.
    Ident(String),
    /// A single-quoted string literal; carries its unescaped content as raw bytes
    /// (byte-exact, so the N6 column key never desyncs from the trie through a
    /// lossy UTF-8 round-trip).
    Str(Vec<u8>),
    /// A numeric literal.
    Number,
    /// A `%`-prefixed date/time literal.
    Date,
    /// The `->` step connector.
    Arrow,
    /// A `.` navigation dot.
    Dot,
    /// A `$` refVar sigil.
    Dollar,
    /// A lone `|` (lambda binder pipe, or the query opener at `Start`).
    Pipe,
    /// A comparison operator (`== != < > <= >=`); carries its operand type-class
    /// eligibility (all are comparisons; ordered-vs-equality is a deferred T2).
    Cmp,
    /// An opening delimiter `(` `[` `{`.
    Open,
    /// A closing delimiter `)` `]` `}`.
    Close,
    /// An argument/list separator `,`.
    Comma,
    /// Any other byte(s) not load-bearing for L2 (`; : ! + - * / && || =`).
    Other,
}

/// Classify a token's raw bytes into a [`Lexeme`].
pub(crate) fn classify(bytes: &[u8]) -> Lexeme {
    if bytes.is_empty() || bytes.iter().all(u8::is_ascii_whitespace) {
        return Lexeme::Ws;
    }
    match bytes {
        b"->" => return Lexeme::Arrow,
        b"==" | b"!=" | b"<=" | b">=" | b"<" | b">" => return Lexeme::Cmp,
        b"." => return Lexeme::Dot,
        b"$" => return Lexeme::Dollar,
        b"|" => return Lexeme::Pipe,
        b"," => return Lexeme::Comma,
        b"(" | b"[" | b"{" => return Lexeme::Open,
        b")" | b"]" | b"}" => return Lexeme::Close,
        _ => {}
    }
    let first = bytes[0];
    if first == b'\'' {
        return Lexeme::Str(unquote(bytes));
    }
    if first == b'%' {
        return Lexeme::Date;
    }
    if first.is_ascii_digit() || (first == b'-' && bytes.get(1).is_some_and(u8::is_ascii_digit)) {
        return Lexeme::Number;
    }
    if first.is_ascii_alphabetic() || first == b'_' {
        // An identifier or `::`-joined classpath.
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Lexeme::Ident(text.to_owned());
        }
    }
    Lexeme::Other
}

/// Strip the surrounding single quotes and undouble `''` from a string literal's
/// raw bytes, yielding its logical content (§5.5 quote doubling).
fn unquote(bytes: &[u8]) -> Vec<u8> {
    let inner = bytes
        .strip_prefix(b"'")
        .and_then(|rest| rest.strip_suffix(b"'"))
        .unwrap_or(bytes);
    // Undouble `''` -> `'` on the raw bytes — byte-exact, no UTF-8 round-trip that
    // a `�` could corrupt.
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        out.push(inner[i]);
        i += if inner[i] == b'\'' && inner.get(i + 1) == Some(&b'\'') {
            2
        } else {
            1
        };
    }
    out
}

/// The schema-consistency constraint that applies at the current position — the
/// key the narrower ([`narrow_into`](crate::schema::narrow::narrow_into)) builds a legal
/// set from. [`None`](L2Position::None) means "no L2 constraint here" (the L1
/// mask passes through unchanged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum L2Position {
    /// N3: the pipeline source classpath must be a real class (or the store).
    SourceIdent,
    /// N1/N2: the identifier after `.` must be a member of `class`.
    Member(String),
    /// T1: the comparison operand's literal type must match `class`.
    ReValue(TypeClass),
    /// N6: a relation-column string reference must name an emitted column.
    Column,
    /// No L2 constraint here — pass the L1 mask through unchanged.
    None,
}

/// Names the L1 methods that establish a named relation scope (§6.4.5/6.4.6):
/// after one of these calls closes, subsequent column references are narrowed
/// (N6). Their own argument lambdas still run over the pre-relation scope, so a
/// reference *inside* them is not narrowed.
const ESTABLISHING_METHODS: &[&str] = &["project", "groupBy", "olapGroupBy"];

/// Names the L1 positions whose string argument is a relation-column *reference*
/// (§6.5 N6): the TDS getters and the sort/column selectors.
const REF_METHODS: &[&str] = &[
    "getInteger",
    "getFloat",
    "getString",
    "getBoolean",
    "sort",
    "asc",
    "desc",
    "restrict",
];

/// An identifier / string lexeme being accumulated across BPE sub-tokens (§6.4).
///
/// Byte-level BPE fragments a schema identifier (`countryName` → `country` +
/// `Name`); the tracker buffers the fragments and dispatches the scope transition
/// (resolve / bind / emit) only once the *whole* lexeme completes, so
/// [`resolve_member`](ScopeTracker::resolve_member) sees the whole name (M3). The
/// buffered bytes also serve as the trie-walk prefix the narrower reads (B1), so
/// the constraint persists across the sub-tokens rather than firing only at the
/// leading one.
#[derive(Debug, Clone)]
struct Pending {
    /// The lexeme class being accumulated.
    kind: LexKind,
    /// The bytes emitted since the anchor (the trie-walk prefix, and the whole
    /// lexeme once it closes).
    buf: Vec<u8>,
    /// The PDA state where the lexeme opened — the "pre-state" the buffered token
    /// is dispatched under, so its scope transition matches the whole-token path.
    anchor: State,
    /// The L2 rule constraining this lexeme (or [`None`](L2Position::None) for an
    /// unnarrowed lexeme such as a keyword or a plain operand), read by
    /// [`position`](ScopeTracker::position) while the lexeme is in flight.
    pos: L2Position,
}

/// The §6.4 scope machine, advanced in lockstep with the byte-PDA.
///
/// It holds the pipeline element class, the lambda variable bindings, the
/// in-flight navigation cursor, and the relation-scope / column-reference
/// bookkeeping N6 keys on. Every field defaults to "unknown", and every
/// transition that cannot be typed leaves the scope unknown — so
/// [`position`](ScopeTracker::position) degrades to [`L2Position::None`] rather
/// than risk masking a real token.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScopeTracker {
    /// The identifier / string lexeme accumulating across sub-tokens, if any.
    pending: Option<Pending>,
    /// The current pipeline element class (the most recent `Class.all()` source).
    cur_class: Option<String>,
    /// Lambda variable → the class it is bound to (`None` = unknown, e.g. a TDS
    /// row binder), for N1 rooted at `$var`.
    var_class: HashMap<String, Option<String>>,
    /// A `$` was just seen; the next identifier is its refVar name.
    pending_refvar: Option<String>,
    /// A `.` was just seen; the class we are navigating *from* (N1/N2 base), or
    /// `None` when the dot is not a member navigation (`.all()`, `.getX`).
    dot_base: Option<String>,
    /// The class a navigation chain has reached so far (feeds N2).
    nav_cursor: Option<String>,
    /// The type-class of the most recently completed primitive navExpr — read by
    /// the *next* comparison operator to arm T1.
    last_resolved: Option<TypeClass>,
    /// The class the most recently completed navExpr resolved to (a to-many/class
    /// nav receiver), used to bind a following method lambda's variable.
    last_nav_class: Option<String>,
    /// T1 is armed: the next operand position expects a literal of this class.
    cmp_pending: Option<TypeClass>,
    /// The first identifier of the current lambda argument (its binder name).
    lambda_first_ident: Option<String>,
    /// Receiver class captured at a `->`, awaiting the method's `(` to become the
    /// enclosing paren's lambda-binding class.
    pending_arrow_receiver: Option<Option<String>>,
    /// Per-open-paren lambda-binding receiver class.
    paren_receiver: Vec<Option<String>>,
    /// Paren depths at which an establishing op is still open.
    est_stack: Vec<u32>,
    /// Paren depths at which a column-reference method is still open.
    ref_stack: Vec<u32>,
    /// The current delimiter nesting depth.
    depth: u32,
    /// The most recent identifier — the candidate method name before a `(`.
    last_ident: Option<String>,
    /// Have we passed a *closed* establishing op (a named relation exists)?
    rel_explicit: bool,
    /// Every string literal emitted so far — the N6 legal column set (a superset,
    /// so a real reference to a previously-emitted name is never masked). Stored as
    /// raw bytes, byte-exact against the trie's `quote` key.
    emitted_strings: Vec<Vec<u8>>,
}

impl ScopeTracker {
    /// A fresh tracker at the start of a stream.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Advance the scope machine by one committed token, given its raw `bytes`, the
    /// pre-fold PDA configuration `pre` (state **and** stack), and the `schema`.
    ///
    /// Called from [`accept_token`](crate::DecoderSession::accept_token) as the
    /// token commits, so scope moves in lockstep with the automaton. A byte-level
    /// BPE token may straddle several lexeme boundaries (`'MaxRevenue')`, `.count`,
    /// `('`): the walk re-drives [`step`] read-only over the token, splitting it at
    /// each interior lexeme boundary and driving every constituent lexeme through
    /// the same per-lexeme logic a lexeme-granular stream uses (constitution §4,
    /// DRY). A run still open at the token's end (an identifier/string arriving in
    /// fragments) is buffered into [`Pending`] and resolved when a later token
    /// closes it (§6.4, B1/M3); a run that closes inside the token is dispatched at
    /// once, so a buried `.`/`(` fires `on_dot`/`on_open` (H2) and a merged closing
    /// quote records the true column bytes (H1). The seed stack lets an interior
    /// closer (`)`) route through the matching frame rather than dying.
    pub(crate) fn observe(&mut self, bytes: &[u8], pre: &Pda, schema: &Schema) {
        let mut state = pre.state();
        let mut stack: Vec<Frame> = pre.stack().to_vec();
        // The first segment continues a lexeme buffered before this token only when
        // the pre-state sits inside that pending lexeme's own class.
        let mut continuing = self
            .pending
            .as_ref()
            .is_some_and(|p| state.lexeme_kind() == Some(p.kind));
        // The pre-state at the current segment's first byte — the anchor its scope
        // transition dispatches under (a continuation inherits the buffered anchor).
        let mut seg_anchor = if continuing {
            self.pending.as_ref().map_or(state, |p| p.anchor)
        } else {
            state
        };
        let mut seg_start = 0usize;
        // A pending that this token does not continue would be an unclosed lexeme L1
        // never admits; flush it defensively so no buffer leaks across tokens.
        if !continuing && let Some(done) = self.pending.take() {
            self.dispatch_token(&done.buf, done.anchor, schema);
        }

        for i in 0..bytes.len() {
            let before = state;
            let prev_kind = before.lexeme_kind();
            let top = stack.last().copied();
            state = match step(before, top, bytes[i]) {
                Step::Next(s) => s,
                Step::Push(frame, s) => {
                    stack.push(frame);
                    s
                }
                Step::Pop(s) => {
                    stack.pop();
                    s
                }
                // The token was pre-validated by L1's fold, so no byte dies here.
                Step::Dead => return,
            };
            let cur_kind = state.lexeme_kind();

            match prev_kind {
                // A lexeme closed via delegation at byte `i`: that byte is the
                // boundary that ended it (not part of it). Dispatch the lexeme
                // (prepending any cross-token buffer), then reopen a segment at `i`.
                Some(k) if cur_kind != Some(k) => {
                    self.emit_lexeme(&bytes[seg_start..i], seg_anchor, continuing, schema);
                    continuing = false;
                    seg_start = i;
                    seg_anchor = before;
                }
                // Still inside the same lexeme — keep accumulating.
                Some(_) => {}
                // In a structural gap; when a lexeme opens at byte `i`, flush the
                // gap that preceded it and start the lexeme segment here.
                None => {
                    if cur_kind.is_some() {
                        self.flush_gap(&bytes[seg_start..i], seg_anchor, schema);
                        seg_start = i;
                        seg_anchor = before;
                        continuing = false;
                    }
                }
            }
        }

        // The trailing segment: an open lexeme is buffered (resolved when a later
        // token closes it); a structural gap is dispatched whole.
        match state.lexeme_kind() {
            Some(kind) => self.buffer_trailing(kind, &bytes[seg_start..], seg_anchor, continuing),
            None => self.flush_gap(&bytes[seg_start..], seg_anchor, schema),
        }
    }

    /// Dispatch a closed lexeme through the per-token scope logic, prepending the
    /// cross-token [`Pending`] buffer when this run continues one. A `Str` lexeme
    /// arrives with both quotes, so [`classify`]/[`unquote`] records its byte-exact
    /// content into `emitted_strings` (H1).
    fn emit_lexeme(&mut self, seg: &[u8], anchor: State, continuing: bool, schema: &Schema) {
        if continuing && let Some(done) = self.pending.take() {
            let mut full = done.buf;
            full.extend_from_slice(seg);
            self.dispatch_token(&full, done.anchor, schema);
            return;
        }
        self.dispatch_token(seg, anchor, schema);
    }

    /// Split a structural gap (operators, punctuation, and — on the block-query
    /// `let` path — a bare keyword identifier) into its constituent tokens and
    /// dispatch each. Maximal munch mirrors [`classify`]'s granularity so a
    /// multi-byte operator (`->`, `==`) stays one token rather than fragmenting
    /// into mis-classified single bytes. Every gap token shares the gap's anchor
    /// pre-state; only `|` (`on_pipe`) and the `let`-path ident read it, and both
    /// classify identically from that anchor.
    fn flush_gap(&mut self, gap: &[u8], anchor: State, schema: &Schema) {
        let mut j = 0;
        while j < gap.len() {
            let b = gap[j];
            if b.is_ascii_whitespace() {
                j += 1;
            } else if j + 1 < gap.len() && is_two_byte_op(b, gap[j + 1]) {
                self.dispatch_token(&gap[j..j + 2], anchor, schema);
                j += 2;
            } else if is_ident_start(b) {
                let mut k = j + 1;
                while k < gap.len() && is_ident_tail(gap[k]) {
                    k += 1;
                }
                self.dispatch_token(&gap[j..k], anchor, schema);
                j = k;
            } else {
                self.dispatch_token(&gap[j..j + 1], anchor, schema);
                j += 1;
            }
        }
    }

    /// Buffer a lexeme still open at the token's end into [`Pending`], resolved and
    /// narrowed once a later token closes it (§6.4, B1/M3). A continuation extends
    /// the existing buffer; a fresh run opens a new one, stamping the rule its
    /// anchor establishes (T1's `ReValue` lever is a whole-token literal-class
    /// test, so its continuation sub-tokens pass through untouched).
    fn buffer_trailing(&mut self, kind: LexKind, seg: &[u8], anchor: State, continuing: bool) {
        if continuing {
            if let Some(pending) = self.pending.as_mut() {
                pending.buf.extend_from_slice(seg);
            }
            return;
        }
        let pos = match self.opening_position(anchor) {
            L2Position::ReValue(_) => L2Position::None,
            narrowed => narrowed,
        };
        self.pending = Some(Pending {
            kind,
            buf: seg.to_vec(),
            anchor,
            pos,
        });
    }

    /// Apply one whole lexeme's scope transition, given its raw `bytes` and the
    /// PDA `pre_state` it opened at. This is the per-token logic a lexeme-granular
    /// stream drives directly; the BPE path routes buffered lexemes through it too
    /// (constitution §4, DRY), so a fragmented and a whole identifier drive scope
    /// identically.
    fn dispatch_token(&mut self, bytes: &[u8], pre_state: State, schema: &Schema) {
        let lex = classify(bytes);
        if lex == Lexeme::Ws {
            return;
        }
        // The operand of an armed comparison consumes the T1 arming (position()
        // has already been read for this token before it was accepted).
        let was_cmp = matches!(lex, Lexeme::Cmp);
        let mut resolved_now: Option<TypeClass> = None;

        match &lex {
            Lexeme::Ident(text) => self.on_ident(text, pre_state, schema, &mut resolved_now),
            Lexeme::Dot => self.on_dot(),
            Lexeme::Arrow => self.on_arrow(),
            Lexeme::Cmp => {
                if let Some(tc) = self.last_resolved {
                    self.cmp_pending = Some(tc);
                }
            }
            Lexeme::Pipe => self.on_pipe(pre_state),
            Lexeme::Open => self.on_open(),
            Lexeme::Close => self.on_close(),
            Lexeme::Comma => {
                self.lambda_first_ident = None;
                self.last_ident = None;
            }
            Lexeme::Str(content) => {
                self.emitted_strings.push(content.clone());
                self.last_ident = None;
            }
            // A `$` sigil, number, date, or other structural byte is not an
            // identifier, so it clears the pending method name. A `$` needs no
            // further work: the refVar name that follows overwrites `pending_refvar`
            // unconditionally, and a fresh navigation reads the bound var (via
            // `on_dot`'s precedence) rather than any stale `nav_cursor`.
            Lexeme::Dollar | Lexeme::Number | Lexeme::Date | Lexeme::Other => {
                self.last_ident = None;
            }
            Lexeme::Ws => {}
        }

        // T1 arming lives exactly one non-whitespace token: it is set by a
        // primitive navExpr and read by the immediately following comparison.
        self.last_resolved = resolved_now;
        // The comparison operand (any non-cmp token after an armed comparison)
        // clears the arming.
        if !was_cmp {
            self.cmp_pending = None;
        }
    }

    fn on_ident(
        &mut self,
        text: &str,
        pre_state: State,
        schema: &Schema,
        resolved_now: &mut Option<TypeClass>,
    ) {
        // A fully-qualified class path only appears as a pipeline source; binding
        // the pipeline element class here also handles nested subquery sources.
        if schema.has_class(text) {
            self.cur_class = Some(text.to_owned());
        }
        match pre_state {
            // A refVar use (`$x`): never a lambda binder, never a member position.
            State::AfterDollar => {
                self.pending_refvar = Some(text.to_owned());
            }
            State::AfterDot => {
                self.resolve_member(text, schema, resolved_now);
            }
            // An identifier at a fresh value position is a lambda binder candidate
            // (`filter(x|…)`, `row: …|…`), recorded so the next binder pipe can
            // bind it. Property/method/refVar/source identifiers arrive in other
            // states and are never binders. A value position holds at most one such
            // identifier before its binder pipe (a body ident sits behind a `.`,
            // `->`, or `$`), so recording it unconditionally is exact — no
            // first-vs-last ambiguity to guard against.
            State::ExpectValue | State::ExpectValueReq => {
                self.lambda_first_ident = Some(text.to_owned());
            }
            _ => {}
        }
        self.last_ident = Some(text.to_owned());
    }

    fn resolve_member(
        &mut self,
        ident: &str,
        schema: &Schema,
        resolved_now: &mut Option<TypeClass>,
    ) {
        let Some(base) = self.dot_base.take() else {
            // A dot that is not a member navigation (`.all()`, `.getX`, `$r.` over
            // an unknown binder): no resolution, no cursor change.
            return;
        };
        match schema.resolve(&base, ident) {
            Some(Resolved::Class { path, .. }) => {
                self.nav_cursor = Some(path.clone());
                self.last_nav_class = Some(path);
            }
            Some(Resolved::Primitive { prim, .. }) => {
                *resolved_now = Some(prim.type_class());
                self.nav_cursor = None;
                self.last_nav_class = None;
            }
            Some(Resolved::Enum { .. }) | None => {
                self.nav_cursor = None;
                self.last_nav_class = None;
            }
        }
    }

    fn on_dot(&mut self) {
        if let Some(var) = self.pending_refvar.take() {
            self.dot_base = self.var_class.get(&var).cloned().flatten();
        } else if let Some(cursor) = &self.nav_cursor {
            self.dot_base = Some(cursor.clone());
        } else {
            self.dot_base = None;
        }
    }

    fn on_arrow(&mut self) {
        // The arrow ends the current navExpr; capture the receiver for a possible
        // following method lambda, then reset the nav cursor.
        let receiver = self
            .last_nav_class
            .take()
            .or_else(|| self.cur_class.clone());
        self.pending_arrow_receiver = Some(receiver);
        self.pending_refvar = None;
        self.nav_cursor = None;
        self.last_ident = None;
    }

    fn on_pipe(&mut self, pre_state: State) {
        // The query-opening `|` at Start is not a binder.
        if matches!(pre_state, State::Start | State::ExpectSource) {
            return;
        }
        if let Some(name) = self.lambda_first_ident.take()
            && !name.is_empty()
        {
            let receiver = self.paren_receiver.last().cloned().flatten();
            self.var_class.insert(name, receiver);
        }
    }

    fn on_open(&mut self) {
        let method = self.last_ident.take();
        self.depth += 1;
        if let Some(name) = &method {
            if ESTABLISHING_METHODS.contains(&name.as_str()) {
                self.est_stack.push(self.depth);
            }
            if REF_METHODS.contains(&name.as_str()) {
                self.ref_stack.push(self.depth);
            }
        }
        let receiver = self
            .pending_arrow_receiver
            .take()
            .unwrap_or_else(|| self.cur_class.clone());
        self.paren_receiver.push(receiver);
        self.lambda_first_ident = None;
    }

    fn on_close(&mut self) {
        if self.ref_stack.last() == Some(&self.depth) {
            self.ref_stack.pop();
        }
        if self.est_stack.last() == Some(&self.depth) {
            self.est_stack.pop();
            // A named relation now exists downstream (§6.4.5/6.4.6): the pipeline
            // element is a TDS row, not a class instance, so a following lambda
            // binder must NOT bind to the (pre-group) source class. Clearing
            // `cur_class` makes such binders unknown → N1 pass-through, never a
            // mask of a TDS-row getter.
            self.rel_explicit = true;
            self.cur_class = None;
        }
        self.paren_receiver.pop();
        self.depth = self.depth.saturating_sub(1);
        self.pending_arrow_receiver = None;
    }

    /// Whether we are inside a column-reference method's arguments *and* a named
    /// relation exists and we are not inside an establishing op's own arguments —
    /// the exact condition for an N6 [`Column`](L2Position::Column) narrowing.
    fn in_column_arg(&self) -> bool {
        !self.ref_stack.is_empty() && self.rel_explicit && self.est_stack.is_empty()
    }

    /// The L2 constraint at the current PDA `state`.
    ///
    /// At an **anchor** state (an inter-lexeme position) the rule is read from the
    /// automaton state and the typed scope. At an **in-lexeme** state (mid
    /// identifier/string, where a BPE sub-token lands) it is the rule the open
    /// accumulation carries — so the trie narrows the continuation sub-tokens, not
    /// only the leading one (B1). An in-lexeme state with no open accumulation, or
    /// an accumulation the anchor did not narrow, is [`None`](L2Position::None).
    pub(crate) fn position(&self, state: State) -> L2Position {
        if state.lexeme_kind().is_some() {
            return match &self.pending {
                Some(pending) => pending.pos.clone(),
                None => L2Position::None,
            };
        }
        self.opening_position(state)
    }

    /// The L2 rule at the anchor `state` where a lexeme opens — read from the
    /// automaton state and the typed scope. Shared by [`position`] (for anchor
    /// states) and by `observe` (to stamp an opening accumulation's rule).
    fn opening_position(&self, state: State) -> L2Position {
        match state {
            State::ExpectSource | State::BlockStmt | State::BlockStmtClose => {
                L2Position::SourceIdent
            }
            State::AfterDot => match &self.dot_base {
                Some(base) => L2Position::Member(base.clone()),
                None => L2Position::None,
            },
            State::ExpectValue | State::ExpectValueReq => {
                if let Some(tc) = self.cmp_pending {
                    L2Position::ReValue(tc)
                } else if self.in_column_arg() {
                    L2Position::Column
                } else {
                    L2Position::None
                }
            }
            _ => L2Position::None,
        }
    }

    /// The identifier/string bytes emitted since the current lexeme's anchor — the
    /// trie-walk prefix the narrower reads. Empty at an anchor (no open
    /// accumulation) so the walk starts at the trie root.
    pub(crate) fn narrow_prefix(&self) -> &[u8] {
        match &self.pending {
            Some(pending) => &pending.buf,
            None => &[],
        }
    }

    /// The N6 legal column set: every string literal emitted so far.
    pub(crate) fn emitted_columns(&self) -> &[Vec<u8>] {
        &self.emitted_strings
    }
}

#[cfg(test)]
mod tests {
    use super::{L2Position, Lexeme, ScopeTracker, classify};
    use crate::grammar::pda::{Pda, State};
    use crate::schema::model::{Schema, TypeClass};

    const SAMPLE: &str = r#"{
      "db_id": "d", "db_path": "spider::d::Db",
      "classes": {
        "A": { "simple_name": "A", "properties": [
          {"name": "n", "type": {"kind": "primitive", "name": "Integer"}, "mult": {"lower": 1, "upper": 1}},
          {"name": "s", "type": {"kind": "primitive", "name": "String"}, "mult": {"lower": 0, "upper": 1}}
        ] } },
      "associations": [], "enums": {}
    }"#;

    fn schema() -> Schema {
        Schema::from_json(SAMPLE).expect("parses")
    }

    /// Drive `tokens` through a fresh PDA + tracker exactly as the session does
    /// (pre-state captured before folding), returning both so a test can read the
    /// position at the live automaton state.
    fn run(tokens: &[&[u8]]) -> (ScopeTracker, Pda) {
        let schema = schema();
        let mut pda = Pda::new();
        let mut tracker = ScopeTracker::new();
        for token in tokens {
            let pre = pda.clone();
            for &byte in *token {
                pda.advance(byte)
                    .expect("test tokens are valid emitted Pure");
            }
            tracker.observe(token, &pre, &schema);
        }
        (tracker, pda)
    }

    #[test]
    fn classify_distinguishes_every_lexeme_class() {
        assert_eq!(classify(b""), Lexeme::Ws);
        assert_eq!(classify(b"  \n"), Lexeme::Ws);
        assert_eq!(classify(b"->"), Lexeme::Arrow);
        assert_eq!(classify(b"=="), Lexeme::Cmp);
        assert_eq!(classify(b">"), Lexeme::Cmp);
        assert_eq!(classify(b"."), Lexeme::Dot);
        assert_eq!(classify(b"$"), Lexeme::Dollar);
        assert_eq!(classify(b"|"), Lexeme::Pipe);
        assert_eq!(classify(b","), Lexeme::Comma);
        assert_eq!(classify(b"("), Lexeme::Open);
        assert_eq!(classify(b"]"), Lexeme::Close);
        assert_eq!(classify(b"42"), Lexeme::Number);
        assert_eq!(classify(b"-7"), Lexeme::Number);
        assert_eq!(classify(b"%2018-01-01"), Lexeme::Date);
        assert_eq!(
            classify(b"spider::d::A"),
            Lexeme::Ident("spider::d::A".to_owned())
        );
        assert_eq!(classify(b"+"), Lexeme::Other);
        assert_eq!(classify(b"-"), Lexeme::Other);
    }

    #[test]
    fn classify_unquotes_and_undoubles_a_string_literal() {
        assert_eq!(classify(b"'ab'"), Lexeme::Str(b"ab".to_vec()));
        // A doubled quote collapses to one (§5.5).
        assert_eq!(classify(b"'a''b'"), Lexeme::Str(b"a'b".to_vec()));
    }

    #[test]
    fn source_position_is_reported_before_any_token() {
        let tracker = ScopeTracker::new();
        assert_eq!(
            tracker.position(State::ExpectSource),
            L2Position::SourceIdent
        );
        assert_eq!(tracker.position(State::BlockStmt), L2Position::SourceIdent);
    }

    #[test]
    fn a_bound_var_dot_yields_a_member_position_on_its_class() {
        // `|A.all()->filter(x|$x.` — x is bound to A, so the dot is N1 on A.
        let tokens: &[&[u8]] = &[
            b"|", b"A", b".", b"all", b"(", b")", b"->", b"filter", b"(", b"x", b"|", b"$", b"x",
            b".",
        ];
        let (tracker, pda) = run(tokens);
        assert_eq!(pda.state(), State::AfterDot);
        assert_eq!(
            tracker.position(pda.state()),
            L2Position::Member("A".to_owned())
        );
    }

    #[test]
    fn an_all_dot_is_not_a_member_navigation() {
        // The `.` of `A.all()` navigates from no bound var — no Member narrowing,
        // so `all` is never masked.
        let (tracker, pda) = run(&[b"|", b"A", b"."]);
        assert_eq!(pda.state(), State::AfterDot);
        assert_eq!(tracker.position(pda.state()), L2Position::None);
    }

    #[test]
    fn a_primitive_navexpr_then_comparison_arms_t1_with_its_type_class() {
        // `$x.n ==` — n is Integer, so the operand position is ReValue(Numeric).
        let numeric: &[&[u8]] = &[
            b"|", b"A", b".", b"all", b"(", b")", b"->", b"filter", b"(", b"x", b"|", b"$", b"x",
            b".", b"n", b"==",
        ];
        let (tracker, pda) = run(numeric);
        assert_eq!(
            tracker.position(pda.state()),
            L2Position::ReValue(TypeClass::Numeric)
        );
        // `$x.s ==` — s is String, so the operand is ReValue(Str).
        let string: &[&[u8]] = &[
            b"|", b"A", b".", b"all", b"(", b")", b"->", b"filter", b"(", b"x", b"|", b"$", b"x",
            b".", b"s", b"==",
        ];
        let (tracker, pda) = run(string);
        assert_eq!(
            tracker.position(pda.state()),
            L2Position::ReValue(TypeClass::Str)
        );
    }

    #[test]
    fn a_merged_closing_quote_records_the_true_column_bytes() {
        // H1: a string literal fused with its trailing `)` into one token
        // (`'ab')`) must still record the byte-exact content `ab` in the emitted
        // set — not the garbage `'ab')` the whole-token `unquote` produced. The
        // buried `)` must also fire `on_close` (the filter paren balances).
        let tokens: &[&[u8]] = &[
            b"|", b"A", b".", b"all", b"(", b")", b"->", b"filter", b"(", b"x", b"|", b"$", b"x",
            b".", b"s", b"==", b"'ab')",
        ];
        let (tracker, pda) = run(tokens);
        assert_eq!(
            tracker.emitted_columns(),
            [b"ab".to_vec()],
            "the merged closing quote records `ab`, byte-exact"
        );
        // The `)` buried in the token closed the filter paren: back at top level.
        assert_eq!(pda.state(), State::AfterValue);
        assert!(pda.stack_top().is_none(), "the filter paren is closed");
    }

    #[test]
    fn a_doubled_quote_in_a_merged_close_undoubles_byte_exact() {
        // `'a''b')` — a doubled `''` inside the literal collapses to one `'`, and
        // the trailing `)` is not part of the recorded content.
        let tokens: &[&[u8]] = &[
            b"|", b"A", b".", b"all", b"(", b")", b"->", b"filter", b"(", b"x", b"|", b"$", b"x",
            b".", b"s", b"==", b"'a''b')",
        ];
        let (tracker, _pda) = run(tokens);
        assert_eq!(tracker.emitted_columns(), [b"a'b".to_vec()]);
    }

    #[test]
    fn a_buried_navigation_dot_still_fires_member_narrowing() {
        // H2: a `.` fused to the leading identifier byte (`.n`) must still fire
        // `on_dot`, arming the member position on the bound var's class — else the
        // buried dot would silently disable N1 (pass-through) rather than narrow.
        let tokens: &[&[u8]] = &[
            b"|", b"A", b".", b"all", b"(", b")", b"->", b"filter", b"(", b"x", b"|", b"$", b"x",
            b".n",
        ];
        let (tracker, pda) = run(tokens);
        assert_eq!(pda.state(), State::InIdent, "landed mid-identifier `n`");
        assert_eq!(
            tracker.position(pda.state()),
            L2Position::Member("A".to_owned()),
            "the buried dot armed N1 on A for the buffered member"
        );
    }

    #[test]
    fn a_multi_byte_operator_swallowed_in_a_gap_is_not_split() {
        // A structural gap fusing a value's tail into `->` (`n->`, then a step) must
        // munch `->` whole (an Arrow), not a stray `>` that would read as a
        // comparison and mis-arm T1. Feeding `n->` then a fresh nav must resolve the
        // navExpr, not leave a dangling comparison arming.
        let numeric: &[&[u8]] = &[
            b"|", b"A", b".", b"all", b"(", b")", b"->", b"filter", b"(", b"x", b"|", b"$", b"x",
            b".", b"n", b"==", b"5",
        ];
        let (tracker, pda) = run(numeric);
        // After the operand `5`, T1 arming is spent; the operand position is clear.
        assert_eq!(tracker.position(pda.state()), L2Position::None);
    }

    #[test]
    fn a_comparison_without_a_resolved_navexpr_does_not_arm_t1() {
        // `take(1 ==` never resolved a primitive navExpr, so no T1 arming — the
        // operand position stays unconstrained (pass-through).
        let (tracker, pda) = run(&[b"|", b"A", b".", b"all", b"(", b")", b"->", b"filter", b"("]);
        assert_eq!(tracker.position(pda.state()), L2Position::None);
    }
}
