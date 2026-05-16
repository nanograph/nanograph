# nanograph query language canon

This document defines what nanograph's query language is, the principles that keep it coherent, and the rules for evolving it. It is not a tutorial or a reference. It is a constitution for design decisions: when someone proposes a feature, this is the document we test it against.

## Identity

A nanograph query is a **typed function over a typed graph**. Its body is a single pattern. Its result is a flat table. Search and traversal are first-class operations within the pattern; mutations are pattern-gated row writes. Every query is a named, parameterized callable.

The `.gq` file is the typed API surface of a database. For a human, each named query is a saved view. **For an LLM agent, each named query is a tool.** The terseness, the static checkability, the `@description` and `@instruction` metadata, the predictable flat result envelope — these all serve the agent-as-consumer case. Designing the language means designing what an agent can call.

This identity is what we are protecting.

We are not Cypher (no procedural pipelines, no path expressions, no graph-shaped output). We are not SQL (no general expression language, no CTEs, no subqueries, no `UNION`). We are not GraphQL (no nested input objects, no tree-shaped projections, no auto-generated CRUD). We are not TypeQL (no multi-stage pipelines, no in-language transactional bundles) or HelixQL (no multi-binding query bodies). We are closest to a typed Datalog with batteries — pattern matching, search predicates, and tabular projection — narrowed deliberately to a shape an agent can pick up and use as a tool.

## Principles

These are the load-bearing decisions. They take precedence over any feature.

### P1. The pattern is the program

A read query body is a single declarative pattern over the graph. There is no imperative control flow, no `WITH … RETURN` pipeline, no temporary results, no recursion (yet), no nested queries. The user describes a shape; the engine finds it. Compute happens elsewhere — in the schema (denormalized properties, materialized embeddings, indexes), in the projection (aggregations, search rankings), or in client code. Never in a procedural script inside the query.

### P2. Types are the contract

Every variable is typed against the catalog. Every property access is checked at lint time. Every parameter has a declared type. There is no `*`-style wildcard projection, no untyped property bag, no escape into raw JSON. The schema and the queries co-evolve; `nanograph lint` catches every structural error before runtime.

### P3. One query = one named, atomic operation

A query is a named function. Calling it is one logical operation. The unit of composition for users is the named query, not the statement. Today this means one read pattern XOR one mutation per body. When multi-statement transactions land, the named-query boundary remains the atomic unit — the body may grow, the unit will not.

### P4. Flat in, flat out

Inputs are scalars, vectors, and lists of scalars. Outputs are flat rows of typed columns. There are no nested input objects, no tree-shaped responses, no graph-shaped outputs. When a result needs structure, the answer is a second named query — not a richer projection language.

### P5. Search is a first-class operator, not a parallel language

`search`, `fuzzy`, `match_text`, `bm25`, `nearest`, `rrf` are functions in the same expression namespace as comparisons. There is no separate "search query" mode, no vector-DB sidecar dialect, no `MATCH AGAINST` clause. Hybrid search is composable function calls. Adding a new search modality is a new function, not a new clause.

### P6. The grammar mirrors the planner

The clause order `match → return → order? → limit?` is the executor's natural pipeline (scan → filter → join → project → sort → limit). Each clause has a place because it has work to do at that stage. New clauses must slot into this pipeline; reordering or non-linear composition is not on the table.

### P7. Mutations are gated row writes

A mutation specifies (a) what type to touch, (b) what fields to set, (c) which rows to touch via a predicate gate. Mutations do not traverse the graph, do not join, do not embed read patterns. The predicate is the gate. Writes are row-level: one mutation = one type = one gate = one write spec.

### P8. The agent orchestrates; the QL is the toolbelt

nanograph queries are designed to be called by an LLM agent as tools, not composed in-language as pipelines. Atomic operations stop at the query boundary. Multi-step workflows live in the agent's planning loop, not in the language. This is what differentiates nanograph from TypeDB, HelixDB, Cypher, and SQL — their target user is a human writing complex queries; ours is a machine selecting and chaining simple ones.

Consequences that take precedence over feature requests:

- Mutations are single-row, single-table, idempotent where possible. `put` (upsert) is the agent's primary mutation verb.
- Race-safety is achieved with single-row CAS (compound `where` + `IS NULL`), not with locks, stage assertions, or transactional bundles.
- Results are observable from the response envelope alone (`matched_nodes`, `affected_nodes`, optional `rows`) — no separate inspection round-trip required.
- A workflow that needs three writes is three named queries the agent calls, not one query body with three stages.
- A read that "needs context" is multiple narrow queries the agent composes, not one wide query with optional blocks.

P8 is the principle most likely to be tested by future feature requests, because in-language orchestration is the dominant pattern in other graph DBs. Resist it: nanograph's distinguishing wager is that *the orchestration loop is the LLM's job*, not the engine's.

## What we have today

A reference catalog of patterns, organized by principle. New features should look like these.

### Read pattern (P1, P6)

```
query open_high_priority()
  @description("Open issues at priority 0 or 1.")
{
    match {
        $i: Issue { status: "open" }
        $i.priority < 2
    }
    return { $i.slug, $i.title, $i.priority }
    order { $i.priority }
    limit 20
}
```

### Traversal (P1)

```
$a expert $m            // direction inferred from schema endpoints
$b blocks{1,3} $i       // bounded transitive
not { $b blocks $i }    // negation as antijoin
```

### Search expressions (P5)

```
search($n.body, $q)                              // FTS predicate (in match)
nearest($n.embedding, $q) as score               // ranking (in order or return)
rrf(nearest($n.embedding, $q),
    bm25($n.body, $q)) as hybrid_score           // hybrid composition
```

### Mutations (P7)

```
insert Issue { slug: $s, title: $t, status: "open", ... }
update Issue set { status: "closed" } where slug = $s
delete Event where slug = $s
```

### Result envelope (P4)

- Read: a flat table of typed columns. JSON modes carry `name`, `description`, `instruction` plus the row array.
- Mutation: `{ affected_nodes, affected_edges }` (will gain `matched_nodes` and optional `returning` when CAS and RETURNING land).

## Acknowledged tensions

These are real ambiguities in the current language. The canon names them so future evolution resolves them deliberately, not by accident.

### Implicit grouping for aggregations

`count($x) as foo` alongside non-aggregate columns currently triggers undocumented grouping (and silently returns empty in some cases — see the codebase example's `module_backlog`). **Pick one**: either (a) implicit grouping by all non-aggregate projection columns is the documented contract and is fixed to actually work, or (b) explicit `group { $m.slug }` is added and implicit grouping becomes a lint error. Aggregation must not stay a footgun.

### `nearest()` polymorphism

`nearest()` appears in `expr` (so projection and ordering) but not as a top-level filter. It produces a similarity score. The dual role is fine; document it. **`nearest()` is a ranking function, not a predicate.** Conversely, the search predicates (`search`, `fuzzy`, `match_text`) are predicates, not rankings.

### Mutation predicate strength

Today: single property = value. The chosen extension is the **`where { ... }` block** — a brace-delimited list of newline-separated filter clauses, structurally symmetric with `match { ... }`. Conjunction is implicit in the block; there are no boolean operators (`AND`, `OR`, `NOT`) at the predicate level. This makes disjunction *unrepresentable in the grammar*, not just discouraged — the R3 first-of-family worry about boolean expression languages is eliminated at the syntax level. Single-predicate inline (`where slug = $s`) remains legal as sugar for `where { slug = $s }`. Pattern-as-gate (using a leading `match` clause as the CAS condition) is **rejected** — it requires multi-statement mutation bodies, which violates P3 and P8.

### Update may match many rows

The grammar permits `update Issue set {...} where status = "open"` which would update many rows. CLAUDE.md says "update requires @key, uses merge" but typecheck does not enforce it. **Resolve by**: explicitly support multi-row updates as a feature; document; rely on `matched_nodes` in the result envelope to surface the row count.

### Edge endpoints are typed strings

`from`/`to` on edges are `String` (the @key of the endpoint type). There is no foreign-key reference type. **This is intentional**: endpoints are content-addressable via the @key string. We will not introduce a typed reference.

### Multi-statement query bodies

A recurring temptation, especially when porting examples from TypeQL/HelixQL/Cypher: should a query body be allowed to contain a sequence of mutations and reads that commit atomically? **Resolved: no.** Per P3 and P8, one query = one named atomic operation = one tool call. Multi-step workflows are sequences of named queries the agent invokes. The single-statement form keeps each query simple to reason about, simple to retry, and simple for an LLM to select. See the anti-patterns table for the full rejection rationale.

## Evolution rules

Apply these in order to any feature proposal. If any returns "no," the feature in its proposed form is rejected.

### R1. Which principle does it serve?

For each P1–P7: does the feature make the principle more or less true? Reinforces all → proceed. Fights one → reframe or reject. The principles are not negotiable.

### R2. Operator/function, or new clause?

Operators and functions (`IS NULL`, `IN`, `OFFSET`, `DISTINCT`, `starts_with`) compose with existing grammar without changing the clause set. **Strongly prefer them.**

New clauses (`group {}`, `optional {}`) change the pipeline shape. They are heavier; each needs to justify itself against P6.

### R3. First-of-family check

Some features look small but are gateways:

- `+` arithmetic → expression language
- `OR` between predicates → boolean expression precedence
- `WITH x AS …` → procedural composition
- `CASE WHEN` → conditionals
- subqueries → nested compute
- inline functions → general computation
- a second statement in one query body → in-language orchestration (transactional bundles, cross-stage bindings, assertions, optional blocks)

If the proposal is the first instance of one of these families, **reject the first instance**. The second and third are inevitable; either commit to the whole family (and accept the language becomes something else), or live without.

### R4. Can it move out of the query?

If the use case can be served by:

- a schema change (new property, new index, denormalization)
- a second named query the agent calls in sequence (per P8)
- client-side or agent-loop post-processing

…prefer that. The query language is for shape-finding and single-row mutations, not for orchestration or general computation.

### R5. Does it preserve symmetry?

The language has design symmetries that should not be broken without reason:

- read body shape mirrors planner stages (scan → filter → join → project → sort → limit)
- mutation has insert / update / delete / (upsert) — all single-type, all gated
- search has predicate / rank / hybrid forms
- every property type has scalar / nullable / list / vector forms

A feature that breaks a symmetry must justify the asymmetry.

## Backlog classification

The gap list from the language review, sorted by canon compatibility.

### Tier A — Reinforcing (ship without architectural worry)

| Feature | Shape |
|---|---|
| `put` (upsert by @key) | Fourth mutation form, parallel to insert/update/delete. The agent's primary mutation verb. |
| `where { }` block in mutations | Newline-separated conjunctive filters, mirrors `match { }`. Reuses existing filter grammar. Conjunction is structural — no boolean operators introduced. Enables single-row CAS. |
| `IS NULL` / `IS NOT NULL` | Predicate operator. Pairs with `where { }` blocks for CAS gates. Also legal as a filter in `match`. |
| `matched_nodes` in mutation result | Preserves flat envelope, gives agents the observable CAS signal. |
| `RETURNING` on single-statement mutations | Preserves flat-out (P4). Mutation result envelope gains optional `rows`. |
| `IN` with list-typed parameters | Operator + parameter type (closes the existing list-literal asymmetry). |
| `OFFSET N` | Clause; slots cleanly into pipeline. |
| `DISTINCT` | Projection modifier. |
| Recursive traversal (`{1,*}`) | Extends bounded `{1,3}`. |
| Time-travel / `as_of` | Query annotation, not body change. |
| `starts_with` predicate | Second basic string predicate; **stop here**. |

### Tier B — Reframing required (do the design work)

| Feature | The reframe |
|---|---|
| Aggregation grouping | Resolve the tension above: either document implicit grouping precisely and fix it, or add an explicit `group { … }` clause. **No HAVING.** |
| Path materialization | Introduce `path` as a typed projection-only value. No path expressions in match; the engine still does the traversal. |

### Tier C — Risk Frankenstein (reject by default)

| Feature | Why rejected |
|---|---|
| **Multi-statement query bodies** | Procedural composition lives in the agent (P8). One query = one named atomic operation (P3). |
| **Cross-statement bindings** | Same. Bindings die at the query boundary. |
| **`try { }` / `optional { }` blocks** | A wide query with optionals is the wrong shape — write narrower queries and let the agent compose. Adds nullable-column complexity to a flat-out language (P4). |
| **`assert` / fail-loud stage guards** | The result envelope (`matched_nodes`) is the observability signal. Stage assertions presuppose multi-stage bodies. |
| **Insert-from-pattern** (`insert Foo from match {…}`) | Conflates read and write; requires multi-statement semantics; agent can iterate match results client-side and call `put` per row. |
| Cross-clause `OR` | Door to boolean expression precedence (R3). `IN` covers same-property OR. |
| `HAVING` (post-aggregate filter) | Pulls SQL aggregation model behind it. Filter in client code or in a second query. |
| Subqueries / `EXISTS` in match | Nesting door (R3). The pattern itself is the existence check. |
| Cross-query bindings, prepared statements, session state | Procedural composition by the back door (P3, P8). |
| String ops beyond `contains` and one prefix predicate | Slippery; once `LIKE` lands, regex follows. |
| Math/string functions in expressions (`UPPER`, `+`, `||`) | First-of-family (R3); never. |
| `CASE WHEN` / inline conditionals | First-of-family (R3); never. |
| Object-shaped or graph-shaped projections | Breaks P4. Use a second named query. |
| Auto-generated mutations per type (Hasura model) | Breaks the named-tool catalog model (P8). |
| `*` projection / untyped result column | Breaks P2. |
| Stored procedures / server-side scripting | Breaks P3 / P1 / P8. |

## Anti-patterns and why

| Anti-pattern | The objection |
|---|---|
| Multi-statement query bodies (sequence of stages) | Procedural composition lives in the agent (P8). Each query is one tool call; chained workflows are sequences of tool calls, not pipelines. |
| Cross-statement variable bindings | Same — bindings die at the query boundary. |
| `try { }` / `optional { }` blocks | Adds nullable-column complexity to a flat-out language (P4). Write narrower queries; the agent calls multiple. |
| `assert` / fail-loud stage guards | The result envelope is the observability signal — `matched_nodes: 0` already tells the agent the CAS lost. Stage assertions presuppose multi-statement bodies. |
| Insert-from-pattern (`insert Foo from match {…}`) | Conflates read and write; agent should iterate match results client-side and call `put` per row. |
| Arithmetic / string functions in expressions | Once `LOWER()` exists, `SUBSTR`, `CONCAT`, `REGEXP_REPLACE` follow. The query language is not a programming language. |
| Boolean operators (`AND`, `OR`, `NOT`, parens, precedence) in predicates | The `where { }` block uses conjunctive structure with no boolean operators — OR is *unrepresentable*, not just rejected. Adding boolean operators would require a parallel expression language. `IN` covers same-property OR. |
| Subqueries returning scalars | Requires an expression language to consume them. |
| `WITH` / CTEs | Procedural composition. The named-query model is our composition tool. |
| `HAVING` | Imports the full SQL aggregation model. |
| Object-shaped or graph-shaped result | The flat-table contract is what makes results trivially serializable, indexable, joinable in client code, and consumable by agents. |
| Auto-generated mutations per type | Hasura ships 12 mutations per table; agents can't tell which to call. Named queries are intentional and discoverable. |
| `*` projection / untyped result column | Breaks P2; lint can't catch downstream consumers. |
| Stored procedures / server-side scripting | Procedural composition by the back door (P3, P8). |

## The six-question checklist

Before merging any language change:

1. Which of P1–P7 does it reinforce? Which (if any) does it fight?
2. Is it an operator/function (Tier A) or a new clause (Tier B)? If Tier B, what justifies the new pipeline stage?
3. Is it the first instance of an anti-pattern family (R3)? If so, reject.
4. Could the use case be served by a schema change, a second named query, or client code (R4)?
5. Is there a parallel construct in the language this feature should be symmetric with (R5)?
6. After this change, does someone reading a `.gq` file still see the same language?

If all six come back clean, the feature is canon-compatible.

## Changing the canon itself

This document evolves more slowly than the language. A canon change is a design pivot, not a feature addition; treat it accordingly. Changes here require:

1. A worked example showing why the existing canon fails on a real workload.
2. The proposed revision in full (no patches; replace the whole section).
3. A walk-through of which existing language features become inconsistent under the revision and how they will be reconciled.
