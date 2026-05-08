---
title: Embeddings
slug: embeddings
---

# Embeddings

nanograph can materialize vector fields from text and media sources.

You declare an embedding target with `@embed(source_prop)` on a `Vector(dim)` property:

```graphql
node Signal {
    slug: String @key
    summary: String
    embedding: Vector(768)? @embed(summary) @index
}
```

For media nodes, the source property can be a `@media_uri(...)` field:

```graphql
node PhotoAsset {
    slug: String @key
    uri: String @media_uri(mime)
    mime: String
    embedding: Vector(768)? @embed(uri) @index
}
```

nanograph uses these vectors for `nearest(...)` semantic ranking and hybrid ranking with `rrf(...)`.

## How embedding materialization works

`@embed(source_prop)` means:

- the target must be `Vector(dim)` or `Vector(dim)?`
- the source can be a text property or a `@media_uri(...)` property
- during `nanograph load`, missing or null target vectors can be generated automatically
- `nanograph embed` backfills or recomputes vectors on existing rows

String query inputs in `nearest($n.embedding, $q)` are also embedded at query time with the currently configured provider and model.

nanograph uses retrieval-aware roles internally:

- stored row/source embeddings use a document role
- string query embeddings use a query role

`nearest()` returns cosine distance, so lower scores are better.

## Provider behavior

| Provider | Default model | Text sources | Media sources |
|----------|---------------|--------------|---------------|
| OpenAI | `text-embedding-3-small` | Supported | Not supported |
| Gemini | `gemini-embedding-2-preview` | Supported | Supported |
| LM Studio | none — set explicitly | Supported | Not supported |
| Mock | deterministic test provider | Supported | Supported in tests/examples |

Important implications:

- OpenAI is text-only in nanograph today
- if `@embed(...)` points at a `@media_uri(...)` field and OpenAI is configured, embedding will fail
- Gemini supports both text and media sources in nanograph
- LM Studio runs locally as a separate desktop app and serves an OpenAI-compatible `/v1/embeddings` endpoint; it is text-only and the loaded model dictates the output dimension
- the mock provider is useful for examples and tests, not production retrieval quality

For OpenAI and Gemini, nanograph asks the provider for the dimension declared in your schema, so `Vector(dim)` is the contract even when the provider's native model dimension is larger. LM Studio does not honor a requested dimension — its loaded model produces a fixed native dimension, and nanograph fails with a clear error if that dimension does not match your schema's `Vector(dim)`.

## Configuring embeddings

Typical `nanograph.toml` setup:

```toml
[embedding]
provider = "openai"
model = "text-embedding-3-small"
batch_size = 64
chunk_size = 0
chunk_overlap_chars = 128
api_key_env = "OPENAI_API_KEY"
```

Gemini example:

```toml
[embedding]
provider = "gemini"
model = "gemini-embedding-2-preview"
api_key_env = "GEMINI_API_KEY"
```

LM Studio example:

```toml
[embedding]
provider = "lmstudio"
# Required: must match the model loaded in LM Studio's Local Server tab.
model = "nomic-embed-text-v1.5"
# Optional: only needed if LM Studio is not on the default localhost:1234.
# base_url = "http://localhost:1234/v1"
# Optional: only needed if LM Studio is fronted by an auth proxy.
# api_key_env = "LMSTUDIO_API_KEY"
```

Make sure LM Studio is running with an embedding model loaded in the Local Server tab before invoking nanograph.

Put the matching secret in `.env.nano`:

```bash
OPENAI_API_KEY=sk-...
# or
GEMINI_API_KEY=...
# LM Studio usually does not need a key
# LMSTUDIO_API_KEY=
```

You can get a free Gemini API key from [Google AI Studio](https://aistudio.google.com/).

You can also override provider/model at runtime with:

- `NANOGRAPH_EMBED_PROVIDER`
- `NANOGRAPH_EMBED_MODEL`

Provider auto-detection is:

- use `NANOGRAPH_EMBED_PROVIDER` if set (accepted values: `openai`, `gemini`, `lmstudio`)
- otherwise, if the configured model starts with `gemini-`, use Gemini
- otherwise, if `GEMINI_API_KEY` is present and `OPENAI_API_KEY` is not, use Gemini
- otherwise, default to OpenAI

LM Studio is never auto-detected — always select it explicitly.

See [config.md](config.md) for the full config precedence and env mapping.

## Text embeddings

Text sources work with both OpenAI and Gemini.

Example:

```graphql
node Character {
    slug: String @key
    bio: String
    embedding: Vector(1536)? @embed(bio) @index
}
```

Backfill existing rows:

```bash
nanograph embed --db app.nano --type Character --property embedding --only-null
```

If you switch providers or models, recompute vectors so they remain comparable:

```bash
nanograph embed --db app.nano
```

### Chunking and batching

For long text workloads, nanograph supports:

- `batch_size` in `nanograph.toml` or `NANOGRAPH_EMBED_BATCH_SIZE`
- `chunk_size` in `nanograph.toml` or `NANOGRAPH_EMBED_CHUNK_CHARS`
- `chunk_overlap_chars` in `nanograph.toml` or `NANOGRAPH_EMBED_CHUNK_OVERLAP_CHARS`

Chunking applies to text inputs only. Media embeddings are not chunked.

## Gemini media embeddings

Gemini is the built-in path for media embeddings in nanograph.

Supported media families:

- images
- audio
- video
- PDF documents

nanograph enforces these Gemini-side limits locally:

- text: conservative local estimate capped at 8192 input tokens
- images: PNG or JPEG only; nanograph batches media requests in groups of up to 6
- audio: `audio/*`
- video: MP4 or MOV only, up to 120 seconds
- documents: PDF only, up to 6 pages

If validation fails, `load` or `embed` fails before the provider call is sent.

### URI handling for media embeddings

nanograph treats different media URI sources differently:

- `@file:` and `@base64:` import bytes into the media root and then embed from local `file://` assets
- local media files are embedded by reading bytes and sending inline data
- `http://` and `https://` media URIs are fetched, validated, and embedded inline
- non-HTTP remote image and audio URIs can be passed through as provider file URIs
- non-HTTP remote PDF and video URIs are rejected because nanograph cannot validate page count or duration without reading the bytes first

For media storage formats and media-root behavior, see [blobs.md](blobs.md).

## `nanograph embed`

Use `nanograph embed` to backfill or recompute `@embed(...)` targets on existing rows:

```bash
nanograph embed --db <db_path> [--type <NodeType>] [--property <vector_prop>] [--only-null] [--limit <n>] [--reindex] [--dry-run]
```

Common patterns:

- `nanograph embed --db app.nano`
  - recompute all embed targets
- `nanograph embed --db app.nano --only-null`
  - fill only missing vectors
- `nanograph embed --db app.nano --type Signal --property embedding`
  - scope to one target field
- `nanograph embed --db app.nano --reindex`
  - rebuild touched vector indexes
- `nanograph embed --db app.nano --dry-run`
  - preview the work without writing

`--property` requires `--type`.

## Querying with embeddings

Text-to-text retrieval:

```graphql
query semantic_search($q: String) {
    match { $s: Signal }
    return {
        $s.slug as slug,
        nearest($s.embedding, $q) as score
    }
    order { nearest($s.embedding, $q) }
    limit 5
}
```

Text-to-media retrieval:

```graphql
query products_from_image_search($q: String) {
    match {
        $product: Product
        $product hasPhoto $img
    }
    return {
        $product.slug as product,
        $img.slug as image,
        nearest($img.embedding, $q) as score
    }
    order { nearest($img.embedding, $q) }
    limit 5
}
```

If you already have vectors from another pipeline, you can load them into a normal `Vector(dim)` property and query them exactly the same way.

## See also

- [search.md](search.md)
- [schema.md](schema.md)
- [config.md](config.md)
- [blobs.md](blobs.md)
