---
title: Project Config
slug: config
---

# Project Config

nanograph supports a project-level `nanograph.toml` plus local env files for secrets and machine-specific overrides.

Use this for:

- default DB and schema paths
- query roots
- query aliases
- embedding defaults
- CLI output defaults

Use `.env.nano` for local secrets like `OPENAI_API_KEY`.

## Files

### `nanograph.toml`

Shared project defaults. Safe to commit.

Typical contents:

```toml
[project]
name = "Star Wars"
description = "Searchable Star Wars graph."
instruction = "Prefer aliases for common queries."

[db]
default_path = "starwars.nano"

[schema]
default_path = "starwars.pg"

[query]
roots = ["."]

[embedding]
provider = "mock"
model = "text-embedding-3-small"
batch_size = 64
chunk_size = 0
chunk_overlap_chars = 128
api_key_env = "OPENAI_API_KEY"

[cli]
output_format = "table"
json = false
table_max_column_width = 80
table_cell_layout = "truncate"

[query_aliases.search]
query = "starwars.gq"
name = "semantic_search"
args = ["q"]
format = "table"
```

### `.env.nano`

Local-only secrets and machine-specific overrides. Do not commit this file.

```bash
OPENAI_API_KEY=sk-...
```

You can also put non-secret local overrides here, but the intended use is secrets.

### `.env`

nanograph also loads `./.env` after `./.env.nano`. It is a fallback compatibility path, not the preferred place for nanograph secrets.

## Discovery

Config discovery is intentionally simple:

- if you pass `--config <path>`, nanograph loads that file
- otherwise, it looks for `./nanograph.toml`
- it does not walk parent directories

Env files are loaded from the config base directory:

- when `--config path/to/nanograph.toml` is used, nanograph looks for `path/to/.env.nano` and `path/to/.env`
- otherwise, it looks in the current working directory

## Precedence

### Command defaults

For command settings like DB path, query path, alias name, and output format:

1. explicit CLI flags
2. `query_aliases.<alias>`
3. shared config defaults such as `db.default_path`, `schema.default_path`, and `cli.output_format`

### Embedding env and secrets

For embedding-related env like `OPENAI_API_KEY`:

1. existing process environment
2. `.env.nano`
3. `.env`
4. `[embedding]` values in `nanograph.toml`
5. engine defaults

This is why the recommended setup is:

- keep `OPENAI_API_KEY` in `.env.nano`
- keep shared embedding defaults like `provider` and `model` in `nanograph.toml`

## Supported sections

### `[project]`

Advisory project metadata.

```toml
[project]
name = "Acme Graph"
description = "Operational context graph for customer work."
instruction = "Prefer trace queries before ad hoc search."
```

Currently this is descriptive metadata for humans and agents. It does not change engine behavior.

### `[db]`

Default database path for commands that operate on a DB.

```toml
[db]
default_path = "app.nano"
```

With this set, commands like these can omit `--db`:

```bash
nanograph describe
nanograph export
nanograph run search "vector databases"
```

### `[schema]`

Default schema path, mainly used by `init`.

```toml
[schema]
default_path = "schema.pg"
```

### `[query]`

Query file search roots for `check` and `run`.

```toml
[query]
roots = ["queries", "examples"]
```

If a relative query path is not found directly, nanograph searches these roots in order.

### `[embedding]`

Shared embedding defaults.

```toml
[embedding]
provider = "openai"   # or "mock"
model = "text-embedding-3-small"
base_url = "https://api.openai.com/v1"
batch_size = 64
chunk_size = 0
chunk_overlap_chars = 128
api_key_env = "OPENAI_API_KEY"
```

Supported fields:

- `provider` — `openai` or `mock`
- `model`
- `base_url`
- `mock`
- `batch_size`
- `chunk_size`
- `chunk_overlap_chars`
- `api_key_env`

Do not put raw API keys in `nanograph.toml`.

### `[cli]`

Shared CLI defaults.

```toml
[cli]
output_format = "table"
json = false
```

`output_format` is used when a command supports multiple formats and no explicit `--format` is passed.
For `nanograph run`, supported formats are `table`, `kv`, `csv`, `jsonl`, and `json`.
`json = true` acts like a shared default for the global `--json` flag on commands that support machine-readable JSON output.
`--quiet` is always a runtime flag; it is not stored in project config.

`table_max_column_width` controls the width budget for `nanograph run --format table`.

`table_cell_layout` controls how wide text cells are rendered:
- `truncate` — default compact preview; long or multiline values are collapsed to one line and truncated
- `wrap` — keep a compact normalized string, but wrap it across multiple table lines instead of truncating

Use `kv`, `json`, `jsonl`, or `csv` when you need the full value payload without table formatting. For `nanograph run`, `json` and `jsonl` also carry query metadata (`name`, `@description`, `@instruction`) for agent consumers.

### `[query_aliases.<alias>]`

Shortcuts for `nanograph run`. They work for both read and mutation queries.

```toml
[query_aliases.search]
query = "queries/search.gq"
name = "semantic_search"
args = ["q"]
format = "table"
```

Then:

```bash
nanograph run search "father and son conflict"
```

Multi-parameter aliases work the same way:

```toml
[query_aliases.family]
query = "queries/search.gq"
name = "family_semantic"
args = ["slug", "q"]
format = "table"
```

```bash
nanograph run family luke-skywalker "chosen one prophecy"
```

Explicit `--param` values still work and override alias-derived positional params when both are present.

## `init` scaffolding

When missing, `nanograph init` scaffolds:

- `nanograph.toml`
- `.env.nano`

It writes them into the inferred project directory shared by the DB path and schema path.

The generated config includes:

- `db.default_path`
- `schema.default_path`
- `query.roots = ["queries"]`
- default `[embedding]` keys
- a commented alias example

## What does not belong here

`nanograph.toml` is not live database state.

Do not use it for:

- manifest values
- tx / CDC cursors
- dataset versions
- any other engine-managed runtime state

That state already lives inside the database folder.

## See also

- [CLI Reference](cli-reference.md)
- [Schema Language Reference](schema.md)
- [Query Language Reference](queries.md)
- [Search Guide](search.md)
