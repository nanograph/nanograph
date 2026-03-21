# Codebase Agent Workspace

Multi-agent code collaboration graph. Models the "database as a git" paradigm where files are structured nodes, agents work on branches, and import relationships are first-class edges.

**Thesis:** Code is high-velocity data. Agent tool calls (read, write, grep) should be graph operations, not filesystem I/O. Conflict detection, blast radius analysis, and review routing become graph traversals.

## Files

| File | Description |
|------|-------------|
| `codebase.pg` | Schema with mutable pointer nodes, append-only event nodes, and agent-facing metadata |
| `codebase.gq` | Query suite: file ops, search (keyword/semantic/hybrid), import graph, conflict detection, review routing, aggregation, negation, filtering, mutations |
| `codebase.jsonl` | Seed data: "Acme API" project with 3 agents, 15 files, 4 concurrent tasks |

## Quick Start

```bash
cd codebase

nanograph init
nanograph load --data codebase.jsonl --mode overwrite
nanograph embed
nanograph check --query codebase.gq
nanograph run active
nanograph run blast src/auth/jwt.ts
nanograph run conflicts task-usage-alerts
nanograph run why "token revocation"
nanograph run why "token revocation" --format kv
```

The checked-in `nanograph.toml` provides:

- `db.default_path = "codebase.nano"`
- `query.roots = ["."]`
- aliases like `read`, `find`, `search`, `blast`, `conflicts`, `reviewers`, `why`, `hotspots`
- deterministic mock embeddings for description and reasoning fields

`nanograph run` supports `table` (default), `kv`, `csv`, `jsonl`, and `json`.
- `kv` is the record-oriented human view
- `json` wraps result rows with query metadata plus `rows`

See also:

- [Search Guide](../../docs/user/search.md)
- [Project Config](../../docs/user/config.md)

## Schema Overview

### Pointer Nodes (Mutable)

Updated in place. All carry `slug @key`, `createdAt`, `updatedAt`.

- **Module** — logical grouping of files (package, directory)
- **File** — source file with full content, path, language, description
- **Agent** — human or AI, with model and capabilities
- **Task** — unit of work mapping 1:1 to a nanograph branch
- **Constraint** — rule that must pass before merge (lint, typecheck, test, security)

### Append-Only Nodes (Immutable)

Never overwritten. All carry `slug @key`, `createdAt` (no `updatedAt`).

- **Decision** — why something was designed or changed a certain way
- **Review** — verdict on a task branch (approved, changes_requested, blocked)

### Edge Types (11)

**Structure:** Contains (Module→File), Imports (File→File)

**Work:** Assigned (Agent→Task), Modifies (Task→File), Expert (Agent→Module), Governs (Constraint→File)

**Collaboration:** Decides (Decision→File), MadeBy (Decision→Agent), Reviews (Review→Task), ReviewedBy (Review→Agent), References (Task→Task)

## Query Catalog

### File Operations
| Query | Parameters | Description |
|-------|-----------|-------------|
| `read_file` | `$path` | Read a file by path |
| `list_files` | `$mod` | List all files in a module |
| `keyword_search_files` | `$q` | Find files by keyword in descriptions |
| `semantic_search_files` | `$q` | Rank files by semantic similarity |
| `hybrid_search_files` | `$q` | Blend semantic and lexical ranking |
| `search_modules` | `$q` | Semantic search across modules |

### Import Graph
| Query | Parameters | Description |
|-------|-----------|-------------|
| `file_dependencies` | `$path` | What does this file import? |
| `file_dependents` | `$path` | What files import this file? (blast radius) |
| `import_chain` | `$path` | Two-hop transitive dependents |

### Task & Agent Management
| Query | Parameters | Description |
|-------|-----------|-------------|
| `agent_tasks` | `$agent` | Tasks assigned to an agent |
| `task_files` | `$task` | Files modified by a task |
| `task_detail` | `$task` | Full task view: agent, files, branch |
| `active_work` | — | All in-progress tasks with agents |
| `open_tasks` | — | Tasks not yet started |
| `search_tasks` | `$q` | Semantic search across task descriptions |

### Conflict Detection
| Query | Parameters | Description |
|-------|-----------|-------------|
| `file_conflicts` | `$task` | Other tasks modifying the same files |
| `import_conflicts` | `$task` | Tasks modifying dependencies of your files |

### Review Routing
| Query | Parameters | Description |
|-------|-----------|-------------|
| `review_candidates` | `$task` | Experts in modules this task touches |
| `task_constraints` | `$task` | Constraints that must pass before merge |
| `task_reviews` | `$task` | Review history on a task |

### Decisions
| Query | Parameters | Description |
|-------|-----------|-------------|
| `search_decisions` | `$q` | Semantic search across past decisions |
| `keyword_search_decisions` | `$q` | Keyword search in decision reasoning |
| `file_decisions` | `$path` | Decisions affecting a specific file |
| `decision_trace` | `$task` | Decisions linked to files a task modifies |

### Agent Expertise
| Query | Parameters | Description |
|-------|-----------|-------------|
| `module_experts` | `$mod` | Who knows this module? |
| `agent_expertise` | `$agent` | What modules does this agent know? |

### Aggregation
| Query | Parameters | Description |
|-------|-----------|-------------|
| `files_per_module` | — | File count per module |
| `tasks_per_agent` | — | Task count per agent |
| `most_imported` | — | Files ranked by import count |
| `most_constrained` | — | Files ranked by constraint count |

### Negation
| Query | Parameters | Description |
|-------|-----------|-------------|
| `ungoverned_files` | — | Files with no constraints |
| `unassigned_tasks` | — | Tasks with no agent assigned |
| `orphan_files` | — | Files not in any module |

### Filtering
| Query | Parameters | Description |
|-------|-----------|-------------|
| `recent_tasks` | `$since` | Tasks created after a date |
| `recent_decisions` | `$since` | Decisions made after a date |
| `error_constraints` | — | Constraints with severity = error |

### Mutations
| Query | Parameters | Description |
|-------|-----------|-------------|
| `write_file` | `$slug, $path, ...` | Insert a new file |
| `update_file_content` | `$path, $content, ...` | Update file content |
| `delete_file` | `$path` | Delete a file |
| `create_task` | `$slug, $title, ...` | Create a new task |
| `start_task` | `$slug` | Move task to in_progress |
| `submit_for_review` | `$slug` | Move task to in_review |
| `merge_task` | `$slug` | Move task to merged |
| `add_decision` | `$slug, ...` | Record a new decision |
| `add_review` | `$slug, ...` | Add a review verdict |

## Seed Data: Acme API

The seed data models a TypeScript API project with three agents working concurrently:

1. **Claude** (AI) — working on refresh token rotation (`feat/refresh-tokens`), modifying auth middleware and JWT module
2. **Devin** (AI) — building usage threshold alerts (`feat/usage-alerts`), modifying billing meter and API routes
3. **Sarah** (human) — reviewed the Redis rate limiter migration, expert on auth and core modules

**The interesting conflict:** Claude's refresh token task and Devin's usage alerts task both modify files that interact through the import graph. The `file_conflicts` query surfaces direct overlaps. The `import_conflicts` query finds subtler dependency-level conflicts.

**The review routing:** When checking reviewers for Claude's refresh token task, the graph traverses Modifies→File→Contains→Module→Expert and finds Sarah (expert on auth module with high confidence).

**The decision memory:** The rate limiter was explicitly designed as a temporary in-memory solution. Searching decisions for "rate limiter temporary" surfaces the original reasoning, giving the agent context before starting the Redis migration.

## Adding Data

```bash
nanograph load --data your-data.jsonl --mode merge
```

To evolve the schema, edit `codebase.nano/schema.pg` then run `nanograph migrate`.
