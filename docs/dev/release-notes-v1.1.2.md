v1.1.2

- Canonical stable release for the current Lance v3 line. `v1.1.2` folds in the graph storage refactor, current Gemini media embedding behavior, SDK/doc alignment, and release/distribution cleanup.

- Refactored storage around v4-ready seams while keeping Lance `v3` authoritative:
  - introduced neutral graph/table version types for graph commits, table snapshots, and future namespace-backed upgrades
  - split table access behind a `TableStore` layer and WAL access behind graph commit/change store interfaces
  - kept `graph.manifest.json` + `_wal.jsonl` authoritative in `v3`

- Added derived Lance graph mirror tables:
  - `__graph_commits` and `__graph_changes` now materialize committed history into Lance
  - mirror writes are best-effort and non-authoritative
  - rebuild/cleanup paths regenerate mirrors from the retained WAL window
  - `doctor` reports mirror problems as warnings instead of hard failures

- Corrected embedding provider behavior:
  - OpenAI is now enforced as text-only embeddings
  - Gemini supports multimodal embeddings for text, images, audio, video, and PDFs
  - Gemini media limits are enforced locally before requests are sent:
    - text up to a conservative 8192-token estimate
    - PNG/JPEG images
    - MP4/MOV videos up to 120 seconds
    - audio media accepted as `audio/*`
    - PDFs up to 6 pages

- Fixed media validation details that affected real Gemini workflows:
  - corrected PDF page counting so split PDFs are validated against the actual page tree instead of unrelated outline counts
  - improved remote/local media request shaping for Gemini multimodal embedding requests

- Expanded graph and CLI coverage:
  - added integration tests for graph mirror parity, rebuild, cleanup retention, and mirror-write failure handling
  - added CLI e2e coverage proving `changes` still reads authoritative WAL and that cleanup recreates missing mirrors
  - added local-only real-media Gemini e2e coverage for PNG and PDF validation

- Aligned SDK and user documentation with the current behavior:
  - TypeScript and Swift SDK docs now match the shipped APIs and media helpers
  - user docs are being split so media/blob storage and embeddings have distinct guides
  - Gemini/OpenAI behavior is documented consistently across schema, config, and SDK docs

- Release/distribution cleanup:
  - version bumped to `1.1.2` across Rust crates, npm package metadata, and Swift packaging assets
  - crates.io packages published for `nanograph`, `nanograph-cli`, `nanograph-ffi`, and `nanograph-ts`
  - external `nanograph-swift` package updated to the `v1.1.2` GitHub release artifact and checksum
