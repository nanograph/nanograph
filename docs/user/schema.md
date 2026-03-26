---
title: Schema
slug: schema
---

# Schema Language Reference

NanoGraph schemas are defined in `.pg` files. A schema declares node types, edge types, property types, and annotations. It is the source of truth for your graph's structure — all queries are validated against it at compile time.

## Node declarations

A node type declares a named entity with typed properties:

```graphql
node Person {
    name: String
    age: I32?
}
```

Nodes can inherit from a parent type:

```graphql
node Employee : Person {
    employee_id: String @unique
}
```

An `Employee` has all `Person` properties plus its own. Queries binding `$e: Employee` can access both inherited and declared properties.

## Edge declarations

An edge type connects two node types:

```graphql
edge Knows: Person -> Person
```

Edges can have properties:

```graphql
edge Knows: Person -> Person {
    since: Date?
}
```

Endpoint types are fixed — each edge type connects exactly one source type to one destination type. If a conceptual relationship spans multiple types, split it into separate edges (e.g. `TouchedRecord: Action -> Record`, `TouchedOpportunity: Action -> Opportunity`).

## Type system

### Scalar types

| Type | Description | Arrow type |
|------|-------------|------------|
| `String` | UTF-8 text | Utf8 |
| `Bool` | Boolean | Boolean |
| `I32` | 32-bit signed integer | Int32 |
| `I64` | 64-bit signed integer | Int64 |
| `U32` | 32-bit unsigned integer | UInt32 |
| `U64` | 64-bit unsigned integer | UInt64 |
| `F32` | 32-bit float | Float32 |
| `F64` | 64-bit float | Float64 |
| `Date` | Calendar date | Date32 |
| `DateTime` | Date-time value parsed from `datetime("...")` literals | Date64 (epoch milliseconds) |

### Vector type

```graphql
embedding: Vector(1536)
```

Fixed-size float vector for semantic search. The dimension must match your embedding model's output (e.g. 1536 for `text-embedding-3-small`). See [search.md](search.md) for the full embedding workflow.

### Enum type

```graphql
status: enum(active, completed, hold)
```

Enums define a closed set of allowed string values. Values are auto-sorted alphabetically and must be lowercase. Duplicate values are rejected at parse time.

### List type

```graphql
tags: [String]
notes: [String]?
```

Lists wrap any scalar type: `[String]`, `[I32]`, `[F64]`, etc. List properties cannot have `@key`, `@unique`, `@index`, or `@embed` annotations.

### Nullable

Append `?` to make any type nullable:

```graphql
rank: String?
age: I32?
tags: [String]?
```

## Annotations

Annotations modify property behavior. They appear after the type:

```graphql
slug: String @key
name: String @unique
email: String @index
embedding: Vector(1536) @embed(bio) @index
old_name: String @rename_from("previous_name")
title: String @description("Short human-readable label")
```

| Annotation | Description |
|------------|-------------|
| `@key` | Primary key for merge/upsert. One per node type. Auto-indexed. Required for edge endpoint resolution via `from`/`to` in JSONL data. |
| `@unique` | Uniqueness constraint enforced on load/upsert. Multiple per type. Nullable unique allows multiple nulls. |
| `@index` | Creates an index for the property: scalar index for scalar fields, vector index for `Vector(dim)` fields. |
| `@embed(source_prop)` | Auto-generates embeddings when the vector field is missing/null during load or backfill. Text sources use text embeddings. `@media_uri(...)` sources use multimodal embeddings. Target must be `Vector(dim)`. See [search.md](search.md#embedding-workflow) and [blobs.md](blobs.md). |
| `@media_uri(mime_prop)` | Marks a `String` property as an external media URI and points to the sibling mime property. Use this for image, document, audio, or video asset nodes. See [blobs.md](blobs.md). |
| `@rename_from("old")` | Tracks property/type renames for schema migration. |
| `@description("...")` | Optional semantic description for node types, edge types, and properties. Intended for `describe --format json`, SDK introspection, and agent context. |
| `@instruction("...")` | Optional agent-facing guidance for node and edge types. Advisory only; it does not change query execution semantics. |

**Restrictions**: List properties cannot have `@key`, `@unique`, `@index`, `@embed`, or `@media_uri`. `@instruction` is only valid on node and edge types, not on properties. `@media_uri(...)` is only valid on `String` or `String?` and must reference a sibling `String` mime field.

These annotations are metadata, not behavior:

- `describe --format json` surfaces them in CLI introspection
- TS and Swift `describe()` surface them in SDK schema metadata
- query-level metadata appears in `nanograph run` table preambles
- they do not change planning, execution, or load semantics

Example with agent-facing metadata:

```graphql
node Task @description("Tracked work item") @instruction("Prefer querying by slug.") {
    slug: String @key @description("Stable external identifier")
    title: String @description("Short human-readable summary")
    status: enum(todo, doing, done) @description("Workflow state")
}

edge DependsOn: Task -> Task
  @description("Hard execution dependency")
  @instruction("Use only for direct blockers, not loose relatedness")
```

## Naming conventions

- **Type names**: PascalCase — `Person`, `ActionItem`, `HasMentor`
- **Property names**: camelCase or snake_case, starting lowercase — `name`, `createdAt`, `release_year`
- **Edge names in queries**: camelCase with lowercase first letter — schema `HasMentor` becomes `hasMentor` in queries. See [queries.md](queries.md#edge-naming) for the conversion rule.

## Comments

```graphql
// Line comment

/* Block comment
   spanning multiple lines */
```

## Schema migration

Edit your database's `schema.pg` file, then run:

```bash
nanograph migrate mydb.nano
```

The command diffs the old and new schema IR and generates a migration plan. Steps have safety levels:

| Level | Behavior |
|-------|----------|
| `safe` | Applied automatically (add nullable property, add new type) |
| `confirm` | Requires `--auto-approve` or interactive confirmation (drop property, rename) |
| `blocked` | Cannot be auto-applied (add non-nullable property to populated type) |

Use `@rename_from("old_name")` to track renames so data is preserved. See [cli-reference.md](cli-reference.md#migrate) for full command options.

## Complete example

A Star Wars search schema with vector embeddings:

```graphql
node Character {
    slug: String @key
    name: String
    alignment: enum(light, dark, neutral)
    era: enum(prequel, original, sequel, imperial)
    bio: String
    embedding: Vector(1536) @embed(bio) @index
}

node Film {
    slug: String @key
    title: String
    release_year: I32
}

edge HasParent: Character -> Character
edge SiblingOf: Character -> Character
edge MarriedTo: Character -> Character
edge HasMentor: Character -> Character
edge DebutsIn: Character -> Film
```

This schema demonstrates:
- `@key` for merge/upsert identity and edge resolution
- `enum()` for constrained categorical values
- `Vector(1536)` with `@embed(bio)` for auto-generated embeddings
- Multiple edge types between the same node types
- Cross-type edges (`DebutsIn: Character -> Film`)
