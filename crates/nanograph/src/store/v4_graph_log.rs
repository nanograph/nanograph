use std::path::Path;
use std::sync::Arc;

use arrow_array::Array;
use arrow_array::{RecordBatch, StringArray, UInt32Array, UInt64Array};
use arrow_schema::{DataType, Field, Schema};
use lance::Dataset;
use lance::dataset::WriteMode;

use crate::error::{NanoError, Result};
use crate::store::graph_types::{GraphChangeRecord, GraphCommitRecord, GraphTableVersion};
use crate::store::lance_io::{
    append_lance_batch_at_version, cleanup_unpublished_manifest_versions,
};
use crate::store::manifest::{DatasetEntry, GraphManifest};
use crate::store::metadata::DatasetLocator;
use crate::store::namespace::{
    GRAPH_CHANGES_TABLE_ID, GRAPH_TX_TABLE_ID, StagedNamespaceTable, namespace_latest_version,
    namespace_published_version_for_table, open_directory_namespace,
    resolve_or_declare_table_location, resolve_table_location, write_namespace_batch,
};

fn manifest_dataset_path(db_path: &Path, location: &str, fallback: &str) -> String {
    let normalized = location.strip_prefix("file://").unwrap_or(location);
    std::path::PathBuf::from(normalized)
        .strip_prefix(db_path)
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| fallback.to_string())
}

pub(crate) async fn stage_graph_commit_record(
    db_path: &Path,
    manifest: &GraphManifest,
    record: &GraphCommitRecord,
) -> Result<StagedNamespaceTable> {
    let namespace = open_directory_namespace(db_path).await?;
    let table_id = GRAPH_TX_TABLE_ID;
    let batch = graph_commit_records_to_batch(std::slice::from_ref(record))?.ok_or_else(|| {
        NanoError::Storage("graph tx append expected at least one record".to_string())
    })?;
    let pinned_version = manifest
        .datasets
        .iter()
        .find(|entry| entry.table_id.as_deref() == Some(table_id))
        .map(|entry| GraphTableVersion::new(table_id, entry.dataset_version))
        .unwrap_or_else(|| GraphTableVersion::new(table_id, 0));
    let pinned_version = if pinned_version.version == 0 {
        ensure_graph_tx_table(db_path)
            .await
            .map(|entry| GraphTableVersion::new(table_id, entry.dataset_version))?
    } else {
        pinned_version
    };
    let location = resolve_or_declare_table_location(namespace.clone(), table_id).await?;
    cleanup_unpublished_manifest_versions(&location, Some(pinned_version.version)).await?;
    let location_path =
        std::path::PathBuf::from(location.strip_prefix("file://").unwrap_or(&location));
    let version = append_lance_batch_at_version(&location_path, &pinned_version, batch).await?;
    let row_count = manifest
        .datasets
        .iter()
        .find(|entry| entry.table_id.as_deref() == Some(table_id))
        .map(|entry| entry.row_count)
        .unwrap_or(0)
        .saturating_add(1);
    let location = resolve_table_location(namespace, table_id).await?;
    let entry = DatasetEntry::internal(
        table_id,
        manifest_dataset_path(db_path, &location, table_id),
        version.version,
        row_count,
    );
    let published_version =
        namespace_published_version_for_table(db_path, table_id, version.version)
            .await?
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "staged graph tx {} version {} is not publishable",
                    table_id, version.version
                ))
            })?;
    Ok(StagedNamespaceTable {
        entry,
        published_version,
    })
}

pub(crate) async fn ensure_graph_tx_table(db_path: &Path) -> Result<DatasetEntry> {
    if let Some(entry) = load_existing_internal_entry(db_path, GRAPH_TX_TABLE_ID).await? {
        return Ok(entry);
    }

    let namespace = open_directory_namespace(db_path).await?;
    let batch = empty_graph_commit_batch();
    let version = write_namespace_batch(
        namespace.clone(),
        GRAPH_TX_TABLE_ID,
        batch,
        WriteMode::Overwrite,
        None,
    )
    .await?;
    let location = resolve_table_location(namespace, GRAPH_TX_TABLE_ID).await?;
    Ok(DatasetEntry::internal(
        GRAPH_TX_TABLE_ID,
        manifest_dataset_path(db_path, &location, GRAPH_TX_TABLE_ID),
        version.version,
        0,
    ))
}

pub(crate) async fn ensure_graph_changes_table(db_path: &Path) -> Result<DatasetEntry> {
    if let Some(entry) = load_existing_internal_entry(db_path, GRAPH_CHANGES_TABLE_ID).await? {
        return Ok(entry);
    }

    let namespace = open_directory_namespace(db_path).await?;
    let batch = empty_graph_change_batch();
    let version = write_namespace_batch(
        namespace.clone(),
        GRAPH_CHANGES_TABLE_ID,
        batch,
        WriteMode::Overwrite,
        None,
    )
    .await?;
    let location = resolve_table_location(namespace, GRAPH_CHANGES_TABLE_ID).await?;
    Ok(DatasetEntry::internal(
        GRAPH_CHANGES_TABLE_ID,
        manifest_dataset_path(db_path, &location, GRAPH_CHANGES_TABLE_ID),
        version.version,
        0,
    ))
}

pub(crate) async fn rewrite_graph_commit_records(
    db_path: &Path,
    records: &[GraphCommitRecord],
) -> Result<StagedNamespaceTable> {
    let namespace = open_directory_namespace(db_path).await?;
    let table_id = GRAPH_TX_TABLE_ID;
    let batch = graph_commit_records_to_batch(records)?.unwrap_or_else(empty_graph_commit_batch);
    let version = write_namespace_batch(
        namespace.clone(),
        table_id,
        batch,
        WriteMode::Overwrite,
        None,
    )
    .await?;
    let location = resolve_table_location(namespace, table_id).await?;
    let entry = DatasetEntry::internal(
        table_id,
        manifest_dataset_path(db_path, &location, table_id),
        version.version,
        records.len() as u64,
    );
    let published_version =
        namespace_published_version_for_table(db_path, table_id, version.version)
            .await?
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "rewritten graph tx {} version {} is not publishable",
                    table_id, version.version
                ))
            })?;
    Ok(StagedNamespaceTable {
        entry,
        published_version,
    })
}

pub(crate) async fn rewrite_graph_change_records(
    db_path: &Path,
    records: &[GraphChangeRecord],
) -> Result<StagedNamespaceTable> {
    let namespace = open_directory_namespace(db_path).await?;
    let table_id = GRAPH_CHANGES_TABLE_ID;
    let batch = graph_change_records_to_batch(records)?.unwrap_or_else(empty_graph_change_batch);
    let version = write_namespace_batch(
        namespace.clone(),
        table_id,
        batch,
        WriteMode::Overwrite,
        None,
    )
    .await?;
    let location = resolve_table_location(namespace, table_id).await?;
    let entry = DatasetEntry::internal(
        table_id,
        manifest_dataset_path(db_path, &location, table_id),
        version.version,
        records.len() as u64,
    );
    let published_version =
        namespace_published_version_for_table(db_path, table_id, version.version)
            .await?
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "rewritten graph changes {} version {} is not publishable",
                    table_id, version.version
                ))
            })?;
    Ok(StagedNamespaceTable {
        entry,
        published_version,
    })
}

pub(crate) async fn stage_graph_change_records(
    db_path: &Path,
    manifest: &GraphManifest,
    graph_version: u64,
    tx_id: &str,
    op_summary: &str,
    records: &[GraphChangeRecord],
) -> Result<StagedNamespaceTable> {
    let namespace = open_directory_namespace(db_path).await?;
    let table_id = GRAPH_CHANGES_TABLE_ID;
    let batch = graph_change_records_to_batch(records)?
        .ok_or_else(|| NanoError::Storage("graph change append expected a batch".to_string()))?;
    let _ = (graph_version, tx_id, op_summary);
    let pinned_version = manifest
        .datasets
        .iter()
        .find(|entry| entry.table_id.as_deref() == Some(table_id))
        .map(|entry| GraphTableVersion::new(table_id, entry.dataset_version))
        .unwrap_or_else(|| GraphTableVersion::new(table_id, 0));
    let pinned_version = if pinned_version.version == 0 {
        ensure_graph_changes_table(db_path)
            .await
            .map(|entry| GraphTableVersion::new(table_id, entry.dataset_version))?
    } else {
        pinned_version
    };
    let location = resolve_or_declare_table_location(namespace.clone(), table_id).await?;
    cleanup_unpublished_manifest_versions(&location, Some(pinned_version.version)).await?;
    let location_path =
        std::path::PathBuf::from(location.strip_prefix("file://").unwrap_or(&location));
    let version = append_lance_batch_at_version(&location_path, &pinned_version, batch).await?;
    let row_count = manifest
        .datasets
        .iter()
        .find(|entry| entry.table_id.as_deref() == Some(table_id))
        .map(|entry| entry.row_count)
        .unwrap_or(0)
        .saturating_add(records.len() as u64);
    let location = resolve_table_location(namespace, table_id).await?;
    let entry = DatasetEntry::internal(
        table_id,
        manifest_dataset_path(db_path, &location, table_id),
        version.version,
        row_count,
    );
    let published_version =
        namespace_published_version_for_table(db_path, table_id, version.version)
            .await?
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "staged graph changes {} version {} is not publishable",
                    table_id, version.version
                ))
            })?;
    Ok(StagedNamespaceTable {
        entry,
        published_version,
    })
}

#[allow(dead_code)]
pub(crate) async fn bootstrap_graph_log_tables(
    db_path: &Path,
    graph_commit: &GraphCommitRecord,
    graph_changes: &[GraphChangeRecord],
) -> Result<Vec<DatasetEntry>> {
    let manifest = GraphManifest::new("bootstrap".to_string());
    let tx_entry = stage_graph_commit_record(db_path, &manifest, graph_commit)
        .await?
        .entry;
    let changes_entry = stage_graph_change_records(
        db_path,
        &manifest,
        graph_commit.graph_version.value(),
        graph_commit.tx_id.as_str(),
        &graph_commit.op_summary,
        graph_changes,
    )
    .await?
    .entry;
    Ok(vec![tx_entry, changes_entry])
}

pub(crate) async fn read_graph_change_records(
    db_path: &Path,
    entry: &DatasetEntry,
) -> Result<Vec<GraphChangeRecord>> {
    let locator = DatasetLocator {
        db_path: db_path.to_path_buf(),
        table_id: entry.effective_table_id().to_string(),
        dataset_path: db_path.join(&entry.dataset_path),
        dataset_version: entry.dataset_version,
        row_count: entry.row_count,
        namespace_managed: true,
    };
    let batches = crate::store::lance_io::read_lance_batches_for_locator(&locator).await?;
    let mut out = Vec::new();
    for batch in &batches {
        out.extend(graph_change_records_from_batch(batch)?);
    }
    out.sort_by(|a, b| {
        a.graph_version
            .value()
            .cmp(&b.graph_version.value())
            .then(a.seq_in_tx.cmp(&b.seq_in_tx))
            .then(a.tx_id.as_str().cmp(b.tx_id.as_str()))
    });
    Ok(out)
}

pub(crate) async fn read_graph_commit_records(
    db_path: &Path,
    entry: &DatasetEntry,
) -> Result<Vec<GraphCommitRecord>> {
    let locator = DatasetLocator {
        db_path: db_path.to_path_buf(),
        table_id: entry.effective_table_id().to_string(),
        dataset_path: db_path.join(&entry.dataset_path),
        dataset_version: entry.dataset_version,
        row_count: entry.row_count,
        namespace_managed: true,
    };
    let batches = crate::store::lance_io::read_lance_batches_for_locator(&locator).await?;
    let mut out = Vec::new();
    for batch in &batches {
        out.extend(graph_commit_records_from_batch(batch)?);
    }
    out.sort_by_key(|record| record.graph_version.value());
    Ok(out)
}

#[allow(dead_code)]
pub(crate) async fn latest_graph_log_versions(db_path: &Path) -> Result<Vec<(String, u64)>> {
    let namespace = open_directory_namespace(db_path).await?;
    let tx = namespace_latest_version(namespace.clone(), GRAPH_TX_TABLE_ID).await?;
    let changes = namespace_latest_version(namespace, GRAPH_CHANGES_TABLE_ID).await?;
    Ok(vec![
        (GRAPH_TX_TABLE_ID.to_string(), tx.version),
        (GRAPH_CHANGES_TABLE_ID.to_string(), changes.version),
    ])
}

async fn load_existing_internal_entry(
    db_path: &Path,
    table_id: &str,
) -> Result<Option<DatasetEntry>> {
    let namespace = open_directory_namespace(db_path).await?;
    let location = match resolve_table_location(namespace.clone(), table_id).await {
        Ok(location) => location,
        Err(_) => return Ok(None),
    };
    let version = namespace_latest_version(namespace.clone(), table_id)
        .await?
        .version;
    let dataset = Dataset::open(&location)
        .await
        .map_err(|err| NanoError::Lance(format!("open {} error: {}", table_id, err)))?
        .checkout_version(version)
        .await
        .map_err(|err| {
            NanoError::Lance(format!(
                "checkout {} version {} error: {}",
                table_id, version, err
            ))
        })?;
    let row_count = dataset
        .count_rows(None)
        .await
        .map_err(|err| NanoError::Lance(format!("count {} rows error: {}", table_id, err)))?
        as u64;
    Ok(Some(DatasetEntry::internal(
        table_id,
        manifest_dataset_path(db_path, &location, table_id),
        version,
        row_count,
    )))
}

fn empty_graph_commit_batch() -> RecordBatch {
    RecordBatch::new_empty(graph_commit_schema())
}

fn empty_graph_change_batch() -> RecordBatch {
    RecordBatch::new_empty(graph_change_schema())
}

fn graph_commit_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("tx_id", DataType::Utf8, false),
        Field::new("db_version", DataType::UInt64, false),
        Field::new("table_versions_json", DataType::Utf8, false),
        Field::new("committed_at", DataType::Utf8, false),
        Field::new("op_summary", DataType::Utf8, false),
    ]))
}

fn graph_change_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("tx_id", DataType::Utf8, false),
        Field::new("db_version", DataType::UInt64, false),
        Field::new("seq_in_tx", DataType::UInt32, false),
        Field::new("op", DataType::Utf8, false),
        Field::new("entity_kind", DataType::Utf8, false),
        Field::new("type_name", DataType::Utf8, false),
        Field::new("entity_key", DataType::Utf8, false),
        Field::new("payload_json", DataType::Utf8, false),
        Field::new("rowid_if_known", DataType::UInt64, true),
        Field::new("committed_at", DataType::Utf8, false),
    ]))
}

fn graph_commit_records_to_batch(records: &[GraphCommitRecord]) -> Result<Option<RecordBatch>> {
    if records.is_empty() {
        return Ok(None);
    }

    let schema = graph_commit_schema();

    let tx_ids = StringArray::from(
        records
            .iter()
            .map(|record| record.tx_id.as_str().to_string())
            .collect::<Vec<_>>(),
    );
    let db_versions = UInt64Array::from(
        records
            .iter()
            .map(|record| record.graph_version.value())
            .collect::<Vec<_>>(),
    );
    let table_versions_json = StringArray::from(
        records
            .iter()
            .map(|record| serde_json::to_string(&record.table_versions))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| NanoError::Manifest(format!("serialize graph commit table map: {}", e)))?,
    );
    let committed_at = StringArray::from(
        records
            .iter()
            .map(|record| record.committed_at.clone())
            .collect::<Vec<_>>(),
    );
    let op_summary = StringArray::from(
        records
            .iter()
            .map(|record| record.op_summary.clone())
            .collect::<Vec<_>>(),
    );

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(tx_ids),
            Arc::new(db_versions),
            Arc::new(table_versions_json),
            Arc::new(committed_at),
            Arc::new(op_summary),
        ],
    )
    .map(Some)
    .map_err(|e| NanoError::Storage(format!("build graph tx batch: {}", e)))
}

fn graph_change_records_to_batch(records: &[GraphChangeRecord]) -> Result<Option<RecordBatch>> {
    if records.is_empty() {
        return Ok(None);
    }

    let schema = graph_change_schema();

    let tx_ids = StringArray::from(
        records
            .iter()
            .map(|record| record.tx_id.as_str().to_string())
            .collect::<Vec<_>>(),
    );
    let db_versions = UInt64Array::from(
        records
            .iter()
            .map(|record| record.graph_version.value())
            .collect::<Vec<_>>(),
    );
    let seq_in_tx = UInt32Array::from(
        records
            .iter()
            .map(|record| record.seq_in_tx)
            .collect::<Vec<_>>(),
    );
    let op = StringArray::from(
        records
            .iter()
            .map(|record| record.op.clone())
            .collect::<Vec<_>>(),
    );
    let entity_kind = StringArray::from(
        records
            .iter()
            .map(|record| record.entity_kind.clone())
            .collect::<Vec<_>>(),
    );
    let type_name = StringArray::from(
        records
            .iter()
            .map(|record| record.type_name.clone())
            .collect::<Vec<_>>(),
    );
    let entity_key = StringArray::from(
        records
            .iter()
            .map(|record| record.entity_key.clone())
            .collect::<Vec<_>>(),
    );
    let payload_json = StringArray::from(
        records
            .iter()
            .map(|record| serde_json::to_string(&record.payload))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| NanoError::Manifest(format!("serialize graph change payload: {}", e)))?,
    );
    let rowid_if_known = UInt64Array::from(
        records
            .iter()
            .map(|record| record.rowid_if_known)
            .collect::<Vec<_>>(),
    );
    let committed_at = StringArray::from(
        records
            .iter()
            .map(|record| record.committed_at.clone())
            .collect::<Vec<_>>(),
    );

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(tx_ids),
            Arc::new(db_versions),
            Arc::new(seq_in_tx),
            Arc::new(op),
            Arc::new(entity_kind),
            Arc::new(type_name),
            Arc::new(entity_key),
            Arc::new(payload_json),
            Arc::new(rowid_if_known),
            Arc::new(committed_at),
        ],
    )
    .map(Some)
    .map_err(|e| NanoError::Storage(format!("build graph change batch: {}", e)))
}

fn graph_commit_records_from_batch(batch: &RecordBatch) -> Result<Vec<GraphCommitRecord>> {
    let tx_ids = batch
        .column_by_name("tx_id")
        .ok_or_else(|| NanoError::Storage("graph tx batch missing tx_id column".to_string()))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| NanoError::Storage("graph tx tx_id column is not Utf8".to_string()))?;
    let db_versions = batch
        .column_by_name("db_version")
        .ok_or_else(|| NanoError::Storage("graph tx batch missing db_version column".to_string()))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| {
            NanoError::Storage("graph tx db_version column is not UInt64".to_string())
        })?;
    let table_versions_json = batch
        .column_by_name("table_versions_json")
        .ok_or_else(|| {
            NanoError::Storage("graph tx batch missing table_versions_json column".to_string())
        })?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| {
            NanoError::Storage("graph tx table_versions_json column is not Utf8".to_string())
        })?;
    let committed_at = batch
        .column_by_name("committed_at")
        .ok_or_else(|| {
            NanoError::Storage("graph tx batch missing committed_at column".to_string())
        })?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| {
            NanoError::Storage("graph tx committed_at column is not Utf8".to_string())
        })?;
    let op_summary = batch
        .column_by_name("op_summary")
        .ok_or_else(|| NanoError::Storage("graph tx batch missing op_summary column".to_string()))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| NanoError::Storage("graph tx op_summary column is not Utf8".to_string()))?;

    let mut out = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let table_versions = serde_json::from_str(table_versions_json.value(row)).map_err(|e| {
            NanoError::Manifest(format!("parse graph tx table_versions_json error: {}", e))
        })?;
        out.push(GraphCommitRecord {
            tx_id: tx_ids.value(row).to_string().into(),
            graph_version: db_versions.value(row).into(),
            table_versions,
            committed_at: committed_at.value(row).to_string(),
            op_summary: op_summary.value(row).to_string(),
            schema_identity_version: 0,
            touched_tables: Vec::new(),
            tx_props: std::collections::BTreeMap::new(),
        });
    }
    Ok(out)
}

fn graph_change_records_from_batch(batch: &RecordBatch) -> Result<Vec<GraphChangeRecord>> {
    let tx_ids = batch
        .column_by_name("tx_id")
        .ok_or_else(|| NanoError::Storage("graph change batch missing tx_id column".to_string()))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| NanoError::Storage("graph change tx_id column is not Utf8".to_string()))?;
    let db_versions = batch
        .column_by_name("db_version")
        .ok_or_else(|| {
            NanoError::Storage("graph change batch missing db_version column".to_string())
        })?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| {
            NanoError::Storage("graph change db_version column is not UInt64".to_string())
        })?;
    let seq_in_tx = batch
        .column_by_name("seq_in_tx")
        .ok_or_else(|| {
            NanoError::Storage("graph change batch missing seq_in_tx column".to_string())
        })?
        .as_any()
        .downcast_ref::<UInt32Array>()
        .ok_or_else(|| {
            NanoError::Storage("graph change seq_in_tx column is not UInt32".to_string())
        })?;
    let op = batch
        .column_by_name("op")
        .ok_or_else(|| NanoError::Storage("graph change batch missing op column".to_string()))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| NanoError::Storage("graph change op column is not Utf8".to_string()))?;
    let entity_kind = batch
        .column_by_name("entity_kind")
        .ok_or_else(|| {
            NanoError::Storage("graph change batch missing entity_kind column".to_string())
        })?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| {
            NanoError::Storage("graph change entity_kind column is not Utf8".to_string())
        })?;
    let type_name = batch
        .column_by_name("type_name")
        .ok_or_else(|| {
            NanoError::Storage("graph change batch missing type_name column".to_string())
        })?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| {
            NanoError::Storage("graph change type_name column is not Utf8".to_string())
        })?;
    let entity_key = batch
        .column_by_name("entity_key")
        .ok_or_else(|| {
            NanoError::Storage("graph change batch missing entity_key column".to_string())
        })?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| {
            NanoError::Storage("graph change entity_key column is not Utf8".to_string())
        })?;
    let payload_json = batch
        .column_by_name("payload_json")
        .ok_or_else(|| {
            NanoError::Storage("graph change batch missing payload_json column".to_string())
        })?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| {
            NanoError::Storage("graph change payload_json column is not Utf8".to_string())
        })?;
    let rowid_if_known = batch
        .column_by_name("rowid_if_known")
        .and_then(|column| column.as_any().downcast_ref::<UInt64Array>());
    let committed_at = batch
        .column_by_name("committed_at")
        .ok_or_else(|| {
            NanoError::Storage("graph change batch missing committed_at column".to_string())
        })?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| {
            NanoError::Storage("graph change committed_at column is not Utf8".to_string())
        })?;

    let mut out = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        out.push(GraphChangeRecord {
            tx_id: tx_ids.value(row).to_string().into(),
            graph_version: db_versions.value(row).into(),
            seq_in_tx: seq_in_tx.value(row),
            op: op.value(row).to_string(),
            entity_kind: entity_kind.value(row).to_string(),
            type_name: type_name.value(row).to_string(),
            entity_key: entity_key.value(row).to_string(),
            payload: serde_json::from_str(payload_json.value(row)).map_err(|err| {
                NanoError::Manifest(format!("parse graph change payload error: {}", err))
            })?,
            rowid_if_known: rowid_if_known
                .and_then(|column| (!column.is_null(row)).then(|| column.value(row))),
            committed_at: committed_at.value(row).to_string(),
        });
    }
    Ok(out)
}
