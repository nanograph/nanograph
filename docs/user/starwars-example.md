---
title: Star Wars
slug: starwars
---

# Star Wars Example

The canonical Star Wars example lives in `examples/starwars/`. It now serves three purposes at once:

- a full graph example with lineage, affiliation, and film traversal
- the primary user-facing search example
- the canonical CLI E2E fixture for Star Wars workflows

## Setup

The example ships with a checked-in `nanograph.toml`, so you can work from the example directory without repeating `--db` or query paths:

```bash
cd examples/starwars

nanograph init
nanograph load --data starwars.jsonl --mode overwrite
nanograph check --query starwars.gq
```

Key defaults from `examples/starwars/nanograph.toml`:

```toml
[db]
default_path = "starwars.nano"

[query]
roots = ["."]

[embedding]
provider = "mock"

[query_aliases.search]
query = "starwars.gq"
name = "semantic_search"
args = ["q"]
format = "table"
```

That means:

- the example uses deterministic mock embeddings by default
- `nanograph run search "father and son conflict"` works out of the box
- explicit flags still override config when you need them

## Schema metadata

The example schema also demonstrates agent-oriented metadata:

- `Character` has `@description(...)` and `@instruction(...)`
- `HasMentor` and `DebutsIn` describe relationship intent
- `Character.slug`, `Character.note`, and `Character.embedding` carry property descriptions

Inspect it with:

```bash
nanograph describe --type Character --format json
```

That JSON now includes:

- `description`
- `instruction`
- `key_property`
- property descriptions
- incoming and outgoing edge summaries

## Query metadata and aliases

Several example queries also include `@description(...)` and `@instruction(...)`. In normal table mode, `nanograph run` prints that context before the result rows.

```bash
nanograph run search "father and son conflict"
```

Example shape:

```text
Query: semantic_search
Description: Rank characters by semantic similarity against their embedded notes.
Instruction: Use for broad conceptual search, not exact spelling.
```

## Useful commands

Semantic search across character notes:

```bash
nanograph run search "who turned evil"
```

Hybrid search with lexical + semantic fusion:

```bash
nanograph run hybrid "father and son conflict"
```

Graph-constrained semantic search over Luke's parents:

```bash
nanograph run family luke-skywalker "chosen one prophecy"
```

Cross-type traversal from character to debut film:

```bash
nanograph run debut anakin-skywalker
```

Exact query-name form still works when you want full control:

```bash
nanograph run --query starwars.gq --name same_debut --format json --param film=a-new-hope
```

## What this example shows

- search can be embedded directly into a graph schema with `@embed(...)`
- aliases plus `nanograph.toml` make the CLI much lighter to use
- schema and query metadata make `describe` and `run` more useful to agents
- graph traversal and search compose naturally: scope first, then rank

## See also

- [Search Guide](search.md)
- [Context Graph Example](context-graph-example.md)
- [Schema Language Reference](schema.md)
- [Query Language Reference](queries.md)
