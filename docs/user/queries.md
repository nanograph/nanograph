---
title: Queries
slug: queries
---

# Query Language Reference

nanograph queries are defined in `.gq` files. The query language uses typed Datalog semantics with GraphQL-shaped syntax — named, parameterized queries validated against the schema at compile time.

## Design philosophy: the agent toolbelt

Each named query in a `.gq` file is a **tool**. The query language is the toolbelt. Workflows are sequences of tool calls — composed by the caller (typically an LLM agent), not by the query language itself.

This is the deliberate distinguishing wager of nanograph versus Cypher / TypeQL / SQL: those languages let humans write complex multi-stage queries because a human is the user. Nanograph's user is an agent, which is much better at *selecting and chaining simple tools* than at *constructing complex queries*. The complexity profile is intentionally inverted.

Concrete consequences for how you write `.gq` files:

- **One query = one atomic operation = one tool call.** A query body holds one read pattern or one mutation. Multi-step workflows are sequences of named queries the agent calls in order. There are no transactional bundles, no `try { }` blocks, no `assert` stages, no `WITH` clauses.

- **Mutations are single-row and idempotent where possible.** Prefer `put` (upsert by `@key`) over insert-then-update dances. Race-safety comes from `where { ... }` blocks with conjunctive filter clauses (e.g. `where { slug = $s; claimedBy is null }`) — there are no boolean operators, conjunction is structural, OR is unrepresentable by design. The result envelope (`matched_nodes`, `affected_nodes`, optional `rows`) tells the agent what happened.

- **Reads are narrow.** If a "detail" view needs the assignee, the module, the recent events, and the dependency chain, that's **four narrow queries** the agent calls in parallel — not one wide query with optional blocks. Each narrow query is trivial to reason about, trivial to retry, and trivial for an LLM to compose.

- **Use denormalized fields when they exist.** If `Issue.claimedBy: String?` already holds the assignee's slug, query that directly. Don't traverse the `Assigned` edge just to get the name — that introduces an inner-join footgun. The schema should make the common-case query a one-binding match.

- **Name queries by what they do, not by what they touch.** `claim_issue`, `assign_issue`, `record_event` — verbs the agent can match to its intent. Avoid generated `update_issue_by_id` patterns; each query is a curated tool, not a CRUD endpoint.

Example: an agent claiming an issue and recording it
```
// 1. CAS-safe atomic claim, observable outcome via matched_nodes
nanograph run claim_issue --param slug=ng-001 --param agent=alice

// 2. Idempotent edge insert
nanograph run assign_issue --param agent=alice --param issue=ng-001

// 3. Append the audit event (idempotent put by event slug)
nanograph run record_event --param ev=ev-... --param issue=ng-001 \
    --param actor=alice --param kind=claimed
```
Three tool calls. Each one is one atomic mutation. If the agent crashes between steps, retrying from the start is safe because each call is idempotent. The orchestration logic — "did the claim succeed? if so, write the edge; if so, emit the event" — lives in the agent, not in the query language.

The full design rationale, principles, and rules for what to add (and not add) to the language live in `ql-canon.md` at the repo root.

## Query structure

```graphql
query friends_of($name: String) {
    match {
        $p: Person { name: $name }
        $p knows $f
        $f.age > 25
    }
    return { $f.name, $f.age }
    order { $f.name asc }
    limit 10
}
```

Every query has:
- **Name** — identifier for `--name` when running
- **Parameters** — typed inputs passed via `--param key=value`
- **Metadata** — optional `@description("...")` and `@instruction("...")` annotations for human/agent context
- **match** — conjunctive clauses that define what to find (required)
- **return** — projections and aggregations (required for read queries)
- **order** — sort order (optional)
- **limit** — max rows (optional, required with `nearest()` or `rrf()` ordering)

### Query metadata

Queries can carry optional human/agent-facing metadata:

```graphql
query semantic_search($q: String)
    @description("Find the closest matching documents by semantic similarity.")
    @instruction("Use for conceptual search. Prefer keyword_search for exact terms.")
{
    match {
        $d: Doc
    }
    return { $d.slug, $d.title, nearest($d.embedding, $q) as score }
    order { nearest($d.embedding, $q) }
    limit 5
}
```

`nanograph run` prints this metadata before results in the human-readable `table` and `kv` outputs. It is advisory only and does not change planning or execution semantics.

Aliases from [Project Config](config.md) compose cleanly with query metadata. For example:

```bash
nanograph run search "father and son conflict"
```

In `table` or `kv` mode this prints the query name plus any `@description(...)` / `@instruction(...)` text before the result rows. `kv` groups rows into clearly separated blocks with an auto header based on `type`, `slug`, `id`, or `name` when available. In `json` mode, `nanograph run` emits a top-level object with query metadata plus `rows`. In `jsonl` mode, it emits a metadata header record first and then the row records. `csv` stays row-only. `--quiet` suppresses the human-readable preamble together with the rest of the human view.

## Parameters

Parameters are typed and prefixed with `$`:

```graphql
query find($name: String, $min_age: I32, $since: DateTime) { ... }
```

Supported parameter types: all scalar types (`String`, `I32`, `I64`, `U64`, `F32`, `F64`, `Bool`, `Date`, `DateTime`) plus `Vector(dim)`.
For exact 64-bit integer values from JS/TS SDKs, pass decimal strings when above `Number.MAX_SAFE_INTEGER`.

Pass values at runtime:

```bash
nanograph run --db my.nano --query q.gq --name find \
  --param name="Alice" --param min_age=25
```

## Match clause

The `match` block contains clauses that are implicitly conjoined (AND). All clauses must hold simultaneously — this is standard Datalog.

### Bindings

Bind a variable to a node type:

```graphql
$p: Person
$p: Person { name: "Alice" }
$p: Person { name: $n, age: 30 }
```

Property matches in braces filter at bind time. Variables (`$n`) capture values for use elsewhere.

### Edge traversal

Traverse edges using Datalog predicate syntax:

```graphql
$p knows $f
```

Direction is inferred from the schema's edge endpoint types — no arrows needed.

#### Edge naming

Schema defines edges in PascalCase, but queries use camelCase (lowercase first letter). The query parser requires edge identifiers to start with a lowercase letter.

| Schema definition | Query syntax |
|-------------------|-------------|
| `edge Knows: Person -> Person` | `$p knows $f` |
| `edge WorksAt: Person -> Company` | `$p worksAt $c` |
| `edge HasMentor: Character -> Character` | `$s hasMentor $m` |
| `edge AffiliatedWith: Character -> Faction` | `$c affiliatedWith $f` |
| `edge DebutsIn: Character -> Film` | `$c debutsIn $f` |
| `edge ClientOwnsRecord: Client -> Record` | `$c clientOwnsRecord $r` |

Using PascalCase in queries (e.g. `$s HasMentor $m`) produces a parse error.

#### Bounded expansion

Multi-hop traversal without recursion:

```graphql
$a knows{1,3} $b
```

Compiles to a finite union of 1-hop, 2-hop, and 3-hop traversals. Bounds must satisfy: min >= 1, max >= min, max is finite.

### Filters

Boolean expressions over bound variables:

```graphql
$f.age > 25
$p.name != "Bob"
$p.createdAt >= datetime("2026-01-01T00:00:00Z")
$p.updatedAt <= now()
$o.amount >= 10000.0
```

Comparison operators: `=`, `!=`, `>`, `<`, `>=`, `<=`.

### Negation

```graphql
not {
    $p worksAt $_
}
```

At least one variable in the negated block must be bound outside it. `$_` is an anonymous wildcard. Semantics: no matching tuples exist for the inner clauses.

`maybe { ... }` (left join) and `or { ... }` (disjunction) are not part of the current query grammar.

### Text search predicates

Filter predicates for text matching (see [search.md](search.md) for full details):

| Predicate | Description |
|-----------|-------------|
| `search($c.bio, $q)` | Token-based keyword match (all query tokens must be present) |
| `fuzzy($c.bio, $q)` | Approximate match (tolerates typos) |
| `match_text($c.bio, $q)` | Contiguous token match (phrase matching) |

These go in the `match` block and act as filters — a row either matches or doesn't.

## Return clause

Projections define what columns appear in the output:

```graphql
return {
    $f.name
    $f.age
    $c.name as company
    count($f) as num_friends
}
```

Use `as` to alias columns.

### Aggregation functions

| Function | Description | Type requirement |
|----------|-------------|------------------|
| `count($var)` | Count of bound variable | Any bound variable |
| `sum($var.prop)` | Sum | Numeric type |
| `avg($var.prop)` | Average | Numeric type |
| `min($var.prop)` | Minimum | Numeric or Date/DateTime |
| `max($var.prop)` | Maximum | Numeric or Date/DateTime |

### Scoring functions in return

`bm25()`, `nearest()`, and `rrf()` can be projected to inspect scores:

```graphql
return {
    $c.slug,
    bm25($c.bio, $q) as lexical_score,
    nearest($c.embedding, $q) as semantic_distance,
    rrf(nearest($c.embedding, $q), bm25($c.bio, $q)) as hybrid_score
}
```

## Order clause

```graphql
order { $f.age desc }
order { $p.name asc, $p.age desc }
```

Default direction is ascending. Ordering expressions:

| Expression | Direction | Meaning |
|------------|-----------|---------|
| `$p.name asc` | Ascending | Alphabetical / numeric ascending |
| `$p.age desc` | Descending | Numeric descending |
| `nearest($c.embedding, $q)` | Ascending (lower = closer) | Cosine distance ordering |
| `bm25($c.bio, $q) desc` | Descending (higher = better) | BM25 relevance ordering |
| `rrf(..., ...) desc` | Descending (higher = better) | Hybrid fusion ordering |

`nearest()` ordering requires a `limit` clause. See [search.md](search.md) for score interpretation.

## Limit clause

```graphql
limit 10
```

Required when using `nearest()` or `rrf()` in the order clause.

## Literals

| Literal | Example |
|---------|---------|
| String | `"Alice"` |
| Integer | `42` |
| Float | `3.14` |
| Boolean | `true`, `false` |
| Date | `date("2026-01-15")` |
| DateTime | `datetime("2026-01-15T10:00:00Z")` |
| List | `[1, 2, 3]`, `["a", "b"]` |

### Built-in runtime values

| Expression | Type | Description |
|------------|------|-------------|
| `now()` | `DateTime` | Current UTC timestamp, resolved once per query execution |

`now()` can be used in filters, projections, bindings, and mutation assignments/predicates:

```graphql
query recently_updated() {
    match {
        $p: Person
        $p.updatedAt <= now()
    }
    return { $p.name, now() as queried_at }
}
```

## Mutations

Mutation queries modify graph data. They use the same `query` wrapper but contain `insert`, `update`, or `delete` instead of `match`/`return`.

### Insert

Append a new node:

```graphql
query add_person($name: String, $age: I32) {
    insert Person { name: $name, age: $age }
}
```

Set a server-side timestamp during mutation:

```graphql
query touch_person($name: String) {
    update Person set { updatedAt: now() } where name = $name
}
```

Insert an edge (endpoints resolved by `@key`):

```graphql
query add_edge($from: String, $to: String) {
    insert Knows { from: $from, to: $to }
}
```

### Update

Update by `@key` (merge semantics — requires the node type to have a `@key` property):

```graphql
query advance_stage() {
    update Opportunity set {
        stage: "won"
        closedAt: datetime("2026-02-14T00:00:00Z")
    } where slug = "opp-stripe-migration"
}
```

### Delete

Delete nodes matching a predicate. Edges where the deleted node is an endpoint are automatically cascaded:

```graphql
query remove_cancelled() {
    delete ActionItem where slug = "ai-draft-proposal"
}
```

## Comments

```graphql
// Line comment

/* Block comment */
```

## Reserved words

The parser treats many terms as contextual keywords. Avoid using them as identifiers in queries.

| Category | Keywords |
|----------|----------|
| Query structure | `query`, `match`, `return`, `order`, `limit` |
| Clauses | `not`, `as` |
| Mutations | `insert`, `update`, `delete`, `set`, `where` |
| Search / ranking | `search`, `fuzzy`, `match_text`, `bm25`, `rrf` |
| Ordering | `asc`, `desc` |
| Aggregation | `count`, `sum`, `avg`, `min`, `max` |
| Date literals | `date`, `datetime` |

`true` and `false` are literal keywords.

## Type rules

The compiler enforces these rules during `nanograph lint`:

| Rule | What it checks |
|------|----------------|
| T1 | Binding type must exist in schema |
| T2 | Property match fields must exist on the bound type |
| T3 | Property match values must match declared types |
| T4 | Traversal edge must exist in schema |
| T5 | Traversal endpoints must match edge declaration |
| T6 | Property access must reference a property on the bound variable's type |
| T7 | Comparison operands must have compatible types |
| T8 | Aggregations must wrap valid expressions (e.g. `sum` requires numeric) |
| T9 | `not` requires at least one externally-bound variable |
| T10 | Mutation shape errors (unknown mutation target, missing assignments) |
| T11 | Mutation property references must exist on the target type |
| T12 | Insert mutations must provide required fields/endpoints (`@embed` targets may be omitted when source is provided) |
| T13 | Duplicate mutation assignments are rejected |
| T14 | Mutation variable values must be declared query parameters |
| T15 | Traversal bounds validation and `nearest()` type/dimension validation |
| T16 | `update` for edge types is not supported |
| T17 | `nearest()` ordering requires a limit clause |
| T18 | Alias-based ordering is restricted when standalone `nearest()` ordering is present |
| T19 | `search()` / `fuzzy()` args must be String |
| T20 | `match_text()` / `bm25()` args must be String |
| T21 | `rrf()` args must be `nearest()` or `bm25()`; optional k must be integer > 0; requires limit |
