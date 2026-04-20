use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::{RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use lance::Dataset;
use lance::blob::{BlobArrayBuilder, blob_field};
use lance::dataset::WriteMode;
use lance_file::version::LanceFileVersion;
use sha2::{Digest, Sha256};

use crate::error::{NanoError, Result};
use crate::store::lance_io::{
    latest_lance_dataset_version, read_lance_projected_batches,
    write_lance_batch_with_mode_and_storage_version,
};
use crate::store::manifest::DatasetEntry;
use crate::store::namespace::{
    BLOB_STORE_TABLE_ID, batch_publish_namespace_versions, local_path_to_file_uri,
    namespace_published_version_for_table, namespace_location_to_dataset_uri,
    namespace_location_to_local_path,
    namespace_location_to_manifest_dataset_path, open_directory_namespace,
    resolve_table_location, write_namespace_batch,
};
use crate::store::snapshot::read_committed_graph_snapshot;
use crate::store::storage_generation::{StorageGeneration, detect_storage_generation};

pub(crate) const BLOB_STORE_DATASET_DIR: &str = "__blob_store";
const MANAGED_BLOB_URI_PREFIX: &str = "lanceblob://sha256/";
const BLOB_COLUMN_NAME: &str = "blob";

pub(crate) struct ManagedBlobRow {
    pub(crate) blob_id: String,
    pub(crate) mime: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) source_hint: Option<String>,
    pub(crate) created_at: String,
}

pub(crate) fn blob_store_dataset_path(db_path: &Path) -> PathBuf {
    db_path.join(BLOB_STORE_DATASET_DIR)
}

pub(crate) fn managed_blob_uri(blob_id: &str) -> String {
    format!("{MANAGED_BLOB_URI_PREFIX}{blob_id}")
}

pub(crate) fn parse_managed_blob_id(uri: &str) -> Option<&str> {
    let blob_id = uri.strip_prefix(MANAGED_BLOB_URI_PREFIX)?;
    if blob_id.is_empty() || !blob_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(blob_id)
}

pub(crate) fn store_managed_blob_blocking(
    db_path: &Path,
    bytes: &[u8],
    mime_type: &str,
    source_hint: Option<&str>,
) -> Result<String> {
    let db_path = db_path.to_path_buf();
    let bytes = bytes.to_vec();
    let mime_type = mime_type.to_string();
    let source_hint = source_hint.map(str::to_string);
    let join = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| {
                NanoError::Storage(format!(
                    "failed to initialize managed media runtime: {}",
                    err
                ))
            })?;
        runtime.block_on(store_managed_blob(
            &db_path,
            &bytes,
            &mime_type,
            source_hint.as_deref(),
        ))
    });

    join.join()
        .map_err(|_| NanoError::Storage("managed media import thread panicked".to_string()))?
}

pub(crate) async fn store_managed_blob(
    db_path: &Path,
    bytes: &[u8],
    mime_type: &str,
    source_hint: Option<&str>,
) -> Result<String> {
    let blob_id = sha256_hex(bytes);
    if find_blob_row_index(db_path, &blob_id).await?.is_some() {
        return Ok(managed_blob_uri(&blob_id));
    }

    let batch = managed_blob_record_batch(&blob_id, bytes, mime_type, source_hint)?;
    match detect_storage_generation(db_path)? {
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage) => {
            let namespace = open_directory_namespace(db_path).await?;
            let version = write_namespace_batch(
                namespace,
                BLOB_STORE_TABLE_ID,
                batch,
                WriteMode::Append,
                None,
            )
            .await?;
            if let Some(published) =
                namespace_published_version_for_table(db_path, BLOB_STORE_TABLE_ID, version.version)
                    .await?
            {
                batch_publish_namespace_versions(db_path, &[published]).await?;
            }
        }
        None => {
            let path = blob_store_dataset_path(db_path);
            let mode = if path.exists() {
                WriteMode::Append
            } else {
                WriteMode::Overwrite
            };
            write_lance_batch_with_mode_and_storage_version(
                &path,
                batch,
                mode,
                Some(LanceFileVersion::V2_2),
            )
            .await?;
        }
    }
    Ok(managed_blob_uri(&blob_id))
}

pub(crate) async fn read_managed_blob_bytes(db_path: &Path, blob_id: &str) -> Result<Vec<u8>> {
    let Some((version, row_index)) = find_blob_row_index(db_path, blob_id).await? else {
        return Err(NanoError::Storage(format!(
            "managed blob {} not found in {}",
            blob_id,
            db_path.display()
        )));
    };

    let dataset = Arc::new(open_blob_store_dataset(db_path, version).await?);

    let blobs = dataset
        .take_blobs_by_indices(&[row_index as u64], BLOB_COLUMN_NAME)
        .await
        .map_err(|err| NanoError::Lance(format!("read managed blob error: {}", err)))?;
    let blob = blobs.first().ok_or_else(|| {
        NanoError::Storage(format!(
            "managed blob {} resolved to an empty blob result",
            blob_id
        ))
    })?;
    let bytes = blob
        .read()
        .await
        .map_err(|err| NanoError::Lance(format!("read managed blob bytes error: {}", err)))?;
    Ok(bytes.to_vec())
}

async fn find_blob_row_index(db_path: &Path, blob_id: &str) -> Result<Option<(u64, usize)>> {
    let Some(entry) = readable_blob_store_entry(db_path).await? else {
        return Ok(None);
    };

    let version = entry.dataset_version;
    let batches = match detect_storage_generation(db_path)? {
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage) => {
            let dataset_path = db_path.join(&entry.dataset_path);
            let locator = crate::store::metadata::DatasetLocator {
                db_path: db_path.to_path_buf(),
                table_id: BLOB_STORE_TABLE_ID.to_string(),
                dataset_path,
                dataset_version: version,
                row_count: entry.row_count,
                namespace_managed: false,
            };
            crate::store::lance_io::read_lance_projected_batches_for_locator(&locator, &["blob_id"])
                .await?
        }
        None => {
            let path = blob_store_dataset_path(db_path);
            if !path.exists() {
                return Ok(None);
            }
            read_lance_projected_batches(&path, version, &["blob_id"]).await?
        }
    };
    let mut row_base = 0usize;
    for batch in batches {
        let ids = batch
            .column_by_name("blob_id")
            .ok_or_else(|| {
                NanoError::Storage("blob store batch missing blob_id column".to_string())
            })?
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| {
                NanoError::Storage("blob store blob_id column is not Utf8".to_string())
            })?;
        for row in 0..batch.num_rows() {
            if ids.value(row) == blob_id {
                return Ok(Some((version, row_base + row)));
            }
        }
        row_base += batch.num_rows();
    }

    Ok(None)
}

pub(crate) async fn ensure_blob_store_table(db_path: &Path) -> Result<DatasetEntry> {
    if let Some(entry) = current_blob_store_entry(db_path).await? {
        return Ok(entry);
    }

    match detect_storage_generation(db_path)? {
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage) => {
            let namespace = open_directory_namespace(db_path).await?;
            let batch = empty_blob_store_batch();
            let version = write_namespace_batch(
                namespace.clone(),
                BLOB_STORE_TABLE_ID,
                batch,
                WriteMode::Overwrite,
                None,
            )
            .await?;
            let location = resolve_table_location(namespace, BLOB_STORE_TABLE_ID).await?;
            Ok(DatasetEntry::internal(
                BLOB_STORE_TABLE_ID,
                manifest_dataset_path(db_path, &location, BLOB_STORE_TABLE_ID)?,
                version.version,
                0,
            ))
        }
        None => {
            let path = blob_store_dataset_path(db_path);
            if !path.exists() {
                write_lance_batch_with_mode_and_storage_version(
                    &path,
                    empty_blob_store_batch(),
                    WriteMode::Overwrite,
                    Some(LanceFileVersion::V2_2),
                )
                .await?;
            }
            current_blob_store_entry(db_path).await?.ok_or_else(|| {
                NanoError::Storage(format!(
                    "blob store table {} was created but could not be reopened",
                    BLOB_STORE_TABLE_ID
                ))
            })
        }
    }
}

pub(crate) async fn current_blob_store_entry(db_path: &Path) -> Result<Option<DatasetEntry>> {
    match detect_storage_generation(db_path)? {
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage) => {
            let namespace = open_directory_namespace(db_path).await?;
            let location =
                match resolve_table_location(namespace.clone(), BLOB_STORE_TABLE_ID).await {
                    Ok(location) => location,
                    Err(_) => return Ok(None),
                };
            let location_path = namespace_location_to_local_path(db_path, &location)?;
            let version = latest_lance_dataset_version(&location_path).await?;
            let dataset_uri = namespace_location_to_dataset_uri(db_path, &location)?;
            let dataset = Dataset::open(&dataset_uri).await.map_err(|err| {
                NanoError::Lance(format!("open namespace blob store error: {}", err))
            })?
            .checkout_version(version)
            .await
            .map_err(|err| {
                NanoError::Lance(format!(
                    "checkout namespace blob store version {} error: {}",
                    version, err
                ))
            })?;
            let row_count =
                dataset.count_rows(None).await.map_err(|err| {
                    NanoError::Lance(format!("count blob store rows error: {}", err))
                })? as u64;
            Ok(Some(DatasetEntry::internal(
                BLOB_STORE_TABLE_ID,
                manifest_dataset_path(db_path, &location, BLOB_STORE_TABLE_ID)?,
                version,
                row_count,
            )))
        }
        None => {
            let path = blob_store_dataset_path(db_path);
            if !path.exists() {
                return Ok(None);
            }
            let version = latest_lance_dataset_version(&path).await?;
            let dataset_uri = local_path_to_file_uri(&path)?;
            let dataset = Dataset::open(&dataset_uri)
                .await
                .map_err(|err| NanoError::Lance(format!("open blob store error: {}", err)))?
                .checkout_version(version)
                .await
                .map_err(|err| {
                    NanoError::Lance(format!(
                        "checkout blob store version {} error: {}",
                        version, err
                    ))
                })?;
            let row_count =
                dataset.count_rows(None).await.map_err(|err| {
                    NanoError::Lance(format!("count blob store rows error: {}", err))
                })? as u64;
            Ok(Some(DatasetEntry::internal(
                BLOB_STORE_TABLE_ID,
                BLOB_STORE_DATASET_DIR,
                version,
                row_count,
            )))
        }
    }
}

async fn readable_blob_store_entry(db_path: &Path) -> Result<Option<DatasetEntry>> {
    match detect_storage_generation(db_path)? {
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage) => {
            let snapshot = read_committed_graph_snapshot(db_path)?;
            if let Some(entry) = snapshot
                .datasets
                .into_iter()
                .find(|entry| entry.effective_table_id() == BLOB_STORE_TABLE_ID)
            {
                return Ok(Some(entry));
            }
            current_blob_store_entry(db_path).await
        }
        None => current_blob_store_entry(db_path).await,
    }
}

fn empty_blob_store_batch() -> RecordBatch {
    RecordBatch::new_empty(managed_blob_schema())
}

fn managed_blob_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("blob_id", DataType::Utf8, false),
        Field::new("mime", DataType::Utf8, false),
        blob_field(BLOB_COLUMN_NAME, false),
        Field::new("source_hint", DataType::Utf8, true),
        Field::new("created_at", DataType::Utf8, false),
    ]))
}

fn manifest_dataset_path(db_path: &Path, location: &str, fallback: &str) -> Result<String> {
    namespace_location_to_manifest_dataset_path(db_path, location, fallback)
}

async fn open_blob_store_dataset(db_path: &Path, version: u64) -> Result<Dataset> {
    match detect_storage_generation(db_path)? {
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage) => {
            if let Some(entry) = readable_blob_store_entry(db_path).await?
                && entry.dataset_version == version
                && !entry.dataset_path.is_empty()
            {
                let path = db_path.join(&entry.dataset_path);
                let uri = local_path_to_file_uri(&path)?;
                let dataset = Dataset::open(&uri).await.map_err(|err| {
                    NanoError::Lance(format!(
                        "open committed blob store {} error: {}",
                        path.display(),
                        err
                    ))
                })?;
                return dataset.checkout_version(version).await.map_err(|err| {
                    NanoError::Lance(format!(
                        "checkout committed blob store version {} error: {}",
                        version, err
                    ))
                });
            }
            let namespace = open_directory_namespace(db_path).await?;
            let location = resolve_table_location(namespace, BLOB_STORE_TABLE_ID).await?;
            let dataset_uri = namespace_location_to_dataset_uri(db_path, &location)?;
            Dataset::open(&dataset_uri)
                .await
                .map_err(|err| {
                    NanoError::Lance(format!("open namespace blob store error: {}", err))
                })?
                .checkout_version(version)
                .await
                .map_err(|err| {
                    NanoError::Lance(format!(
                        "checkout namespace blob store version {} error: {}",
                        version, err
                    ))
                })
        }
        None => {
            let path = blob_store_dataset_path(db_path);
            let uri = local_path_to_file_uri(&path)?;
            let dataset = Dataset::open(&uri)
                .await
                .map_err(|err| NanoError::Lance(format!("open blob store error: {}", err)))?;
            dataset.checkout_version(version).await.map_err(|err| {
                NanoError::Lance(format!(
                    "checkout blob store version {} error: {}",
                    version, err
                ))
            })
        }
    }
}

fn managed_blob_record_batch(
    blob_id: &str,
    bytes: &[u8],
    mime_type: &str,
    source_hint: Option<&str>,
) -> Result<RecordBatch> {
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());

    managed_blob_batch(&[ManagedBlobRow {
        blob_id: blob_id.to_string(),
        mime: mime_type.to_string(),
        bytes: bytes.to_vec(),
        source_hint: source_hint.map(str::to_string),
        created_at,
    }])
}

pub(crate) fn managed_blob_batch(rows: &[ManagedBlobRow]) -> Result<RecordBatch> {
    let mut blob_builder = BlobArrayBuilder::new(rows.len());
    for row in rows {
        blob_builder
            .push_bytes(&row.bytes)
            .map_err(|err| NanoError::Storage(format!("build managed blob column: {}", err)))?;
    }
    let blob_array = blob_builder
        .finish()
        .map_err(|err| NanoError::Storage(format!("finalize managed blob column: {}", err)))?;

    let schema = managed_blob_schema();

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.blob_id.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|row| row.mime.clone()).collect::<Vec<_>>(),
            )),
            blob_array,
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.source_hint.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.created_at.clone())
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(|err| NanoError::Storage(format!("build managed blob batch: {}", err)))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{:02x}", byte);
    }
    out
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::store::database::Database;
    use crate::store::database::LoadMode;
    use crate::store::lance_io::latest_lance_dataset_version;
    use crate::store::migration::{MigrationStatus, execute_schema_migration};
    use crate::store::namespace::{namespace_latest_version, open_directory_namespace};
    use crate::store::namespace_lineage_internal::merge_namespace_lineage_internal_dataset_entries;
    use crate::store::snapshot::read_committed_graph_snapshot;
    use crate::{ParamMap, RunResult};

    #[tokio::test]
    async fn managed_blob_store_round_trips_bytes() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join(".nano");
        std::fs::create_dir_all(&db_path).unwrap();

        let uri = store_managed_blob(&db_path, b"hello", "text/plain", Some("unit-test"))
            .await
            .unwrap();
        let blob_id = parse_managed_blob_id(&uri).unwrap();
        let bytes = read_managed_blob_bytes(&db_path, blob_id).await.unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn managed_blob_store_deduplicates_by_content_hash() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join(".nano");
        std::fs::create_dir_all(&db_path).unwrap();

        let uri_a = store_managed_blob(&db_path, b"same", "text/plain", None)
            .await
            .unwrap();
        let uri_b = store_managed_blob(&db_path, b"same", "image/png", None)
            .await
            .unwrap();
        assert_eq!(uri_a, uri_b);

        let version = latest_lance_dataset_version(&blob_store_dataset_path(&db_path))
            .await
            .unwrap();
        let batches =
            read_lance_projected_batches(&blob_store_dataset_path(&db_path), version, &["blob_id"])
                .await
                .unwrap();
        let total_rows: usize = batches.iter().map(|batch| batch.num_rows()).sum();
        assert_eq!(total_rows, 1);
    }

    #[tokio::test]
    async fn namespace_lineage_internal_entries_track_latest_blob_store_version() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("db.nano");
        Database::init(
            &db_path,
            r#"
node Note {
    slug: String @key
}
"#,
        )
        .await
        .unwrap();

        let committed = read_committed_graph_snapshot(&db_path).unwrap();
        let committed_blob = committed
            .datasets
            .iter()
            .find(|entry| entry.effective_table_id() == BLOB_STORE_TABLE_ID)
            .unwrap()
            .clone();

        let uri = store_managed_blob(&db_path, b"hello", "text/plain", Some("unit-test"))
            .await
            .unwrap();
        assert!(uri.starts_with("lanceblob://sha256/"));

        let mut merged_entries = committed.datasets.clone();
        merge_namespace_lineage_internal_dataset_entries(&db_path, &mut merged_entries)
            .await
            .unwrap();
        let merged_blob = merged_entries
            .iter()
            .find(|entry| entry.effective_table_id() == BLOB_STORE_TABLE_ID)
            .unwrap();

        assert!(merged_blob.dataset_version > committed_blob.dataset_version);
        assert_eq!(merged_blob.row_count, 1);
    }

    #[tokio::test]
    async fn namespace_lineage_commits_blob_store_rows_visible_after_load() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("db.nano");
        let fixture_dir = temp.path().join("fixtures");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        let image_path = fixture_dir.join("space.png");
        std::fs::write(&image_path, b"\x89PNG\r\n\x1a\nblob-store-visible-after-load").unwrap();
        let data_path = fixture_dir.join("data.jsonl");
        std::fs::write(
            &data_path,
            format!(
                "{{\"type\":\"Asset\",\"data\":{{\"slug\":\"space\",\"uri\":\"@file:{}\"}}}}\n",
                image_path.display()
            ),
        )
        .unwrap();

        let db = Database::init(
            &db_path,
            r#"
node Asset {
    slug: String @key
    uri: String @media_uri(mime)
    mime: String
    embedding: Vector(16)?
}
"#,
        )
        .await
        .unwrap();
        db.load_file_with_mode(&data_path, LoadMode::Overwrite)
            .await
            .unwrap();
        drop(db);

        let committed = read_committed_graph_snapshot(&db_path).unwrap();
        let committed_blob = committed
            .datasets
            .iter()
            .find(|entry| entry.effective_table_id() == BLOB_STORE_TABLE_ID)
            .unwrap();
        let latest_blob = current_blob_store_entry(&db_path).await.unwrap().unwrap();
        let published_blob = namespace_latest_version(
            open_directory_namespace(&db_path).await.unwrap(),
            BLOB_STORE_TABLE_ID,
        )
        .await
        .unwrap();
        assert_eq!(
            committed_blob.dataset_version, latest_blob.dataset_version,
            "committed snapshot pinned blob store version {} but latest local version is {}",
            committed_blob.dataset_version, latest_blob.dataset_version
        );
        assert_eq!(
            committed_blob.dataset_version, published_blob.version,
            "blob store version after load is not published: committed={} published={}",
            committed_blob.dataset_version, published_blob.version
        );
        assert_eq!(committed_blob.row_count, 1);

        let reopened = Database::open(&db_path).await.unwrap();
        let rows = match reopened
            .run(
                r#"
query asset_uri($slug: String) {
    match { $asset: Asset { slug: $slug } }
    return { $asset.uri as uri }
}
"#,
                "asset_uri",
                &ParamMap::from([(
                    "slug".to_string(),
                    crate::query::ast::Literal::String("space".to_string()),
                )]),
            )
            .await
            .unwrap()
        {
            RunResult::Query(rows) => rows.to_rust_json(),
            other => panic!("expected query rows, got {:?}", other),
        };
        let uri = rows[0]["uri"].as_str().unwrap();
        let blob_id = parse_managed_blob_id(uri).unwrap();
        let bytes = read_managed_blob_bytes(&db_path, blob_id).await.unwrap();
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\nblob-store-visible-after-load");
    }

    #[tokio::test]
    async fn namespace_lineage_preserves_blob_store_rows_across_schema_migration() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("db.nano");
        let fixture_dir = temp.path().join("fixtures");
        std::fs::create_dir_all(&fixture_dir).unwrap();
        let image_path = fixture_dir.join("space.png");
        std::fs::write(&image_path, b"\x89PNG\r\n\x1a\nblob-store-after-migration").unwrap();
        let data_path = fixture_dir.join("data.jsonl");
        std::fs::write(
            &data_path,
            format!(
                "{{\"type\":\"Asset\",\"data\":{{\"slug\":\"space\",\"uri\":\"@file:{}\"}}}}\n",
                image_path.display()
            ),
        )
        .unwrap();

        let plain_schema = r#"
node Asset {
    slug: String @key
    uri: String @media_uri(mime)
    mime: String
    embedding: Vector(16)?
}
"#;
        let embed_schema = r#"
node Asset {
    slug: String @key
    uri: String @media_uri(mime)
    mime: String
    embedding: Vector(16)? @embed(uri)
}
"#;
        let db = Database::init(&db_path, plain_schema).await.unwrap();
        db.load_file_with_mode(&data_path, LoadMode::Overwrite)
            .await
            .unwrap();
        drop(db);
        let committed_before = read_committed_graph_snapshot(&db_path).unwrap();
        let committed_blob_before = committed_before
            .datasets
            .iter()
            .find(|entry| entry.effective_table_id() == BLOB_STORE_TABLE_ID)
            .unwrap()
            .clone();
        let reopened_before = Database::open(&db_path).await.unwrap();
        let rows_before = match reopened_before
            .run(
                r#"
query asset_uri($slug: String) {
    match { $asset: Asset { slug: $slug } }
    return { $asset.uri as uri }
}
"#,
                "asset_uri",
                &ParamMap::from([(
                    "slug".to_string(),
                    crate::query::ast::Literal::String("space".to_string()),
                )]),
            )
            .await
            .unwrap()
        {
            RunResult::Query(rows) => rows.to_rust_json(),
            other => panic!("expected query rows, got {:?}", other),
        };
        let blob_id_before = parse_managed_blob_id(rows_before[0]["uri"].as_str().unwrap()).unwrap();
        let bytes_before = read_managed_blob_bytes(&db_path, blob_id_before).await.unwrap();
        assert_eq!(bytes_before, b"\x89PNG\r\n\x1a\nblob-store-after-migration");

        std::fs::write(db_path.join("schema.pg"), embed_schema).unwrap();
        let migration = execute_schema_migration(&db_path, None, false, true)
            .await
            .unwrap();
        assert_eq!(migration.status, MigrationStatus::Applied);
        let committed_after = read_committed_graph_snapshot(&db_path).unwrap();
        let committed_blob_after = committed_after
            .datasets
            .iter()
            .find(|entry| entry.effective_table_id() == BLOB_STORE_TABLE_ID)
            .unwrap()
            .clone();
        assert_eq!(
            committed_blob_after.row_count, 1,
            "blob store row count after migration: {:?}",
            committed_blob_after
        );
        assert_eq!(
            committed_blob_after.dataset_version,
            committed_blob_before.dataset_version,
            "blob store version changed across schema migration: before={} after={}",
            committed_blob_before.dataset_version,
            committed_blob_after.dataset_version
        );

        let reopened = Database::open(&db_path).await.unwrap();
        let rows = match reopened
            .run(
                r#"
query asset_uri($slug: String) {
    match { $asset: Asset { slug: $slug } }
    return { $asset.uri as uri }
}
"#,
                "asset_uri",
                &ParamMap::from([(
                    "slug".to_string(),
                    crate::query::ast::Literal::String("space".to_string()),
                )]),
            )
            .await
            .unwrap()
        {
            RunResult::Query(rows) => rows.to_rust_json(),
            other => panic!("expected query rows, got {:?}", other),
        };
        let uri = rows[0]["uri"].as_str().unwrap();
        let blob_id = parse_managed_blob_id(uri).unwrap();
        let bytes = read_managed_blob_bytes(&db_path, blob_id).await.unwrap();
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\nblob-store-after-migration");
    }
}
