# Storage Modes: In-Memory vs On-Demand

Status: proposal
Date: 2026-03-20

Two architectural paths for nanograph's memory model, and how to support both.

## Path 1: Full Graph In Memory

### What it means

The CLI becomes a long-running process. Either a daemon (`nanograph serve`) that accepts queries over a socket/pipe, or a REPL (`nanograph shell`) that accepts interactive commands. The `Database` object lives across queries, same as the SDK today.

### Benefits

**Traversal is as fast as it gets.** `ExpandExec` does: CSR lookup (nanoseconds) → HashMap lookup for destination row (nanoseconds) → `take()` from in-memory batch (nanoseconds). No I/O on the query hot path. This is the latency profile you want for agent workloads doing many small traversals.

**Amortized open cost.** Load once, query many times. The 500ms open on a 1M-node graph is paid once.

**Simplest mental model.** Everything is in memory, everything is fast, no cache coherence questions.

### Shortcomings

**Memory ceiling.** The graph must fit in RAM. Today's architecture loads everything twice (raw batches + CSR adjacency). A 10M-node, 50M-edge graph with 100 bytes of properties per entity: ~5 GB of node data + ~5 GB of edge data + ~2 GB of CSR = ~12 GB. That's a hard wall.

**Mutations stay O(N).** This is the big one. Stateful CLI doesn't fix the mutation problem — it makes it worse. A `delete` of 1 node still clones the entire `GraphStorage`, copies every type, rebuilds all CSR/CSC. In a long-running process, this blocks all reads while it runs. Today the CLI exits after mutation so nobody notices the cost. A daemon that blocks for 2 seconds on a 1-node delete is a visible regression.

**CDC stays O(N).** Same snapshot-diff problem regardless of CLI lifecycle.

**Process management.** The CLI becomes a service: needs graceful shutdown, crash recovery, signal handling, port management (if socket-based), or TTY management (if REPL). `nanograph run` from scripts becomes `nanograph connect --query` or similar. CI/CD scripts that do `nanograph load && nanograph run check_query` need a persistent process between commands.

**Stale state.** If another process (or the SDK in another app) writes to the same `.nano` directory, the daemon's in-memory state is stale. Needs either file watchers, version polling, or exclusive locking.

**Startup latency still exists.** First open is still O(total graph). For a script that starts the daemon, runs one query, and stops — you're back to the same cost as today.

## Path 2: Indexes/Metadata In Memory

### What it means

`Database::open()` reads the manifest (microseconds). CSR/CSC built on first traversal query per edge type, cached for the session. All node data read from Lance on demand. CLI stays stateless.

### Benefits

**O(1) open.** Every CLI command starts instantly.

**Memory proportional to use.** Only edge types you traverse consume memory (CSR). Node data never resident. A 10M-node graph uses the same memory as a 100-node graph if you're running the same query.

**Mutations are O(changed).** Go directly to Lance. No GraphStorage clone. `delete 1 node` deletes 1 row from Lance + cascades edges. Invalidate the CSR cache entry. Done.

**CDC is O(changed).** Mutation code emits events directly, or Lance delta API derives them from version diff.

**Scales beyond RAM.** Lance reads pages on demand. The OS page cache keeps hot pages warm. A 100GB graph works fine if queries are selective.

**Multi-process safe.** Each CLI invocation opens Lance at a pinned version (MVCC). No stale state. Multiple readers, single writer. Same model as SQLite.

### Shortcomings

**Traversal has I/O on the critical path.** After CSR gives you neighbor IDs, you need a Lance scan to resolve destination node properties:

```
Path 1: csr.neighbors(42) → [7, 13, 99] → id_to_row[7] → batch[row] → nanoseconds
Path 2: csr.neighbors(42) → [7, 13, 99] → Lance WHERE id IN (7, 13, 99) → microseconds
```

The Lance scan is batched (one query for all destination IDs), uses a BTree index on `id`, and benefits from OS page cache. But it's I/O, not memory access. For a single-hop query returning 100 destinations: maybe 0.5–2ms instead of microseconds. For 3-hop returning 10K destinations: maybe 5–20ms.

**CSR build cost per CLI invocation.** For a stateless CLI running a traversal query, the CSR for that edge type must be built from scratch each time. Building CSR for 1M edges: read 24 bytes × 1M from Lance (~24 MB), sort, build offsets — maybe 100–200ms. OS page cache makes repeated invocations faster, but first run after boot pays full I/O.

**No benefit for repeated identical queries in CLI.** Each invocation is independent. The SDK caches CSR across queries, but the CLI doesn't.

## Implementing Both

The difference between the two paths is narrow: **how destination nodes are resolved after CSR gives you neighbor IDs.**

```rust
trait NodeResolver: Send + Sync {
    async fn resolve_nodes(&self, type_name: &str, ids: &[u64]) -> Result<RecordBatch>;
}

// Path 1: pre-loaded batches
struct InMemoryResolver {
    node_batches: HashMap<String, RecordBatch>,
    id_to_row: HashMap<String, HashMap<u64, usize>>,
}

// Path 2: Lance on demand
struct LanceResolver {
    dataset_paths: HashMap<String, PathBuf>,
    manifest_versions: HashMap<String, u64>,
}
```

`ExpandExec` takes a `Arc<dyn NodeResolver>` instead of `Arc<GraphStorage>`. CSR/CSC is shared by both paths — it's always in memory when traversal happens.

`NodeScanExec` always uses Lance (it's already better for filtered scans even in Path 1).

### Configuration

```toml
# nanograph.toml
[db]
storage_mode = "on-demand"   # or "in-memory"
```

```bash
nanograph run --storage-mode in-memory query1   # pre-loads node data
nanograph run query1                            # default: on-demand
```

### Implementation order

Build Path 2 first. It's the foundation — simpler, fewer bugs, works for all graph sizes. Path 1 becomes an optimization layer on top:

1. **Implement Path 2**: Lance-backed node resolution, lazy CSR, direct mutations.
2. **Add `InMemoryResolver`**: At open time, optionally pre-load node batches into memory. `ExpandExec` uses the in-memory path. Everything else (NodeScan, mutations, CDC) stays on Path 2 regardless.
3. **Make it configurable**: `storage_mode` in config. Default to `on-demand`. CLI users with small graphs and latency-sensitive traversals opt into `in-memory`.

The SDK gets both modes too. An agent doing rapid-fire traversals on a 50K-node graph uses `in-memory`. A data pipeline processing a 10M-node graph uses `on-demand`.

### Cost comparison

| | Path 1: in-memory | Path 2: on-demand |
|---|---|---|
| Open | O(total graph) | O(1) |
| Node scan | Lance (same) | Lance (same) |
| Traversal | CSR + memory lookup (ns) | CSR + Lance lookup (us-ms) |
| Mutation | O(changed)* | O(changed) |
| CDC | O(changed) | O(changed) |
| Memory | Entire graph | CSR for traversed types |
| Max graph size | Fits in RAM | Fits on disk |

*Path 1 mutations still go through Lance directly (Path 2 mutation logic). The in-memory cache just gets invalidated and lazily reloaded.

**Path 2 is the architecture. Path 1 is a cache on top of it.**

### Incremental CSR Updates

Both paths benefit from a `CsrIndex` that supports incremental mutation instead of full rebuild:

```rust
fn insert_edge(&mut self, src: u64, dst: u64, edge_id: u64) {
    let idx = self.neighbors.len() as u32;
    self.neighbors.push(dst);
    self.edge_ids.push(edge_id);
    let range = self.ranges.entry(src).or_insert((idx, idx));
    range.1 = idx + 1;
}
```

O(1) per edge instead of O(total edges) for a full rebuild. This is the same insight Redis uses — mutate the data structure incrementally, don't rebuild it.
