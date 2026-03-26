# External Media Nodes & Multimodal Embeddings

Proposal for multimodal search in nanograph without storing binary payloads inside `.nano/`.

## Decision

Nanograph should not add `Blob` as a scalar property type.

Instead:

- media assets are modeled as first-class nodes
- domain nodes connect to media nodes with edges
- media bytes live outside the database in folders or object storage
- nanograph stores only:
  - external media URI
  - mime type
  - optional derived metadata
  - optional embeddings

This is the right fit for nanograph because `.nano/` is intended to sync cleanly through Git. Putting binary payloads into dataset-managed storage would create repo bloat, binary churn, and poor diffs.

## Motivation

Nanograph currently supports text-only embeddings via OpenAI. Two capabilities are still desirable:

1. multimodal embeddings for image/audio/video/PDF retrieval
2. durable references to media assets

But the original blob-as-property design is the wrong storage shape for nanograph:

- it makes binary payloads look like row values
- it complicates mutation params and query output
- it pushes media into dataset-managed storage unless external references are enforced everywhere

Media nodes solve this cleanly:

- assets get identity
- metadata is natural
- many nodes can point to the same asset
- embeddings belong to the asset node
- bytes stay outside `.nano/`

## Background

### Gemini Embedding 2

`gemini-embedding-2-preview` is a multimodal embedding model that maps text, images, audio, video, and PDFs into one vector space.

Relevant constraints:

| Modality | Constraints |
|----------|------------|
| Text | 8,192 tokens max |
| Image | Max 6 per request; PNG, JPEG |
| Audio | Max 80 seconds; MP3, WAV |
| Video | Max 120 seconds; MP4, MOV; 32-frame max sampling |
| PDF | Max 6 pages |

Output dimensions: 128–3,072.

Task types distinguish document embeddings from query embeddings, so the provider contract must model both.

### Lance Blob V2

Lance Blob V2 supports inline, packed, dedicated, and external blob storage. For nanograph, the important lesson is not to use managed blob payload storage at all.

Even `Dedicated` blobs are still dataset-managed files, not a separate user-controlled media store.

For nanograph’s use case:

- `External` is the only relevant Lance blob mode
- but for v1 media nodes, we do not need Blob V2 columns at all
- storing external URIs as normal typed properties is simpler and aligns better with graph modeling

Blob V2 remains useful background because it confirms the “external URI only” direction is sane, but the proposal below does not depend on Blob V2 columns.

## Design

### Phase 1: Media Nodes, Not Blob Properties

Model media as normal node types with URI, mime, metadata, and optional embeddings.

Example:

```ngql
node Product {
    name: String @key
    description: String
}

node PhotoAsset {
    uri: String @media_uri(mime) @key
    mime: String
    sha256: String?
    width: Int?
    height: Int?
    embedding: Vector(768)? @embed(uri)
}

edge HasPhoto {
    from: Product
    to: PhotoAsset
}
```

Another example:

```ngql
node DocumentAsset {
    uri: String @media_uri(mime) @key
    mime: String
    sha256: String?
    title: String?
    embedding: Vector(768)? @embed(uri)
}

edge Attachment {
    from: Ticket
    to: DocumentAsset
}
```

This means:

- media bytes are never stored in `nodes/*` or `edges/*`
- the graph stores references and derived state only
- media is reusable and queryable as its own entity

### New Schema Annotation: `@media_uri`

Add `@media_uri(mime_prop)` on `String` or `String?` properties.

Example:

```ngql
uri: String @media_uri(mime) @key
mime: String
```

Rules:

- only valid on `String` / `String?`
- argument must name a sibling `String` / `String?` property that stores mime type
- value is a URI, not free text
- `@embed(uri)` on a `@media_uri(...)` property means multimodal embedding, not text embedding
- `@media_uri` properties are returnable and filterable as strings if needed, because they are URIs, not bytes

### External Storage Model

Media bytes live outside `.nano/`.

Supported locations:

- local folder via `file://...`
- object store via `s3://...`
- later: other URI schemes if operationally supported

Recommended local layout:

```text
repo/
├── db/
│   └── mygraph.nano/
└── media/
    ├── photos/
    ├── pdfs/
    └── audio/
```

The media root may be inside the repo and gitignored, or outside the repo entirely.

Nanograph stores only URIs like:

- `file:///Users/andrew/code/project/media/photos/sunset.png`
- `s3://bucket/path/to/manual.pdf`

### JSONL Input Formats

Media-node URI properties accept convenient import forms:

```jsonl
{"type": "PhotoAsset", "data": {"uri": "@file:photos/sunset.png"}}
{"type": "PhotoAsset", "data": {"uri": "@base64:iVBORw0KGgo..."}}
{"type": "PhotoAsset", "data": {"uri": "@uri:file:///mnt/media/library/sunset.png"}}
{"type": "PhotoAsset", "data": {"uri": "@uri:s3://media-bucket/photos/sunset.png"}}
```

Meaning:

- `@file:path` → import into configured media root, then store the resulting external URI
- `@base64:data` → decode, import into configured media root, then store the resulting external URI
- `@uri:...` → store URI directly, no copy

Plain strings without a prefix are rejected for `@media_uri` properties.

### Loader Semantics

`store/loader/jsonl.rs` changes:

- detect `@media_uri(...)` properties
- parse `@file:`, `@base64:`, `@uri:`
- if input is `@file:` or `@base64:`, import bytes into configured media root
- detect mime from magic bytes, with extension only as fallback
- populate the visible `mime` property if missing
- validate provided `mime` if user supplied one explicitly
- optionally compute `sha256`

Important: the loader translates convenient inputs into durable external URIs. The dataset stores the URI, not the payload.

### Query Output

No base64 blob output is needed in the normal query path.

Media nodes return normal scalar fields:

- `uri`
- `mime`
- optional metadata
- optional embeddings if explicitly requested

Example:

```ngql
query photos {
    $p: PhotoAsset
    return { $p.uri, $p.mime, $p.width, $p.height }
}
```

This is intentionally simpler than trying to push raw bytes through JSON query output.

### Export

`nanograph export` should preserve URIs by default.

Optional portable export mode may:

- copy referenced media into an export-local `media/` folder
- rewrite exported JSONL to `@file:` references

Default behavior should not silently copy large external assets.

### Mutations

Query mutations should support typed runtime parameters for media URIs.

Example parameter envelopes:

```json
{"$media": {"file": "photos/sunset.png"}}
{"$media": {"base64": "iVBORw0KGgo...", "mime_type": "image/png"}}
{"$media": {"uri": "s3://media-bucket/photos/sunset.png", "mime_type": "image/png"}}
```

These are only valid for `@media_uri(...)` properties.

The query AST does not need media literals in Phase 1. This is a runtime parameter type only.

Delete semantics:

- deleting a media node removes the graph record
- deleting an edge only removes the relationship
- external media lifecycle is a separate policy question
- nanograph must not assume it owns deletion of files referenced by URI

## Phase 2: Embedding Provider Abstraction

Before adding Gemini, refactor embeddings around a provider trait.

```rust
#[async_trait]
pub(crate) trait EmbeddingProvider: Send + Sync {
    async fn embed_texts(
        &self,
        inputs: &[String],
        dim: usize,
        role: EmbedRole,
    ) -> Result<Vec<Vec<f32>>>;

    async fn embed_media(
        &self,
        inputs: &[MediaSource],
        dim: usize,
        role: EmbedRole,
    ) -> Result<Vec<Vec<f32>>> {
        Err(NanoError::Execution(
            "this provider does not support multimodal embeddings".into(),
        ))
    }

    fn model_name(&self) -> &str;
    fn supports_modality(&self, modality: Modality) -> bool;
}

pub(crate) enum MediaSource {
    LocalFile {
        path: PathBuf,
        mime_type: String,
        size_bytes: u64,
    },
    RemoteUri {
        uri: String,
        mime_type: String,
    },
    TempFile {
        path: PathBuf,
        mime_type: String,
        size_bytes: u64,
    },
}

pub(crate) enum EmbedRole {
    Document,
    Query,
}

pub(crate) enum Modality {
    Text,
    Image,
    Audio,
    Video,
    Pdf,
}
```

Notes:

- no `Vec<u8>`-only API
- providers may stream or stage content as needed
- `Document` and `Query` are explicit because retrieval semantics require both

Configuration:

```toml
[embedding]
provider = "gemini"           # "openai" | "gemini" | "mock"
model = "gemini-embedding-2-preview"
dimensions = 768
document_task_type = "RETRIEVAL_DOCUMENT"
query_task_type = "RETRIEVAL_QUERY"
```

## Phase 3: Multimodal `@embed` on Media URIs

Allow `@embed(source)` where `source` is a `@media_uri(...)` property.

Example:

```ngql
node PhotoAsset {
    uri: String @media_uri(mime) @key
    mime: String
    embedding: Vector(768)? @embed(uri)
}
```

Rules:

- `@embed(uri)` where `uri` has `@media_uri(...)` uses multimodal embedding
- nullable media URI requires nullable vector target
- mime type comes from the sibling mime property named by `@media_uri(...)`
- if mime is absent at load/materialization time, nanograph detects it and persists it first
- query-time text embedding for `nearest($asset.embedding, $q)` uses `EmbedRole::Query`
- stored media embeddings use `EmbedRole::Document`

### Materialization Changes

`store/loader/embeddings.rs`:

- detect whether an embed source is text or media URI
- for media URI sources:
  - resolve local file or remote URI into `MediaSource`
  - determine modality from mime
  - call `provider.embed_media(..., dim, EmbedRole::Document)`
  - store the vector on the media node row

Batching defaults:

- Text: `EMBED_BATCH_SIZE`
- Image: up to 6
- Audio/Video: batch 1
- PDF: up to 6

### Cache Changes

Cache key must include:

- provider
- model
- dimensions
- role
- mime type

Recommended identity fields:

- if imported into local media root: `sha256`
- if direct URI: `uri` plus optional future version/etag signal

For direct external URIs, nanograph should treat media as immutable by convention unless the user explicitly requests re-embedding.

## Persistence Layout Changes

```text
<name>.nano/
├── ...existing files...
├── nodes/<type_id_hex>/
│   └── *.lance          # URI, mime, metadata, embeddings
└── _embedding_cache.jsonl
```

No blob payload bytes are stored under `.nano/`.

Media lives outside the database:

```text
<media-root>/
├── photos/
├── audio/
├── video/
└── pdf/
```

Or in object storage:

```text
s3://bucket/path/...
```

## Migration

- Existing databases do not need storage-format migration.
- This is a schema/modeling change, not a Lance blob-format change.
- Adopting this pattern means:
  - adding media node types
  - adding media edges
  - moving any future blob-like fields into media-node relationships instead of scalar properties
- Existing embedding caches gain `provider` and `role` fields; missing values are interpreted compatibly.

## Query Language

No new query syntax for traversal is required.

New schema annotation:

- `@media_uri(mime_prop)`

Media nodes participate in the graph like any other nodes:

- traverse to them
- filter on `mime`, metadata, or URI if needed
- search by `nearest()` over their embedding vectors

Example:

```ngql
query product_photos($q: String) {
    $p: Product
    $p -[HasPhoto]-> $m: PhotoAsset
    order by nearest($m.embedding, $q)
    limit 10
    return { $p.name, $m.uri, $m.mime }
}
```

This is the main benefit of the design: media is now a graph entity instead of a strange scalar blob value.

## What This Enables

```ngql
node Product {
    name: String @key
    description: String
}

node PhotoAsset {
    uri: String @media_uri(mime) @key
    mime: String
    embedding: Vector(768)? @embed(uri)
}

edge HasPhoto {
    from: Product
    to: PhotoAsset
}

query find_products_by_image_description($q: String) {
    $p: Product
    $p -[HasPhoto]-> $m: PhotoAsset
    order by nearest($m.embedding, $q)
    limit 10
    return { $p.name, $m.uri }
}
```

Cross-modal retrieval works because text queries and media embeddings live in the same vector space, while the bytes themselves remain outside the DB.

## Risks & Open Questions

1. **External lifecycle**
   Nanograph stores references, not ownership. Delete/GC policy for external files must be explicit.

2. **Import policy**
   `@file:` and `@base64:` need deterministic import naming, deduplication, and overwrite rules.

3. **URI stability**
   If users point at mutable external URIs, embeddings can drift from content. Imported media and stored `sha256` mitigate this.

4. **Remote auth**
   S3 or other remote URI embedding/backfill requires credentials and predictable access in loader and query-time embedding paths.

5. **Provider limits**
   Large PDFs, audio, and video still need staging and batching discipline. This is why `MediaSource` must not be an all-bytes-in-memory API.

## Phasing Summary

| Phase | Scope | Dependencies |
|-------|-------|-------------|
| **1: Media Nodes** | `@media_uri`, external media import, media-node modeling | None |
| **2: Providers** | provider trait, Gemini provider, config | None |
| **3: Multimodal** | `@embed` on media URI properties | Phase 1 + Phase 2 |

Phases 1 and 2 are independent. Phase 3 requires both.
