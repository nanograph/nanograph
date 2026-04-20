use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arrow_array::RecordBatch;
use lance::dataset::WriteMode;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::catalog::schema_ir::{NodeTypeDef, SchemaIR};
use crate::error::{NanoError, Result};
use crate::store::blob_store::store_managed_blob;
use crate::store::database::Database;
use crate::store::database::persist::{json_rows_to_record_batch, record_batch_to_json_rows};
use crate::store::graph_types::{GraphCommitRecord, GraphTableVersion, GraphTouchedTableWindow};
use crate::store::indexing::{
    rebuild_node_scalar_indexes, rebuild_node_text_indexes, rebuild_node_vector_indexes,
};
use crate::store::lance_io::{latest_lance_dataset_version, read_lance_batches_for_locator};
use crate::store::manifest::DatasetEntry;
use crate::store::metadata::DatabaseMetadata;
use crate::store::namespace::{
    namespace_location_to_manifest_dataset_path, open_directory_namespace,
    resolve_table_location, write_namespace_batch,
};
use crate::store::namespace_lineage_internal::merge_namespace_lineage_internal_dataset_entries;
use crate::store::snapshot::read_committed_graph_snapshot;
use crate::store::storage_generation::{StorageGeneration, detect_storage_generation};
use crate::store::txlog::{
    commit_graph_records_and_manifest, commit_graph_records_and_manifest_namespace_lineage,
};
use crate::store::v4_internal::merge_v4_internal_dataset_entries;

const SCHEMA_PG_FILENAME: &str = "schema.pg";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StorageMigrationResult {
    pub db_path: String,
    pub backup_path: String,
    pub graph_version: u64,
    pub node_tables: usize,
    pub edge_tables: usize,
    pub media_uris_rewritten: usize,
}

pub async fn migrate_storage_to_lance_v4(db_path: &Path) -> Result<StorageMigrationResult> {
    match detect_storage_generation(db_path)? {
        Some(StorageGeneration::V4Namespace) => {
            return Err(NanoError::Storage(format!(
                "database {} already uses lance-v4 storage",
                db_path.display()
            )));
        }
        Some(StorageGeneration::NamespaceLineage) => {
            return Err(NanoError::Storage(format!(
                "database {} already uses NamespaceLineage storage",
                db_path.display()
            )));
        }
        None => {}
    }

    let backup_path = backup_db_path(db_path)?;
    if backup_path.exists() {
        return Err(NanoError::Storage(format!(
            "backup path already exists: {}",
            backup_path.display()
        )));
    }
    let schema_source = std::fs::read_to_string(db_path.join(SCHEMA_PG_FILENAME))?;
    std::fs::rename(db_path, &backup_path)?;

    let migration_result = async {
        let legacy = DatabaseMetadata::open_v3_legacy(&backup_path)?;
        let legacy_manifest = legacy.manifest().clone();
        let node_table_count = legacy.schema_ir().node_types().count();
        let edge_table_count = legacy.schema_ir().edge_types().count();

        Database::init_with_generation(db_path, &schema_source, StorageGeneration::V4Namespace)
            .await?;

        let mut dataset_entries = Vec::new();
        let mut media_uris_rewritten = 0usize;
        let namespace = open_directory_namespace(db_path).await?;

        for node_def in legacy.schema_ir().node_types() {
            let Some(batch) = load_full_batch(legacy.node_dataset_locator(&node_def.name)).await?
            else {
                continue;
            };
            let batch = rewrite_node_media_uris(
                db_path,
                &backup_path,
                db_path,
                node_def,
                &batch,
                &mut media_uris_rewritten,
            )
            .await?;
            let table_id = format!("nodes/{}", SchemaIR::dir_name(node_def.type_id));
            write_namespace_batch(
                namespace.clone(),
                &table_id,
                batch.clone(),
                WriteMode::Overwrite,
                None,
            )
            .await?;
            let manifest_path =
                resolve_manifest_dataset_path(db_path, namespace.clone(), &table_id).await?;
            let dataset_physical_path = db_path.join(&manifest_path);
            rebuild_node_scalar_indexes(&dataset_physical_path, node_def).await?;
            rebuild_node_text_indexes(&dataset_physical_path, node_def).await?;
            rebuild_node_vector_indexes(&dataset_physical_path, node_def).await?;
            let version = latest_lance_dataset_version(&dataset_physical_path).await?;
            dataset_entries.push(DatasetEntry::new(
                node_def.type_id,
                node_def.name.clone(),
                "node",
                table_id,
                manifest_path,
                version,
                batch.num_rows() as u64,
            ));
        }

        for edge_def in legacy.schema_ir().edge_types() {
            let Some(batch) = load_full_batch(legacy.edge_dataset_locator(&edge_def.name)).await?
            else {
                continue;
            };
            let table_id = format!("edges/{}", SchemaIR::dir_name(edge_def.type_id));
            let version = write_namespace_batch(
                namespace.clone(),
                &table_id,
                batch.clone(),
                WriteMode::Overwrite,
                None,
            )
            .await?;
            let manifest_path =
                resolve_manifest_dataset_path(db_path, namespace.clone(), &table_id).await?;
            dataset_entries.push(DatasetEntry::new(
                edge_def.type_id,
                edge_def.name.clone(),
                "edge",
                table_id,
                manifest_path,
                version.version,
                batch.num_rows() as u64,
            ));
        }

        let mut manifest = read_committed_graph_snapshot(db_path)?;
        manifest.db_version = legacy_manifest.db_version;
        manifest.last_tx_id = format!("storage-migrate-v4-{}", legacy_manifest.db_version);
        manifest.committed_at = now_unix_seconds_string();
        manifest.next_node_id = legacy_manifest.next_node_id;
        manifest.next_edge_id = legacy_manifest.next_edge_id;
        manifest.next_type_id = legacy_manifest.next_type_id;
        manifest.next_prop_id = legacy_manifest.next_prop_id;
        manifest.schema_identity_version = legacy_manifest.schema_identity_version;
        manifest.datasets = dataset_entries;
        merge_v4_internal_dataset_entries(db_path, &mut manifest.datasets).await?;

        let graph_commit = GraphCommitRecord {
            tx_id: manifest.last_tx_id.clone().into(),
            graph_version: manifest.db_version.into(),
            table_versions: manifest
                .datasets
                .iter()
                .map(|entry| {
                    GraphTableVersion::new(
                        entry.effective_table_id().to_string(),
                        entry.dataset_version,
                    )
                })
                .collect(),
            committed_at: manifest.committed_at.clone(),
            op_summary: "storage:migrate:v3-to-lance-v4".to_string(),
            schema_identity_version: manifest.schema_identity_version,
            touched_tables: Vec::new(),
            tx_props: std::collections::BTreeMap::new(),
        };
        commit_graph_records_and_manifest(db_path, &graph_commit, &[], &manifest)?;

        Ok(StorageMigrationResult {
            db_path: db_path.display().to_string(),
            backup_path: backup_path.display().to_string(),
            graph_version: legacy_manifest.db_version,
            node_tables: node_table_count,
            edge_tables: edge_table_count,
            media_uris_rewritten,
        })
    }
    .await;

    if migration_result.is_err() {
        let _ = std::fs::remove_dir_all(db_path);
        let _ = std::fs::rename(&backup_path, db_path);
    }

    migration_result
}

pub async fn migrate_storage_to_lineage_native(db_path: &Path) -> Result<StorageMigrationResult> {
    match detect_storage_generation(db_path)? {
        Some(StorageGeneration::NamespaceLineage) => {
            return Err(NanoError::Storage(format!(
                "database {} already uses NamespaceLineage storage",
                db_path.display()
            )));
        }
        Some(StorageGeneration::V4Namespace) | None => {}
    }

    let backup_path = backup_db_path(db_path)?;
    if backup_path.exists() {
        return Err(NanoError::Storage(format!(
            "backup path already exists: {}",
            backup_path.display()
        )));
    }
    let schema_source = std::fs::read_to_string(db_path.join(SCHEMA_PG_FILENAME))?;
    std::fs::rename(db_path, &backup_path)?;

    let migration_result = async {
        let source = match detect_storage_generation(&backup_path)? {
            Some(StorageGeneration::V4Namespace) => DatabaseMetadata::open(&backup_path)?,
            Some(StorageGeneration::NamespaceLineage) => {
                return Err(NanoError::Storage(format!(
                    "unexpected lineage-native backup during migration: {}",
                    backup_path.display()
                )));
            }
            None => DatabaseMetadata::open_v3_legacy(&backup_path)?,
        };
        let source_manifest = source.manifest().clone();
        let node_table_count = source.schema_ir().node_types().count();
        let edge_table_count = source.schema_ir().edge_types().count();

        Database::init_with_generation(db_path, &schema_source, StorageGeneration::NamespaceLineage)
            .await?;

        let mut dataset_entries = Vec::new();
        let mut media_uris_rewritten = 0usize;
        let namespace = open_directory_namespace(db_path).await?;

        for node_def in source.schema_ir().node_types() {
            let Some(batch) = load_full_batch(source.node_dataset_locator(&node_def.name)).await?
            else {
                continue;
            };
            let batch = rewrite_node_media_uris(
                db_path,
                &backup_path,
                db_path,
                node_def,
                &batch,
                &mut media_uris_rewritten,
            )
            .await?;
            let table_id = format!("nodes/{}", SchemaIR::dir_name(node_def.type_id));
            write_namespace_batch(
                namespace.clone(),
                &table_id,
                batch.clone(),
                WriteMode::Overwrite,
                None,
            )
            .await?;
            let manifest_path =
                resolve_manifest_dataset_path(db_path, namespace.clone(), &table_id).await?;
            let dataset_physical_path = db_path.join(&manifest_path);
            rebuild_node_scalar_indexes(&dataset_physical_path, node_def).await?;
            rebuild_node_text_indexes(&dataset_physical_path, node_def).await?;
            rebuild_node_vector_indexes(&dataset_physical_path, node_def).await?;
            let version = latest_lance_dataset_version(&dataset_physical_path).await?;
            dataset_entries.push(DatasetEntry::new(
                node_def.type_id,
                node_def.name.clone(),
                "node",
                table_id,
                manifest_path,
                version,
                batch.num_rows() as u64,
            ));
        }

        for edge_def in source.schema_ir().edge_types() {
            let Some(batch) = load_full_batch(source.edge_dataset_locator(&edge_def.name)).await?
            else {
                continue;
            };
            let table_id = format!("edges/{}", SchemaIR::dir_name(edge_def.type_id));
            let version = write_namespace_batch(
                namespace.clone(),
                &table_id,
                batch.clone(),
                WriteMode::Overwrite,
                None,
            )
            .await?;
            let manifest_path =
                resolve_manifest_dataset_path(db_path, namespace.clone(), &table_id).await?;
            dataset_entries.push(DatasetEntry::new(
                edge_def.type_id,
                edge_def.name.clone(),
                "edge",
                table_id,
                manifest_path,
                version.version,
                batch.num_rows() as u64,
            ));
        }

        let mut manifest = read_committed_graph_snapshot(db_path)?;
        manifest.db_version = 1;
        manifest.last_tx_id = "storage-migrate-lineage-native-1".to_string();
        manifest.committed_at = now_unix_seconds_string();
        manifest.next_node_id = source_manifest.next_node_id;
        manifest.next_edge_id = source_manifest.next_edge_id;
        manifest.next_type_id = source_manifest.next_type_id;
        manifest.next_prop_id = source_manifest.next_prop_id;
        manifest.schema_identity_version = source_manifest.schema_identity_version.max(1);
        manifest.datasets = dataset_entries;
        merge_namespace_lineage_internal_dataset_entries(db_path, &mut manifest.datasets).await?;

        let graph_commit = GraphCommitRecord {
            tx_id: manifest.last_tx_id.clone().into(),
            graph_version: manifest.db_version.into(),
            table_versions: manifest
                .datasets
                .iter()
                .map(|entry| {
                    GraphTableVersion::new(
                        entry.effective_table_id().to_string(),
                        entry.dataset_version,
                    )
                })
                .collect(),
            committed_at: manifest.committed_at.clone(),
            op_summary: "storage:migrate:lineage-native".to_string(),
            schema_identity_version: manifest.schema_identity_version,
            touched_tables: manifest
                .datasets
                .iter()
                .filter(|entry| matches!(entry.kind.as_str(), "node" | "edge"))
                .map(|entry| {
                    GraphTouchedTableWindow::new(
                        entry.effective_table_id().to_string(),
                        entry.kind.clone(),
                        entry.type_name.clone(),
                        0,
                        entry.dataset_version,
                    )
                })
                .collect(),
            tx_props: BTreeMap::from([
                ("graph_version".to_string(), manifest.db_version.to_string()),
                ("tx_id".to_string(), manifest.last_tx_id.clone()),
                (
                    "op_summary".to_string(),
                    "storage:migrate:lineage-native".to_string(),
                ),
            ]),
        };
        commit_graph_records_and_manifest_namespace_lineage(
            db_path,
            &graph_commit,
            &[],
            &manifest,
        )?;

        Ok(StorageMigrationResult {
            db_path: db_path.display().to_string(),
            backup_path: backup_path.display().to_string(),
            graph_version: manifest.db_version,
            node_tables: node_table_count,
            edge_tables: edge_table_count,
            media_uris_rewritten,
        })
    }
    .await;

    if migration_result.is_err() {
        let _ = std::fs::remove_dir_all(db_path);
        let _ = std::fs::rename(&backup_path, db_path);
    }

    migration_result
}

async fn load_full_batch(
    locator: Option<crate::store::metadata::DatasetLocator>,
) -> Result<Option<RecordBatch>> {
    let Some(locator) = locator else {
        return Ok(None);
    };
    // Migration wants the exact snapshot-pinned physical table version. For namespace-managed
    // sources that can be more reliable than reopening through the namespace version surface,
    // especially after index rebuilds.
    let direct_locator = crate::store::metadata::DatasetLocator {
        namespace_managed: false,
        ..locator.clone()
    };
    let batches = if locator.namespace_managed && locator.dataset_path.exists() {
        match read_lance_batches_for_locator(&direct_locator).await {
            Ok(batches) => batches,
            Err(_) => read_lance_batches_for_locator(&locator).await?,
        }
    } else {
        read_lance_batches_for_locator(&locator).await?
    };
    if batches.is_empty() {
        return Ok(None);
    }
    if batches.len() == 1 {
        return Ok(Some(batches[0].clone()));
    }
    let schema = batches[0].schema();
    let batch = arrow_select::concat::concat_batches(&schema, &batches)
        .map_err(|err| NanoError::Storage(format!("concat migration batch error: {}", err)))?;
    Ok(Some(batch))
}

async fn rewrite_node_media_uris(
    target_db_path: &Path,
    source_db_path: &Path,
    source_uri_root: &Path,
    node_def: &NodeTypeDef,
    batch: &RecordBatch,
    rewritten: &mut usize,
) -> Result<RecordBatch> {
    let media_props: Vec<(&str, &str)> = node_def
        .properties
        .iter()
        .filter_map(|prop| {
            prop.media_mime_prop
                .as_deref()
                .map(|mime_prop| (prop.name.as_str(), mime_prop))
        })
        .collect();
    if media_props.is_empty() {
        return Ok(batch.clone());
    }

    let mut rows = record_batch_to_json_rows(batch);
    for row in &mut rows {
        for (uri_prop, mime_prop) in &media_props {
            let Some(uri) = row.get(*uri_prop).and_then(|value| value.as_str()) else {
                continue;
            };
            let Some(path) = file_uri_to_path(uri) else {
                continue;
            };
            let path = remap_managed_media_path(&path, source_uri_root, source_db_path);
            let mime_type = row
                .get(*mime_prop)
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    NanoError::Storage(format!(
                        "row for {} is missing media mime property {}",
                        node_def.name, mime_prop
                    ))
                })?;
            let bytes = std::fs::read(&path).map_err(|err| {
                NanoError::Storage(format!(
                    "read managed media during migration {}: {}",
                    path.display(),
                    err
                ))
            })?;
            let migrated_uri =
                store_managed_blob(target_db_path, &bytes, mime_type, Some(uri)).await?;
            if migrated_uri != uri {
                row.insert(
                    (*uri_prop).to_string(),
                    serde_json::Value::String(migrated_uri),
                );
                *rewritten += 1;
            }
        }
    }

    json_rows_to_record_batch(batch.schema().as_ref(), &rows)
}

fn remap_managed_media_path(path: &Path, source_uri_root: &Path, source_db_path: &Path) -> PathBuf {
    if path.exists() {
        return path.to_path_buf();
    }
    path.strip_prefix(source_uri_root)
        .map(|relative| source_db_path.join(relative))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

async fn resolve_manifest_dataset_path(
    db_path: &Path,
    namespace: std::sync::Arc<dyn lance_namespace::LanceNamespace>,
    table_id: &str,
) -> Result<String> {
    let location = resolve_table_location(namespace, table_id).await?;
    namespace_location_to_manifest_dataset_path(db_path, &location, table_id)
}

fn backup_db_path(db_path: &Path) -> Result<PathBuf> {
    let parent = db_path.parent().ok_or_else(|| {
        NanoError::Storage(format!(
            "database path has no parent directory: {}",
            db_path.display()
        ))
    })?;
    let name = db_path.file_name().ok_or_else(|| {
        NanoError::Storage(format!(
            "database path has no final path component: {}",
            db_path.display()
        ))
    })?;
    Ok(parent.join(format!("{}.v3-backup", name.to_string_lossy())))
}

fn now_unix_seconds_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use arrow_array::{RecordBatch, StringArray, UInt64Array};
    use arrow_schema::{DataType, Field, Schema};
    use tempfile::TempDir;

    use super::*;
    use crate::catalog::schema_ir::{build_catalog_from_ir, build_schema_ir};
    use crate::schema::parser::parse_schema;
    use crate::store::export::build_export_rows_at_path;
    use crate::store::lance_io::write_lance_batch;
    use crate::store::manifest::{GraphManifest, hash_string};
    use crate::store::metadata::SCHEMA_IR_FILENAME;
    use crate::store::txlog::{
        CdcLogEntry, collect_visible_lineage_shadow_cdc_entries, commit_manifest_and_logs,
        read_tx_catalog_entries, read_visible_cdc_entries,
    };

    fn legacy_schema_src() -> &'static str {
        r#"node Person {
    slug: String @key
    name: String
}
node Photo {
    slug: String @key
    uri: String @media_uri(mime)
    mime: String
}
edge Knows: Person -> Person
"#
    }

    fn person_batch() -> RecordBatch {
        let schema = std::sync::Arc::new(Schema::new(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("slug", DataType::Utf8, false),
            Field::new("name", DataType::Utf8, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(UInt64Array::from(vec![1u64, 2u64])),
                std::sync::Arc::new(StringArray::from(vec!["alice", "bob"])),
                std::sync::Arc::new(StringArray::from(vec!["Alice", "Bob"])),
            ],
        )
        .unwrap()
    }

    fn photo_batch(uri: String) -> RecordBatch {
        let schema = std::sync::Arc::new(Schema::new(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("slug", DataType::Utf8, false),
            Field::new("uri", DataType::Utf8, false),
            Field::new("mime", DataType::Utf8, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(UInt64Array::from(vec![3u64])),
                std::sync::Arc::new(StringArray::from(vec!["hero"])),
                std::sync::Arc::new(StringArray::from(vec![uri])),
                std::sync::Arc::new(StringArray::from(vec!["image/png"])),
            ],
        )
        .unwrap()
    }

    fn knows_batch() -> RecordBatch {
        let schema = std::sync::Arc::new(Schema::new(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("src", DataType::UInt64, false),
            Field::new("dst", DataType::UInt64, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                std::sync::Arc::new(UInt64Array::from(vec![10u64])),
                std::sync::Arc::new(UInt64Array::from(vec![1u64])),
                std::sync::Arc::new(UInt64Array::from(vec![2u64])),
            ],
        )
        .unwrap()
    }

    async fn build_legacy_v3_db(db_path: &Path) {
        std::fs::create_dir_all(db_path.join("nodes")).unwrap();
        std::fs::create_dir_all(db_path.join("edges")).unwrap();

        let schema_source = legacy_schema_src();
        let schema = parse_schema(schema_source).unwrap();
        let schema_ir = build_schema_ir(&schema).unwrap();
        let _catalog = build_catalog_from_ir(&schema_ir).unwrap();
        std::fs::write(db_path.join(SCHEMA_PG_FILENAME), schema_source).unwrap();
        let ir_json = serde_json::to_string_pretty(&schema_ir).unwrap();
        std::fs::write(db_path.join(SCHEMA_IR_FILENAME), &ir_json).unwrap();

        let media_dir = db_path.join("legacy-media");
        std::fs::create_dir_all(&media_dir).unwrap();
        let image_path = media_dir.join("hero.png");
        std::fs::write(&image_path, b"\x89PNG\r\n\x1a\nlegacy").unwrap();
        let image_uri = Url::from_file_path(&image_path).unwrap().to_string();

        let person_type = schema_ir
            .node_types()
            .find(|node| node.name == "Person")
            .unwrap();
        let photo_type = schema_ir
            .node_types()
            .find(|node| node.name == "Photo")
            .unwrap();
        let knows_type = schema_ir
            .edge_types()
            .find(|edge| edge.name == "Knows")
            .unwrap();

        let person_table = format!("nodes/{}", SchemaIR::dir_name(person_type.type_id));
        let photo_table = format!("nodes/{}", SchemaIR::dir_name(photo_type.type_id));
        let knows_table = format!("edges/{}", SchemaIR::dir_name(knows_type.type_id));

        let person_version = write_lance_batch(&db_path.join(&person_table), person_batch())
            .await
            .unwrap();
        let photo_version = write_lance_batch(&db_path.join(&photo_table), photo_batch(image_uri))
            .await
            .unwrap();
        let knows_version = write_lance_batch(&db_path.join(&knows_table), knows_batch())
            .await
            .unwrap();

        let mut manifest = GraphManifest::new(hash_string(&ir_json));
        manifest.db_version = 1;
        manifest.last_tx_id = "legacy-1".to_string();
        manifest.committed_at = "1".to_string();
        manifest.next_node_id = 4;
        manifest.next_edge_id = 11;
        manifest.next_type_id = schema_ir.types.len() as u32 + 1;
        manifest.next_prop_id = 10;
        manifest.datasets = vec![
            DatasetEntry::new(
                person_type.type_id,
                person_type.name.clone(),
                "node",
                &person_table,
                &person_table,
                person_version,
                2,
            ),
            DatasetEntry::new(
                photo_type.type_id,
                photo_type.name.clone(),
                "node",
                &photo_table,
                &photo_table,
                photo_version,
                1,
            ),
            DatasetEntry::new(
                knows_type.type_id,
                knows_type.name.clone(),
                "edge",
                &knows_table,
                &knows_table,
                knows_version,
                1,
            ),
        ];
        let cdc = vec![CdcLogEntry {
            tx_id: "legacy-1".to_string(),
            db_version: 1,
            seq_in_tx: 0,
            op: "insert".to_string(),
            entity_kind: "node".to_string(),
            type_name: "Person".to_string(),
            entity_key: "alice".to_string(),
            payload: serde_json::json!({"slug":"alice","name":"Alice"}),
            committed_at: "1".to_string(),
        }];
        commit_manifest_and_logs(db_path, &manifest, &cdc, "legacy:init").unwrap();
    }

    #[tokio::test]
    async fn migrate_storage_to_v4_copies_visible_state_and_rewrites_media() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("legacy.nano");
        build_legacy_v3_db(&db_path).await;

        let result = migrate_storage_to_lance_v4(&db_path).await.unwrap();
        assert_eq!(result.graph_version, 1);
        assert!(Path::new(&result.backup_path).exists());
        assert!(Path::new(&result.backup_path).join("_wal.jsonl").exists());

        let db = Database::open(&db_path).await.unwrap();
        let exported = build_export_rows_at_path(&db_path, false, true)
            .await
            .unwrap();
        assert_eq!(exported.len(), 4);
        assert!(exported.iter().any(|row| row["type"] == "Person"));
        assert!(exported.iter().any(|row| row["edge"] == "Knows"));
        let photo = exported.iter().find(|row| row["type"] == "Photo").unwrap();
        let migrated_uri = photo["data"]["uri"].as_str().unwrap();
        assert!(migrated_uri.starts_with("lanceblob://sha256/"));

        let changes = read_visible_cdc_entries(&db_path, 0, None).unwrap();
        assert!(changes.is_empty());
        let _ = db;
    }

    #[tokio::test]
    async fn migrated_v4_database_accepts_new_writes_and_reports_v4_cdc() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("legacy.nano");
        build_legacy_v3_db(&db_path).await;

        migrate_storage_to_lance_v4(&db_path).await.unwrap();

        let db = Database::open(&db_path).await.unwrap();
        db.load(
            r#"{"type":"Person","data":{"slug":"charlie","name":"Charlie"}}
{"edge":"Knows","from":"alice","to":"charlie"}"#,
        )
        .await
        .unwrap();

        let tx_rows = read_tx_catalog_entries(&db_path).unwrap();
        assert_eq!(tx_rows.len(), 2);
        assert_eq!(tx_rows[0].db_version, 1);
        assert_eq!(tx_rows[1].db_version, 2);

        let cdc_rows = read_visible_cdc_entries(&db_path, 0, None).unwrap();
        assert!(!cdc_rows.is_empty());
        assert!(cdc_rows.iter().all(|row| row.db_version == 2));
        assert!(
            cdc_rows
                .iter()
                .any(|row| row.type_name == "Person" && row.op == "insert")
        );
        assert!(
            cdc_rows
                .iter()
                .any(|row| row.type_name == "Knows" && row.op == "insert")
        );

        let exported = build_export_rows_at_path(&db_path, false, true)
            .await
            .unwrap();
        assert!(
            exported
                .iter()
                .any(|row| row["type"] == "Person" && row["data"]["slug"] == "charlie")
        );
        assert!(
            exported
                .iter()
                .any(|row| row["edge"] == "Knows" && row["to"] == "charlie")
        );
    }

    #[tokio::test]
    async fn migrated_v4_database_survives_compact_cleanup_and_doctor() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("legacy.nano");
        build_legacy_v3_db(&db_path).await;

        migrate_storage_to_lance_v4(&db_path).await.unwrap();

        let db = Database::open(&db_path).await.unwrap();
        let committed_before_charlie = read_committed_graph_snapshot(&db_path).unwrap();
        db.load(r#"{"type":"Person","data":{"slug":"charlie","name":"Charlie"}}"#)
            .await
            .unwrap();
        let committed_after_charlie = read_committed_graph_snapshot(&db_path).unwrap();
        let shadow_after_charlie = collect_visible_lineage_shadow_cdc_entries(
            &db_path,
            committed_before_charlie.db_version,
            Some(committed_after_charlie.db_version),
        )
        .unwrap();
        assert_eq!(
            shadow_after_charlie.shadow_entries,
            shadow_after_charlie.authoritative_entries_compared
        );

        let compact = db
            .compact(crate::store::database::CompactOptions::default())
            .await
            .unwrap();
        assert!(compact.datasets_considered >= 1);

        let cleanup = db
            .cleanup(crate::store::database::CleanupOptions {
                retain_tx_versions: 1,
                retain_dataset_versions: 1,
            })
            .await
            .unwrap();
        assert!(cleanup.tx_rows_kept >= 1);
        let shadow_after_cleanup = collect_visible_lineage_shadow_cdc_entries(
            &db_path,
            committed_before_charlie.db_version,
            Some(committed_after_charlie.db_version),
        )
        .unwrap();
        assert_eq!(
            shadow_after_cleanup.shadow_entries,
            shadow_after_cleanup.authoritative_entries_compared
        );

        let report = db.doctor().await.unwrap();
        assert!(
            report.healthy,
            "expected healthy report: {:?}",
            report.issues
        );
        assert!(report.issues.is_empty());
        assert!(report.tx_rows >= 1);
    }

    #[tokio::test]
    async fn open_rejects_legacy_v3_storage_with_migration_hint() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("legacy.nano");
        build_legacy_v3_db(&db_path).await;

        let err = match Database::open(&db_path).await {
            Ok(_) => panic!("legacy v3 open unexpectedly succeeded"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("nanograph storage migrate --db"));
    }
}
