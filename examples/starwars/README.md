# Star Wars Knowledge Graph (Merged)

Typed property graph of the Star Wars universe with a rich relationship model and realistic temporal anchor via films.

## Files

| File | Description |
|------|-------------|
| `starwars.pg` | Merged schema (9 node types, 26 edge types, slug `@key` identity) |
| `starwars.jsonl` | Merged data (~66 nodes, ~146 edges) |
| `starwars.gq` | Read + mutation query suite (legacy reads + film/date + mutations) |

## Notable Modeling Choices

- `slug: String @key` on every node type for stable identity.
- Enums on categorical fields (`alignment`, `era`, `side`, `climate`).
- `Film.release_date: Date` is the only date field in the example.
- Character and battle timeline context is expressed through edges:
  - `DebutsIn: Character -> Film`
  - `DepictedIn: Battle -> Film`

## Quick Start

This example ships with a checked-in `nanograph.toml`, so you can work directly from the example directory without repeating `--db` or query paths:

```bash
cd examples/starwars

nanograph init
nanograph load --data starwars.jsonl --mode overwrite
nanograph check --query starwars.gq
nanograph run search "father and son conflict"
nanograph run debut anakin-skywalker
```

The config also enables deterministic mock embeddings and exposes aliases like `search`, `hybrid`, `family`, and `debut`.

## Mutation + CDC Walkthrough

```bash
nanograph run --query starwars.gq --name add_character
nanograph run --query starwars.gq --name delete_character --param slug=ezra-bridger

nanograph changes --from 2 --to 3 --format json
nanograph changes --since 2 --format jsonl
```

## Introspection

```bash
nanograph version
nanograph describe --type Character --format json
nanograph export --format jsonl
```
