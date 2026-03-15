---
title: Best Practices
slug: best-practices
---

# Best Practices

NanoGraph is built for AI agents, but agents routinely misuse it — batch reingestion instead of mutations, skipping typechecks, wrong JSONL format, stale schemas. This guide covers the most common mistakes and how to avoid them.

## Start here

If you remember nothing else:

- Run nanograph commands from the directory where `nanograph.toml` lives.
- Use mutation queries for data changes — do not export, edit JSONL, and re-load.
- Put `@key` on every node type.
- After editing `.gq` or `.pg` files, run `nanograph check --query <file>`.
- Use `--json`, `--format json`, or `--format jsonl` for machine-readable output.

## Project setup

### Run from the project directory

NanoGraph resolves `nanograph.toml` from the current working directory. It does not walk parent directories.

```bash
# Wrong — "database path is required" or missing-config behavior
cd /some/other/dir && nanograph describe

# Right — run from the project root
cd /path/to/project && nanograph describe
```

If the project uses `[db] default_path` and `[schema] default_path` in `nanograph.toml`, commands can omit `--db` and `--schema` — but only from the right directory.

### Treat `nanograph.toml` as the operational backbone

`nanograph.toml` is the interface contract between the project and the agents operating on it. A good config eliminates fragile one-off CLI invocations:

```toml
[project]
name = "My Graph"
description = "What this graph is for — agents read this."
instruction = "Run from this directory. Use aliases for common operations."

[db]
default_path = "app.nano"

[schema]
default_path = "schema.pg"

[query]
roots = ["."]

[embedding]
provider = "openai"

[cli]
output_format = "table"

[query_aliases.search]
query = "queries.gq"
name = "semantic_search"
args = ["q"]
format = "json"

[query_aliases.add-task]
query = "queries.gq"
name = "add_task"
args = ["slug", "title", "status"]
format = "jsonl"
```

Without this config, every command requires `--db`, `--query`, `--name`, and `--format`.

### Commit the right artifacts to git

| File | Commit? | Why |
|------|---------|-----|
| `nanograph.toml` | Yes | Shared project config, aliases, defaults |
| `schema.pg` | Yes | Schema is code |
| `queries.gq` | Yes | Query and mutation library |
| `seed.jsonl` | Yes | Reproducible bootstrap data |
| `*.nano/` | Yes, if small enough | Embedded database state is often part of the project |
| `.env.nano` | No | Secrets |

The database is not regenerable from seed. Once mutations have been applied, the DB contains records, edges, CDC history, and embeddings that do not exist anywhere else.

Small databases under roughly 50 MB per file are fine to commit — treat them like a SQLite file checked into the repo. Large ones should be backed up outside git (S3 sync, filesystem snapshots, rsync), but the schema, queries, config, and seed files should still be committed.

### Never commit `.env.nano`

`.env.nano` holds secrets like `OPENAI_API_KEY`. Add it to `.gitignore` before the first commit:

```gitignore
.env.nano
```

If it was already committed:

```bash
git rm --cached .env.nano
echo ".env.nano" >> .gitignore
git add .gitignore
git commit -m "stop tracking .env.nano"
```

Then rotate the exposed key immediately.

### Add nanograph rules to `CLAUDE.md` or `AGENTS.md`

AI agents read `CLAUDE.md` and `AGENTS.md` at the repo root. If a project uses NanoGraph, include operational rules:

```markdown
## NanoGraph

This project uses a nanograph database at `app.nano`.

- Always run nanograph commands from the directory where `nanograph.toml` lives
- Use `nanograph describe --format json` before querying
- Use mutation queries for data changes — never export/edit/reimport JSONL
- Use `nanograph run <alias>` instead of constructing raw `--query` / `--name` / `--param` calls
- After editing `.gq` or `.pg`, run `nanograph check --query <file>`
- Never commit `.env.nano`
- Use `--format json` or `--format jsonl` for machine-readable output
```

Tailor to the project: list aliases, slug conventions, and any required fields.

## Safe data changes

### Use mutations, not batch reingestion

The most common agent mistake. Do not export the graph as JSONL, edit rows, and `load --mode overwrite` to change a few records. That destroys CDC continuity, invalidates Lance dataset history, and is far slower than a targeted mutation.

```bash
# Wrong — full reingestion to update one field
nanograph export --format jsonl > dump.jsonl
# ... edit dump.jsonl ...
nanograph load --data dump.jsonl --mode overwrite

# Right — targeted mutation
nanograph run --query queries.gq --name update_status \
  --param slug=cli-acme --param status=healthy
```

Use each operation for its intended purpose:

| Goal | Command |
|------|---------|
| Add one record | `insert` mutation query |
| Update a field on an existing record | `update` mutation query |
| Remove a record | `delete` mutation query |
| Initial bulk data load | `nanograph load --mode overwrite` |
| Add a batch of new records | `nanograph load --mode append` |
| Reconcile external data into keyed records | `nanograph load --mode merge` |

Write reusable parameterized mutations in the `.gq` file when designing the schema, not later:

```graphql
node Task {
    slug: String @key
    title: String
    status: enum(open, in_progress, done)
}

query add_task($slug: String, $title: String, $status: String) {
    insert Task { slug: $slug, title: $title, status: $status }
}

query update_task_status($slug: String, $status: String) {
    update Task set { status: $status } where slug = $slug
}

query remove_task($slug: String) {
    delete Task where slug = $slug
}
```

### Mutation constraints

- All mutations should be parameterized. Do not hardcode slugs, dates, or one-off values into the query body.
- `update` requires `@key` on the target node type.
- `now()` is available in filters, projections, and mutation assignments and resolves once per execution:
  ```graphql
  query touch_client($slug: String) {
      update Client set { updatedAt: now() } where slug = $slug
  }
  ```
- Array params cannot be passed through `--param`.
- Edge inserts use `from:` and `to:` with endpoint `@key` values, not internal IDs:
  ```graphql
  query link_signal($signal: String, $client: String) {
      insert SignalAffects { from: $signal, to: $client }
  }
  ```
- Edge names in queries use camelCase even though the schema defines them in PascalCase. `HasMentor` becomes `hasMentor`.

### Put `@key` on every node type

Every node type that participates in edges or will ever need updates should have a `@key`. In practice, every node type should have one.

```graphql
node Client {
    slug: String @key
    name: String @unique
    status: enum(active, churned, prospect)
}
```

Without `@key`:

- `update` mutations cannot target a stable identity
- Edge JSONL cannot resolve `from` / `to`
- `load --mode merge` has nothing to match on

Use stable human-readable slugs where possible. They make debugging, edge wiring, aliases, and CLI workflows much easier.

### Define aliases for every agent operation

Aliases turn fragile multi-flag commands into short positional calls:

```toml
[query_aliases.search]
query = "queries.gq"
name = "semantic_search"
args = ["q"]
format = "json"

[query_aliases.client]
query = "queries.gq"
name = "client_lookup"
args = ["slug"]
format = "json"

[query_aliases.add-task]
query = "queries.gq"
name = "add_task"
args = ["slug", "title", "status"]
format = "jsonl"

[query_aliases.status]
query = "queries.gq"
name = "update_task_status"
args = ["slug", "status"]
format = "jsonl"
```

```bash
nanograph run search "renewal risk"
nanograph run client cli-acme
nanograph run add-task task-42 "Draft proposal" open
nanograph run status task-42 done
```

Alias design rules:

- Always set `format`. Use `json` for reads, `jsonl` for mutations.
- Use short verb-like names.
- Put the natural scoping argument first.
- List the available aliases in `CLAUDE.md` or `AGENTS.md`.
- Do not create aliases for built-in commands like `describe` or `doctor`.

## Schema and query authoring

### Start with `describe` and `doctor`

Before writing queries, running mutations, or ingesting data, inspect the database:

```bash
nanograph describe --format json
nanograph describe --type Signal --format json
nanograph doctor
```

`describe --format json` returns type names, properties and their types, key properties, edge summaries, row counts, and `@description` / `@instruction` annotations. Agents that skip `describe` and try to infer the schema from output or JSONL make systematic mistakes.

`doctor` is the health check. Use `doctor --schema` to compare the DB against a desired schema file, and `doctor --verbose` to see per-dataset Lance storage format versions.

### Use `@description` and `@instruction`

These annotations do not change execution, but they give agents structured intent through `describe` and `run`:

```graphql
node Signal
  @description("Observed customer or market signals that can surface opportunities.")
  @instruction("Use Signal.slug for trace queries. summary is the retrieval field for search.")
{
    slug: String @key @description("Stable signal identifier for trace and search queries.")
    summary: String @description("Primary textual content for lexical and semantic retrieval.")
    urgency: enum(low, medium, high, critical)
}

query semantic_search($q: String)
  @description("Rank signals by semantic similarity.")
  @instruction("Use for broad conceptual search, not exact spelling.")
{
    match { $s: Signal }
    return { $s.slug, nearest($s.embedding, $q) as score }
    order { nearest($s.embedding, $q) }
    limit 5
}
```

Write these annotations when creating the schema or query. They cost nothing and prevent agent misuse.

### Always typecheck after editing `.gq` or `.pg`

No query or schema edit is complete until `check` passes:

```bash
nanograph check --query queries.gq
```

This catches wrong property names, type mismatches, invalid traversals, missing parameters, and malformed mutations.

When the schema changed, also preview the migration plan:

```bash
nanograph migrate --dry-run
nanograph schema-diff --from old.pg --to new.pg
```

### Migrate, don't rebuild

When the schema changes, update the schema file and run `nanograph migrate`. Do not rebuild from scratch unless the seed is complete and current DB state does not matter.

```bash
# Right
nanograph migrate

# Wrong — destroys all mutations applied since last seed
nanograph init --schema new-schema.pg
nanograph load --data seed.jsonl --mode overwrite
```

Migration rules:

- Adding a nullable property is safe — auto-applied.
- Adding a node or edge type is safe — auto-applied.
- Renames require `@rename_from("old_name")`.
- Adding a non-nullable property to a populated type is blocked.
- Dropping properties requires `--auto-approve`.

If a migration fails and leaves a stale journal in `PREPARED` state, delete the journal before retrying.

If the project keeps the desired schema outside the DB directory, configure `schema.default_path` so `migrate`, `check`, and `doctor --schema` all refer to the same file.

## Search and embeddings

### Scope with graph traversal, then rank

NanoGraph's strength is combining graph structure with search. Avoid global search followed by application-side filtering:

```graphql
// Wrong — search globally, then filter later
query bad_search($q: String) {
    match { $s: Signal }
    return { $s.slug, $s.summary, nearest($s.embedding, $q) as score }
    order { nearest($s.embedding, $q) }
    limit 20
}

// Right — constrain first, then rank
query client_signals($client: String, $q: String) {
    match {
        $c: Client { slug: $client }
        $s signalAffects $c
    }
    return { $s.slug, $s.summary, nearest($s.embedding, $q) as score }
    order { nearest($s.embedding, $q) }
    limit 5
}
```

Traverse first, then rank inside the relevant subgraph.

### Backfill embeddings after `@embed` changes

When adding or changing an `@embed(source_prop)` field, existing rows do not automatically gain vectors. Backfill them explicitly:

```bash
nanograph embed --only-null              # fill only missing vectors
nanograph embed                          # recompute all embeddings
nanograph embed --type Signal --only-null # restrict to one type
nanograph embed --dry-run                # preview without writing
```

If the field has `@index`, rebuild the vector index after embedding:

```bash
nanograph embed --only-null --reindex
```

## Operational workflow

### Post-change checklist

Schema and query edits should follow this sequence:

1. Edit `.pg` and/or `.gq`.
2. Typecheck: `nanograph check --query queries.gq`.
3. If the schema changed: `nanograph migrate` (preview with `--dry-run` first).
4. If `@embed(...)` fields changed: `nanograph embed --only-null`.
5. Smoke test: `nanograph run <alias>`.

Skipping or reordering these steps leads to stale-schema errors, missing embeddings, and broken runtime behavior.

### Compact and cleanup periodically

After many mutations, Lance datasets accumulate deletion markers and fragmented files:

```bash
nanograph compact
nanograph cleanup --retain-tx-versions 50
nanograph doctor
```

Do not compact after every single mutation. Batch work and compact periodically.

### Keep nanograph up to date

NanoGraph is under active development. Updating regularly matters for migration, CDC, embeddings, and CLI behavior:

```bash
nanograph version
brew upgrade nanograph
```

When something behaves unexpectedly, check the CLI version first.

### Lance storage format

NanoGraph 1.0 uses Lance v3, which writes datasets in storage format v2.2. Databases created with earlier versions use format v2. Both formats are readable, but v2.2 is required for new Lance features. Check with `nanograph doctor --verbose`.

See [Lance Migration](lance-migration.md) for the upgrade procedure and a summary of v2.2 improvements.

## Agent output

### Always use structured output

`table` output is for humans. Agents should use machine-readable output.

For `nanograph run`:

```bash
nanograph run search "renewal risk" --format json
nanograph run search "renewal risk" --format jsonl
```

For other commands:

```bash
nanograph describe --json
nanograph migrate --dry-run --json
nanograph doctor --json
```

Set `format` in aliases so agents do not need to remember output flags.

### Prefer aliases over raw invocations

```bash
# Fragile
nanograph run --query queries.gq --name update_task_status \
  --param slug=task-123 --param status=done --format jsonl

# Better
nanograph run status task-123 done
```

## Data formats

### JSONL: nodes vs edges

Nodes use `"type"` and `"data"`:

```json
{"type": "Client", "data": {"slug": "cli-acme", "name": "Acme Corp", "status": "active"}}
```

Edges use `"edge"`, `"from"`, and `"to"`:

```json
{"edge": "SignalAffects", "from": "sig-renewal", "to": "cli-acme"}
```

Common mistakes:

- Using `type` for edge rows — this is parsed as a node record.
- Using `src` / `dst` instead of `from` / `to`.
- Using internal IDs instead of endpoint `@key` values.
- Type and edge names in JSONL use schema PascalCase exactly (unlike queries, which use camelCase for edges).

## Common mistakes

| Mistake | Fix |
|---------|-----|
| Full reingestion to change one record | Use an `update` mutation query |
| Running from the wrong directory | `cd` to where `nanograph.toml` lives |
| Missing `@key` on node types | Add `slug: String @key` or another stable key |
| PascalCase edge names in queries | Use camelCase: `HasMentor` → `hasMentor` |
| Using `type` for edge JSONL rows | Use `edge` with `from` / `to` |
| Rebuilding the DB instead of migrating | Use `nanograph migrate` |
| Not committing schema, queries, config | Commit `.pg`, `.gq`, `nanograph.toml`, seed `.jsonl` |
| Committing `.env.nano` | Add to `.gitignore` before first commit |
| Parsing `table` output | Use `--format json` or `--format jsonl` |
| No `@description` / `@instruction` | Add annotations so agents can self-orient |
| No aliases or missing alias `format` | Alias every important agent operation |
| Guessing schema instead of calling `describe` | Run `nanograph describe --format json` first |
| Global search plus post-filtering | Traverse first, then rank |
| Editing `.gq`/`.pg` without typechecking | Run `nanograph check` after every edit |
| Hardcoded values in mutations | Parameterize everything |
| Missing embeddings after `@embed` changes | Run `nanograph embed --only-null` |
| Skipping post-change workflow | Edit → check → migrate → embed → smoke test |
| Running an outdated CLI | `brew upgrade nanograph` regularly |
| No nanograph rules in `CLAUDE.md` | Add a short operational section |

## See also

- [CLI Reference](cli-reference.md)
- [Schema Language Reference](schema.md)
- [Query Language Reference](queries.md)
- [Search Guide](search.md)
- [Project Config](config.md)
- [Lance Migration](lance-migration.md)
