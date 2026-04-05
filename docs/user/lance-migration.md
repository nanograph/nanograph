---
title: Lance Migration
slug: lance-migration
---

# Lance Storage Format Migration

nanograph 1.0 uses Lance v3, which writes new datasets in Lance storage format `2.2`. Databases created with earlier nanograph versions typically use Lance storage format `2.0`. Both remain readable and operational in nanograph 1.0.

Note: "Lance v3" refers to the Lance SDK/library version that nanograph links against, while "format 2.0" and "format 2.2" refer to the on-disk storage format that the SDK reads and writes.

## Why upgrade to v2.2

### Better compression

This is the biggest direct benefit for nanograph today.

nanograph datasets are mostly made of:

- text fields
- enums and other repetitive categorical values
- dates and timestamps
- graph relationship columns
- vector columns for embeddings

Lance `2.2` improves compression coverage across these kinds of columns. In practice, that means:

- less disk usage
- cheaper backups and copies
- less I/O during open, scan, compact, and cleanup operations

### Safe gradual adoption

This is the second major benefit for nanograph.

nanograph 1.0 can:

- create new databases on `2.2`
- continue opening and operating existing `2.0` databases
- avoid forcing an in-place file-format migration just to upgrade the CLI/runtime

That makes adoption much less risky. You can move to nanograph 1.0 immediately, keep existing databases working, and decide later whether a logical export/rebuild migration to `2.2` is worth it for your project.

## Checking your current format

```bash
nanograph doctor --verbose
```

The output includes a storage format column for each tracked dataset. If some datasets show `2.0`, they are still supported in nanograph 1.0. Migration to `2.2` is optional and mainly worth doing when you want the newer default format and its storage-efficiency benefits. If all datasets already show `2.2`, no migration is needed.

## What the migration preserves and what it does not

**Preserved:** All node and edge records, schema, property values, and graph structure.

**Not preserved:**

- **Legacy CDC history** — old WAL-style transaction and CDC logs do not survive export/reimport. The new database starts with a fresh lineage-native graph history.
- **Dataset versions** — Lance version history is reset. Time-travel to previous dataset versions is no longer possible.
- **Computed embeddings** — vectors from `@embed(...)` fields are stripped during export and regenerated afterward. If using a real embedding provider (`provider = "openai"`), this makes API calls.

## Migration procedure

The migration creates a fresh database alongside the old one. The old database is never modified until the new one is fully verified.

Substitute `<db>` with the actual database name (e.g., `omni`, `starwars`, `app`).

### 1. Pre-flight

```bash
# Confirm migration is needed
nanograph doctor --verbose

# Record current row counts
nanograph describe --json > /tmp/pre-migration-describe.json

# Back up the database directory
cp -r <db>.nano <db>.nano.backup
```

Do not skip the backup. If anything goes wrong, `cp -r <db>.nano.backup <db>.nano` restores the original.

### 2. Export

Export the full graph without embeddings. Embedding vectors are stripped because they will be regenerated cleanly in step 5.

```bash
nanograph export --db <db>.nano --format jsonl --no-embeddings > <db>-export.jsonl
```

### 3. Validate the export

Compare node and edge counts against the live database:

```bash
grep -c '"type"' <db>-export.jsonl    # node count
grep -c '"edge"' <db>-export.jsonl    # edge count
nanograph describe --db <db>.nano     # compare
```

If the schema has been updated since the last load, some exported records may contain values that are invalid under the current schema (e.g., old enum values). Check the export against the schema and fix any mismatches with targeted `jq` or `sed` before loading. Document what was fixed.

### 4. Create a new database

Initialize from the current schema. Use a temporary name — never overwrite the original until verification is complete.

```bash
nanograph init --db <db>-v2.nano --schema <schema_path>
```

Use the schema from `nanograph.toml`'s `schema.default_path`, or from `<db>.nano/schema.pg` if the project does not keep a separate schema file.

### 5. Load the exported data

```bash
nanograph load --db <db>-v2.nano --data <db>-export.jsonl --mode overwrite
```

If the load fails with schema validation errors, fix the export (step 3) and retry.

### 6. Regenerate embeddings

If the schema has any `@embed(...)` properties:

```bash
nanograph embed --db <db>-v2.nano
```

With `provider = "mock"`, this is instant. With `provider = "openai"`, this makes API calls proportional to the data volume. Scope to a single type with `--type <NodeType>` if needed.

### 7. Verify

```bash
# Confirm v2.2 format
nanograph doctor --db <db>-v2.nano --verbose

# Compare row counts
nanograph describe --db <db>-v2.nano --json > /tmp/post-migration-describe.json

# Integrity check
nanograph doctor --db <db>-v2.nano
```

Compare row counts between `/tmp/pre-migration-describe.json` and `/tmp/post-migration-describe.json`. Every node and edge type should match.

Run smoke queries — especially search, traversal, and aggregation:

```bash
nanograph run search "test query" --db <db>-v2.nano
nanograph run --db <db>-v2.nano --query queries.gq --name <query> --param key=value
```

If any check fails, stop. The old database is untouched.

### 8. Swap

Only after all verification passes:

```bash
mv <db>.nano <db>.nano.old
mv <db>-v2.nano <db>.nano
```

If `nanograph.toml` uses `db.default_path`, no config change is needed — the path has not changed.

### 9. Final verification

```bash
nanograph doctor --verbose
nanograph describe
```

### 10. Clean up

Once confident the migration succeeded:

```bash
rm -rf <db>.nano.backup
rm -rf <db>.nano.old
rm <db>-export.jsonl
rm /tmp/pre-migration-describe.json /tmp/post-migration-describe.json
```

Keep the backup for at least a day before deleting.

## Rollback

**Before step 8** (swap): the old database is still in place. Clean up the failed attempt:

```bash
rm -rf <db>-v2.nano
rm <db>-export.jsonl
```

**After step 8** (swap): restore from backup:

```bash
rm -rf <db>.nano
mv <db>.nano.old <db>.nano
```

## Checklist

- [ ] `doctor --verbose` confirms the current storage format (`2.0` or `2.2`)
- [ ] Backup created (`<db>.nano.backup`)
- [ ] Pre-migration row counts saved
- [ ] Export completed with `--no-embeddings`
- [ ] Export counts match live database
- [ ] New database initialized from current schema
- [ ] Data loaded into new database
- [ ] Embeddings regenerated (if applicable)
- [ ] New database shows v2.2 in `doctor --verbose`
- [ ] Row counts match between old and new
- [ ] `doctor` passes on new database
- [ ] Smoke queries return expected results
- [ ] Databases swapped
- [ ] Final `doctor` and `describe` pass
- [ ] Backup and old database cleaned up

## See also

- [Best Practices](best-practices.md)
- [CLI Reference](cli-reference.md) — `doctor`, `export`, `embed` command details
- [Search Guide](search.md) — embedding workflow
