# PureCard Spec — L1 Grammar

_[Spec index](README.md) · [domain model](../domain-model.md)_

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
