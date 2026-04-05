use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{RecordBatch, StringArray, UInt32Array, UInt64Array};
use arrow_schema::{DataType, Field, Schema};

use crate::error::{NanoError, Result};
use crate::store::graph_types::{GraphChangeRecord, GraphCommitRecord};
use crate::store::lance_io::{TableStore, V3TableStore, write_lance_batch};
use crate::store::txlog::{read_visible_graph_change_records, read_visible_graph_commit_records};

pub(crate) const GRAPH_COMMITS_DATASET_DIR: &str = "__graph_commits";
pub(crate) const GRAPH_CHANGES_DATASET_DIR: &str = "__graph_changes";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GraphMirrorStatus {
    pub(crate) commits_present: bool,
    pub(crate) changes_present: bool,
    pub(crate) latest_commit_version: Option<u64>,
    pub(crate) latest_change_version: Option<u64>,
}

pub(crate) fn graph_commits_dataset_path(db_path: &Path) -> PathBuf {
    db_path.join(GRAPH_COMMITS_DATASET_DIR)
}

pub(crate) fn graph_changes_dataset_path(db_path: &Path) -> PathBuf {
    db_path.join(GRAPH_CHANGES_DATASET_DIR)
}

pub(crate) async fn rebuild_graph_mirror_from_wal(db_path: &Path) -> Result<()> {
    let commits = read_visible_graph_commit_records(db_path)?;
    let changes = read_visible_graph_change_records(db_path, 0, None)?;
    rewrite_graph_commit_mirror(db_path, &commits).await?;
    rewrite_graph_change_mirror(db_path, &changes).await?;
    Ok(())
}

pub(crate) async fn read_graph_commit_mirror(db_path: &Path) -> Result<Vec<GraphCommitRecord>> {
    let path = graph_commits_dataset_path(db_path);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let store = V3TableStore::new();
    let version = store.latest_version(&path).await?;
    let batches = store.read_all_batches(&version).await?;
    let mut out = Vec::new();
    for batch in &batches {
        out.extend(graph_commit_records_from_batch(batch)?);
    }
    out.sort_by_key(|record| record.graph_version.value());
    Ok(out)
}

pub(crate) async fn read_graph_change_mirror(db_path: &Path) -> Result<Vec<GraphChangeRecord>> {
    let path = graph_changes_dataset_path(db_path);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let store = V3TableStore::new();
    let version = store.latest_version(&path).await?;
    let batches = store.read_all_batches(&version).await?;
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

pub(crate) async fn inspect_graph_mirror(db_path: &Path) -> Result<GraphMirrorStatus> {
    let commits = read_graph_commit_mirror(db_path).await?;
    let changes = read_graph_change_mirror(db_path).await?;
    Ok(GraphMirrorStatus {
        commits_present: !commits.is_empty(),
        changes_present: !changes.is_empty(),
        latest_commit_version: commits.last().map(|record| record.graph_version.value()),
        latest_change_version: changes.last().map(|record| record.graph_version.value()),
    })
}

async fn rewrite_graph_commit_mirror(db_path: &Path, records: &[GraphCommitRecord]) -> Result<()> {
    rewrite_graph_mirror_dataset(
        &graph_commits_dataset_path(db_path),
        records.is_empty(),
        graph_commit_records_to_batch(records)?,
    )
    .await
}

async fn rewrite_graph_change_mirror(db_path: &Path, records: &[GraphChangeRecord]) -> Result<()> {
    rewrite_graph_mirror_dataset(
        &graph_changes_dataset_path(db_path),
        records.is_empty(),
        graph_change_records_to_batch(records)?,
    )
    .await
}

async fn rewrite_graph_mirror_dataset(
    path: &Path,
    empty: bool,
    batch: Option<RecordBatch>,
) -> Result<()> {
    if empty {
        if path.exists() {
            std::fs::remove_dir_all(path)?;
        }
        return Ok(());
    }

    let batch = batch.ok_or_else(|| {
        NanoError::Storage(format!(
            "mirror dataset {} expected a batch but none was provided",
            path.display()
        ))
    })?;
    write_lance_batch(path, batch).await?;
    Ok(())
}

fn graph_commit_records_to_batch(records: &[GraphCommitRecord]) -> Result<Option<RecordBatch>> {
    if records.is_empty() {
        return Ok(None);
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("tx_id", DataType::Utf8, false),
        Field::new("db_version", DataType::UInt64, false),
        Field::new("table_versions_json", DataType::Utf8, false),
        Field::new("committed_at", DataType::Utf8, false),
        Field::new("op_summary", DataType::Utf8, false),
    ]));

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
    .map_err(|e| NanoError::Storage(format!("build graph commit mirror batch: {}", e)))
}

fn graph_change_records_to_batch(records: &[GraphChangeRecord]) -> Result<Option<RecordBatch>> {
    if records.is_empty() {
        return Ok(None);
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("tx_id", DataType::Utf8, false),
        Field::new("db_version", DataType::UInt64, false),
        Field::new("seq_in_tx", DataType::UInt32, false),
        Field::new("op", DataType::Utf8, false),
        Field::new("entity_kind", DataType::Utf8, false),
        Field::new("type_name", DataType::Utf8, false),
        Field::new("entity_key", DataType::Utf8, false),
        Field::new("payload_json", DataType::Utf8, false),
        Field::new("committed_at", DataType::Utf8, false),
    ]));

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
            Arc::new(committed_at),
        ],
    )
    .map(Some)
    .map_err(|e| NanoError::Storage(format!("build graph change mirror batch: {}", e)))
}

fn graph_commit_records_from_batch(batch: &RecordBatch) -> Result<Vec<GraphCommitRecord>> {
    let tx_ids = string_column(batch, "tx_id")?;
    let db_versions = u64_column(batch, "db_version")?;
    let table_versions_json = string_column(batch, "table_versions_json")?;
    let committed_at = string_column(batch, "committed_at")?;
    let op_summary = string_column(batch, "op_summary")?;

    let mut rows = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let table_versions = serde_json::from_str(table_versions_json.value(row)).map_err(|e| {
            NanoError::Manifest(format!("parse graph commit table_versions_json: {}", e))
        })?;
        rows.push(GraphCommitRecord {
            tx_id: tx_ids.value(row).into(),
            graph_version: db_versions.value(row).into(),
            table_versions,
            committed_at: committed_at.value(row).to_string(),
            op_summary: op_summary.value(row).to_string(),
            schema_identity_version: 0,
            touched_tables: Vec::new(),
            tx_props: std::collections::BTreeMap::new(),
        });
    }
    Ok(rows)
}

fn graph_change_records_from_batch(batch: &RecordBatch) -> Result<Vec<GraphChangeRecord>> {
    let tx_ids = string_column(batch, "tx_id")?;
    let db_versions = u64_column(batch, "db_version")?;
    let seq_in_tx = u32_column(batch, "seq_in_tx")?;
    let op = string_column(batch, "op")?;
    let entity_kind = string_column(batch, "entity_kind")?;
    let type_name = string_column(batch, "type_name")?;
    let entity_key = string_column(batch, "entity_key")?;
    let payload_json = string_column(batch, "payload_json")?;
    let committed_at = string_column(batch, "committed_at")?;

    let mut rows = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let payload = serde_json::from_str(payload_json.value(row))
            .map_err(|e| NanoError::Manifest(format!("parse graph change payload_json: {}", e)))?;
        rows.push(GraphChangeRecord {
            tx_id: tx_ids.value(row).into(),
            graph_version: db_versions.value(row).into(),
            seq_in_tx: seq_in_tx.value(row),
            op: op.value(row).to_string(),
            entity_kind: entity_kind.value(row).to_string(),
            type_name: type_name.value(row).to_string(),
            entity_key: entity_key.value(row).to_string(),
            payload,
            rowid_if_known: None,
            committed_at: committed_at.value(row).to_string(),
        });
    }
    Ok(rows)
}

fn string_column<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    batch
        .column_by_name(name)
        .ok_or_else(|| NanoError::Storage(format!("mirror batch missing {} column", name)))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| NanoError::Storage(format!("mirror {} column is not Utf8", name)))
}

fn u64_column<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a UInt64Array> {
    batch
        .column_by_name(name)
        .ok_or_else(|| NanoError::Storage(format!("mirror batch missing {} column", name)))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| NanoError::Storage(format!("mirror {} column is not UInt64", name)))
}

fn u32_column<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a UInt32Array> {
    batch
        .column_by_name(name)
        .ok_or_else(|| NanoError::Storage(format!("mirror batch missing {} column", name)))?
        .as_any()
        .downcast_ref::<UInt32Array>()
        .ok_or_else(|| NanoError::Storage(format!("mirror {} column is not UInt32", name)))
}
