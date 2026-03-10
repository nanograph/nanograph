# RevOps Example — Omni Context Graph

Personal CRM and decision trace system. Demonstrates the complete physics of execution: from intelligence capture through enrichment, screening, decision-making, and execution.

**Design docs:** [Context Graph Example](../../docs/user/context-graph-example.md)

## Files

| File | Description |
|------|-------------|
| `revops.pg` | Schema with trace-friendly node and edge types plus agent-facing metadata |
| `revops.gq` | Query suite for traces, search, aggregation, filtering, and mutations |
| `revops.jsonl` | Seed data for the Stripe Migration scenario |

## Quick Start

```bash
cd examples/revops

nanograph init
nanograph load --data revops.jsonl --mode overwrite
nanograph check --query revops.gq
nanograph run why opp-stripe-migration
nanograph run pipeline
nanograph run signals cli-priya-shah "procurement approval timing"
nanograph run why opp-stripe-migration --format kv
```

The checked-in `nanograph.toml` provides:

- `db.default_path = "omni.nano"`
- `query.roots = ["."]`
- aliases like `why`, `trace`, `value`, `pipeline`, and `signals`
- deterministic mock embeddings for `Signal.summaryEmbedding`

`nanograph run` supports `table` (default), `kv`, `csv`, `jsonl`, and `json`.
- `kv` is the record-oriented human view
- `json` wraps result rows with query metadata plus `rows`
- `jsonl` emits a metadata header first, then row records

See also:

- [Context Graph Example](../../docs/user/context-graph-example.md)
- [Search Guide](../../docs/user/search.md)
- [Project Config](../../docs/user/config.md)

## Schema Overview

### Node Types

**Pointer Nodes (Mutable)** — updated in place, carry `slug @key`, `createdAt`, `updatedAt`, `notes`

- **Client** — contacts and organizations
- **Actor** — humans and AI agents
- **Record** — external artifacts (documents, meetings, enrichment profiles)
- **Opportunity** — deals and pipeline
- **Project** — active engagements
- **ActionItem** — pending work

**Claims & Events (Append-Only)** — never overwritten, carry `slug @key`, `createdAt`

- **Decision** — recorded intent with domain and assertion
- **Signal** — observed intelligence
- **Policy** — versioned business rules (screening gates)
- **Action** — execution log (proof of work)

### Edge Types (21)

**Decision Spine:** MadeBy, DecisionAffects, SignalAffects, InformedBy, ScreenedBy, ResultedIn, TouchedRecord, TouchedOpportunity, SourcedFrom

**Value Loop:** Surfaced, Targets, DecisionGenerates, AssignedTo, Resolves

**Versioning & Structure:** BasedOnPrecedent, Supersedes, ClientOwnsRecord, ClientOwnsOpportunity, ClientOwnsProject, Contains, DerivedFrom

## Query Catalog

### Lookups
| Query | Parameters | Description |
|-------|-----------|-------------|
| `all_clients` | — | All clients |
| `client_lookup` | `$name` | Find client by name |
| `pipeline_by_stage` | `$stage` | Opportunities in a stage |

### Decision Traces
| Query | Parameters | Description |
|-------|-----------|-------------|
| `decision_trace` | `$opp` | Signal -> Decision -> Actor for an opportunity |
| `execution_trace` | `$opp` | Action -> Decision -> Actor for an opportunity |
| `full_trace` | `$sig` | Complete cycle: Signal -> Opportunity -> Decision -> Action |

### Signal-to-Value
| Query | Parameters | Description |
|-------|-----------|-------------|
| `signal_value` | `$sig` | Signal -> Opportunity + affected Client |
| `signal_to_project` | `$sig` | Signal -> Opportunity -> Project (multi-hop) |

### Reverse Traversal
| Query | Parameters | Description |
|-------|-----------|-------------|
| `opportunity_owner` | `$opp` | Client who owns an opportunity |
| `client_records` | `$client` | Records owned by a client |
| `client_projects` | `$client` | Projects owned by a client |

### Enrichment & Screening
| Query | Parameters | Description |
|-------|-----------|-------------|
| `enrichment_from_policy` | `$pol` | Enrichment profiles derived from a policy |
| `policy_versions` | `$key` | Policy versioning chain |

### Task Management
| Query | Parameters | Description |
|-------|-----------|-------------|
| `actor_tasks` | `$actor` | Tasks assigned to an actor |
| `unresolved_tasks` | — | Tasks with no resolving Action (negation) |

### Aggregation
| Query | Parameters | Description |
|-------|-----------|-------------|
| `pipeline_summary` | — | Count and total value by stage |
| `decisions_by_domain` | — | Decision count by domain |

### Filtering
| Query | Parameters | Description |
|-------|-----------|-------------|
| `recent_signals` | `$since` | Signals after a DateTime |
| `successful_actions` | — | Actions where success = true |
| `urgent_signals` | — | Signals with urgency = high |

### Mutations
| Query | Parameters | Description |
|-------|-----------|-------------|
| `add_signal` | — | Insert a new Signal |
| `advance_stage` | — | Update Opportunity stage |
| `complete_task` | — | Complete an ActionItem |
| `remove_cancelled` | — | Delete an ActionItem |

## Seed Data: Stripe Migration Trace

The seed data implements the complete three-phase trace from the [context graph example](../../docs/user/context-graph-example.md):

1. **Intelligence** — Jamie's coffee chat produces a Signal ("Priya hates her vendor"). The Signal surfaces the Stripe Migration opportunity.
2. **Enrichment & Screening** — Andrew decides to make a proposal. The agent builds an Enrichment Profile from the Screening Policy criteria. The decision passes screening.
3. **Execution** — The agent drafts and sends the proposal. The deal advances to Won. A Data Pipeline project is spawned.

## Adding Data

Load additional data with merge mode (upserts by `@key`):

```bash
nanograph load --data your-data.jsonl --mode merge
```

To evolve the schema, edit `omni.nano/schema.pg` then run `nanograph migrate`.
