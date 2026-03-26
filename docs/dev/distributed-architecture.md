# Distributed Architecture

Design document for scaling nanograph from embedded single-node to distributed multi-player.

## Guiding Principle

Don't replace the stack — distribute it. Lance, DataFusion, and Arrow already have distributed capabilities. The goal is to activate them, not swap them for a KV store or custom storage engine.

## Current Stack (Single-Node)

```
.gq text → parse_query() → QueryAST
         → typecheck_query() → TypeContext
         → lower_query() → QueryIR
         → build_physical_plan() → DataFusion ExecutionPlan (single-node)
         → execute_query() → Vec<RecordBatch>
```

Storage: Lance on local filesystem. Graph index: in-memory CSR/CSC rebuilt on open.

## Target Stack (Distributed)

```
.gq text → parse_query() → QueryAST          (keep — unchanged)
         → typecheck_query() → TypeContext    (keep — unchanged)
         → lower_query() → QueryIR            (keep — mostly unchanged)
         → distributed planner → partitioned ExecutionPlans  (new)
         → coordinator routes to compute nodes via Arrow Flight (new)
         → each compute node runs DataFusion + graph ops against Lance on S3 (modified)
         → coordinator merges results → Vec<RecordBatch>  (new)
```

## Architecture

```
                 ┌──────────────────────┐
                 │   Query Front-End    │
                 │ parse → typecheck    │
                 │      → IR            │
                 └──────────┬───────────┘
                            │
                 ┌──────────▼───────────┐
                 │   Coordinator Node   │
                 │ partition-aware       │
                 │ planner + merge      │
                 └──────────┬───────────┘
                            │ Arrow Flight
            ┌───────────────┼───────────────┐
            ▼               ▼               ▼
     ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
     │ Compute Node│ │ Compute Node│ │ Compute Node│
     │ DataFusion  │ │ DataFusion  │ │ DataFusion  │
     │ + graph ops │ │ + graph ops │ │ + graph ops │
     └──────┬──────┘ └──────┬──────┘ └──────┬──────┘
            │               │               │
            └───────────────┼───────────────┘
                            │
                 ┌──────────▼───────────┐
                 │    Lance on S3       │
                 │  (shared storage)    │
                 └──────────────────────┘
```

## What Stays, What Changes

| Layer | Status | Notes |
|-------|--------|-------|
| Query parser (.gq grammar, Pest) | **Keep** | Syntax is execution-topology-agnostic |
| Schema parser (.pg grammar, Pest) | **Keep** | Same |
| Typechecker | **Keep** | Validates against catalog, not against storage |
| IR lowering | **Keep (mostly)** | May need distribution hints but operator model transfers |
| Physical planner | **Replace** | New partition-aware planner that emits sub-plans per compute node |
| NodeScanExec | **Modify** | Scatter to relevant compute nodes, merge via Arrow Flight |
| ExpandExec | **Replace** | Distributed traversal (see below) |
| CrossJoinExec, AntiJoinExec | **Replace** | Distributed join variants |
| Lance storage | **Config change** | `s3://bucket/...` instead of `./path/...` |
| CSR/CSC graph index | **Rethink** | Per-compute-node local index, or replicated adjacency |
| CDC log | **Modify** | Write to S3, coordinator serializes writes |
| json_output, result types | **Keep** | Arrow→JSON serialization is transport-agnostic |

Roughly ~40% of the codebase (query front-end) transfers unchanged. ~60% (plan, execute, store) needs modification or replacement.

## Storage: Lance on S3

Lance already supports object-store backends. The storage change is configuration, not code:

```rust
// embedded (today)
lance::dataset::Dataset::open("./mydb.nano/nodes/a1b2c3d4/")

// distributed
lance::dataset::Dataset::open("s3://bucket/mydb.nano/nodes/a1b2c3d4/")
```

Nanograph already partitions data by type — each node/edge type is a separate Lance dataset under `nodes/{type_id_hex}/` and `edges/{type_id_hex}/`. This maps naturally to distributed partitioning: each compute node handles a subset of types, or fragments within a type.

Lance's existing capabilities that carry over:
- **MVCC versioning** — concurrent readers see consistent snapshots
- **Filter pushdown** — predicates pushed to storage, reducing data transfer
- **Compaction** — background optimization without blocking reads
- **Fragment-level parallelism** — DataFusion already parallelizes across Lance fragments

## Distributed Traversal (ExpandExec)

Graph traversal is the hardest operator to distribute because hops cross partition boundaries. Three options evaluated:

### Option A: Scatter-Gather (recommended for v1)

Coordinator sends "expand from these node IDs" to the compute node that owns the adjacency data. Results come back, next hop sent out.

```
Coordinator: "expand Person IDs [1, 5, 12] via 'knows'"
  → Compute Node 2 (owns 'knows' edges): returns [{src:1, dst:42}, {src:5, dst:87}, ...]
Coordinator: "fetch Person properties for IDs [42, 87, ...]"
  → Compute Node 1 (owns Person nodes): returns RecordBatch
```

- One network round-trip per hop
- Simple to implement
- Fine for nanograph's typical 1–3 hop bounded expansion (`knows{1,3}`)
- Latency grows linearly with hop count

### Option B: Co-Located Adjacency

Replicate the adjacency index (CSR/CSC) to all compute nodes. Only property lookups are remote. Traversal stays local.

- More memory usage (full adjacency on every node)
- Traversal as fast as embedded mode
- Property fetches still require network hops
- Good for read-heavy traversal workloads

### Option C: BSP (Pregel-Style)

Each compute node processes its local vertices, sends messages to neighbors. Synchronize between supersteps.

- Best for bulk graph algorithms (PageRank, community detection)
- Overkill for nanograph's OLTP-style queries
- Consider later if graph analytics features are added

**Decision**: Start with scatter-gather. It's simplest, matches nanograph's query patterns, and can be optimized to co-located adjacency later for hot paths.

## Distributed Mutations

Single-writer coordinator model:

1. All mutations route to the coordinator
2. Coordinator validates (typecheck, constraints), applies to Lance on S3
3. CDC log entries written to S3 alongside data
4. Compute nodes see new data on next Lance dataset refresh (MVCC)

This avoids distributed consensus for writes. Trade-off: single write throughput bottleneck, but graph mutations are typically low-volume compared to reads.

For higher write throughput later: partition writes by type (each type has an independent write path since they're separate Lance datasets).

## Coordinator Responsibilities

- Parse, typecheck, lower queries (front-end pipeline)
- Partition the IR plan: determine which compute nodes to involve
- Route sub-plans to compute nodes via Arrow Flight
- Merge partial results (concatenate RecordBatches, apply final sort/limit/aggregation)
- Serialize all writes (mutations, CDC, schema changes)
- Health checks and compute node registry

The coordinator is stateless except for the compute node registry. Query state lives in the plan; data state lives in Lance on S3.

## Network Transport: Arrow Flight

Arrow Flight is the natural choice:
- Zero-copy Arrow RecordBatch transfer over gRPC
- Streaming results (don't wait for full materialization)
- Built-in flow control and backpressure
- Rust support via `arrow-flight` crate

Query flow:
1. Client sends `.gq` text to coordinator via Flight `do_action`
2. Coordinator plans, sends sub-plans to compute nodes via Flight `do_get`/`do_put`
3. Compute nodes stream RecordBatches back
4. Coordinator merges and streams final result to client

## Why Not a KV Store

Evaluated and rejected KV (Redis, FoundationDB, TiKV, RocksDB) as a storage replacement:

| Concern | Lance on S3 | KV Store |
|---------|-------------|----------|
| Columnar scans ("all Persons where age > 30") | Fast (vectorized batch) | Slow (row-at-a-time) |
| Filter pushdown | Yes (Lance native) | No |
| Arrow zero-copy | Yes | No (serialize/deserialize) |
| Analytical queries (aggregation, sorting) | Fast (DataFusion) | Slow |
| Point lookups by ID | Moderate | Fast |
| New storage engine to maintain | No | Yes |

Nanograph's query patterns are scan-heavy and traversal-heavy, not point-lookup-heavy. A KV store optimizes the wrong access pattern and gives up the columnar advantage that makes nanograph fast for its intended workloads.

The only KV-like operation (adjacency lookup by node ID) can be handled by Lance's indexed scans or by a lightweight in-memory adjacency cache on compute nodes.

## Incremental Build Plan

Each phase is independently deployable and valuable:

### Phase 1: Lance on S3 (~days)
- Configure Lance to use S3 backend
- Multiple embedded nanograph instances can read from the same S3 location
- Single writer, multiple readers (Lance MVCC handles this)
- No new code — configuration and testing

### Phase 2: Arrow Flight Server (~weeks)
- Wrap the existing execution pipeline in an Arrow Flight service
- Single-node server mode (no distribution yet)
- Clients connect remotely instead of embedding the library
- Enables multi-user access to the same database

### Phase 3: Coordinator + Compute Nodes (~weeks)
- Split the pipeline: front-end on coordinator, execution on compute nodes
- Coordinator routes NodeScanExec to compute nodes
- Each compute node runs DataFusion against Lance on S3
- Merge results at coordinator

### Phase 4: Distributed ExpandExec (~weeks)
- Implement scatter-gather traversal across compute nodes
- Adjacency index built per-compute-node from local Lance fragments
- Multi-hop queries coordinate through the coordinator
- Distributed AntiJoinExec and CrossJoinExec

### Phase 5: Distributed Mutations (~weeks)
- Coordinator serializes writes to Lance on S3
- CDC log on S3
- Compute nodes refresh Lance dataset handles to see new versions

### Phase 6: Operational Readiness
- Compute node auto-scaling and health checks
- Query timeout and resource limits
- Auth (per-connection identity, per-type RBAC)
- Monitoring and metrics (query latency, compute node utilization)

## Deployment: On-Prem First

The architecture has zero cloud-managed service dependencies. Every component runs on commodity hardware.

### Why on-prem is the natural fit

| Component | Cloud | On-Prem Equivalent |
|-----------|-------|---------------------|
| Lance storage | S3 | MinIO (S3-compatible, single binary) |
| Arrow Flight | gRPC | gRPC (no cloud dependency) |
| Coordinator | EC2/VM | Any Linux box |
| Compute nodes | EC2/VM | Any Linux box |
| Service discovery | Cloud LB | DNS, etcd, or static config |

No DynamoDB, no managed Kafka, no Lambda, no cloud IAM. The stack is portable by design.

### MinIO as storage backend

Lance uses the `object_store` crate which supports S3-compatible APIs. MinIO is a drop-in replacement:

```bash
# single-node MinIO (dev/test)
minio server /data

# multi-node MinIO (production)
minio server http://minio{1...4}/data{1...4}
```

```rust
// Lance connects to MinIO the same way as S3
lance::dataset::Dataset::open("s3://nanograph-data/mydb.nano/nodes/a1b2c3d4/")
// with endpoint override: AWS_ENDPOINT=http://minio:9000
```

MinIO provides:
- S3-compatible API (Lance works unmodified)
- Erasure coding for durability without replication overhead
- Bucket versioning for point-in-time recovery
- Multi-node deployment for storage scaling
- Single static binary, no JVM, no dependencies

### Deployment topology

#### Small (1–3 machines)

All-in-one: coordinator, compute, and MinIO co-located.

```
┌──────────────────────┐
│ Machine 1            │
│ coordinator + compute│
│ + MinIO              │
└──────────────────────┘
```

This is essentially "server-mode nanograph" — a single binary that does everything. Suitable for teams of 5–20 users, millions of nodes.

#### Medium (3–10 machines)

Separate storage and compute. Coordinator on a dedicated node or co-located with a compute node.

```
┌────────────┐  ┌────────────┐  ┌────────────┐
│ Coordinator│  │ Compute 1  │  │ Compute 2  │
│ + Compute 0│  │            │  │            │
└─────┬──────┘  └─────┬──────┘  └─────┬──────┘
      │               │               │
      └───────────────┬┘───────────────┘
                      │
        ┌─────────────▼──────────────┐
        │  MinIO (3-node cluster)    │
        └────────────────────────────┘
```

#### Large (10+ machines)

Dedicated coordinator(s) with HA failover. Compute pool scales independently of storage pool.

```
┌──────────────┐ ┌──────────────┐
│ Coordinator  │ │ Coordinator  │  (active/standby)
│ (primary)    │ │ (standby)    │
└──────┬───────┘ └──────┬───────┘
       │                │
       └────────┬───────┘
                │ Arrow Flight
    ┌───────────┼───────────┐
    ▼           ▼           ▼
┌────────┐ ┌────────┐ ┌────────┐
│Compute │ │Compute │ │Compute │  (scale out)
│Pool    │ │Pool    │ │Pool    │
└───┬────┘ └───┬────┘ └───┬────┘
    │          │          │
    └──────────┼──────────┘
               │
    ┌──────────▼──────────┐
    │  MinIO cluster      │  (scale independently)
    └─────────────────────┘
```

### Packaging and orchestration

**Single binary**: Coordinator and compute node compile into one binary with mode flags:

```bash
nanograph-server --mode coordinator --compute-nodes host1:9001,host2:9001
nanograph-server --mode compute --storage s3://nanograph-data/
nanograph-server --mode standalone  # all-in-one for small deployments
```

**Kubernetes**: Helm chart with separate coordinator and compute StatefulSets. MinIO via the MinIO Operator or external.

**Bare metal**: systemd units. Static config file listing compute nodes. No orchestrator dependency.

### On-prem advantages over cloud

- **Data sovereignty** — graph data never leaves the network. Critical for regulated industries, defense, healthcare.
- **Predictable latency** — no cross-AZ network hops, no noisy neighbors, no cold starts.
- **Predictable cost** — hardware is a capex, not a metered opex that scales with query volume.
- **Air-gapped operation** — works with zero internet access. No license servers, no telemetry, no cloud auth.
- **Embedded-to-server migration** — teams start with embedded nanograph on a laptop, promote the same `.nano` directory to a server when they need multi-user access. Same schema, same queries, same data.

### Hardware sizing (rough guidelines)

| Scale | Coordinator | Compute Nodes | MinIO | Total |
|-------|------------|---------------|-------|-------|
| 1M nodes, 5 users | 4 vCPU, 8 GB | 1 × 4 vCPU, 16 GB | Co-located | 1 machine |
| 10M nodes, 20 users | 4 vCPU, 8 GB | 3 × 8 vCPU, 32 GB | 3 × 4 vCPU, 1 TB SSD | 7 machines |
| 100M+ nodes, 50+ users | 8 vCPU, 16 GB (×2 HA) | 6+ × 16 vCPU, 64 GB | 4+ × 8 vCPU, 4 TB NVMe | 12+ machines |

Compute nodes are memory-bound (CSR/CSC adjacency index + Arrow batch processing). MinIO nodes are storage-bound. Coordinator is lightweight.

## Open Questions

- **Partition strategy**: By type (natural, already how Lance datasets are organized) vs. by ID range within a type (needed for very large single types)?
- **Adjacency cache consistency**: How quickly must compute nodes see new edges? Lance MVCC gives snapshot isolation but there's a refresh lag.
- **Schema changes**: Distributed `nanograph migrate` needs to coordinate across compute nodes. Quiesce writes, apply schema, refresh all nodes.
- **Embedded mode preservation**: The distributed architecture must not degrade the embedded single-node experience. Feature-flag or separate binary?
