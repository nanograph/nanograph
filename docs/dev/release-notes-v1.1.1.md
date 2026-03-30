v1.1.1

- Historical patch release. `v1.1.1` is superseded by `v1.1.2` as the canonical stable release. It kept the `v1.1.0` feature set and fixed the SDK media test harness so CI and release smoke tests passed cleanly.

- Added external media-node support for multimodal workflows:
  - `@media_uri(mime)` for external media URIs
  - `@embed(uri)` for multimodal embeddings on media nodes
  - text-to-image retrieval with `nearest(...)`
  - traversal from matched image nodes into the graph

- Added Gemini embedding support:
  - text embeddings with document/query role separation
  - image embeddings for media nodes via `gemini-embedding-2-preview`
  - documented image limits and supported source types

- Refactored the runtime to be metadata-first:
  - `Database::open()` no longer restores the full graph into memory
  - reads run against lazy manifest-pinned Lance datasets
  - graph-aware writes persist touched datasets instead of rebuilding graph snapshots

- Refactored the query engine toward Lance + DataFusion:
  - graph traversal remains custom
  - supported relational tails now use DataFusion for filtering, projection, aggregation, ordering, and limits
  - prepared reads and traversal no longer depend on the old snapshot-heavy runtime

- Cleaned up the write path:
  - dataset-scoped mutation planning via `MutationDelta` and `DatasetMutationPlan`
  - WAL-derived change projection replaces older mixed CDC-shaped internals

- Aligned the TypeScript and Swift SDKs with the new media workflow:
  - `describe()` now exposes `mediaMimeProp`
  - both SDKs now expose `embed(...)`
  - both SDKs add typed media ingest helpers
  - both SDKs now cover text-to-image retrieval plus traversal in automated tests

- Improved CLI and docs:
  - added a user guide for blobs and multimodal embeddings
  - added a Lance migration guide
  - updated config docs for Gemini

- Notable cleanup for Rust consumers:
  - `Database::snapshot()` removed
  - `GraphStorage` removed from the supported production runtime surface
  - old public `execute_query()` surface removed

- Storage defaults:
  - new datasets are written with Lance v3 / storage format `2.2`
  - existing `2.0` datasets remain readable
