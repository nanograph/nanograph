---
title: Database Folder Structure
slug: folder-structure
---

# Database Folder Structure

A nanograph database is a single directory with the `.nano` suffix. Everything the engine needs to open, query, and evolve the graph lives inside that folder, so it is easy to move, back up, or delete.

## Layout

```
<name>.nano/
├── schema.pg                 # human-authored schema source (.pg DSL)
├── schema.ir.json            # compiled schema IR used at runtime
├── _embedding_cache.jsonl    # content-hashed embedding cache (only if @embed is used)
├── __graph_snapshot/         # Lance table: committed graph snapshot payload
├── __graph_tx/               # Lance table: committed transaction windows (CDC)
├── __graph_deletes/          # Lance table: delete tombstones for lineage-native CDC
├── __graph_changes/          # Lance table: legacy CDC log (older graphs only)
├── __blob_store/             # Lance table: managed imported media blobs
├── nodes/<type_id_hex>/      # one Lance dataset per node type
└── edges/<type_id_hex>/      # one Lance dataset per edge type
```

Type ID directories under `nodes/` and `edges/` are named by the FNV-1a hash of `"node:TypeName"` or `"edge:TypeName"`, rendered as lowercase u32 hex.

## File and directory reference

### Schema

- `schema.pg` is the source of truth that you edit. It is plain text in the nanograph schema DSL.
- `schema.ir.json` is the compiled, validated schema IR. The engine rewrites it whenever the schema changes via `nanograph migrate`. Do not edit it by hand.

### Graph data

- `nodes/<type_id_hex>/` holds one Lance dataset per node type declared in the schema. Rows are typed property records.
- `edges/<type_id_hex>/` holds one Lance dataset per edge type. CSR and CSC adjacency indices are built in memory from these datasets at open time.

### Snapshot and CDC

- `__graph_snapshot/` stores the committed graph snapshot payload that backs the `GraphManifest`.
- `__graph_tx/` stores committed transaction windows. New graphs use the `NamespaceLineage` generation, which reconstructs CDC from Lance lineage plus this table.
- `__graph_deletes/` records delete tombstones so lineage-native CDC can report removals.
- `__graph_changes/` only exists on legacy graphs created before lineage-native CDC. `nanograph storage migrate --target lineage-native` retires it.

### Media and embeddings

- `__blob_store/` is used when media is imported through the managed blob workflow. Databases that only reference external URIs will not have this table.
- `_embedding_cache.jsonl` caches embeddings keyed by content hash so repeated loads do not re-call the embedding API. It only appears if at least one property uses `@embed`.

## What is not in the folder

Project configuration lives outside `.nano/`:

- `nanograph.toml` sits next to the database and holds shared defaults, query roots, and aliases.
- `.env.nano` sits next to the database and holds local secrets such as `OPENAI_API_KEY`. It is gitignored by the `nanograph init` scaffold.

See [`config.md`](config.md) for the full config reference.

## Operational notes

- The whole `.nano/` directory is safe to copy, tar, or sync while the database is closed. Do not copy a live database mid-write.
- `nanograph compact` and `nanograph cleanup` reclaim space inside the Lance datasets. `nanograph doctor` checks the folder for structural issues.
- Deleting the `.nano/` folder removes the database. Re-running `nanograph init` and `nanograph load` rebuilds it from source data.
- Version control: `schema.pg` is worth committing. The Lance datasets under `nodes/`, `edges/`, and the `__graph_*` tables are binary and regenerable, so most projects gitignore the `.nano/` folder and keep only schemas and source JSONL in git.
