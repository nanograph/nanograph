# Codebase Agent Workspace

Persistent planning graph for AI agents. Issues, dependencies, decisions, and audit trail. Inspired by [beads](https://github.com/gastownhall/beads).

**Thesis:** Agents need persistent structured memory for long-horizon work. What's ready to work on, what's blocked, who decided what, what's been tried. This example models that memory as a typed property graph: issues as the primary entity, dependency edges for the plan, decisions as append-only organizational memory, and events as the audit trail.

This is **not** a code store. Files, imports, and commits live in git. This graph is the layer above: the plan, the rationale, and the record.

## Files

| File | Description |
|------|-------------|
| `codebase.pg` | Schema: Issue, Agent, Decision, Event, Module, Constraint + dependency, scoping, and audit edges |
| `codebase.gq` | 55 queries: ready, blockers, my_queue, similar_issues, claim_issue, decisions_for, etc. |
| `codebase.jsonl` | Seed data: 18 issues across 2 epics, 4 decisions, 12 events, dependency chains |
| `nanograph.toml` | Query aliases (`ready`, `queue`, `blockers`, `claim`, `similar`, `why`, ...) |

## Quick Start

```bash
cd examples/codebase

nanograph init
nanograph load --data codebase.jsonl --mode overwrite
nanograph embed
nanograph lint --query codebase.gq

# The headline queries
nanograph run ready
nanograph run queue claude
nanograph run blockers ng-rt03
nanograph run similar "refresh token rotation"
nanograph run why "rate limiter"

# Full context
nanograph run detail ng-rt02
nanograph run decisions ng-rt02
nanograph run history ng-rt02
```

## Data Model

Six node types, fourteen edge types.

### Nodes

- **Issue** - the primary work unit. Tasks, bugs, features, epics, messages are all Issues with a `type` discriminator. Slug-keyed (`ng-a1b2` style).
- **Agent** - human or AI that works on issues. Has capabilities.
- **Decision** - append-only rationale with embedded reasoning for semantic search.
- **Event** - append-only audit log; one record per state change.
- **Module** - logical grouping (area, component, domain).
- **Constraint** - rule that must pass before an issue can close.

### Edge groups

| Group | Edges |
|-------|-------|
| Dependency | `Blocks`, `RelatesTo`, `Duplicates`, `Supersedes`, `ParentOf`, `RepliesTo` |
| People | `Assigned`, `Authored` |
| Grouping & gating | `Scopes`, `Expert`, `Requires`, `AppliesTo` |
| Knowledge & audit | `Explains`, `Records` |

## Headline Queries

### `ready` - the money query

```
query ready() {
    match {
        $i: Issue { status: "open" }
        not {
            $b blocks $i
            $b.status != "closed"
        }
    }
    return { $i.slug, $i.title, $i.type, $i.priority }
    order { $i.priority }
}
```

Returns open issues that are not blocked by any still-open issue. Ordered by priority. This is the first thing an agent asks on wake-up.

### `similar_issues` - dedupe before creating

```
query similar_issues($q: String) {
    match { $i: Issue }
    return {
        $i.slug, $i.title, $i.status
        rrf(nearest($i.descriptionEmbedding, $q), bm25($i.description, $q)) as hybrid_score
    }
    order { rrf(nearest($i.descriptionEmbedding, $q), bm25($i.description, $q)) desc }
    limit 10
}
```

Reciprocal rank fusion over semantic and lexical rankings. "Has anyone already filed this?"

### `blockers` and `blocking_chain`

```
nanograph run blockers ng-rt03     # direct blockers
nanograph run chain ng-rt03        # two-hop transitive
nanograph run unblocks ng-rt02     # what closing this unblocks
```

### `decisions_for` - why did we do this?

Traverses `Decision -> Explains -> Issue` to surface prior rationale. The `why` alias runs semantic search across all decisions.

### `claim_issue` - lifecycle mutation

```
nanograph run claim ng-bug01 claude
```

Sets `status=in_progress`, `claimedBy=claude`, `claimedAt=now()`, `updatedAt=now()` atomically in a single mutation. See the gaps note below on multi-agent race safety.

## Query Catalog

### Workflow
| Alias | Query | Args |
|-------|-------|------|
| `ready` | `ready` | - |
| `queue` | `my_queue` | agent |
| `active` | `active_work` | - |

### Dependency Graph
| Alias | Query | Args |
|-------|-------|------|
| `blockers` | `blockers` | slug |
| `chain` | `blocking_chain` | slug |
| `unblocks` | `blocked_by_me` | slug |

### Issue Detail & Listing
| Alias | Query | Args |
|-------|-------|------|
| `detail` | `issue_detail` | slug |
| `open` | `open_issues` | - |
| `unassigned` | `unassigned_open` | - |
| `my` | `agent_issues` | agent |

### Search
| Alias | Query | Args |
|-------|-------|------|
| `find` | `search_issues` | q |
| `similar` | `similar_issues` | q |

### Epic Hierarchy
| Alias | Query | Args |
|-------|-------|------|
| `tree` | `epic_tree` | epic |
| `progress` | `epic_progress` | epic |
| `epics` | `open_epics` | - |

### Messages
| Alias | Query | Args |
|-------|-------|------|
| `thread` | `thread` | root |

### Decisions
| Alias | Query | Args |
|-------|-------|------|
| `why` | `search_decisions` | q |
| `decisions` | `decisions_for` | slug |

### Modules
| Alias | Query | Args |
|-------|-------|------|
| `mod` | `issues_in_module` | mod |
| `experts` | `module_experts` | mod |
| `reviewers` | `review_candidates` | slug |
| `backlog` | `module_backlog` | - |

### Constraints
| Alias | Query | Args |
|-------|-------|------|
| `checks` | `issue_constraints` | slug |
| `mod-checks` | `module_constraints` | mod |

### Audit
| Alias | Query | Args |
|-------|-------|------|
| `history` | `recent_events` | slug |

### Mutations
| Alias | Query | Args |
|-------|-------|------|
| `claim` | `claim_issue` | slug, agent |
| `close` | `close_issue` | slug, reason |
| `review` | `submit_for_review` | slug |

## Seed Data: Acme API Backlog

Three agents coordinating across two epics plus standalone bugs, features, and maintenance:

- **Claude** (AI) - security and testing focus. Claimed `ng-rt02` (refresh endpoint) and `ng-bug01` (Safari JWT bug).
- **Devin** (AI) - feature development, frontend. Claimed `ng-ua01` (usage meter) and `ng-bug02` (billing double-charge).
- **Sarah** (human) - architecture and review. Authored most epics, reviews everything.

**The interesting scenarios:**

1. **Dependency chains.** `ng-rt02` blocks `ng-rt03`, `ng-rt04`, and `ng-feat01`. Run `nanograph run blockers ng-rt03` to see the chain. Run `nanograph run unblocks ng-rt02` to see what a merge of Claude's work releases.

2. **Semantic dedup.** Try `nanograph run similar "refresh token"` - it surfaces the whole refresh-token epic plus related bugs, ranked by a hybrid of semantic distance and BM25.

3. **Organizational memory.** `nanograph run why "rate limiter"` finds the `dec-redis-ratelimit` decision explaining why the in-memory limiter was replaced. Agents can surface prior rationale before re-litigating.

4. **Review routing.** `nanograph run reviewers ng-rt02` traverses `Issue -> Scopes -> Module <- Expert - Agent` to find Claude and Sarah as auth-module experts.

5. **Audit trail.** `nanograph run history ng-rt02` shows the Event stream: claimed, lint passed, typecheck passed.

## vs. beads

This example borrows beads' data model (Issue with dependency edges, hierarchical epics, message threading, append-only audit). nanograph brings typed schema, first-class hybrid search, and distinct Decision/Event node types with semantic indexing. beads brings versioned storage (Dolt), race-safe atomic claims, and a mature agent-facing CLI.

Use beads if you want a drop-in issue tracker with multi-agent write safety and no deployment concerns. Use this example (or fork it) if you are already on nanograph, want typed graph queries across plan/decisions/audit, or want embedded hybrid search over issue descriptions as part of the same engine.

## Known Gaps

1. **Race-safe claim.** `claim_issue` is last-write-wins because nanograph's `update ... where` does not yet support a compound predicate like `where slug = $slug AND claimedBy IS NULL`. For multi-agent use, add your own check-then-update sequence through the audit log or wait for grammar support.
2. **No versioning.** Two agents writing to the same DB stomp each other. There is no cell-level merge. Use one writer or add an external sync layer.
3. **Compaction is manual.** beads summarizes old closed issues to save agent context window; here that would be a bespoke mutation rewriting `description` on closed issues. Not built in.

## Adding Data

```bash
nanograph load --data new-issues.jsonl --mode merge
```

Mutations via `nanograph run`:

```bash
nanograph run claim ng-bug01 claude
nanograph run close ng-bug01 "Fixed in commit abc123"
```

To evolve the schema, edit `codebase.nano/schema.pg` then run `nanograph migrate`.
