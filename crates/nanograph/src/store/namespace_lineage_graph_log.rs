use std::path::Path;
use std::sync::Arc;

use arrow_array::{Array, RecordBatch, StringArray, UInt32Array, UInt64Array};
use arrow_schema::{DataType, Field, Schema};
use lance::Dataset;
use lance::dataset::WriteMode;

use crate::error::{NanoError, Result};
use crate::store::graph_types::{
    GraphCommitRecord, GraphDeleteRecord, GraphTableVersion, GraphTouchedTableWindow,
};
use crate::store::lance_io::{
    append_lance_batch_at_version, cleanup_unpublished_manifest_versions,
    latest_lance_dataset_version,
};
use crate::store::manifest::{DatasetEntry, GraphManifest};
use crate::store::metadata::DatasetLocator;
use crate::store::namespace::{
    GRAPH_DELETES_TABLE_ID, GRAPH_TX_TABLE_ID, StagedNamespaceTable, namespace_published_version_for_table,
    namespace_location_to_dataset_uri, namespace_location_to_local_path,
    namespace_location_to_manifest_dataset_path, open_directory_namespace,
    resolve_or_declare_table_location, resolve_table_location, write_namespace_batch,
};

fn manifest_dataset_path(db_path: &Path, location: &str, fallback: &str) -> Result<String> {
    namespace_location_to_manifest_dataset_path(db_path, location, fallback)
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
    let location_path = namespace_location_to_local_path(db_path, &location)?;
    let version = latest_lance_dataset_version(&location_path).await?;
    let dataset_uri = namespace_location_to_dataset_uri(db_path, &location)?;
    let dataset = Dataset::open(&dataset_uri)
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
        manifest_dataset_path(db_path, &location, table_id)?,
        version,
        row_count,
    )))
}

pub(crate) async fn ensure_namespace_lineage_graph_tx_table(
    db_path: &Path,
) -> Result<DatasetEntry> {
    if let Some(entry) = load_existing_internal_entry(db_path, GRAPH_TX_TABLE_ID).await? {
        return Ok(entry);
    }

    let namespace = open_directory_namespace(db_path).await?;
    let version = write_namespace_batch(
        namespace.clone(),
        GRAPH_TX_TABLE_ID,
        empty_namespace_lineage_graph_commit_batch(),
        WriteMode::Overwrite,
        None,
    )
    .await?;
    let location = resolve_table_location(namespace, GRAPH_TX_TABLE_ID).await?;
    Ok(DatasetEntry::internal(
        GRAPH_TX_TABLE_ID,
        manifest_dataset_path(db_path, &location, GRAPH_TX_TABLE_ID)?,
        version.version,
        0,
    ))
}

pub(crate) async fn ensure_graph_deletes_table(db_path: &Path) -> Result<DatasetEntry> {
    if let Some(entry) = load_existing_internal_entry(db_path, GRAPH_DELETES_TABLE_ID).await? {
        return Ok(entry);
    }

    let namespace = open_directory_namespace(db_path).await?;
    let version = write_namespace_batch(
        namespace.clone(),
        GRAPH_DELETES_TABLE_ID,
        empty_graph_delete_batch(),
        WriteMode::Overwrite,
        None,
    )
    .await?;
    let location = resolve_table_location(namespace, GRAPH_DELETES_TABLE_ID).await?;
    Ok(DatasetEntry::internal(
        GRAPH_DELETES_TABLE_ID,
        manifest_dataset_path(db_path, &location, GRAPH_DELETES_TABLE_ID)?,
        version.version,
        0,
    ))
}

pub(crate) async fn stage_namespace_lineage_graph_commit_record(
    db_path: &Path,
    manifest: &GraphManifest,
    record: &GraphCommitRecord,
) -> Result<StagedNamespaceTable> {
    let namespace = open_directory_namespace(db_path).await?;
    let table_id = GRAPH_TX_TABLE_ID;
    let batch = namespace_lineage_graph_commit_records_to_batch(std::slice::from_ref(record))?
        .ok_or_else(|| {
        NanoError::Storage("graph tx append expected at least one record".to_string())
    })?;
    let pinned_version = manifest
        .datasets
        .iter()
        .find(|entry| entry.effective_table_id() == table_id)
        .map(|entry| GraphTableVersion::new(table_id, entry.dataset_version))
        .unwrap_or_else(|| GraphTableVersion::new(table_id, 0));
    let pinned_version = if pinned_version.version == 0 {
        ensure_namespace_lineage_graph_tx_table(db_path)
            .await
            .map(|entry| GraphTableVersion::new(table_id, entry.dataset_version))?
    } else {
        pinned_version
    };
    let location = resolve_or_declare_table_location(namespace.clone(), table_id).await?;
    cleanup_unpublished_manifest_versions(db_path, &location, Some(pinned_version.version)).await?;
    let location_path = namespace_location_to_local_path(db_path, &location)?;
    let version = append_lance_batch_at_version(&location_path, &pinned_version, batch).await?;
    let row_count = manifest
        .datasets
        .iter()
        .find(|entry| entry.effective_table_id() == table_id)
        .map(|entry| entry.row_count)
        .unwrap_or(0)
        .saturating_add(1);
    let location = resolve_table_location(namespace, table_id).await?;
    let entry = DatasetEntry::internal(
        table_id,
        manifest_dataset_path(db_path, &location, table_id)?,
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

pub(crate) async fn stage_graph_delete_records(
    db_path: &Path,
    manifest: &GraphManifest,
    records: &[GraphDeleteRecord],
) -> Result<StagedNamespaceTable> {
    let namespace = open_directory_namespace(db_path).await?;
    let table_id = GRAPH_DELETES_TABLE_ID;
    let batch = graph_delete_records_to_batch(records)?
        .ok_or_else(|| NanoError::Storage("graph delete append expected a batch".to_string()))?;
    let pinned_version = manifest
        .datasets
        .iter()
        .find(|entry| entry.effective_table_id() == table_id)
        .map(|entry| GraphTableVersion::new(table_id, entry.dataset_version))
        .unwrap_or_else(|| GraphTableVersion::new(table_id, 0));
    let pinned_version = if pinned_version.version == 0 {
        ensure_graph_deletes_table(db_path)
            .await
            .map(|entry| GraphTableVersion::new(table_id, entry.dataset_version))?
    } else {
        pinned_version
    };
    let location = resolve_or_declare_table_location(namespace.clone(), table_id).await?;
    cleanup_unpublished_manifest_versions(db_path, &location, Some(pinned_version.version)).await?;
    let location_path = namespace_location_to_local_path(db_path, &location)?;
    let version = append_lance_batch_at_version(&location_path, &pinned_version, batch).await?;
    let row_count = manifest
        .datasets
        .iter()
        .find(|entry| entry.effective_table_id() == table_id)
        .map(|entry| entry.row_count)
        .unwrap_or(0)
        .saturating_add(records.len() as u64);
    let location = resolve_table_location(namespace, table_id).await?;
    let entry = DatasetEntry::internal(
        table_id,
        manifest_dataset_path(db_path, &location, table_id)?,
        version.version,
        row_count,
    );
    let published_version =
        namespace_published_version_for_table(db_path, table_id, version.version)
            .await?
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "staged graph deletes {} version {} is not publishable",
                    table_id, version.version
                ))
            })?;
    Ok(StagedNamespaceTable {
        entry,
        published_version,
    })
}

pub(crate) async fn rewrite_namespace_lineage_graph_commit_records(
    db_path: &Path,
    records: &[GraphCommitRecord],
) -> Result<StagedNamespaceTable> {
    let namespace = open_directory_namespace(db_path).await?;
    let batch = namespace_lineage_graph_commit_records_to_batch(records)?
        .unwrap_or_else(empty_namespace_lineage_graph_commit_batch);
    let version = write_namespace_batch(
        namespace.clone(),
        GRAPH_TX_TABLE_ID,
        batch,
        WriteMode::Overwrite,
        None,
    )
    .await?;
    let location = resolve_table_location(namespace, GRAPH_TX_TABLE_ID).await?;
    let entry = DatasetEntry::internal(
        GRAPH_TX_TABLE_ID,
        manifest_dataset_path(db_path, &location, GRAPH_TX_TABLE_ID)?,
        version.version,
        records.len() as u64,
    );
    let published_version =
        namespace_published_version_for_table(db_path, GRAPH_TX_TABLE_ID, version.version)
            .await?
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "rewritten graph tx {} version {} is not publishable",
                    GRAPH_TX_TABLE_ID, version.version
                ))
            })?;
    Ok(StagedNamespaceTable {
        entry,
        published_version,
    })
}

pub(crate) async fn rewrite_graph_delete_records(
    db_path: &Path,
    records: &[GraphDeleteRecord],
) -> Result<StagedNamespaceTable> {
    let namespace = open_directory_namespace(db_path).await?;
    let batch = graph_delete_records_to_batch(records)?.unwrap_or_else(empty_graph_delete_batch);
    let version = write_namespace_batch(
        namespace.clone(),
        GRAPH_DELETES_TABLE_ID,
        batch,
        WriteMode::Overwrite,
        None,
    )
    .await?;
    let location = resolve_table_location(namespace, GRAPH_DELETES_TABLE_ID).await?;
    let entry = DatasetEntry::internal(
        GRAPH_DELETES_TABLE_ID,
        manifest_dataset_path(db_path, &location, GRAPH_DELETES_TABLE_ID)?,
        version.version,
        records.len() as u64,
    );
    let published_version =
        namespace_published_version_for_table(db_path, GRAPH_DELETES_TABLE_ID, version.version)
            .await?
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "rewritten graph deletes {} version {} is not publishable",
                    GRAPH_DELETES_TABLE_ID, version.version
                ))
            })?;
    Ok(StagedNamespaceTable {
        entry,
        published_version,
    })
}

pub(crate) async fn read_namespace_lineage_graph_commit_records(
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
        out.extend(namespace_lineage_graph_commit_records_from_batch(batch)?);
    }
    out.sort_by_key(|record| record.graph_version.value());
    Ok(out)
}

pub(crate) async fn read_graph_delete_records(
    db_path: &Path,
    entry: &DatasetEntry,
) -> Result<Vec<GraphDeleteRecord>> {
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
        out.extend(graph_delete_records_from_batch(batch)?);
    }
    out.sort_by(|a, b| {
        a.graph_version
            .value()
            .cmp(&b.graph_version.value())
            .then(a.table_id.as_str().cmp(b.table_id.as_str()))
            .then(a.logical_key.cmp(&b.logical_key))
    });
    Ok(out)
}

fn empty_namespace_lineage_graph_commit_batch() -> RecordBatch {
    RecordBatch::new_empty(namespace_lineage_graph_commit_schema())
}

fn empty_graph_delete_batch() -> RecordBatch {
    RecordBatch::new_empty(graph_delete_schema())
}

fn namespace_lineage_graph_commit_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("tx_id", DataType::Utf8, false),
        Field::new("graph_version", DataType::UInt64, false),
        Field::new("table_versions_json", DataType::Utf8, false),
        Field::new("touched_tables_json", DataType::Utf8, false),
        Field::new("committed_at", DataType::Utf8, false),
        Field::new("op_summary", DataType::Utf8, false),
        Field::new("schema_identity_version", DataType::UInt32, false),
        Field::new("tx_props_json", DataType::Utf8, false),
    ]))
}

fn graph_delete_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("tx_id", DataType::Utf8, false),
        Field::new("graph_version", DataType::UInt64, false),
        Field::new("committed_at", DataType::Utf8, false),
        Field::new("entity_kind", DataType::Utf8, false),
        Field::new("type_name", DataType::Utf8, false),
        Field::new("table_id", DataType::Utf8, false),
        Field::new("rowid", DataType::UInt64, false),
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("logical_key", DataType::Utf8, false),
        Field::new("row_json", DataType::Utf8, false),
        Field::new("previous_graph_version", DataType::UInt64, true),
    ]))
}

fn namespace_lineage_graph_commit_records_to_batch(
    records: &[GraphCommitRecord],
) -> Result<Option<RecordBatch>> {
    if records.is_empty() {
        return Ok(None);
    }

    let tx_ids = StringArray::from(
        records
            .iter()
            .map(|record| record.tx_id.as_str().to_string())
            .collect::<Vec<_>>(),
    );
    let graph_versions = UInt64Array::from(
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
            .map_err(|err| {
                NanoError::Manifest(format!(
                    "serialize namespace-lineage graph tx table_versions: {}",
                    err
                ))
            })?,
    );
    let touched_tables_json = StringArray::from(
        records
            .iter()
            .map(|record| serde_json::to_string(&record.touched_tables))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| {
                NanoError::Manifest(format!(
                    "serialize namespace-lineage graph tx touched_tables: {}",
                    err
                ))
            })?,
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
    let schema_identity_version = UInt32Array::from(
        records
            .iter()
            .map(|record| record.schema_identity_version)
            .collect::<Vec<_>>(),
    );
    let tx_props_json = StringArray::from(
        records
            .iter()
            .map(|record| serde_json::to_string(&record.tx_props))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| {
                NanoError::Manifest(format!(
                    "serialize namespace-lineage graph tx tx_props: {}",
                    err
                ))
            })?,
    );

    RecordBatch::try_new(
        namespace_lineage_graph_commit_schema(),
        vec![
            Arc::new(tx_ids),
            Arc::new(graph_versions),
            Arc::new(table_versions_json),
            Arc::new(touched_tables_json),
            Arc::new(committed_at),
            Arc::new(op_summary),
            Arc::new(schema_identity_version),
            Arc::new(tx_props_json),
        ],
    )
    .map(Some)
    .map_err(|err| {
        NanoError::Storage(format!(
            "build namespace-lineage graph tx batch: {}",
            err
        ))
    })
}

fn graph_delete_records_to_batch(records: &[GraphDeleteRecord]) -> Result<Option<RecordBatch>> {
    if records.is_empty() {
        return Ok(None);
    }

    let tx_ids = StringArray::from(
        records
            .iter()
            .map(|record| record.tx_id.as_str().to_string())
            .collect::<Vec<_>>(),
    );
    let graph_versions = UInt64Array::from(
        records
            .iter()
            .map(|record| record.graph_version.value())
            .collect::<Vec<_>>(),
    );
    let committed_at = StringArray::from(
        records
            .iter()
            .map(|record| record.committed_at.clone())
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
    let table_id = StringArray::from(
        records
            .iter()
            .map(|record| record.table_id.as_str().to_string())
            .collect::<Vec<_>>(),
    );
    let rowid = UInt64Array::from(records.iter().map(|record| record.rowid).collect::<Vec<_>>());
    let entity_id = UInt64Array::from(
        records
            .iter()
            .map(|record| record.entity_id)
            .collect::<Vec<_>>(),
    );
    let logical_key = StringArray::from(
        records
            .iter()
            .map(|record| record.logical_key.clone())
            .collect::<Vec<_>>(),
    );
    let row_json = StringArray::from(
        records
            .iter()
            .map(|record| serde_json::to_string(&record.row))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| {
                NanoError::Manifest(format!("serialize graph delete row_json: {}", err))
            })?,
    );
    let previous_graph_version = UInt64Array::from(
        records
            .iter()
            .map(|record| record.previous_graph_version)
            .collect::<Vec<_>>(),
    );

    RecordBatch::try_new(
        graph_delete_schema(),
        vec![
            Arc::new(tx_ids),
            Arc::new(graph_versions),
            Arc::new(committed_at),
            Arc::new(entity_kind),
            Arc::new(type_name),
            Arc::new(table_id),
            Arc::new(rowid),
            Arc::new(entity_id),
            Arc::new(logical_key),
            Arc::new(row_json),
            Arc::new(previous_graph_version),
        ],
    )
    .map(Some)
    .map_err(|err| NanoError::Storage(format!("build graph delete batch: {}", err)))
}

fn namespace_lineage_graph_commit_records_from_batch(
    batch: &RecordBatch,
) -> Result<Vec<GraphCommitRecord>> {
    let tx_ids = string_column(batch, "tx_id")?;
    let graph_versions = uint64_column(batch, "graph_version")?;
    let table_versions_json = string_column(batch, "table_versions_json")?;
    let touched_tables_json = string_column(batch, "touched_tables_json")?;
    let committed_at = string_column(batch, "committed_at")?;
    let op_summary = string_column(batch, "op_summary")?;
    let schema_identity_version = batch
        .column_by_name("schema_identity_version")
        .ok_or_else(|| {
            NanoError::Storage(
                "namespace-lineage graph tx batch missing schema_identity_version".to_string(),
            )
        })?
        .as_any()
        .downcast_ref::<UInt32Array>()
        .ok_or_else(|| {
            NanoError::Storage(
                "namespace-lineage graph tx schema_identity_version column is not UInt32"
                    .to_string(),
            )
        })?;
    let tx_props_json = string_column(batch, "tx_props_json")?;

    let mut out = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let table_versions = serde_json::from_str(table_versions_json.value(row)).map_err(|err| {
            NanoError::Manifest(format!(
                "parse namespace-lineage graph tx table_versions_json error: {}",
                err
            ))
        })?;
        let touched_tables: Vec<GraphTouchedTableWindow> =
            serde_json::from_str(touched_tables_json.value(row)).map_err(|err| {
                NanoError::Manifest(format!(
                    "parse namespace-lineage graph tx touched_tables_json error: {}",
                    err
                ))
            })?;
        let tx_props = serde_json::from_str(tx_props_json.value(row)).map_err(|err| {
            NanoError::Manifest(format!(
                "parse namespace-lineage graph tx tx_props_json error: {}",
                err
            ))
        })?;
        out.push(GraphCommitRecord {
            tx_id: tx_ids.value(row).to_string().into(),
            graph_version: graph_versions.value(row).into(),
            table_versions,
            committed_at: committed_at.value(row).to_string(),
            op_summary: op_summary.value(row).to_string(),
            schema_identity_version: schema_identity_version.value(row),
            touched_tables,
            tx_props,
        });
    }
    Ok(out)
}

fn graph_delete_records_from_batch(batch: &RecordBatch) -> Result<Vec<GraphDeleteRecord>> {
    let tx_ids = string_column(batch, "tx_id")?;
    let graph_versions = uint64_column(batch, "graph_version")?;
    let committed_at = string_column(batch, "committed_at")?;
    let entity_kind = string_column(batch, "entity_kind")?;
    let type_name = string_column(batch, "type_name")?;
    let table_id = string_column(batch, "table_id")?;
    let rowid = uint64_column(batch, "rowid")?;
    let entity_id = uint64_column(batch, "entity_id")?;
    let logical_key = string_column(batch, "logical_key")?;
    let row_json = string_column(batch, "row_json")?;
    let previous_graph_version = batch
        .column_by_name("previous_graph_version")
        .and_then(|column| column.as_any().downcast_ref::<UInt64Array>());

    let mut out = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        out.push(GraphDeleteRecord {
            tx_id: tx_ids.value(row).to_string().into(),
            graph_version: graph_versions.value(row).into(),
            committed_at: committed_at.value(row).to_string(),
            entity_kind: entity_kind.value(row).to_string(),
            type_name: type_name.value(row).to_string(),
            table_id: table_id.value(row).to_string().into(),
            rowid: rowid.value(row),
            entity_id: entity_id.value(row),
            logical_key: logical_key.value(row).to_string(),
            row: serde_json::from_str(row_json.value(row)).map_err(|err| {
                NanoError::Manifest(format!("parse graph delete row_json error: {}", err))
            })?,
            previous_graph_version: previous_graph_version
                .and_then(|column| (!column.is_null(row)).then(|| column.value(row))),
        });
    }
    Ok(out)
}

fn string_column<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    batch
        .column_by_name(name)
        .ok_or_else(|| NanoError::Storage(format!("batch missing {} column", name)))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| NanoError::Storage(format!("{} column is not Utf8", name)))
}

fn uint64_column<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a UInt64Array> {
    batch
        .column_by_name(name)
        .ok_or_else(|| NanoError::Storage(format!("batch missing {} column", name)))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| NanoError::Storage(format!("{} column is not UInt64", name)))
}
