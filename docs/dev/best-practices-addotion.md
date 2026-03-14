# nanograph — Best Practices

## Schema

- `spec/schema.pg` is the source of truth. The DB keeps its own copy at `omni.nano/schema.pg`.
- After editing `spec/schema.pg`, always migrate: `ngdev migrate omni.nano --auto-approve`
- After adding or changing `@embed` fields, backfill embeddings: `ngdev embed --only-null`

## Queries

- After editing any `.gq` file, always typecheck: `ngdev check --query <file>`
- All mutations must be parameterized — no hardcoded slugs or dates.
- Use `now()` for all timestamp fields (`createdAt`, `updatedAt`, `observedAt`, `executedAt`).
- Removed the old hardcoded `add_signal` and `advance_stage` examples — they existed as demos but were not safe to run.

## Aliases

Convention: **lookup aliases are nouns, vector search aliases are `{type}-vect`**.

| Alias | Query | Args |
|-------|-------|------|
| `clients` | `all_clients` | — |
| `client` | `client_lookup` | `name` |
| `pipeline` | `pipeline_summary` | — |
| `stage` | `pipeline_by_stage` | `stage` |
| `trace` | `full_trace` | `sig` |
| `tasks` | `unresolved_tasks` | — |
| `opp-vect` | `search_opportunities` | `q` |
| `signal-vect` | `search_signals` | `q` |
| `project-vect` | `search_projects` | `q` |
| `task-vect` | `search_tasks` | `q` |
| `decision-vect` | `search_decisions` | `q` |

Args are positional, so `nanograph run signal-vect "neo4j"` works — no `--param` needed.

## Workflow after changes

1. Edit `.pg` or `.gq` files
2. `ngdev check --query spec/revops.gq`
3. If schema changed: `ngdev migrate omni.nano --auto-approve`
4. If `@embed` fields added/changed: `ngdev embed --only-null`
5. Smoke test: `nanograph run <alias>` to confirm runtime works

## CLI

- Use `nanograph` (stable) for day-to-day queries and mutations.
- Use `ngdev` for typechecking, migrations, schema diffs, and embedding.
- Use mutations for individual record changes, not JSONL loads.
