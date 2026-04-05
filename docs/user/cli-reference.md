---
title: CLI
slug: cli
---

# CLI Reference

```
nanograph <command> [options]
```

## Global options

| Option | Description |
|--------|-------------|
| `--json` | Emit machine-readable JSON output for supported commands |
| `--quiet`, `-q` | Suppress human-readable stdout while leaving machine formats and stderr errors intact |
| `--config <nanograph.toml>` | Load CLI defaults from a project config file. If omitted, `./nanograph.toml` is used when present. Local secrets are loaded from `./.env.nano` and then `./.env` when present. |
| `--help` | Show help |
| `--version` | Show CLI version |

See [Project Config](config.md) for `nanograph.toml`, `.env.nano`, alias syntax, and precedence.

## Commands

### `version`

Show CLI version and optional database manifest/dataset version info.

```bash
nanograph version [--db <db_path>]
```

With `--db` or `db.default_path`, includes current manifest `db_version` and per-dataset Lance versions.

### `describe`

Describe schema + manifest summary for a database.

```bash
nanograph describe [--db <db_path>] [--format table|json] [--type <TypeName>] [--verbose]
```

If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.

`--type` filters the output down to a single node or edge type. `--verbose` expands the human table view with manifest, schema hash, type IDs, dataset versions, and dataset paths. JSON output includes agent-facing schema metadata such as `description`, `instruction`, derived key properties, unique properties, relationship summaries, and edge endpoint keys.

### `schema-diff`

Compare two schema files without opening a database.

```bash
nanograph schema-diff --from <old_schema.pg> --to <new_schema.pg> [--format table|json]
```

This command classifies changes as `additive`, `compatible_with_confirmation`, `breaking`, or `blocked`, and emits remediation hints such as `@rename_from("...")` where applicable.

### `export`

Export the full graph (nodes first, then edges) to stdout.

```bash
nanograph export [--db <db_path>] [--format jsonl|json] [--no-embeddings]
```

If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.

- `jsonl` is the portable seed format: nodes plus edges with `from` / `to` resolved through schema `@key` values
- `json` keeps more debug-oriented internal detail for inspection
- `--no-embeddings` omits vector embedding properties from exported payloads

### `init`

Create a new database from a schema file.

```bash
nanograph init [--db <db_path>] [--schema <schema.pg>]
```

Creates the `<db_path>/` directory with `schema.pg`, `schema.ir.json`, and an empty manifest.

When missing, `init` also scaffolds `nanograph.toml` and `.env.nano` in the inferred project directory shared by the DB path and schema path. `nanograph.toml` is for shared defaults; `.env.nano` is for local secrets like `OPENAI_API_KEY`.

If `db.default_path` and/or `schema.default_path` are set in `nanograph.toml`, `--db` and/or `--schema` can be omitted.

### `load`

Load JSONL data into an existing database.

```bash
nanograph load [--db <db_path>] --data <data.jsonl> --mode <overwrite|append|merge>
```

| Mode | Behavior |
|------|----------|
| `overwrite` | Replace the entire current graph snapshot with the loaded data |
| `append` | Add rows without deduplication |
| `merge` | Upsert by `@key` — update existing rows, insert new ones |

`merge` requires at least one node type in the schema to have a `@key` property.
If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.

### `embed`

Backfill or recompute `@embed(...)` vector properties on existing rows.

```bash
nanograph embed --db <db_path> [--type <NodeType>] [--property <vector_prop>] [--only-null] [--limit <n>] [--reindex] [--dry-run]
```

If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.

Useful modes:

- `nanograph embed --db omni.nano`
  - recompute every `@embed(...)` property on every matching row
- `nanograph embed --db omni.nano --type Signal`
  - restrict to one node type
- `nanograph embed --db omni.nano --type Signal --property embedding`
  - restrict to one `@embed(...)` target property
- `nanograph embed --db omni.nano --only-null`
  - only fill rows where the target vector is currently null
- `nanograph embed --db omni.nano --limit 500`
  - process at most 500 rows
- `nanograph embed --db omni.nano --reindex`
  - rebuild vector indexes for touched indexed embed properties; if no rows need updates, reindex matching indexed types anyway
- `nanograph embed --db omni.nano --dry-run`
  - report what would be embedded without writing changes

`--property` requires `--type`.

See [embeddings.md](embeddings.md) for provider behavior, media support, and recompute guidance.

### `check`

Parse and typecheck a query file without executing.

```bash
nanograph check [--db <db_path>] --query <queries.gq> [--schema <schema.pg>]
```

If the provided query path is relative and not found directly, `nanograph` also searches the configured `query.roots` from `nanograph.toml`.
If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.
If `schema.default_path` is set in `nanograph.toml`, `check` also uses it for stale-schema diagnostics. When a query references a type or property that is missing from the current DB schema, `--schema` helps `check` tell you whether the desired schema differs and whether you likely need `nanograph migrate`.
Successful checks warn when a mutation declares zero params, because hardcoded mutations are easy to miss during review and automation.

### `run`

Execute a named query.

```bash
nanograph run [alias] [--db <db_path>] [--query <queries.gq>] [--name <query_name>] [options]
```

| Option | Description |
|--------|-------------|
| `--format table\|kv\|csv\|jsonl\|json` | Output format (default: `table`) |
| `--param key=value` | Query parameter (repeatable) |

Supports both read queries and mutation queries (`insert`, `update`, `delete`) in DB mode.

If the provided query path is relative and not found directly, `nanograph` also searches the configured `query.roots` from `nanograph.toml`.
If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.

When the selected query includes `@description("...")` and/or `@instruction("...")`, `nanograph run` includes that context in every output mode. Human-readable `table` and `kv` views print it as a preamble. `json` wraps rows in an object with query metadata, and `jsonl` emits a metadata header record before the row records. `csv` remains row-only.

Use `--quiet` to suppress human-readable `table` / `kv` output while still executing the query. Machine-oriented formats continue to print normally.

`table` defaults to a compact preview layout. Adjust the preview width with `cli.table_max_column_width` in `nanograph.toml`, or switch the layout with `cli.table_cell_layout = "wrap"` if you prefer multi-line wrapped cells. `kv` renders full values with one row per block, using an auto header plus dividers between rows. Use `json`, `jsonl`, or `csv` for machine-oriented output.

### Query aliases

`nanograph.toml` can define short aliases for `run` on both read and mutation queries:

```toml
[db]
default_path = "app.nano"

[query_aliases.search]
query = "queries/search.gq"
name = "semantic_search"
args = ["q"]
format = "table"
```

Then you can run:

```bash
nanograph run search "vector databases"
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
nanograph run family luke "chosen one prophecy"
```

Explicit `--param` values still work and override alias-derived positional params when both are provided.

Precedence is:
1. explicit CLI flags such as `--query`, `--name`, `--format`, `--db`
2. `query_aliases.<alias>`
3. shared defaults like `db.default_path` and `cli.output_format`

See [Project Config](config.md) for the full alias model.

### `delete`

Delete nodes by predicate with automatic edge cascade.

```bash
nanograph delete [--db <db_path>] --type <NodeType> --where <predicate>
```

Predicate format: `property=value` or `property>=value`, etc.

All edges where the deleted node is a source or destination are automatically removed.
If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.

### `changes`

Read committed graph changes.

```bash
nanograph changes [--db <db_path>] [--since <graph_version> | --from <graph_version> --to <graph_version>] [--format jsonl|json] [--no-embeddings]
```

If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.
`--no-embeddings` strips vector embedding properties from returned row images to keep output compact.

For new graphs, nanograph uses the `NamespaceLineage` storage generation by default. In that mode, `changes` is lineage-native.

## CDC semantics

- Source of truth for new graphs: `__graph_tx` defines committed transaction windows, Lance lineage reconstructs inserts and updates, and `__graph_deletes` stores delete tombstones.
- Commit visibility: only fully committed transactions at or below the current committed `graph_version` are visible.
- Windowing:
  - `--since X` returns rows with `graph_version > X`
  - `--from A --to B` returns rows in inclusive range `[A, B]`
- Ordering for new graphs is deterministic:
  - `graph_version`
  - `entity_kind`
  - `type_name`
  - `rowid` when present, otherwise `logical_key`
  - `change_kind`
- Returned fields for new graphs are:
  - `graph_version`
  - `tx_id`
  - `committed_at`
  - `change_kind`
  - `entity_kind`
  - `type_name`
  - `table_id`
  - `rowid`
  - `entity_id`
  - `logical_key`
  - `row`
  - `previous_graph_version`
- `row` is the current row image for inserts and updates, and the tombstoned last-visible row image for deletes.
- `seq_in_tx` is not part of the new public CDC contract.
- Retention impact: `nanograph cleanup --retain-tx-versions N` guarantees replay only for the retained graph-version window. nanograph keeps the required Lance table history automatically for that window.
- Legacy graphs may still expose older CDC shapes during migration or compatibility workflows.

### `compact`

Compact manifest-tracked Lance datasets and commit updated pinned versions.

```bash
nanograph compact [--db <db_path>] [--target-rows-per-fragment <n>] [--materialize-deletions <bool>] [--materialize-deletions-threshold <f32>]
```

If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.

### `cleanup`

Prune old graph history and Lance versions while preserving committed replay windows.

```bash
nanograph cleanup [--db <db_path>] [--retain-tx-versions <n>] [--retain-dataset-versions <n>]
```

If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.
For new `NamespaceLineage` graphs, `retain-tx-versions` is the primary retention control. nanograph derives the necessary Lance version retention automatically. `retain-dataset-versions` is mainly relevant for legacy storage generations.

### `doctor`

Validate manifest/dataset/log consistency and graph integrity.
Also reports the Lance storage format used by each tracked dataset.

```bash
nanograph doctor [--db <db_path>] [--schema <schema.pg>] [--verbose]
```

Returns non-zero when issues are detected.
If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.
With `--schema`, `doctor` also compares the current DB schema against the desired schema file and reports drift as part of the health check. This is useful when queries have been updated ahead of the database schema.
Use `--verbose` to show per-dataset Lance storage formats in human output. JSON output always includes `dataset_storage_formats`.

### `cdc-materialize`

Materialize currently visible change rows into derived Lance dataset `__cdc_analytics` for analytics acceleration.
This does not change `changes` semantics or CDC authority.

```bash
nanograph cdc-materialize [--db <db_path>] [--min-new-rows <n>] [--force]
```

If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.

### `migrate`

Apply schema changes to an existing database.

```bash
nanograph migrate [--db <db_path>] [--schema <schema.pg>] [options]
```

By default, `migrate` reads the desired schema from `schema.default_path` in `nanograph.toml` when set, otherwise from `<db_path>/schema.pg`. The command diffs the current DB schema IR against that desired schema and generates a migration plan.

| Option | Description |
|--------|-------------|
| `--schema <path>` | Override the desired schema file |
| `--dry-run` | Show plan without applying |
| `--auto-approve` | Apply `confirm`-level steps without prompting |
| `--format table\|json` | Output format (default: `table`) |

Migration steps have safety levels:

| Level | Behavior |
|-------|----------|
| `safe` | Applied automatically (add nullable property, add new type) |
| `confirm` | Requires `--auto-approve` or interactive confirmation (drop property, rename) |
| `blocked` | Cannot be auto-applied (add non-nullable property to populated type) |

Use `@rename_from("old_name")` in the schema to track type/property renames.
If `db.default_path` is set in `nanograph.toml`, `--db` can be omitted.

## Data format

JSONL with one record per line. **Nodes and edges use different key names** — mixing them up is a common error.

Nodes use `"type"` + `"data"`:
```json
{"type": "Person", "data": {"name": "Alice", "age": 30}}
```

Edges use `"edge"` + `"from"` + `"to"` (matched by node `@key` value within source/destination types):
```json
{"edge": "Knows", "from": "Alice", "to": "Bob"}
```

> **Common mistake:** Using `{"type": "Knows", "src": "Alice", "dst": "Bob"}` for edges. This is parsed as a node record and will fail with "unknown node type".

For each edge endpoint type, `@key` is required so `from`/`to` can be resolved.

Edges with properties:
```json
{"edge": "Knows", "from": "Alice", "to": "Bob", "data": {"since": "2020-01-01"}}
```

`nanograph export --format jsonl` emits this same portable seed shape. It does not include internal storage fields like node `id` or edge `id` / `src` / `dst`.

## Debug logging

```bash
RUST_LOG=debug nanograph run --db mydb.nano --query q.gq --name my_query
```

## Upgrading from 0.6 to 0.7

The internal schema IR format changed in 0.7. Databases created with 0.6 will fail to open with a `schema mismatch` error. To migrate, export your data as JSON from the old CLI, then reimport with the new one:

```bash
# 1. Export data using your current (0.6) build
nanograph export --db mydb.nano --format jsonl > backup.jsonl

# 2. Build the new (0.7) CLI
cargo build -p nanograph-cli

# 3. Create a fresh database from the original schema
nanograph init mydb-v07.nano --schema mydb.nano/schema.pg

# 4. Load the exported data
nanograph load mydb-v07.nano --data backup.jsonl --mode overwrite

# 5. Verify
nanograph doctor mydb-v07.nano
```

If you no longer have the 0.6 binary, you can patch the manifest hash manually: compute the FNV-1a hash of the current `schema.ir.json` file and update `schema_ir_hash` in `graph.manifest.json`, then export.

## See also

- [Schema Language Reference](schema.md) — types, annotations, naming conventions
- [Query Language Reference](queries.md) — match, return, traversal, mutations
- [Search Guide](search.md) — text search, vector search, embedding env vars
