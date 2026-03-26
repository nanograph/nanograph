# NanoGraph TypeScript SDK

`nanograph-db` is the first-party Node/TypeScript SDK for NanoGraph.

## Media nodes and multimodal embeddings

NanoGraph stores media as external URIs, not blob bytes inside `.nano/`.

Typical schema:

```graphql
node PhotoAsset {
  slug: String @key
  uri: String @media_uri(mime)
  mime: String
  embedding: Vector(768)? @embed(uri) @index
}

node Product {
  slug: String @key
  name: String
}

edge HasPhoto: Product -> PhotoAsset
```

## Example

```ts
import { Database, mediaFile, mediaUri } from "nanograph-db";

const schema = `
node PhotoAsset {
  slug: String @key
  uri: String @media_uri(mime)
  mime: String
  embedding: Vector(768)? @embed(uri) @index
}

node Product {
  slug: String @key
  name: String
}

edge HasPhoto: Product -> PhotoAsset
`;

const queries = `
query products_from_image_search($q: String) {
  match {
    $product: Product
    $product hasPhoto $img
  }
  return { $product.slug as product, $img.slug as image, $img.uri as uri }
  order { nearest($img.embedding, $q) }
  limit 5
}
`;

const db = await Database.init("app.nano", schema);

await db.loadRows(
  [
    {
      type: "PhotoAsset",
      data: {
        slug: "space",
        uri: mediaFile("/absolute/path/space.jpg", "image/jpeg"),
        embedding: Array(768).fill(0),
      },
    },
    {
      type: "Product",
      data: { slug: "rocket", name: "Rocket Poster" },
    },
    {
      edge: "HasPhoto",
      from: "rocket",
      to: "space",
    },
  ],
  "overwrite",
);

process.env.NANOGRAPH_EMBED_PROVIDER = "gemini";
process.env.NANOGRAPH_EMBED_MODEL = "gemini-embedding-2-preview";
process.env.GEMINI_API_KEY = "...";

await db.embed({ typeName: "PhotoAsset", property: "embedding", onlyNull: false });

const rows = await db.run(queries, "products_from_image_search", { q: "space scene" });
```

## Notes

- `loadRows()` is a helper over normal JSONL load semantics
- `mediaFile(...)`, `mediaBase64(...)`, and `mediaUri(...)` serialize to NanoGraph's media source forms
- `describe()` includes `mediaMimeProp` for `@media_uri(...)` properties
- `embed()` uses the same provider/env setup as the CLI
- Gemini media support currently follows core engine limits, including image-only multimodal embedding in NanoGraph
