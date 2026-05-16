# nanograph QL Update — agent-toolbelt mutations

A design proposal for the next round of query-language changes. Companion to `ql-canon.md`.

## Executive summary

Add five small operator-level features to make nanograph's QL a complete agent toolbelt for single-row atomic mutations. The whole batch:

1. `put` (upsert by `@key`) — fourth mutation form
2. `where { ... }` block in mutations (conjunctive by structure, no boolean operators)
3. `IS NULL` / `IS NOT NULL` predicate operator
4. `matched_nodes` field in `MutationResult`
5. `returning` clause on single-statement mutations

Plus three read-side paper cuts: `OFFSET`, `IN` with list-typed parameters, `DISTINCT`. Plus a bug fix: aggregation grouping.

**Total cost**: ~900 LOC across grammar, AST, IR, planner, executor, docs. 4–6 working days. Two PRs (plus an optional third for aggregation).

**What this gives the agent**:
- An idempotent "ensure this exists" verb (`put`).
- A race-safe atomic-claim primitive in one query (compound `where` + `IS NULL`).
- Observable outcomes via `matched_nodes` — the agent can tell from the response envelope whether its CAS won, the row was missing, or it was a no-op.
- An optional `returning` clause so a mutation can hand back the new state without a follow-up read.
- Bulk targeted reads (`IN`), pagination (`OFFSET`), and dedup (`DISTINCT`).

**What this explicitly is not**: a pipeline architecture, transactional bundles, optional matches, or cross-statement bindings. Per `ql-canon.md` P8 ("the agent orchestrates; the QL is the toolbelt"), multi-step workflows live in the agent's planning loop, not in the query language.

**Backward compatibility**: zero breakage. Every existing query parses and executes identically.

## Why this shape (and not pipelines)

An earlier draft of this document proposed a pipeline architecture — query bodies as sequences of `match → mutation+ → return?` stages with cross-stage bindings, `try { }` optional blocks, and `assert` guards. That proposal was rejected and the reasons are recorded in `ql-canon.md` P8 and in the Tier C anti-patterns table. Briefly:

- Nanograph's user is an LLM agent. The agent's strength is *selecting and chaining simple tool calls*; its weakness is constructing complex multi-stage queries. The complexity profile is inverted from TypeDB/HelixDB/Cypher.
- Embedded queries are sub-millisecond. Round-trip cost between calls is irrelevant compared to LLM token-generation latency.
- Multi-statement query bodies are the *first instance* of an in-language orchestration family (CTEs, stored procedures, transactional bundles, scripting). The canon's R3 rejects first-instances of anti-pattern families.
- Single-row mutations + observable results give the agent all the primitives it needs to orchestrate workflows itself.

This proposal stays inside those guardrails.

## The five mutation features

### 1. `put` (upsert by `@key`)

**Syntax**: identical to `insert`, new keyword.
```
put Issue {
    slug: $s,
    title: $t,
    status: "open",
    createdAt: now(),
    updatedAt: now()
}
```

**Semantics**:
- The `@key` value (here, `slug: $s`) is the gate.
- Row exists → update the named non-key properties on that row.
- Row doesn't exist → insert with all named properties.
- Always one row affected. `MutationResult { matched_nodes: 1, affected_nodes: 1 }`.

**Edge form**:
```
put Assigned { from: $a, to: $i }                          // idempotent, no props
put Expert   { from: $a, to: $m, confidence: "high" }      // updates confidence on conflict
```
The (from, to) pair is the natural key.

**Typecheck (T22)**:
- For node `put`: type must have `@key`; @key property must be in the assignments; all non-nullable non-key properties must be in the assignments (the insert path needs them).
- For edge `put`: `from` and `to` required; non-nullable edge properties required.

**Why this fits the agent toolbelt**: idempotent retries. Agent calls `put Issue {...}` and gets the same result whether the row existed or not. The agent's loop simplifies — no try/insert/catch-duplicate dance.

### 2. `where { }` block in mutations (conjunctive by structure)

**Syntax**: extend mutation `where` to accept a brace-delimited block of filter clauses, structurally symmetric with `match { ... }`. Single-predicate inline form remains legal as sugar.

```
// One predicate — current syntax still works
update Issue set { status: "closed" } where slug = $s

// Multiple predicates — new block form
update Issue set { claimedBy: $a, status: "in_progress", updatedAt: now() }
where {
    slug = $s
    claimedBy is null
}

// Range + key example
delete Event where {
    issueSlug = $s
    at < $cutoff
}
```

**Grammar diff**:
```diff
- mutation_predicate = { ident ~ comp_op ~ match_value }
+ mutation_predicate = { mutation_pred_block | mutation_pred_atom }
+ mutation_pred_block = { "{" ~ mutation_pred_atom+ ~ "}" }
+ mutation_pred_atom = {
+     ident ~ comp_op ~ match_value
+   | ident ~ "IS" ~ "NULL"
+   | ident ~ "IS" ~ "NOT" ~ "NULL"
+ }
```

**Semantics**: all atoms in the block must hold. Clauses inside the block are the same `filter` grammar already used inside `match`, just without the variable-prefix (single target type — `slug` not `$i.slug`).

**Why this shape over an `AND` keyword**:
- **No new keyword.** Conjunction is implicit in the block structure, exactly as in `match { ... }`.
- **OR is unrepresentable at the grammar level**, not just rejected by convention. There's no operator to combine clauses, only clauses themselves. The R3 first-of-family worry about boolean expression languages is eliminated structurally.
- **Symmetric with `match`.** Anyone who understands `match { ... }` understands `where { ... }`.
- **Backward compatible**: `where slug = $s` parses identically; new block form is purely additive.

**Why this fits the agent toolbelt**: race-safe single-row CAS in one query, observable outcome via `matched_nodes`.

### 3. `IS NULL` / `IS NOT NULL` predicate operator

Shipped together with #2. The same atom shape (`ident IS [NOT] NULL`) is also legal in read-side `filter` clauses inside `match`:
```
match {
    $i: Issue
    $i.claimedBy IS NULL
}
return { $i.slug, $i.title }
```

### 4. `matched_nodes` in `MutationResult`

```rust
pub struct MutationResult {
    pub affected_nodes: usize,
    pub affected_edges: usize,
    pub matched_nodes: usize,           // NEW
}
```

Disambiguates "predicate matched 0 rows" from "row exists but write didn't change anything." Required for CAS to be observable — without it, `affected_nodes: 0` is silent failure.

JSON envelope:
```json
{
  "matched_nodes": 0,
  "affected_nodes": 0,
  "affected_edges": 0
}
```

Agent reads `matched_nodes == 0` → "my CAS lost or the row doesn't exist; decide next step."

### 5. `returning` clause on single-statement mutations

**Syntax**: optional clause after `where`.
```
update Issue set {
    claimedBy: $a, status: "in_progress", updatedAt: now()
} where {
    slug = $s
    claimedBy is null
}
returning { $i.slug, $i.status, $i.claimedBy, $i.updatedAt }
```

**Semantics**: post-mutation state of every matched-and-affected row, projected through the `returning` clause. Result envelope gains an optional `rows` field:
```json
{
  "matched_nodes": 1,
  "affected_nodes": 1,
  "rows": [{"slug": "ng-001", "status": "in_progress", "claimedBy": "alice", "updatedAt": "..."}]
}
```

**Variable binding**: the `returning` clause uses the same flat projection grammar as `return`. The bound variable is the type being mutated (here `$i` referring to the Issue rows touched). Convention: the implicit binding name is `$<lowercase first letter of type>` if not explicitly named; can be changed if there's a preference.

**Why this fits**: saves one round-trip without violating single-statement atomicity. Stays inside one Lance commit.

## The three read-side paper cuts

### `OFFSET N`
```
order { $i.priority }
limit 10
offset 20
```
Pure pagination addition. Slots into the existing read pipeline as another tail stage.

### `IN` with list-typed parameters
```
query bulk_inspect($slugs: [String]) {
    match { $i: Issue; $i.slug IN $slugs }
    return { $i.slug, $i.status }
}
```
Two changes:
- Parameter type system accepts `[String]`, `[I32]`, etc. (the list-literal grammar already exists; this closes the asymmetry).
- `IN` predicate operator in filter expressions and (optionally) in mutation `where` atoms.

### `DISTINCT`
```
return distinct { $f.country }
```
Projection modifier. Planner already has DataFusion `DISTINCT` support; just a syntax surface.

## The aggregation bug fix

Today `count($x) as foo` with non-aggregate columns silently returns 0 rows in some cases (see codebase example's `module_backlog`). Two options:

- **Option A**: document and fix implicit grouping. Group keys = all non-aggregate columns in `return`. Make this actually work in the executor.
- **Option B**: add an explicit `group { $m.slug }` clause between `return` and `order`. Implicit grouping becomes a lint error.

**Recommended**: A first (smaller, no grammar change). If implicit semantics turn out to be subtly wrong, fall back to B.

Either way, this is a separate PR — can land before, during, or after the rest.

## Worked examples

### Idempotent claim with CAS

```
query claim_issue($slug: String, $agent: String) {
    update Issue set {
        claimedBy: $agent,
        status: "in_progress",
        updatedAt: now()
    } where {
        slug = $slug
        claimedBy is null
    }
    returning { $i.slug, $i.status, $i.claimedBy, $i.updatedAt }
}
```

Agent calls. Three observable outcomes from one response envelope:
- `matched_nodes: 1, affected_nodes: 1, rows: [{...}]` → won the claim, see new state.
- `matched_nodes: 0, affected_nodes: 0, rows: []` → row missing or already claimed. Agent decides whether to retry, escalate, or give up.

### Idempotent edge insertion

```
query assign_issue($agent: String, $issue: String) {
    put Assigned { from: $agent, to: $issue }
}
```

Calling twice is safe. Agent doesn't need to check whether the edge exists first.

### Atomic state transition with gate

```
query close_issue($slug: String, $reason: String) {
    update Issue set {
        status: "closed",
        closedAt: now(),
        closedReason: $reason,
        updatedAt: now()
    } where {
        slug = $slug
        status != "closed"
    }
    returning { $i.slug, $i.status, $i.closedAt }
}
```

Preserves the original `closedAt` if called twice on an already-closed issue (predicate excludes already-closed rows).

### Bulk-targeted prune

```
query prune_old_events($since: DateTime, $issue: String) {
    delete Event where {
        issueSlug = $issue
        at < $since
    }
}
```

Before the `where { }` block this required either deleting events one by one or a CLI-level workaround. Now: one query.

## How the codebase example bugs get fixed

The codebase example's headline lifecycle bugs (`detail`, `my`, `active`, `history` returning wrong results after a runtime claim) get fixed by **rewriting the queries**, not the language. The language additions above make the rewrite possible.

### `claim_issue` — idempotent + race-safe

Today's broken version:
```
update Issue set { claimedBy: $a, status: "in_progress", updatedAt: now() }
where slug = $s
```

After:
```
update Issue set { claimedBy: $a, status: "in_progress", updatedAt: now() }
where {
    slug = $s
    claimedBy is null
}
returning { $i.slug, $i.status, $i.claimedBy }
```

### `assign_issue`, `record_event` — separate idempotent calls

```
query assign_issue($a, $i) { put Assigned { from: $a, to: $i } }

query record_event($ev, $i, $actor, $kind) {
    put Event { slug: $ev, issueSlug: $i, actor: $actor,
                kind: $kind, payload: "{}", at: now() }
}

query records_event_edge($ev, $i) { put Records { from: $ev, to: $i } }
```

Agent's claim workflow becomes four named tool calls:
1. `claim_issue` — atomic CAS; if `matched_nodes: 0`, abort.
2. `assign_issue` — idempotent edge.
3. `record_event` — idempotent event.
4. `records_event_edge` — idempotent audit-trail edge.

If the agent crashes between any two, retrying from the start is safe (every call after #1 is idempotent). The audit log might be incomplete on crash — that's the trade-off captured by P8: nanograph isn't the right tool for workloads needing strict cross-mutation atomicity.

### `issue_detail` — use the denormalized field, not the edge

Today's broken version inner-joins on `Assigned` and returns empty for unassigned issues. The fix is querying-side:
```
query issue_detail($slug: String) {
    match { $i: Issue { slug: $slug } }
    return {
        $i.slug, $i.title, $i.status, $i.priority,
        $i.description, $i.acceptance, $i.design,
        $i.claimedBy as assignee        // already denormalized on Issue
    }
}
```

For richer assignee data (Agent's name, capabilities), a separate narrow query:
```
query issue_assignee($slug: String) {
    match {
        $i: Issue { slug: $slug }
        $a: Agent { slug: $i.claimedBy }   // requires future syntax support
    }
    return { $a.slug, $a.name, $a.kind, $a.capabilities }
}
```

(Note: the property-equality binding `{ slug: $i.claimedBy }` already works today via match-value variable references. The agent calls `issue_detail` first to get `claimedBy`, then `issue_assignee` if needed.)

Same pattern for `issue_module` — a narrow query the agent calls when it needs the module.

## Open design questions

Three small calls. My recommendation in each.

### Q1. `null` literal in `prop_match` braces

The `IS NULL` operator covers null-checks inside `where { }` blocks. Should we also allow `null` as a value inside binding braces?

```
match { $i: Issue { slug: $s, claimedBy: null } }
```

**Recommendation**: yes — small grammar add, makes "find unclaimed issues" a one-line match. Trivially symmetric with the `IS NULL` operator.

### Q2. `returning` variable name convention

```
update Issue set {...} where slug = $s
returning { $i.slug, $i.status }    // implicit binding name?
```

Options:
- A) Implicit binding `$i` (lowercase first letter of the type name).
- B) Explicit binding: `update Issue as $i set {...} where ... returning { $i.slug }`.
- C) Always `$row`, `$it`, or some fixed name.

**Recommendation**: A. Predictable, terse, matches what people would type anyway. Document the rule explicitly.

### Q3. `IN` list-parameter type form

Two ways to declare a list parameter:
- A) `$slugs: [String]` — matches the existing list literal syntax.
- B) `$slugs: List<String>` — more GraphQL-ish.

**Recommendation**: A. The list-literal syntax already uses `[...]`; symmetric.

## Implementation plan

### Two reviewable PRs (plus an optional third)

#### PR-1: agent-toolbelt mutations (~500 LOC, 2–3 days)

- `put` keyword + `put_stmt` grammar rule + typecheck T22.
- `mutation_predicate` extended to `where { }` block (brace-delimited list of `mutation_pred_atom` clauses, conjunctive by structure).
- `IS NULL` / `IS NOT NULL` available as both a `mutation_pred_atom` and a `filter` clause in `match`.
- `MutationResult.matched_nodes` added; populated in executor; serialized in JSON envelope.
- Lint accepts `null` in `prop_match` (Q1).
- ~10–15 new unit tests; one CAS race-condition integration test.

After PR-1 ships: race-safe claims work end-to-end. `put` works. The headline codebase-example bug becomes fixable by rewriting `claim_issue`.

#### PR-2: read-side polish (~400 LOC, 2–3 days)

- `returning` clause on insert/update/delete/put (Q2 binding convention).
- `OFFSET N` clause after `limit`.
- `IN` predicate operator + list parameter types (`[String]`, `[I32]`, etc.) (Q3).
- `DISTINCT` projection modifier.
- Result envelope gains optional `rows` field.

#### PR-3 (optional, separate): aggregation fix

- Either fix implicit grouping or add explicit `group { }` clause.
- Independent from the other PRs.

### Calendar

| Phase | Effort | Dependencies |
|---|---|---|
| PR-1 | 2–3 days | none |
| PR-2 | 2–3 days | PR-1 (uses the new mutation grammar foundation) |
| PR-3 | 1–2 days | independent |
| **Total** | **4–6 days** | sequential or parallel |

### Test plan

- Unit tests for every new grammar rule.
- Typecheck unit tests: `put` constraints; `where { }` block with mixed comp-ops and IS-NULL atoms; list parameter type validation.
- CAS race integration test: two concurrent claims on the same issue; exactly one matches.
- Idempotency test: call `put` 100 times with the same key; verify single row, deterministic state.
- `returning` test: every mutation form, verify `rows` matches post-mutation state.
- `IN` test: list param with various element types.
- `OFFSET` test: pagination correctness with sort.
- Backward compat: every existing example query lints clean and produces identical results.

## Migration and backward compatibility

- **No breaking changes.** Every existing query parses and executes identically.
- **Result envelope extension**: `matched_nodes` field is additive; consumers that ignore unknown fields continue to work. `rows` is an optional new field, only present when `returning` is used.
- **No CLI changes.** All new shapes work with existing `nanograph run`, `nanograph lint`, etc.
- **No SDK changes.** FFI and TS SDK call into the same `lower → execute → json_output` path; the result envelope extension is additive.
- **No storage format changes.**

## Out of scope

Per `ql-canon.md` P8 and the Tier C anti-patterns table, these are explicitly **not** being proposed:

- Multi-statement query bodies (transactional bundles).
- Cross-statement variable bindings.
- `try { }` / `optional { }` blocks in `match`.
- `assert` / fail-loud stage guards.
- Insert-from-pattern (`insert Foo from match {…}`).
- Pattern-as-gate for mutations (using a leading `match` as the CAS condition).

If a future use case requires any of these, it triggers a canon review — not a feature addition.

## Success criteria

This work is done when:

1. The codebase example's headline lifecycle is correct under the new model:
   - `claim_issue` is race-safe (concurrent claims → exactly one wins; loser sees `matched_nodes: 0`).
   - The lifecycle workflow is documented as a sequence of named-query tool calls in the codebase-example README.
   - `issue_detail` returns correct results for assigned and unassigned issues without an inner-join footgun.
2. `put` is idempotent under retry for both nodes and edges.
3. `returning` saves a round-trip when the agent needs post-mutation state.
4. `IN`, `OFFSET`, `DISTINCT` cover the read-side paper cuts identified in the language review.
5. All existing tests in the workspace pass unchanged.
6. `ql-canon.md` reflects the shipped state.
7. `docs/user/queries.md` documents the new mutation forms and the agent-toolbelt design philosophy.

## References

- `ql-canon.md` — the language constitution this proposal is tested against, especially P3, P7, P8 and the Tier C anti-patterns table.
- `docs/user/queries.md` — user-facing query reference.

## Decision log

- **2026-05 (shipped, PR-1)** Mutation predicate shape: chose **`where { atom+ }` block** over the `AND`-keyword conjunction. Conjunction is structural (no boolean operators); OR is unrepresentable at the grammar level. See `ql-canon.md` "Acknowledged tensions → Mutation predicate strength".
- **2026-05 (shipped, PR-1)** PR-1 features landed: `put` (upsert), `where { }` block, `IS NULL` / `IS NOT NULL` in mutation predicates, `matched_nodes` on `MutationResult`.
- **(deferred to PR-2)** Q1: `null` literal in `prop_match` braces and `IS NULL` in `match` filters — read-side ergonomics, not blocking the agent toolbelt. Requires reworking `IRFilter` to enum form; deferred to its own PR.
- **(open, PR-2)** Q2: `returning` implicit binding name — recommended `$<lowercase first letter>` of type.
- **(open, PR-2)** Q3: list parameter type syntax — recommended `[String]` matching list literals.
