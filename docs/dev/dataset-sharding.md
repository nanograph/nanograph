# Dataset Sharding: Per-Type vs Unified

Status: analysis
Date: 2026-03-20

nanograph currently uses one Lance dataset per node type and one per edge type. This document analyzes the tradeoffs and alternatives.

## Current Architecture

```
my_graph.nano/
├── nodes/a1b2c3d4/   # Person Lance dataset (id, name, age)
├── nodes/b2c3d4e5/   # Company Lance dataset (id, name, founded, revenue)
├── edges/e5f6a7b8/   # Knows Lance dataset (id, src, dst, since)
└── edges/f6a7b8c9/   # WorksAt Lance dataset (id, src, dst, role, salary)
```

Each type has a different Arrow schema:

```
Person:  {id: U64, name: Utf8, age: I32}
Company: {id: U64, name: Utf8, founded: Date32, revenue: F64}
Knows:   {id: U64, src: U64, dst: U64, since: Date32}
WorksAt: {id: U64, src: U64, dst: U64, role: Utf8, salary: F64}
```

Arrow/Lance is columnar — every row in a dataset must have the same schema.

## Why Per-Type

### Does it enforce type safety?

**No.** Type safety enforcement happens at the load layer (`jsonl.rs:274`, `jsonl.rs:905`), not at the Lance storage layer. Lance doesn't reject writes with nulls in non-nullable columns — nanograph's JSONL parser does, before the data ever reaches Lance. With a unified dataset, the same loader code runs the same checks at the same point.

### What it actually buys

1. **Fragment statistics are clean.** A Person fragment's min/max for `age` reflects real Person data. In a unified dataset, if Person and Company rows end up in the same fragment, the statistics for `age` are diluted — Lance can't prune that fragment even though half the rows are irrelevant. You can mitigate this by partitioning fragments by type, but then you're manually reimplementing what per-type datasets give you automatically.

2. **Indexes are scoped.** A BTree index on `Person.age` covers only Person rows. In a unified dataset, the index on `age` spans all node types — Company rows contribute NULL entries that bloat the index and waste I/O. An FTS index on `Person.bio` would also index Company rows (returning nothing useful). You'd need to pair every index scan with a `_type = 'Person'` filter.

3. **Schema evolution is isolated.** Adding a property to Person doesn't touch the Company dataset. In a unified dataset, adding a column widens the schema for all types. Lance handles this efficiently (metadata-only for all-NULL additions), so this is a minor advantage.

4. **Compaction is independent.** `compact` on a heavily-mutated type doesn't rewrite fragments belonging to other types.

### What it costs

~3,200 lines of custom cross-dataset transaction coordination (N5). This is the dominant engineering cost of the persistence layer.

## Unified Alternative

Two datasets total (all-nodes, all-edges) with a `_type` discriminator column:

```
nodes dataset (unified):
  Schema: {id: U64, _type: Utf8, name: Utf8?, age: I32?, founded: Date32?, revenue: F64?}

  Fragment 0 (Person rows):  data files for {id, _type, name, age}
  Fragment 1 (Company rows): data files for {id, _type, name, founded, revenue}
```

Lance supports fragment-level schema heterogeneity. Different fragments within one dataset can have different columns. Missing columns read as NULL. This is how `add_columns()` works internally.

### What this buys

Cross-type atomicity for free. One `dataset.append()` / `dataset.delete()` call. No custom manifest, no tx catalog, no cross-dataset coordination. N5 disappears entirely.

### What this costs

1. **All property columns become nullable at the storage level.** Even if the schema says `age: I32` (non-nullable), the unified dataset must declare `age: I32?` because Company rows don't have it. Type safety enforcement moves from Lance to the load/query layer — but it was already there.

2. **Indexes are dataset-wide.** A BTree index on `age` spans all node types. Bitmap on `_type` gives fast type filtering. Every index scan must pair with `_type` filter.

3. **Fragment statistics dilution is avoidable.** If you partition fragments by type (Person rows in their own fragments, Company in theirs), min/max statistics stay meaningful. A scan for `WHERE _type = 'Person' AND age > 25` prunes Company fragments entirely via the `_type` bitmap index, then uses `age` statistics within Person fragments.

4. **Schema evolution is wider.** Adding a property to Person adds a nullable column to the unified dataset. All existing fragments are unaffected — metadata-only operation for all-NULL additions.

## Assessment

Per-type sharding is the cleaner data model. But it created a coordination problem (N5) that dominates the engineering cost of the entire persistence layer. None of the per-type benefits are dealbreakers for the unified approach — they're all workable with fragment-by-type partitioning and `_type` filters.

The tradeoff: per-type datasets buy schema isolation + effective pushdown at the cost of cross-dataset coordination. A unified dataset would eliminate coordination but sacrifice some columnar advantages.
