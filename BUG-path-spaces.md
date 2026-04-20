# Bug: paths with spaces break Lance namespace resolution

## Summary

Any `Database::init` / `Database::open` on a path whose filesystem location
contains a space character fails when subsequent operations try to open the
internal `__graph_snapshot` / `__graph_tx` / `__graph_deletes` Lance datasets.

This affects every macOS user, because `~/Library/Application Support/` — the
canonical location for app-private data on macOS — contains a literal space.
It also affects user-chosen paths like `~/Documents/My Graph.nano`.

## Reproducer

```sh
$ cargo run -p nanograph-cli -- init "/tmp/with spaces.nano" --schema crates/nanograph/tests/fixtures/test.pg
Error: lance error: open staged dataset __graph_snapshot error:
  Dataset at path tmp/with spaces.nano/bbf0e98a___graph_snapshot was not found:
  Not found: tmp/with spaces.nano/bbf0e98a___graph_snapshot/_versions, ...

$ cargo run -p nanograph-cli -- init "/tmp/no-space.nano" --schema crates/nanograph/tests/fixtures/test.pg
OK: Initialized database at /tmp/no-space.nano
```

Notice in the failing case:

1. The leading `/` is missing (`tmp/...` instead of `/tmp/...`).
2. The path is URL-decoded *inside* the error message — `with spaces` stays
   as a literal space rather than `%20`. But when the same operation is
   triggered from a path that was already percent-encoded by macOS's
   `NSFileManager` (e.g. `Application%20Support`), the `%20` *survives* into
   the Lance lookup, and the directory search fails because the real
   filesystem entry has a literal space.

Both symptoms point at the same root cause: nanograph hands a POSIX path to
Lance where Lance expects a URI.

## Root cause

`crates/nanograph/src/store/namespace.rs:58`:

```rust
pub(crate) async fn open_directory_namespace(db_path: &Path) -> Result<Arc<dyn LanceNamespace>> {
    let namespace = DirectoryNamespaceBuilder::new(db_path.to_string_lossy().to_string())
        .manifest_enabled(true)
        .dir_listing_enabled(false)
        .table_version_tracking_enabled(true)
        .table_version_storage_enabled(true)
        .inline_optimization_enabled(true)
        .build()
        .await
        .map_err(|err| NanoError::Lance(format!("open directory namespace error: {}", err)))?;
    Ok(Arc::new(namespace))
}
```

`DirectoryNamespaceBuilder::new(...)` is expecting a URI string. Passing a
POSIX path that happens to have no space characters works by accident —
every character is URI-legal, so URL parsing yields the same string back.
Once the path contains any reserved character (space is the most common;
`#`, `?`, `%`, `[`, `]` are all also illegal in unescaped URIs), Lance's
parsing drifts from the actual filesystem path.

## Better implementation proposal

This should be treated as a URI/path normalization fix, not just a one-line
`file://` wrapper.

### Goals

1. Always give Lance a proper `file://` URI when opening a local
   `DirectoryNamespace`.
2. Keep compatibility with existing nanograph code and older local DBs that
   may still hold plain filesystem paths.
3. Stop using `strip_prefix("file://")` as a fake URI parser, because that
   does not decode `%20` back into a real space.

### Why the simple fix is incomplete

Changing only this:

```rust
DirectoryNamespaceBuilder::new(db_path.to_string_lossy().to_string())
```

to this:

```rust
DirectoryNamespaceBuilder::new(Url::from_file_path(db_path)?.to_string())
```

is not enough.

Today nanograph has many places that do this pattern:

```rust
let normalized = location.strip_prefix("file://").unwrap_or(location);
let path = PathBuf::from(normalized);
```

That works only for raw filesystem paths. Once the namespace starts returning
real URIs like:

```text
file:///tmp/with%20spaces.nano/__graph_snapshot.lance
```

the current code turns that into:

```text
/tmp/with%20spaces.nano/__graph_snapshot.lance
```

which is still wrong on disk.

So the fix needs two parts:

1. encode local DB roots as proper `file://` URIs on the way into Lance
2. decode `file://` URIs back into real filesystem paths on the way out

### Recommended implementation shape

Add two small helpers in `crates/nanograph/src/store/namespace.rs` and make
them the only path/URI bridge for namespace code.

```rust
use std::path::{Path, PathBuf};
use url::Url;

fn absolutize_local_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

pub(crate) fn local_path_to_file_uri(path: &Path) -> Result<String> {
    let absolute = absolutize_local_path(path)?;
    let url = Url::from_directory_path(&absolute).map_err(|_| {
        NanoError::Lance(format!(
            "failed to convert database path to file URI: {}",
            absolute.display()
        ))
    })?;
    Ok(url.to_string().trim_end_matches('/').to_string())
}

pub(crate) fn namespace_location_to_local_path(
    db_dir: &Path,
    location: &str,
) -> Result<PathBuf> {
    if let Ok(url) = Url::parse(location) {
        if url.scheme() == "file" {
            return url.to_file_path().map_err(|_| {
                NanoError::Lance(format!(
                    "failed to convert file URI to local path: {}",
                    location
                ))
            });
        }
    }

    // Backward-compat path for older plain-path values.
    let path = PathBuf::from(location);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(absolutize_local_path(db_dir)?.join(path))
    }
}
```

Then change `open_directory_namespace(...)` to use the URI helper:

```rust
pub(crate) async fn open_directory_namespace(db_path: &Path) -> Result<Arc<dyn LanceNamespace>> {
    let root_uri = local_path_to_file_uri(db_path)?;
    let namespace = DirectoryNamespaceBuilder::new(root_uri)
        .manifest_enabled(true)
        .dir_listing_enabled(false)
        .table_version_tracking_enabled(true)
        .table_version_storage_enabled(true)
        .inline_optimization_enabled(true)
        .build()
        .await
        .map_err(|err| NanoError::Lance(format!("open directory namespace error: {}", err)))?;
    Ok(Arc::new(namespace))
}
```

### Important compatibility note

Do not require absolute paths at the public API boundary. `Database::init`
currently accepts relative paths, so the helper should absolutize them before
converting to a URI.

### Required follow-up refactor

Replace every local pattern like this:

```rust
location.strip_prefix("file://").unwrap_or(location)
PathBuf::from(...)
```

with the shared helper above.

At minimum, this needs to be updated in:

- `store/snapshot.rs`
- `store/namespace_commit.rs`
- `store/v4_graph_log.rs`
- `store/namespace_lineage_graph_log.rs`
- `store/lance_io.rs`
- `store/blob_store.rs`
- `store/storage_migrate.rs`
- `store/migration.rs`
- `store/database/persist.rs`

That keeps the change surgical but complete.

### Dependency note

If nanograph imports `url::Url` directly, `url` should be added as a direct
dependency. Being transitive via Lance is not enough for Rust source usage.

## Regression test

Something like:

```rust
#[tokio::test]
async fn database_works_with_path_containing_spaces() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("my folder/db.nano");

    let db = Database::init(&db_path, TEST_SCHEMA).await.unwrap();
    db.load(TEST_JSONL).await.unwrap();

    let reopened = Database::open(&db_path).await.unwrap();
    let _ = reopened.changes(0, None).await.unwrap();
}

#[tokio::test]
async fn database_works_with_reserved_chars_in_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("hash#percent%db/db.nano");

    let db = Database::init(&db_path, TEST_SCHEMA).await.unwrap();
    db.load(TEST_JSONL).await.unwrap();

    let reopened = Database::open(&db_path).await.unwrap();
    let _ = reopened.changes(0, None).await.unwrap();
}
```

## Blast radius

This is still the right surgical fix point.

`open_directory_namespace(...)` is the shared source for local namespace
construction across the storage stack, and it is used by many callers across
`store/` (`snapshot`, `namespace_commit`, `lance_io`, `blob_store`,
`storage_migrate`, graph-log code, maintenance, migration, and tests).

So the blast radius is broad in effect, but narrow in implementation:

1. fix the URI construction at `open_directory_namespace(...)`
2. replace ad hoc `file://` string stripping with one real URI-to-path helper

## Observed in

- macOS 26 (Tahoe), APFS case-insensitive.
- Reproduces via both `nanograph-cli` directly and through `nanograph-ffi`
  → Swift SDK.
- Reported while integrating nanograph into a sandboxed Mac app, where the
  only writable per-app container path (`~/Library/Containers/<id>/Data/Library/Application Support/`)
  contains a space and triggered this on every launch.
