# Research: Database Storage Patterns

Status: reference
Date: 2026-03-20

Best practices from Redis, SQLite, DuckDB, and graph databases, mapped to nanograph's architecture decisions.

## Redis

### Persistence Models

Three strategies trading off performance vs durability:

**RDB (Snapshots):** Point-in-time binary snapshots via `fork()` + copy-on-write. Child process serializes entire dataset while parent continues serving. Data loss window = time since last snapshot (5–60 min typical). `BGSAVE` is non-blocking.

**AOF (Append Only File):** Logs every write command. Since Redis 7.0, uses multi-part structure: base file (RDB-format snapshot) + incremental files (commands since base) + manifest. Three fsync policies:
- `always` — fsync after every command, zero data loss
- `everysec` — background fsync every second, ~1s data loss
- `no` — OS controls flushing, ~30s data loss

**Hybrid (recommended):** RDB + AOF. On restart, load RDB preamble then replay AOF incrementals.

### Single-Threaded Event Loop

Command execution is single-threaded and sequential. Eliminates all lock contention, context switching, and race conditions. Since Redis is memory-bound (not CPU-bound), the bottleneck is network I/O, not CPU. Redis 6.0+ added optional I/O threading that parallelizes socket reads/writes while keeping command execution single-threaded — parallelized I/O with serialized execution.

### Memory Management

`maxmemory` configuration with eviction policies: `allkeys-lru`, `allkeys-lfu`, `volatile-ttl`, `noeviction`, etc. Redis uses approximated LRU — samples `maxmemory-samples` random keys (default 5) and evicts the oldest. With 10 samples, behavior is nearly indistinguishable from true LRU.

### fork() + COW for Snapshots

1. `fork()` creates child process — parent and child share same physical memory pages (marked read-only)
2. Child serializes entire dataset to disk (consistent snapshot at fork point)
3. Parent continues serving — when either modifies a page, OS copies it (copy-on-write)
4. Memory overhead = only modified pages, not total dataset

For 24GB dataset with 10% write rate during snapshot: ~2.4GB overhead. The `fork()` itself copies page tables (~48MB for 24GB, ~62ms on bare metal).

**Not applicable to nanograph:** Lance's versioned dataset model already provides COW semantics at the storage layer. Each write produces a new version; readers hold references to their version. macOS also discourages `fork()` in multi-threaded programs (tokio).

## SQLite WAL Mode

### How It Works

Traditional journal: copy original page to journal, modify database, delete journal. WAL inverts this: original database never modified during transactions. Changes appended to WAL file. Commit = append commit record.

### Concurrent Access

Reader starts transaction → records position of last commit ("end mark") → fixed for duration of read. When reader needs a page: search WAL up to end mark, if not found read from database file. Different readers can have different end marks = different points in time.

**"Readers don't block writers, writers don't block readers"** — readers read from frozen database + their WAL snapshot. Writers append past all readers' end marks. Neither conflicts.

### WAL-Index (Shared Memory)

Memory-mapped file (`<database>-shm`) avoids scanning entire WAL for page lookups. Under 32 KiB, never synced to disk, reconstructable from WAL.

### Checkpointing

Transfers committed WAL pages back to database file. Four modes:
- `PASSIVE` (default): does as much as possible without blocking
- `FULL`: blocks new writers until complete
- `RESTART`/`TRUNCATE`: resets WAL to beginning

Auto-checkpoint at 1000 pages (~4MB). Checkpoint starvation occurs when overlapping readers prevent completion.

## DuckDB

### In-Process Model

Runs entirely within host process — no server, no IPC. Data transfer via direct memory access (pointer passing), not serialization. The "SQLite for analytics" model.

### Concurrency

Single-writer, multiple-reader within a single process via MVCC with optimistic concurrency control. Append operations never conflict. Updates to overlapping rows cause second transaction to fail. Multi-process writing explicitly not supported.

### Memory Management: Three-Tier

1. **Streaming execution**: Data processed in chunks, never fully materialized.
2. **Intermediate spilling**: Hash tables, sort buffers, aggregation state spill to disk when exceeding memory.
3. **Buffer manager**: Caches pages from persistent storage. Evicts under pressure.

`memory_limit` defaults to 80% of physical RAM. Buffer manager and intermediates share the same limit. Even in-memory databases can spill to disk for larger-than-RAM workloads.

## Neo4j

### Native Graph Storage

Fixed-size records in separate store files:
- Node records: 15 bytes (inUse flag, first relationship ID ptr, first property ID ptr, labels)
- Relationship records: 34 bytes (start/end node IDs, relationship type, 4 directional pointers for doubly-linked lists)
- Property records: 41 bytes (key+type + 4×8-byte blocks; small values inline, large values pointer-chase)

### Index-Free Adjacency

Since all records are fixed-size, address = `Record ID × Record Size`. Traversal is O(1) pointer math:
1. Read node at `nodeId × 15` → get first relationship ID
2. Jump to relationship at `relId × 34` → get start/end nodes + next/prev pointers
3. Follow doubly-linked list

Time to traverse N relationships from a node is O(N), independent of total graph size. No B-tree lookup needed.

### Memory Model

Dual architecture:
- **Java Heap** (GC-managed): query planning, result buffering, transaction state
- **Page Cache** (off-heap): caches on-disk store files and indexes

Works with graphs larger than RAM — degrades gracefully. Fixed-size record layout means even cold reads are a single seek, not a scan. Recommended: size page cache at `1.2 × total_store_size`.

## KuzuDB

### The Closest Analog to nanograph

Embedded graph database, C++, disk-backed columnar storage, CSR-based adjacency.

### Node Groups

Data partitioned into fixed-size chunks (131,072 tuples per group). Properties stored as independent column chunks within each group. Column chunk metadata cached at initialization for pruning.

### CSR on Disk

Relationship tables use CSR with double indexing (forward + backward). Within a node group, all relationship lists share a single offsets array. `DirectedCSRIndex` maps source/destination offsets to relationship offsets for bidirectional traversal.

### Buffer Manager

- Virtual memory via mmap (up to 8TB address space reserved)
- Page state machine: EVICTED → LOCKED → MARKED → UNLOCKED
- Second-chance FIFO eviction
- Optimistic reads that avoid lock acquisition for read-heavy workloads
- Default buffer pool = ~80% of physical memory

Separate `MemoryManager` for intermediate query results, competing with page cache under unified limit.

### Traversal

Pull-based execution: operators call `getNextTuple()` down to `ScanNodeTable` or `Extend` operators, which acquire buffer pins, perform optimistic reads, and return vectorized batches.

## Memgraph

### Pure In-Memory

Everything in heap: skip-list data structures. Vertex: 144 bytes min. Edge: 96 bytes min. Adjacency via in-memory pointers — no CSR, no disk. Fastest possible traversal.

### Durability

WAL + periodic snapshots. Every mutation creates a Delta object logged to WAL before applying. Snapshots are periodic full-state dumps. Recovery: load snapshot + replay WAL.

### Larger-Than-RAM

`IN_MEMORY_TRANSACTIONAL` mode: throws exception if data exceeds RAM.
`ON_DISK_TRANSACTIONAL` mode (added later): uses RocksDB backing store. But all graph objects touched by a single transaction must still fit in RAM.

## DGraph

### Posting Lists

Stores graph as posting lists in triple/RDF model. A posting list contains all triples sharing the same `<subject, predicate>` pair. Stored as single values in BadgerDB (LSM-tree based KV store).

### Predicate-Based Sharding

Shards by predicate (not by node). All edges of a given type go to one tablet on one server. Single traversal step = one RPC to the server owning that predicate.

## Graph DB Storage Patterns Summary

| Approach | System | Read Perf | Write Perf | Memory Model |
|----------|--------|-----------|------------|--------------|
| Fixed-size record linked lists | Neo4j | O(1) per hop via pointer math | O(1) append to linked list | Page cache over disk files |
| Pure in-memory skip lists | Memgraph | Fastest (pointer-chasing in RAM) | Fast (skip list insert) | All in RAM or fail |
| LSM-tree + posting lists | Dgraph | O(log N) via LSM lookup | O(1) append to WAL | BadgerDB manages caching |
| Disk-backed CSR in node groups | KuzuDB | O(1) within CSR + buffer pool | Batch updates via node groups | Buffer manager + mmap |

**nanograph today:** Memgraph's memory model (everything in heap) but without Memgraph's mutation efficiency (skip-list insert vs full graph copy). No production graph database loads the entire graph into heap memory and rebuilds it on every mutation.

## Memory-Mapped Storage Notes

The influential paper "Are You Sure You Want to Use MMAP in Your DBMS?" (Crotty et al., CIDR 2022) identified problems: page table contention, single-threaded eviction (kswapd), TLB shootdowns. Naive mmap leads to 2–20x worse bandwidth than explicit buffer pool on NVMe.

However, the "mmap with manual eviction" approach (KuzuDB's pattern) treats mmap as a virtual address space allocator rather than a complete I/O layer, sidestepping several issues. This is formalized in VMCache (SIGMOD 2023): use mmap for address space reservation, implement your own page state tracking and eviction on top.
