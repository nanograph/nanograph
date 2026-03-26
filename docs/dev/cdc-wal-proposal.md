# CDC Proposal: WAL-First, Parquet as Derived Sink

## Goal

Add first-class CDC to NanoGraph while keeping write durability, crash recovery, and replay semantics strong.

## Core Decision

Use a durable append log (WAL-like) as the source of truth for all writes.  
Generate Parquet CDC files asynchronously from that log.

## Why Not "Write CDC Directly to Parquet on Every Operation"

Direct-to-Parquet CDC on the write path has major drawbacks:

- Parquet is not append-friendly at very small write granularity.
- High write amplification and latency on every mutation.
- Hard atomicity problem between base graph state and CDC artifact.
- Small-file explosion and heavy compaction pressure.
- Harder crash recovery and ordering guarantees.

## Proposed Architecture

1. **WAL/Event Log (canonical)**
   - Append-only, ordered by monotonic `lsn`.
   - Contains all committed graph operations.
   - Durably fsynced before acknowledging success.

2. **Materialized Graph State (derived)**
   - Existing Lance datasets, indexes, manifest.
   - Updated from committed WAL records.
   - Recoverable/rebuildable from WAL + checkpoints.

3. **CDC Parquet (derived)**
   - Async export pipeline reads WAL records by `lsn`.
   - Writes larger Parquet segments in batches.

## WAL Event Model

Each record should include at least:

- `lsn` (global ordered offset)
- `txn_id`
- `op` (insert/update/delete/edge_insert/edge_delete/schema_migration/etc)
- `type_name`
- endpoint/key fields where relevant
- `before` (optional, for delete/update semantics)
- `after` (optional)
- `ts_unix_ms`
- `schema_identity_version`
- checksum/version

## Write Protocol (Transactional)

For every write transaction:

1. Append `BEGIN(txn_id)`.
2. Append operation records.
3. Append `COMMIT(txn_id)`.
4. `fsync` WAL.
5. Acknowledge success.
6. Apply to materialized state (sync or async), advancing `applied_lsn`.

If process crashes before `COMMIT`, txn is ignored on replay.

## Recovery Protocol

On open:

1. Read checkpoint + `applied_lsn`.
2. Scan WAL from `applied_lsn + 1`.
3. Replay committed txns in `lsn` order.
4. Ignore incomplete txns (missing `COMMIT`).
5. Rebuild/refresh indexes as needed.

## CDC Consumption Contract

Expose CDC readers as:

- `from_lsn` cursor API
- exactly-once via monotonic offsets
- stable event ordering by `lsn`

## Parquet Materialization Strategy

- Background worker tails WAL.
- Buffers events by size/time window.
- Writes partitioned Parquet segments (for example by date/op/type).
- Tracks sink watermark (`parquet_exported_lsn`).
- Idempotent writes keyed by `(segment_id, lsn range)`.

## Checkpointing and Retention

- Periodic state checkpoint stores latest fully materialized `applied_lsn`.
- WAL truncation only when:
  - checkpoint is durable, and
  - all required sinks/consumers advanced beyond truncation boundary.
- CDC retention policy handled independently from WAL durability window.

## Guarantees (Target)

- Durable commit once WAL fsync succeeds.
- Total order of events by `lsn`.
- Deterministic replay.
- Exactly-once CDC delivery for cursor-based consumers.

## Main Tradeoffs

- Extra storage: WAL + CDC sink + materialized graph.
- More components: checkpointing, compaction, watermark tracking.
- Careful schema-evolution compatibility required for long-lived replay.

## Suggested Rollout

1. Introduce WAL append + replay for existing write paths.
2. Add `lsn`/checkpoint fields to manifest.
3. Build internal CDC reader API (`from_lsn`).
4. Add async Parquet sink worker.
5. Add ops tooling: inspect WAL, verify replay, compact/truncate safely.

