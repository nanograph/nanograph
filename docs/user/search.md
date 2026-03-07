---
title: Search
slug: search
---

# Search Guide

NanoGraph supports lexical search, semantic search, hybrid ranking, and graph-constrained search in the same query language. The canonical search example is `examples/starwars/`.

## Setup

```bash
cd examples/starwars

nanograph init
nanograph load --data starwars.jsonl --mode overwrite
nanograph check --query starwars.gq
```

This example includes:

- `Character.note: String` for lexical search
- `Character.embedding: Vector(8) @embed(note) @index` for semantic search
- `nanograph.toml` with `db.default_path`, query roots, aliases, and mock embeddings

If you want real embeddings instead of the example's default mock provider, put your key in `.env.nano`:

```bash
OPENAI_API_KEY=...
```

## Lexical search

Token search:

```bash
nanograph run --query starwars.gq --name keyword_search --format json --param 'q=chosen one'
```

Typo-tolerant name search:

```bash
nanograph run --query starwars.gq --name fuzzy_search --format json --param 'q=Skywaker'
```

Phrase-style text matching:

```bash
nanograph run --query starwars.gq --name match_text_search --format json --param 'q=Jedi Grand Master'
```

Ranked lexical search with BM25:

```bash
nanograph run --query starwars.gq --name bm25_search --format json --param 'q=clone wars lightsaber'
```

Use lexical search when exact words matter, typo tolerance is useful, or you want interpretable BM25 ranking.

## Semantic search

The example defines a config alias for semantic search:

```bash
nanograph run search "who turned evil"
```

That alias resolves to:

```graphql
query semantic_search($q: String) {
    match { $c: Character }
    return {
        $c.slug as slug,
        $c.name as name,
        nearest($c.embedding, $q) as score
    }
    order { nearest($c.embedding, $q) }
    limit 5
}
```

`nearest()` returns cosine distance, so lower is better.

Use semantic search when the user gives intent or concept language instead of exact terms.

## Hybrid search

Hybrid search combines semantic distance and BM25 with `rrf(...)`:

```bash
nanograph run hybrid "father and son conflict"
```

This is usually the best default when you want both exact-term sensitivity and broader semantic recall.

## Graph-constrained search

Graph structure narrows the candidate set before search ranks inside that subgraph.

Parents of Luke ranked semantically:

```bash
nanograph run family luke-skywalker "chosen one prophecy"
```

Students of Obi-Wan ranked lexically:

```bash
nanograph run --query starwars.gq --name students_search --format json \
  --param mentor=obi-wan-kenobi \
  --param 'q=dark side'
```

This pattern is one of NanoGraph's strengths:

- traverse first to define context
- rank only inside that context

## Query metadata in the CLI

When a query carries `@description(...)` and `@instruction(...)`, table output prints that context before the rows:

```bash
nanograph run search "father and son conflict"
```

Example shape:

```text
Query: semantic_search
Description: Rank characters by semantic similarity against their embedded notes.
Instruction: Use for broad conceptual search, not exact spelling.
```

Machine-oriented formats like `json`, `jsonl`, and `csv` keep their existing payload shape.

## Next steps

- [Star Wars Example](starwars-example.md)
- [Context Graph Example](context-graph-example.md)
- [Schema Language Reference](schema.md)
- [Query Language Reference](queries.md)
