---
title: Best Practices
slug: best-practices
---

# Best Practices for AI Agents

This guide addresses the most common mistakes agents make when operating a nanograph database. Follow these rules to avoid data loss, unnecessary rebuilds, and broken workflows.

## 1. Always run from the project directory

nanograph resolves `nanograph.toml` from the current working directory. It does not walk parent directories.

```bash
# Wrong — "database path is required" error
cd /some/other/dir && nanograph describe

# Right — cd to the project root first
cd /path/to/project && nanograph describe
```

If the project uses `[db] default_path` and `[schema] default_path` in `nanograph.toml`, all commands work without `--db` or `--schema` flags — but only when you're in the right directory.

## 2. Use mutations, not batch reingestion

This is the single most common agent mistake. Do not export the full graph as JSONL, edit rows in the export, and `load --mode overwrite` to change a few records. This destroys CDC history, invalidates Lance dataset versions, and is orders of magnitude slower than a targeted mutation.

```bash
# Wrong — full reingestion to update one field
nanograph export --format jsonl > dump.jsonl
# ... edit dump.jsonl ...
nanograph load --data dump.jsonl --mode overwrite

# Right — targeted mutation
nanograph run --query queries.gq --name update_status \
  --param slug=cli-acme --param status=healthy
```

**When to use each operation:**

| Goal | Command |
|------|---------|
| Add one record | `insert` mutation query |
| Update a field on an existing record | `update` mutation query (requires `@key`) |
| Remove a record | `delete` mutation query (cascades edges) |
| Initial bulk data load | `nanograph load --mode overwrite` |
| Add a batch of new records to existing data | `nanograph load --mode append` |
| Sync from an external system (reconcile) | `nanograph load --mode merge` (requires `@key`) |

Write reusable parameterized mutation queries in your `.gq` file — one query per operation. **Design mutations alongside the schema, not as an afterthought.** When you add a node type, immediately write the corresponding `insert`, `update`, and `delete` mutations plus any edge-linking mutations. This gives agents a safe, tested interface from day one instead of tempting them to fall back on raw JSONL loads.

```graphql
// Schema
node Task {
    slug: String @key
    title: String
    status: enum(open, in_progress, done)
}

// Mutations — written at the same time as the schema
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

Then wire up aliases in `nanograph.toml` for frequent operations:

```toml
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

## 3. Mutation constraints to know

- **All mutations must be parameterized.** Never hardcode slugs, dates, or values directly in mutation queries. Hardcoded mutations are single-use throwaway code — agents write them once, run them, and leave dead queries in the `.gq` file. Use parameters for every variable value:
  ```graphql
  // Wrong — hardcoded, single-use
  query advance_stage() {
      update Opportunity set { stage: "won" } where slug = "opp-stripe-migration"
  }

  // Right — reusable
  query advance_stage($slug: String, $stage: String) {
      update Opportunity set { stage: $stage } where slug = $slug
  }
  ```
- **`update` requires `@key`** on the target node type. Without it, the engine has no identity to match against.
- **`now()` is available for timestamps.** Use `now()` in filters, projections, and mutation assignments instead of hardcoding datetime literals. It resolves to the current UTC time once per query execution:
  ```graphql
  query touch_client($slug: String) {
      update Client set { updatedAt: now() } where slug = $slug
  }
  ```
- **Array params cannot be passed via `--param`.** Hardcode list values in the query body, or restructure to avoid list params at the CLI boundary.
- **Edge inserts use `from:` / `to:` with `@key` values**, not node IDs or variable references:
  ```graphql
  query link_signal($signal: String, $client: String) {
      insert SignalAffects { from: $signal, to: $client }
  }
  ```
- **Edge names in queries use camelCase** (lowercase first letter), even though the schema defines them in PascalCase. `HasMentor` in the schema becomes `hasMentor` in queries. Using PascalCase in queries is a parse error.

## 4. Design schemas with `@key` on every node type

Every node type that participates in edges or will ever need updates must have a `@key` property. In practice, every node type should have one.

```graphql
node Client {
    slug: String @key          // always include this
    name: String @unique
    status: enum(active, churned, prospect)
}
```

Without `@key`:
- `update` mutations won't work (no identity for merge)
- Edge JSONL can't resolve `from`/`to` values
- `load --mode merge` has nothing to match on

Use stable, human-readable slugs (e.g. `cli-acme`, `sig-renewal-risk`) rather than UUIDs. Slugs make CLI debugging, edge wiring, and query aliases far easier.

## 5. Use `@description` and `@instruction` to guide agents

nanograph supports `@description("...")` and `@instruction("...")` annotations on node types, edge types, properties, and queries. These are metadata — they don't change execution — but they surface in `describe --format json` and in `run` output, giving agents structured context about what things mean and how to use them.

**On schema types and properties:**

```graphql
node Signal
  @description("Observed customer or market signals that can surface opportunities.")
  @instruction("Use Signal.slug for trace queries. summary is the retrieval field for search.")
{
    slug: String @key @description("Stable signal identifier for trace and search queries.")
    summary: String @description("Primary textual content for lexical and semantic retrieval.")
    urgency: enum(low, medium, high, critical)
}

edge InformedBy: Decision -> Signal
  @description("Connects a decision to the signals that informed it.")
{
    influence: enum(minor, supporting, primary)
}
```

**On queries:**

```graphql
query semantic_search($q: String)
  @description("Rank characters by semantic similarity against their embedded notes.")
  @instruction("Use for broad conceptual search, not exact spelling. Prefer keyword_search for exact terms.")
{
    match { $c: Character }
    return { $c.slug, $c.name, nearest($c.embedding, $q) as score }
    order { nearest($c.embedding, $q) }
    limit 5
}
```

Without these annotations, agents guess at what types mean and which queries to use. With them, `nanograph describe --type Signal --format json` returns machine-readable intent, and `nanograph run` prints query guidance before results. Write them when you create the schema — they cost nothing and prevent agent misuse.

## 6. Commit nanograph artifacts to git

Agents frequently exclude graph files from version control. These files **should** be committed:

| File | Commit? | Why |
|------|---------|-----|
| `nanograph.toml` | Yes | Shared project config, aliases, defaults |
| `schema.pg` | Yes | Schema is code — review it like any source file |
| `queries.gq` | Yes | Query library — the operational interface |
| `seed.jsonl` | Yes | Seed data for reproducible bootstrapping |
| `*.nano/` | **Yes** (small) | The database holds state mutations have created — it is not disposable |
| `.env.nano` | **No** | Secrets (API keys) — gitignore this |

**The database is not regenerable from seed.** Once agents start making mutations, the database holds records, edges, CDC history, and embeddings that don't exist anywhere else. The seed file only captures the initial load. Do not gitignore the database and assume you can rebuild it.

**Small databases (under ~50 MB):** Commit the entire `*.nano/` directory. nanograph databases are embedded assets — treat them like a SQLite file checked into the repo. This is the default for most agent-scale graphs. GitHub warns at 50 MB per file and blocks at 100 MB.

**Large databases (over ~50 MB):** The `*.nano/` directory contains binary Lance datasets that will bloat git history. Gitignore it, but still commit everything else — `schema.pg`, `queries.gq`, `nanograph.toml`, and seed `.jsonl`. These let you recreate the database structure if needed. For the database itself, set up a real backup strategy — S3 sync, filesystem snapshots, or rsync to durable storage. The database directory is the complete source of truth: Lance datasets, CDC log, transaction catalog, manifest, and schema IR. Back up the entire directory as-is.

```
# Always gitignore secrets
.env.nano

# Only gitignore the database if it's too large for git
# *.nano/
```

## 7. Schema changes: migrate, don't rebuild

When the schema needs to change, edit `<db>/schema.pg` and run `nanograph migrate`. Do not rebuild the database from scratch unless you're certain you have a complete seed file.

```bash
# Right — incremental migration
nanograph migrate

# Wrong — blows away all data including mutations applied since last seed
nanograph init --schema new-schema.pg
nanograph load --data seed.jsonl --mode overwrite
```

Key migration rules:

- **Adding a nullable property** is safe — auto-applied.
- **Adding a new node/edge type** is safe — auto-applied.
- **Renaming** requires `@rename_from("old_name")` in the schema so data is preserved:
  ```graphql
  node Customer @rename_from("Client") {
      slug: String @key
  }
  ```
- **Adding a non-nullable property to a populated type** is blocked — make it nullable or provide a default in a new seed load.
- **Dropping a property** requires `--auto-approve`.
- **Adding enum values** works immediately in mutations without migration — the enum set in the schema is expanded at parse time.

Use `nanograph schema-diff --from old.pg --to new.pg` to preview changes before editing the DB schema.

If a migration fails and leaves a stale journal file (`*.migration.journal.json` in PREPARED state), delete the journal manually before retrying any db command.

Migration reads the schema from `<db>/schema.pg`, not from your project's spec directory. Copy your updated schema into the DB folder before migrating, or use a config where both point to the same file.

## 8. Use `nanograph describe` and `nanograph doctor` for orientation

Before operating on a database, inspect its current state:

```bash
# Schema summary with row counts
nanograph describe

# Single type with agent-facing metadata (JSON)
nanograph describe --type Signal --format json

# Full integrity check
nanograph doctor
```

`describe --format json` returns structured metadata including `@description`, `@instruction`, key properties, edge summaries, and row counts. This is the best way for an agent to understand a graph's structure before querying.

## 9. Define aliases for every operation agents will use

Aliases are the single most effective way to prevent agent errors. Without them, agents construct long `--query`/`--name`/`--param`/`--format` commands from memory and get them wrong. With aliases, every operation becomes a short positional call that's hard to break.

**Cover reads, mutations, and lookups.** A well-aliased project has one alias per agent-facing operation:

```toml
# Reads
[query_aliases.search]
query = "queries.gq"
name = "semantic_search"
args = ["q"]
format = "json"

[query_aliases.pipeline]
query = "queries.gq"
name = "pipeline_summary"
format = "json"

# Lookups
[query_aliases.client]
query = "queries.gq"
name = "client_lookup"
args = ["slug"]
format = "json"

# Mutations
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
# Agents use these — no flags to remember
nanograph run search "renewal risk"
nanograph run client cli-acme
nanograph run add-task task-42 "Draft proposal" open
nanograph run status task-42 done
```

**Alias design rules:**

- **Always set `format`** in the alias. Use `json` for reads (agents get metadata + rows), `jsonl` for mutations (one confirmation record). Don't rely on agents remembering `--format`.
- **Use short, verb-like names.** `search`, `add-task`, `status`, `trace` — not `run_semantic_search_query` or `update_task_status_mutation`. Agents type these frequently.
- **Match `args` order to natural usage.** Put the scoping argument first (e.g. slug), then the action or search term. `nanograph run signals cli-acme "renewal"` reads naturally.
- **List your aliases in `CLAUDE.md` or `AGENTS.md`.** Agents can't discover aliases from `nanograph.toml` unless told to look. A simple list like "Available aliases: `search`, `client`, `add-task`, `status`" is enough.
- **Don't create aliases that duplicate CLI commands.** `nanograph describe` and `nanograph doctor` already work — aliases are for queries defined in `.gq` files.

## 10. JSONL data format: nodes vs. edges

Getting the JSONL format wrong is a common agent error.

**Nodes** use `"type"` + `"data"`:
```json
{"type": "Client", "data": {"slug": "cli-acme", "name": "Acme Corp", "status": "active"}}
```

**Edges** use `"edge"` + `"from"` + `"to"`:
```json
{"edge": "SignalAffects", "from": "sig-renewal", "to": "cli-acme"}
```

Common mistakes:
- Using `{"type": "Knows", "src": "alice", "dst": "bob"}` for edges — `type` makes it a node record; `src`/`dst` are not valid keys
- Omitting `@key` values in `from`/`to` — these must be the key property values of the endpoint nodes, not IDs
- Using PascalCase inconsistently — type/edge names in JSONL match the schema PascalCase exactly (unlike queries which use camelCase for edges)

## 11. Search: scope with graph traversal, then rank

nanograph's strength is combining graph structure with search. Don't search the entire graph when you can narrow first:

```graphql
// Wrong pattern — global search, then filter in application code
query bad_search($q: String) {
    match { $s: Signal }
    return { $s.slug, $s.summary, nearest($s.embedding, $q) as score }
    order { nearest($s.embedding, $q) }
    limit 20
}
// ... then filter by client in agent code

// Right pattern — graph-constrained search
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

The second query traverses first, then ranks only within the relevant subgraph. This is faster, more precise, and avoids agents having to post-filter results.

## 12. Maintenance: compact and cleanup periodically

After many mutations, Lance datasets accumulate deletion markers and fragmented files. Run maintenance:

```bash
# Compact fragmented datasets
nanograph compact

# Prune old transaction/CDC history
nanograph cleanup --retain-tx-versions 50

# Verify integrity
nanograph doctor
```

Don't run `compact` after every single mutation — batch your mutations and compact periodically (e.g. after a workflow completes).

## 13. Always typecheck after editing `.gq` or `.pg` files

Agents frequently edit query or schema files and move on without validating. This leaves broken queries or invalid schemas that fail at runtime — sometimes much later, in a different workflow.

**After editing a `.gq` file**, run `check` immediately:

```bash
nanograph check --query queries.gq
```

This catches wrong property names, type mismatches, invalid traversals, missing parameters, and malformed mutations before they reach execution. It is fast and costs nothing.

**After editing a `.pg` schema file**, validate it the same way — `check` parses and compiles the schema as part of query validation. If the schema itself is broken, `check` will report parse errors. For schema changes to an existing database, also preview the migration plan before applying:

```bash
# Preview what migration would do
nanograph migrate --dry-run

# Or diff two schema files without a database
nanograph schema-diff --from old.pg --to new.pg
```

**Make this a hard rule:** no `.gq` or `.pg` edit is complete until `check` passes. Agents that skip this step produce silent errors that compound — a typo in a mutation query can go unnoticed until a user tries to run it days later.

## 14. Backfill embeddings after `@embed` changes

When you add a new `@embed(source_prop)` property to the schema, or change the source property or embedding settings, existing rows won't have vectors. Agents often forget this step and then get empty results from `nearest()` queries.

```bash
# Fill only rows where the vector is currently null
nanograph embed --only-null

# Recompute all embeddings (e.g. after changing model or chunk settings)
nanograph embed

# Restrict to one type
nanograph embed --type Signal --only-null

# Preview what would be embedded without writing
nanograph embed --dry-run
```

After backfilling, if the property has `@index`, add `--reindex` to rebuild the vector index:

```bash
nanograph embed --only-null --reindex
```

## 15. Follow the post-change workflow

Schema and query changes involve multiple steps that must happen in order. Agents that skip steps end up with typechecking errors, missing embeddings, or a DB schema that's drifted from the spec. Follow this checklist every time:

1. **Edit** `.pg` and/or `.gq` files
2. **Typecheck**: `nanograph check --query queries.gq`
3. **If schema changed**: `nanograph migrate` (use `--dry-run` first to preview)
4. **If `@embed` fields added or changed**: `nanograph embed --only-null`
5. **Smoke test**: `nanograph run <alias>` to confirm runtime works

Don't skip steps 2-4. Don't reorder them — migrating before typechecking means you might apply a broken schema, and querying before embedding means search returns no results.

## 16. Keep nanograph up to date

nanograph is under active development. New releases fix bugs, improve migration reliability, add query features, and tighten agent-facing output. Agents that operate against an old CLI version hit avoidable errors — especially around schema migration, CDC, and search.

```bash
# Check current version
nanograph version

# Update via Homebrew
brew upgrade nanograph

# Or rebuild from source
cargo install nanograph-cli
```

Make `brew upgrade nanograph` part of your project setup. When something behaves unexpectedly, check the version first — the fix may already be shipped.

## 17. Treat `nanograph.toml` as the operational backbone

`nanograph.toml` is not optional boilerplate — it is the primary interface contract between the project and agents operating on it. A well-configured `nanograph.toml` means agents can run short, predictable commands instead of constructing long flag-heavy invocations that are easy to get wrong.

A complete config should include:

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
provider = "mock"          # or "openai" for real embeddings

[cli]
output_format = "table"

# One alias per common operation — reads and mutations
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

Without this config, agents must pass `--db`, `--query`, `--name`, and `--format` on every command. They will get paths wrong, forget flags, and produce unparseable output. With it, `nanograph run search "renewal risk"` just works.

**Aliases for mutations are especially important.** They turn a fragile multi-flag command into a simple positional call that agents can't easily break:

```bash
# Without alias — agents get this wrong constantly
nanograph run --query queries.gq --name update_task_status --param slug=task-123 --param status=done --format jsonl

# With alias — hard to mess up
nanograph run status task-123 done
```

## 18. Always use `--json` or `--format json` for agent output

`table` output is for humans reading a terminal. Agents should never parse it — column widths change, values get truncated, and multiline content is collapsed. Always use structured output.

**For `nanograph run`** (queries and mutations), use `--format json` or `--format jsonl`:

```bash
# json — single object with metadata + rows array
nanograph run search "renewal risk" --format json

# jsonl — metadata header record, then one record per row
nanograph run search "renewal risk" --format jsonl
```

`json` wraps results in an object with `query_name`, `description`, `instruction` (from `@description`/`@instruction` annotations), and `rows`. This gives agents both the data and the context to interpret it.

**For other commands**, use the `--json` global flag:

```bash
# Structured schema metadata
nanograph describe --json

# Structured migration plan
nanograph migrate --dry-run --json

# Structured integrity report
nanograph doctor --json
```

Set `format = "json"` in your query aliases so agents get structured output by default without remembering to pass the flag.

## 19. Use `describe` as the agent's entry point to a graph

Before an agent writes queries, runs mutations, or ingests data, it should call `describe` to understand the graph's structure. This is especially important when the schema includes `@description` and `@instruction` annotations — they tell the agent what each type means and how to use it.

```bash
# Overview: all types, row counts, relationships
nanograph describe --format json

# Deep dive: one type with properties, annotations, edge summaries
nanograph describe --type Signal --format json
```

The JSON output includes:
- `description` and `instruction` for each type (from `@description`/`@instruction`)
- `key_property` — which field to use in mutations and edge resolution
- Property names, types, and descriptions
- Incoming and outgoing edge summaries with endpoint key metadata

This is everything an agent needs to construct correct queries, mutations, and JSONL data without guessing. Agents that skip `describe` and try to infer the schema from query results or JSONL files make systematic errors — wrong property names, missing required fields, incorrect edge wiring.

**Pattern for agent workflows:**

1. `nanograph describe --format json` — learn the schema
2. `nanograph check --query queries.gq` — verify queries are valid
3. `nanograph run <alias> ...` — execute queries and mutations

## 20. Never commit `.env.nano` — always gitignore it

`.env.nano` holds secrets like `OPENAI_API_KEY`. Agents creating new projects frequently forget to gitignore it, or worse, commit it and push.

Add this to `.gitignore` at project setup — before the first commit:

```
.env.nano
```

If an agent has already committed `.env.nano`:

```bash
# Remove from tracking without deleting the file
git rm --cached .env.nano
echo ".env.nano" >> .gitignore
git add .gitignore
git commit -m "stop tracking .env.nano"
```

Then rotate the exposed API key immediately.

Keep secrets in `.env.nano` and shared defaults in `nanograph.toml`. Never put raw API keys in `nanograph.toml` — use `api_key_env = "OPENAI_API_KEY"` to reference the env var by name.

## 21. Add nanograph rules to your project's `CLAUDE.md` or `AGENTS.md`

AI agents read `CLAUDE.md` and `AGENTS.md` at the repo root for project-specific instructions. If your project uses nanograph, add operational rules there — otherwise agents will discover the graph by trial and error and repeat every mistake in this guide.

A minimal nanograph section:

```markdown
## NanoGraph

This project uses a nanograph database at `app.nano`.

- Always run nanograph commands from the directory where `nanograph.toml` lives
- Use `nanograph describe --format json` to understand the schema before querying
- Use mutation queries for data changes — never export/edit/reimport JSONL
- Use `nanograph run <alias>` with aliases defined in `nanograph.toml` — do not construct raw `--query`/`--name`/`--param` commands
- After editing any `.gq` or `.pg` file, run `nanograph check --query <file>`
- Never commit `.env.nano`
- Use `--format json` or `--format jsonl` for machine-readable output
```

Tailor this to your project. Include:
- Where the database and config live relative to the repo root
- Which aliases are available and what they do
- Which node types are mutable vs. append-only (if applicable)
- Any project-specific conventions (slug prefixes, required fields, etc.)

Without these instructions, every new agent session starts from zero. With them, agents operate the graph correctly from the first command.

## Quick reference: common agent mistakes

| Mistake | Fix |
|---------|-----|
| Full reingestion to change one record | Use `update` mutation query |
| Running from wrong directory | `cd` to where `nanograph.toml` lives |
| Missing `@key` on node types | Add `slug: String @key` to every node type |
| PascalCase edge names in queries | Use camelCase: `HasMentor` → `hasMentor` |
| Using `type` key for edge JSONL records | Use `edge` key with `from`/`to` |
| Rebuilding DB instead of migrating | Use `nanograph migrate` with `@rename_from` |
| Not committing schema/queries/database to git | Commit `.pg`, `.gq`, `nanograph.toml`, seed `.jsonl`, and `*.nano/` (if under ~50 MB) |
| Committing `.env.nano` with API keys | Add `.env.nano` to `.gitignore` before first commit |
| Parsing `table` output programmatically | Use `--format json` or `--format jsonl` |
| No `@description`/`@instruction` on types | Add metadata so agents can self-orient via `describe` |
| No aliases or missing `format` in aliases | Alias every agent operation with `format` set |
| Guessing schema instead of calling `describe` | `nanograph describe --format json` first |
| Global search + post-filter | Graph-constrained search (traverse, then rank) |
| Editing `.gq`/`.pg` without typechecking | Run `nanograph check --query <file>` after every edit |
| Hardcoded slugs/values in mutations | Parameterize everything — no single-use queries |
| Missing embeddings after `@embed` change | Run `nanograph embed --only-null` after schema changes |
| Skipping steps after schema/query edits | Follow the 5-step post-change workflow |
| Running on outdated CLI version | `brew upgrade nanograph` regularly |
| No nanograph rules in `CLAUDE.md`/`AGENTS.md` | Add operational rules so agents don't start from zero |

## See also

- [CLI Reference](cli-reference.md) — all commands and options
- [Schema Language Reference](schema.md) — types, annotations, naming
- [Query Language Reference](queries.md) — match, return, traversal, mutations
- [Search Guide](search.md) — text search, vector search, hybrid ranking
- [Project Config](config.md) — `nanograph.toml`, `.env.nano`, aliases
