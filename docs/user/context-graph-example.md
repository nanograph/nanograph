---
title: Context Graph
slug: context-graph
---

# Context Graph Example

The canonical RevOps / context-graph example lives in `examples/revops/`. It models signals, decisions, actions, opportunities, and projects in one graph, and it now showcases:

- schema `@description(...)` and `@instruction(...)`
- query `@description(...)` and `@instruction(...)`
- checked-in `nanograph.toml` defaults and aliases
- lexical and semantic signal search scoped by graph traversal

## Setup

```bash
cd examples/revops

nanograph init
nanograph load --data revops.jsonl --mode overwrite
nanograph check --query revops.gq
```

The example config gives you:

- `db.default_path = "omni.nano"`
- `query.roots = ["."]`
- aliases like `why`, `trace`, `value`, `pipeline`, and `signals`
- deterministic mock embeddings for `Signal.summaryEmbedding`

See [Project Config](config.md) for the full config and env model.

## Describe the schema

The schema carries agent-oriented metadata on types like `Decision` and `Signal`.

```bash
nanograph describe --type Signal --format json
```

That JSON includes:

- `description`
- `instruction`
- `key_property`
- property descriptions
- edge summaries and endpoint-key metadata

## Useful aliases

Why did an opportunity happen?

```bash
nanograph run why opp-stripe-migration
```

Trace a signal through decision and action:

```bash
nanograph run trace sig-hates-vendor
```

Show value created by a signal:

```bash
nanograph run value sig-hates-vendor
```

Pipeline summary:

```bash
nanograph run pipeline
```

Semantic signal search scoped to one client:

```bash
nanograph run signals cli-priya-shah "procurement approval timing"
```

## Query metadata in action

Queries like `decision_trace` and `semantic_signals_for_client` include `@description(...)` and `@instruction(...)`, so table output gives agents extra context before the rows:

```text
Query: decision_trace
Description: Trace an opportunity back to the decision, actor, and signal that created motion.
Instruction: Use as the default 'why did this happen?' query for an opportunity slug.
```

For record-oriented inspection of one result at a time, switch to `kv`:

```bash
nanograph run why opp-stripe-migration --format kv
```

## Why this example matters

This example shows how NanoGraph can represent operational reasoning directly:

- `Signal` captures observed evidence
- `Decision` records why work should happen
- `Action` proves what actually happened
- graph traversal lets you move across those layers without flattening everything into one table

With the new search fields, you can also ask retrieval-style questions inside that reasoning graph instead of keeping search and graph logic separate.

## See also

- [Project Config](config.md)
- [Star Wars Example](starwars-example.md)
- [Search Guide](search.md)
- [Schema Language Reference](schema.md)
- [Query Language Reference](queries.md)
