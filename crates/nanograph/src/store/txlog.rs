use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::thread;

use arrow_array::{Array, UInt64Array};
use futures::TryStreamExt;
use lance::Dataset;
use lance::dataset::scanner::DatasetRecordBatchStream;
use serde::{Deserialize, Serialize};

use crate::error::{NanoError, Result};
use crate::store::database::cdc::record_batch_row_to_json_map;
use crate::store::graph_types::{
    GraphChangeRecord, GraphCommitRecord, GraphDeleteRecord, GraphTableVersion,
};
use crate::store::lance_io::{
    LANCE_INTERNAL_ID_FIELD, open_dataset_for_locator, read_lance_batches_for_locator,
};
use crate::store::manifest::{DatasetEntry, GraphManifest};
use crate::store::metadata::DatasetLocator;
use crate::store::namespace::{
    GRAPH_DELETES_TABLE_ID, open_directory_namespace, resolve_table_location,
};
use crate::store::namespace_commit::commit_with_namespace_adapter;
use crate::store::namespace_commit::publish_snapshot_bundle_with_staged_entries;
use crate::store::namespace_lineage_graph_log::{
    read_graph_delete_records, read_namespace_lineage_graph_commit_records,
    stage_graph_delete_records, stage_namespace_lineage_graph_commit_record,
};
use crate::store::snapshot::{publish_committed_graph_snapshot, read_committed_graph_snapshot};
use crate::store::storage_generation::{StorageGeneration, detect_storage_generation};
use crate::store::v4_graph_log::{read_graph_change_records, read_graph_commit_records};

const WAL_FILENAME: &str = "_wal.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TxCatalogEntry {
    pub tx_id: String,
    pub db_version: u64,
    pub dataset_versions: BTreeMap<String, u64>,
    pub committed_at: String,
    pub op_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CdcLogEntry {
    pub tx_id: String,
    pub db_version: u64,
    pub seq_in_tx: u32,
    pub op: String,
    pub entity_kind: String,
    pub type_name: String,
    pub entity_key: String,
    pub payload: serde_json::Value,
    pub committed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VisibleChangeRow {
    pub graph_version: u64,
    pub tx_id: String,
    pub committed_at: String,
    pub change_kind: String,
    pub entity_kind: String,
    pub type_name: String,
    pub table_id: String,
    pub rowid: u64,
    pub entity_id: u64,
    pub logical_key: String,
    pub row: serde_json::Value,
    pub previous_graph_version: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibleCdcSource {
    Authoritative,
    LineageShadow,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalEntry {
    pub tx_id: String,
    pub db_version: u64,
    pub dataset_versions: BTreeMap<String, u64>,
    pub committed_at: String,
    pub op_summary: String,
    #[serde(default)]
    pub changes: Vec<CdcLogEntry>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LogPruneStats {
    pub(crate) tx_rows_removed: usize,
    pub(crate) tx_rows_kept: usize,
    pub(crate) cdc_rows_removed: usize,
    pub(crate) cdc_rows_kept: usize,
}

#[allow(dead_code)]
pub(crate) trait GraphCommitStore {
    fn read_commits(&self, db_dir: &Path) -> Result<Vec<GraphCommitRecord>>;
}

pub(crate) trait GraphChangeStore {
    fn read_changes(&self, db_dir: &Path) -> Result<Vec<GraphChangeRecord>>;
    fn read_visible_changes(
        &self,
        db_dir: &Path,
        from_graph_version_exclusive: u64,
        to_graph_version_inclusive: Option<u64>,
    ) -> Result<Vec<GraphChangeRecord>>;
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct JsonlGraphStore;

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LineageShadowCdcEntries {
    pub(crate) shadow_entries: Vec<CdcLogEntry>,
    pub(crate) authoritative_entries_compared: Vec<CdcLogEntry>,
    pub(crate) skipped_windows: Vec<LineageShadowCdcSkip>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ShadowLineageChangeSet {
    inserts: BTreeSet<(u64, u64)>,
    updates: BTreeSet<(u64, u64)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LineageShadowCdcSkip {
    pub(crate) graph_version: u64,
    pub(crate) kind: String,
    pub(crate) type_name: String,
    pub(crate) reason: String,
}

impl WalEntry {
    fn graph_commit_record(&self) -> GraphCommitRecord {
        GraphCommitRecord {
            tx_id: self.tx_id.clone().into(),
            graph_version: self.db_version.into(),
            table_versions: self
                .dataset_versions
                .iter()
                .map(|(table_id, version)| GraphTableVersion::new(table_id.as_str(), *version))
                .collect(),
            committed_at: self.committed_at.clone(),
            op_summary: self.op_summary.clone(),
            schema_identity_version: 0,
            touched_tables: Vec::new(),
            tx_props: BTreeMap::new(),
        }
    }

    fn graph_change_records(&self) -> Vec<GraphChangeRecord> {
        self.changes
            .iter()
            .cloned()
            .map(graph_change_from_cdc_entry)
            .collect()
    }
}

fn graph_commit_to_tx_catalog_entry(record: &GraphCommitRecord) -> TxCatalogEntry {
    TxCatalogEntry {
        tx_id: record.tx_id.as_str().to_string(),
        db_version: record.graph_version.value(),
        dataset_versions: record
            .table_versions
            .iter()
            .map(|version| (version.table_id.as_str().to_string(), version.version))
            .collect(),
        committed_at: record.committed_at.clone(),
        op_summary: record.op_summary.clone(),
    }
}

fn graph_change_to_cdc_entry(record: GraphChangeRecord) -> CdcLogEntry {
    CdcLogEntry {
        tx_id: record.tx_id.as_str().to_string(),
        db_version: record.graph_version.value(),
        seq_in_tx: record.seq_in_tx,
        op: record.op,
        entity_kind: record.entity_kind,
        type_name: record.type_name,
        entity_key: record.entity_key,
        payload: record.payload,
        committed_at: record.committed_at,
    }
}

fn graph_change_from_cdc_entry(entry: CdcLogEntry) -> GraphChangeRecord {
    GraphChangeRecord {
        tx_id: entry.tx_id.into(),
        graph_version: entry.db_version.into(),
        seq_in_tx: entry.seq_in_tx,
        op: entry.op,
        entity_kind: entry.entity_kind,
        type_name: entry.type_name,
        entity_key: entry.entity_key,
        payload: entry.payload,
        rowid_if_known: None,
        committed_at: entry.committed_at,
    }
}

fn graph_commit_from_manifest(manifest: &GraphManifest, op_summary: &str) -> GraphCommitRecord {
    GraphCommitRecord {
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
        op_summary: op_summary.to_string(),
        schema_identity_version: manifest.schema_identity_version,
        touched_tables: Vec::new(),
        tx_props: BTreeMap::new(),
    }
}

fn run_v4_async<T, F>(label: &str, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let label = label.to_string();
    thread::spawn(move || work())
        .join()
        .map_err(|_| NanoError::Storage(format!("{} worker thread panicked", label)))?
}

fn wal_entry_from_graph_records(
    commit: &GraphCommitRecord,
    changes: &[GraphChangeRecord],
) -> WalEntry {
    WalEntry {
        tx_id: commit.tx_id.as_str().to_string(),
        db_version: commit.graph_version.value(),
        dataset_versions: commit
            .table_versions
            .iter()
            .map(|version| (version.table_id.as_str().to_string(), version.version))
            .collect(),
        committed_at: commit.committed_at.clone(),
        op_summary: commit.op_summary.clone(),
        changes: changes
            .iter()
            .cloned()
            .map(graph_change_to_cdc_entry)
            .collect(),
    }
}

fn wal_path(db_dir: &Path) -> PathBuf {
    db_dir.join(WAL_FILENAME)
}

impl GraphCommitStore for JsonlGraphStore {
    fn read_commits(&self, db_dir: &Path) -> Result<Vec<GraphCommitRecord>> {
        Ok(read_wal_entries(db_dir)?
            .into_iter()
            .map(|entry| entry.graph_commit_record())
            .collect())
    }
}

impl GraphChangeStore for JsonlGraphStore {
    fn read_changes(&self, db_dir: &Path) -> Result<Vec<GraphChangeRecord>> {
        Ok(flatten_graph_change_rows(read_wal_entries(db_dir)?))
    }

    fn read_visible_changes(
        &self,
        db_dir: &Path,
        from_graph_version_exclusive: u64,
        to_graph_version_inclusive: Option<u64>,
    ) -> Result<Vec<GraphChangeRecord>> {
        read_visible_graph_change_records(
            db_dir,
            from_graph_version_exclusive,
            to_graph_version_inclusive,
        )
    }
}

fn repair_wal_log(db_dir: &Path) -> Result<()> {
    truncate_trailing_partial_jsonl(&wal_path(db_dir))?;
    Ok(())
}

pub(crate) fn reconcile_logs_to_manifest(db_dir: &Path, manifest_db_version: u64) -> Result<()> {
    if matches!(
        detect_storage_generation(db_dir)?,
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage)
    ) {
        let _ = manifest_db_version;
        return Ok(());
    }
    repair_wal_log(db_dir)?;
    let path = wal_path(db_dir);
    let keep_len = compute_wal_visible_prefix(&path, manifest_db_version)?;
    truncate_file_to_len(&path, keep_len)?;
    Ok(())
}

fn append_wal_entry(db_dir: &Path, entry: &WalEntry) -> Result<(u64, u64)> {
    append_jsonl_row(&wal_path(db_dir), entry)
}

#[allow(dead_code)]
pub(crate) fn append_tx_catalog_entry(db_dir: &Path, entry: &TxCatalogEntry) -> Result<(u64, u64)> {
    append_wal_entry(
        db_dir,
        &WalEntry {
            tx_id: entry.tx_id.clone(),
            db_version: entry.db_version,
            dataset_versions: entry.dataset_versions.clone(),
            committed_at: entry.committed_at.clone(),
            op_summary: entry.op_summary.clone(),
            changes: Vec::new(),
        },
    )
}

pub fn read_wal_entries(db_dir: &Path) -> Result<Vec<WalEntry>> {
    read_jsonl_rows(&wal_path(db_dir))
}

pub fn read_tx_catalog_entries(db_dir: &Path) -> Result<Vec<TxCatalogEntry>> {
    Ok(read_visible_graph_commit_records(db_dir)?
        .into_iter()
        .map(|entry| graph_commit_to_tx_catalog_entry(&entry))
        .collect())
}

pub(crate) fn read_cdc_log_entries(db_dir: &Path) -> Result<Vec<CdcLogEntry>> {
    match detect_storage_generation(db_dir)? {
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage) => {
            let manifest = read_committed_graph_snapshot(db_dir)?;
            read_visible_cdc_entries(db_dir, 0, Some(manifest.db_version))
        }
        None => Ok(JsonlGraphStore
            .read_changes(db_dir)?
            .into_iter()
            .map(graph_change_to_cdc_entry)
            .collect()),
    }
}

pub fn read_visible_change_rows(
    db_dir: &Path,
    from_graph_version_exclusive: u64,
    to_graph_version_inclusive: Option<u64>,
) -> Result<Vec<VisibleChangeRow>> {
    match detect_storage_generation(db_dir)? {
        Some(StorageGeneration::NamespaceLineage) => read_visible_change_rows_namespace_lineage(
            db_dir,
            from_graph_version_exclusive,
            to_graph_version_inclusive,
        ),
        Some(StorageGeneration::V4Namespace) | None => Err(NanoError::Storage(format!(
            "lineage-native visible change rows are not available for storage generation at {}",
            db_dir.display()
        ))),
    }
}

/// Read CDC rows that are visible through the committed manifest window.
///
/// Visibility rules:
/// - only WAL rows with `db_version <= manifest.db_version` are considered
/// - rows are filtered to `(from_db_version_exclusive, to_db_version_inclusive]`
/// - output is ordered by `(db_version, seq_in_tx, tx_id)`
pub fn read_visible_cdc_entries(
    db_dir: &Path,
    from_db_version_exclusive: u64,
    to_db_version_inclusive: Option<u64>,
) -> Result<Vec<CdcLogEntry>> {
    read_visible_cdc_entries_with_source(
        db_dir,
        from_db_version_exclusive,
        to_db_version_inclusive,
        VisibleCdcSource::Authoritative,
    )
}

pub fn read_visible_cdc_entries_with_source(
    db_dir: &Path,
    from_db_version_exclusive: u64,
    to_db_version_inclusive: Option<u64>,
    source: VisibleCdcSource,
) -> Result<Vec<CdcLogEntry>> {
    match detect_storage_generation(db_dir)? {
        Some(StorageGeneration::NamespaceLineage) => {
            return read_visible_cdc_entries_namespace_lineage(
                db_dir,
                from_db_version_exclusive,
                to_db_version_inclusive,
            );
        }
        Some(StorageGeneration::V4Namespace) | None => {}
    }

    match source {
        VisibleCdcSource::Authoritative | VisibleCdcSource::LineageShadow => {
            if matches!(
                detect_storage_generation(db_dir)?,
                Some(StorageGeneration::V4Namespace)
            ) {
                return collect_lineage_backed_cdc_entries(
                    db_dir,
                    from_db_version_exclusive,
                    to_db_version_inclusive,
                );
            }
            Ok(JsonlGraphStore
                .read_visible_changes(db_dir, from_db_version_exclusive, to_db_version_inclusive)?
                .into_iter()
                .map(graph_change_to_cdc_entry)
                .collect())
        }
    }
}

fn collect_lineage_backed_cdc_entries(
    db_dir: &Path,
    from_db_version_exclusive: u64,
    to_db_version_inclusive: Option<u64>,
) -> Result<Vec<CdcLogEntry>> {
    let shadow = collect_visible_lineage_shadow_cdc_entries(
        db_dir,
        from_db_version_exclusive,
        to_db_version_inclusive,
    )?;
    if !shadow.skipped_windows.is_empty() {
        let sample = shadow
            .skipped_windows
            .iter()
            .take(3)
            .map(|skip| {
                format!(
                    "{} {} @ graph_version {} ({})",
                    skip.kind, skip.type_name, skip.graph_version, skip.reason
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(NanoError::Storage(format!(
            "lineage-backed CDC is incomplete for {} window(s): {}",
            shadow.skipped_windows.len(),
            sample
        )));
    }
    if shadow.shadow_entries != shadow.authoritative_entries_compared {
        return Err(NanoError::Storage(format!(
            "lineage-backed CDC diverged from the committed payload log: {}",
            summarize_cdc_shadow_mismatch(
                &shadow.authoritative_entries_compared,
                &shadow.shadow_entries
            )
        )));
    }
    Ok(shadow.shadow_entries)
}

#[derive(Debug, Clone, PartialEq)]
struct NamespaceLineageInternalChangeRow {
    visible: VisibleChangeRow,
    before_row: Option<serde_json::Value>,
}

fn read_visible_cdc_entries_namespace_lineage(
    db_dir: &Path,
    from_graph_version_exclusive: u64,
    to_graph_version_inclusive: Option<u64>,
) -> Result<Vec<CdcLogEntry>> {
    let rows = collect_namespace_lineage_change_rows_internal(
        db_dir,
        from_graph_version_exclusive,
        to_graph_version_inclusive,
    )?;
    let mut seq_in_tx_by_tx = BTreeMap::<(u64, String), u32>::new();
    Ok(rows
        .into_iter()
        .map(|row| {
            let key = (row.visible.graph_version, row.visible.tx_id.clone());
            let seq = seq_in_tx_by_tx.entry(key).or_insert(0);
            *seq = seq.saturating_add(1);
            CdcLogEntry {
                tx_id: row.visible.tx_id.clone(),
                db_version: row.visible.graph_version,
                seq_in_tx: *seq,
                op: row.visible.change_kind.clone(),
                entity_kind: row.visible.entity_kind.clone(),
                type_name: row.visible.type_name.clone(),
                entity_key: row.visible.logical_key.clone(),
                payload: match row.visible.change_kind.as_str() {
                    "update" => serde_json::json!({
                        "before": row.before_row,
                        "after": row.visible.row,
                    }),
                    _ => row.visible.row,
                },
                committed_at: row.visible.committed_at.clone(),
            }
        })
        .collect())
}

fn read_visible_change_rows_namespace_lineage(
    db_dir: &Path,
    from_graph_version_exclusive: u64,
    to_graph_version_inclusive: Option<u64>,
) -> Result<Vec<VisibleChangeRow>> {
    Ok(collect_namespace_lineage_change_rows_internal(
        db_dir,
        from_graph_version_exclusive,
        to_graph_version_inclusive,
    )?
    .into_iter()
    .map(|row| row.visible)
    .collect())
}

fn collect_namespace_lineage_change_rows_internal(
    db_dir: &Path,
    from_graph_version_exclusive: u64,
    to_graph_version_inclusive: Option<u64>,
) -> Result<Vec<NamespaceLineageInternalChangeRow>> {
    let manifest = read_committed_graph_snapshot(db_dir)?;
    let upper = to_graph_version_inclusive
        .unwrap_or(manifest.db_version)
        .min(manifest.db_version);
    if upper <= from_graph_version_exclusive {
        return Ok(Vec::new());
    }

    let commits = read_visible_graph_commit_records(db_dir)?
        .into_iter()
        .filter(|commit| {
            let version = commit.graph_version.value();
            version > from_graph_version_exclusive && version <= upper
        })
        .collect::<Vec<_>>();
    let deletes = read_visible_graph_delete_records_namespace_lineage(
        db_dir,
        from_graph_version_exclusive,
        Some(upper),
    )?;
    let delete_map = deletes.into_iter().fold(
        BTreeMap::<u64, Vec<GraphDeleteRecord>>::new(),
        |mut out, row| {
            out.entry(row.graph_version.value()).or_default().push(row);
            out
        },
    );
    let data_entries_by_table_id = manifest
        .datasets
        .iter()
        .filter(|entry| matches!(entry.kind.as_str(), "node" | "edge"))
        .map(|entry| (entry.effective_table_id().to_string(), entry.clone()))
        .collect::<BTreeMap<_, _>>();

    let db_dir = db_dir.to_path_buf();
    run_v4_async("namespace-lineage visible change rows", move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| {
                NanoError::Storage(format!(
                    "initialize namespace-lineage visible change rows runtime error: {}",
                    err
                ))
            })?;
        runtime.block_on(async move {
            let mut out = Vec::new();
            for commit in &commits {
                for window in &commit.touched_tables {
                    let entry = if let Some(entry) =
                        data_entries_by_table_id.get(window.table_id.as_str())
                    {
                        entry.clone()
                    } else {
                        let namespace = open_directory_namespace(&db_dir).await?;
                        let location =
                            resolve_table_location(namespace, window.table_id.as_str()).await?;
                        DatasetEntry {
                            type_id: 0,
                            type_name: window.type_name.clone(),
                            kind: window.entity_kind.clone(),
                            table_id: Some(window.table_id.as_str().to_string()),
                            dataset_path: location,
                            dataset_version: window.after_version,
                            row_count: 0,
                        }
                    };
                    out.extend(
                        collect_namespace_lineage_rows_for_window(&db_dir, &entry, window, commit)
                            .await?,
                    );
                }
                if let Some(rows) = delete_map.get(&commit.graph_version.value()) {
                    out.extend(
                        rows.iter()
                            .cloned()
                            .map(namespace_lineage_delete_to_internal_change),
                    );
                }
            }
            sort_namespace_lineage_internal_change_rows(&mut out);
            Ok(out)
        })
    })
}

async fn collect_namespace_lineage_rows_for_window(
    db_dir: &Path,
    entry: &DatasetEntry,
    window: &crate::store::graph_types::GraphTouchedTableWindow,
    commit: &GraphCommitRecord,
) -> Result<Vec<NamespaceLineageInternalChangeRow>> {
    let actual = collect_actual_lineage_changes_for_commit_shadow(
        db_dir,
        entry,
        window.after_version,
        (window.before_version > 0).then_some(window.before_version),
    )
    .await?
    .ok_or_else(|| {
        NanoError::Storage(format!(
            "lineage data is unavailable for {} {} graph_version {}",
            window.entity_kind,
            window.type_name,
            commit.graph_version.value()
        ))
    })?;

    let current_rows =
        read_logical_rows_by_id(db_dir, window.table_id.as_str(), window.after_version).await?;
    let previous_rows = if window.before_version > 0 {
        read_logical_rows_by_id(db_dir, window.table_id.as_str(), window.before_version).await?
    } else {
        BTreeMap::new()
    };

    let mut inserts = actual.inserts.into_iter().collect::<BTreeMap<u64, u64>>();
    let mut updates = actual.updates.into_iter().collect::<BTreeMap<u64, u64>>();

    for (entity_id, rowid) in inserts.clone() {
        if previous_rows.contains_key(&entity_id) {
            inserts.remove(&entity_id);
            updates.insert(entity_id, rowid);
        }
    }
    for (entity_id, rowid) in updates.clone() {
        if !previous_rows.contains_key(&entity_id) {
            updates.remove(&entity_id);
            inserts.insert(entity_id, rowid);
        }
    }

    let mut out = Vec::new();
    for (entity_id, rowid) in inserts {
        let row = current_rows.get(&entity_id).cloned().ok_or_else(|| {
            NanoError::Storage(format!(
                "missing insert row image for {} {} id={} at table {} version {}",
                window.entity_kind,
                window.type_name,
                entity_id,
                window.table_id.as_str(),
                window.after_version
            ))
        })?;
        out.push(NamespaceLineageInternalChangeRow {
            visible: VisibleChangeRow {
                graph_version: commit.graph_version.value(),
                tx_id: commit.tx_id.as_str().to_string(),
                committed_at: commit.committed_at.clone(),
                change_kind: "insert".to_string(),
                entity_kind: window.entity_kind.clone(),
                type_name: window.type_name.clone(),
                table_id: window.table_id.as_str().to_string(),
                rowid,
                entity_id,
                logical_key: namespace_lineage_logical_key(&window.entity_kind, entity_id, &row),
                row,
                previous_graph_version: (commit.graph_version.value() > 1)
                    .then_some(commit.graph_version.value() - 1),
            },
            before_row: None,
        });
    }
    for (entity_id, rowid) in updates {
        let row = current_rows.get(&entity_id).cloned().ok_or_else(|| {
            NanoError::Storage(format!(
                "missing update row image for {} {} id={} at table {} version {}",
                window.entity_kind,
                window.type_name,
                entity_id,
                window.table_id.as_str(),
                window.after_version
            ))
        })?;
        let before_row = previous_rows.get(&entity_id).cloned().ok_or_else(|| {
            NanoError::Storage(format!(
                "missing previous update row image for {} {} id={} at table {} version {}",
                window.entity_kind,
                window.type_name,
                entity_id,
                window.table_id.as_str(),
                window.before_version
            ))
        })?;
        out.push(NamespaceLineageInternalChangeRow {
            visible: VisibleChangeRow {
                graph_version: commit.graph_version.value(),
                tx_id: commit.tx_id.as_str().to_string(),
                committed_at: commit.committed_at.clone(),
                change_kind: "update".to_string(),
                entity_kind: window.entity_kind.clone(),
                type_name: window.type_name.clone(),
                table_id: window.table_id.as_str().to_string(),
                rowid,
                entity_id,
                logical_key: namespace_lineage_logical_key(&window.entity_kind, entity_id, &row),
                row,
                previous_graph_version: (commit.graph_version.value() > 1)
                    .then_some(commit.graph_version.value() - 1),
            },
            before_row: Some(before_row),
        });
    }
    Ok(out)
}

fn namespace_lineage_delete_to_internal_change(
    row: GraphDeleteRecord,
) -> NamespaceLineageInternalChangeRow {
    NamespaceLineageInternalChangeRow {
        visible: VisibleChangeRow {
            graph_version: row.graph_version.value(),
            tx_id: row.tx_id.as_str().to_string(),
            committed_at: row.committed_at.clone(),
            change_kind: "delete".to_string(),
            entity_kind: row.entity_kind,
            type_name: row.type_name,
            table_id: row.table_id.as_str().to_string(),
            rowid: row.rowid,
            entity_id: row.entity_id,
            logical_key: row.logical_key,
            row: row.row,
            previous_graph_version: row.previous_graph_version,
        },
        before_row: None,
    }
}

async fn read_logical_rows_by_id(
    db_dir: &Path,
    table_id: &str,
    dataset_version: u64,
) -> Result<BTreeMap<u64, serde_json::Value>> {
    let locator = DatasetLocator {
        db_path: db_dir.to_path_buf(),
        table_id: table_id.to_string(),
        dataset_path: std::path::PathBuf::new(),
        dataset_version,
        row_count: 0,
        namespace_managed: true,
    };
    let batches = read_lance_batches_for_locator(&locator).await?;
    let mut out = BTreeMap::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            let object = record_batch_row_to_json_map(batch, row)?;
            let entity_id = object
                .get("__ng_id")
                .and_then(|value| value.as_u64())
                .or_else(|| object.get("id").and_then(|value| value.as_u64()))
                .ok_or_else(|| {
                    NanoError::Storage(format!(
                        "batch for {} version {} is missing id while reconstructing namespace-lineage CDC",
                        table_id, dataset_version
                    ))
                })?;
            out.insert(entity_id, serde_json::Value::Object(object));
        }
    }
    Ok(out)
}

fn namespace_lineage_logical_key(
    entity_kind: &str,
    entity_id: u64,
    row: &serde_json::Value,
) -> String {
    let Some(object) = row.as_object() else {
        return format!("id={}", entity_id);
    };
    if entity_kind == "edge" {
        let src = object.get("src").and_then(|value| value.as_u64());
        let dst = object.get("dst").and_then(|value| value.as_u64());
        if let (Some(src), Some(dst)) = (src, dst) {
            return format!("id={},src={},dst={}", entity_id, src, dst);
        }
    }
    format!("id={}", entity_id)
}

fn sort_namespace_lineage_internal_change_rows(rows: &mut [NamespaceLineageInternalChangeRow]) {
    rows.sort_by(|a, b| {
        a.visible
            .graph_version
            .cmp(&b.visible.graph_version)
            .then(a.visible.entity_kind.cmp(&b.visible.entity_kind))
            .then(a.visible.type_name.cmp(&b.visible.type_name))
            .then(a.visible.rowid.cmp(&b.visible.rowid))
            .then(a.visible.logical_key.cmp(&b.visible.logical_key))
            .then(a.visible.change_kind.cmp(&b.visible.change_kind))
    });
}

pub(crate) fn read_visible_graph_delete_records_namespace_lineage(
    db_dir: &Path,
    from_graph_version_exclusive: u64,
    to_graph_version_inclusive: Option<u64>,
) -> Result<Vec<GraphDeleteRecord>> {
    let manifest = read_committed_graph_snapshot(db_dir)?;
    let upper = to_graph_version_inclusive
        .unwrap_or(manifest.db_version)
        .min(manifest.db_version);
    if upper <= from_graph_version_exclusive {
        return Ok(Vec::new());
    }
    let Some(entry) = manifest
        .datasets
        .iter()
        .find(|entry| entry.effective_table_id() == GRAPH_DELETES_TABLE_ID)
        .cloned()
    else {
        return Ok(Vec::new());
    };
    let db_dir = db_dir.to_path_buf();
    run_v4_async("namespace-lineage graph deletes read", move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| {
                NanoError::Storage(format!(
                    "initialize namespace-lineage graph deletes runtime error: {}",
                    err
                ))
            })?;
        runtime.block_on(async move {
            let rows = read_graph_delete_records(&db_dir, &entry).await?;
            Ok(rows
                .into_iter()
                .filter(|row| {
                    let version = row.graph_version.value();
                    version > from_graph_version_exclusive && version <= upper
                })
                .collect())
        })
    })
}

pub(crate) fn collect_visible_lineage_shadow_cdc_entries(
    db_dir: &Path,
    from_db_version_exclusive: u64,
    to_db_version_inclusive: Option<u64>,
) -> Result<LineageShadowCdcEntries> {
    if matches!(
        detect_storage_generation(db_dir)?,
        Some(StorageGeneration::NamespaceLineage)
    ) {
        let rows = read_visible_cdc_entries_namespace_lineage(
            db_dir,
            from_db_version_exclusive,
            to_db_version_inclusive,
        )?;
        return Ok(LineageShadowCdcEntries {
            shadow_entries: rows.clone(),
            authoritative_entries_compared: rows,
            skipped_windows: Vec::new(),
        });
    }

    if !matches!(
        detect_storage_generation(db_dir)?,
        Some(StorageGeneration::V4Namespace)
    ) {
        let rows = JsonlGraphStore
            .read_visible_changes(db_dir, from_db_version_exclusive, to_db_version_inclusive)?
            .into_iter()
            .map(graph_change_to_cdc_entry)
            .collect::<Vec<_>>();
        return Ok(LineageShadowCdcEntries {
            shadow_entries: rows.clone(),
            authoritative_entries_compared: rows,
            skipped_windows: Vec::new(),
        });
    }

    let manifest = read_committed_graph_snapshot(db_dir)?;
    let upper = to_db_version_inclusive
        .unwrap_or(manifest.db_version)
        .min(manifest.db_version);
    if upper <= from_db_version_exclusive {
        return Ok(LineageShadowCdcEntries::default());
    }

    let all_commits = read_visible_graph_commit_records(db_dir)?;
    let seed_previous_versions = all_commits
        .iter()
        .rev()
        .find(|commit| commit.graph_version.value() <= from_db_version_exclusive)
        .map(|commit| {
            commit
                .table_versions
                .iter()
                .map(|version| (version.table_id.as_str().to_string(), version.version))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let commits = all_commits
        .into_iter()
        .filter(|commit| {
            let version = commit.graph_version.value();
            version > from_db_version_exclusive && version <= upper
        })
        .collect::<Vec<_>>();
    let visible_changes =
        read_visible_graph_change_records(db_dir, from_db_version_exclusive, Some(upper))?;
    let db_dir = db_dir.to_path_buf();

    run_v4_async("v4 lineage shadow cdc", move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| {
                NanoError::Storage(format!(
                    "initialize v4 lineage shadow cdc runtime error: {}",
                    err
                ))
            })?;
        runtime.block_on(async move {
            collect_visible_lineage_shadow_cdc_entries_v4(
                &db_dir,
                &manifest,
                &commits,
                &seed_previous_versions,
                &visible_changes,
            )
            .await
        })
    })
}

pub(crate) fn commit_manifest_and_logs(
    db_dir: &Path,
    manifest: &GraphManifest,
    cdc_entries: &[CdcLogEntry],
    op_summary: &str,
) -> Result<()> {
    let graph_commit = graph_commit_from_manifest(manifest, op_summary);
    let graph_changes = cdc_entries
        .iter()
        .cloned()
        .map(graph_change_from_cdc_entry)
        .collect::<Vec<_>>();
    commit_graph_records_and_manifest(db_dir, &graph_commit, &graph_changes, manifest)
}

pub(crate) fn commit_graph_records_and_manifest(
    db_dir: &Path,
    graph_commit: &GraphCommitRecord,
    graph_changes: &[GraphChangeRecord],
    manifest: &GraphManifest,
) -> Result<()> {
    match detect_storage_generation(db_dir)? {
        Some(StorageGeneration::V4Namespace) => {
            return commit_with_namespace_adapter(db_dir, graph_commit, graph_changes, manifest);
        }
        Some(StorageGeneration::NamespaceLineage) => {
            return Err(NanoError::Storage(format!(
                "NamespaceLineage storage at {} must commit through __graph_deletes + __graph_tx, not __graph_changes",
                db_dir.display()
            )));
        }
        None => {}
    }
    let wal_entry = wal_entry_from_graph_records(graph_commit, graph_changes);
    append_wal_entry(db_dir, &wal_entry)?;
    publish_committed_graph_snapshot(db_dir, manifest)?;
    Ok(())
}

pub(crate) fn commit_graph_records_and_manifest_namespace_lineage(
    db_dir: &Path,
    graph_commit: &GraphCommitRecord,
    graph_deletes: &[GraphDeleteRecord],
    manifest: &GraphManifest,
) -> Result<()> {
    let db_dir = db_dir.to_path_buf();
    let graph_commit = graph_commit.clone();
    let graph_deletes = graph_deletes.to_vec();
    let manifest = manifest.clone();
    run_v4_async("commit namespace-lineage graph update", move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| {
                NanoError::Storage(format!(
                    "initialize namespace-lineage graph commit runtime error: {}",
                    err
                ))
            })?;
        runtime.block_on(async move {
            let mut staged_entries = vec![
                stage_namespace_lineage_graph_commit_record(&db_dir, &manifest, &graph_commit)
                    .await?,
            ];
            if !graph_deletes.is_empty() {
                staged_entries
                    .push(stage_graph_delete_records(&db_dir, &manifest, &graph_deletes).await?);
            }
            let mut next_snapshot = manifest.clone();
            let replaced_ids = staged_entries
                .iter()
                .map(|staged| staged.entry.effective_table_id().to_string())
                .collect::<BTreeSet<_>>();
            next_snapshot
                .datasets
                .retain(|entry| !replaced_ids.contains(entry.effective_table_id()));
            next_snapshot
                .datasets
                .extend(staged_entries.iter().map(|staged| staged.entry.clone()));
            publish_snapshot_bundle_with_staged_entries(&db_dir, &next_snapshot, &staged_entries)
        })
    })
}

/// Prune WAL history to the last N visible db versions.
pub(crate) fn prune_logs_for_replay_window(
    db_dir: &Path,
    retain_tx_versions: u64,
) -> Result<LogPruneStats> {
    if matches!(
        detect_storage_generation(db_dir)?,
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage)
    ) {
        let _ = retain_tx_versions;
        return Ok(LogPruneStats::default());
    }
    if retain_tx_versions == 0 {
        return Err(NanoError::Manifest(
            "retain_tx_versions must be >= 1".to_string(),
        ));
    }

    let manifest = read_committed_graph_snapshot(db_dir)?;
    reconcile_logs_to_manifest(db_dir, manifest.db_version)?;

    let wal_rows = read_wal_entries(db_dir)?;
    let tx_rows_before = wal_rows.len();
    let cdc_rows_before = wal_rows
        .iter()
        .map(|entry| entry.changes.len())
        .sum::<usize>();

    if wal_rows.is_empty() {
        rewrite_jsonl_rows(&wal_path(db_dir), &[] as &[WalEntry])?;
        return Ok(LogPruneStats {
            tx_rows_removed: 0,
            tx_rows_kept: 0,
            cdc_rows_removed: 0,
            cdc_rows_kept: 0,
        });
    }

    let min_db_version = manifest
        .db_version
        .saturating_sub(retain_tx_versions.saturating_sub(1));
    let kept_wal: Vec<WalEntry> = wal_rows
        .into_iter()
        .filter(|entry| {
            entry.db_version >= min_db_version && entry.db_version <= manifest.db_version
        })
        .collect();
    let tx_rows_kept = kept_wal.len();
    let cdc_rows_kept = kept_wal
        .iter()
        .map(|entry| entry.changes.len())
        .sum::<usize>();

    rewrite_jsonl_rows(&wal_path(db_dir), &kept_wal)?;

    Ok(LogPruneStats {
        tx_rows_removed: tx_rows_before.saturating_sub(tx_rows_kept),
        tx_rows_kept,
        cdc_rows_removed: cdc_rows_before.saturating_sub(cdc_rows_kept),
        cdc_rows_kept,
    })
}

pub(crate) fn read_visible_graph_commit_records(db_dir: &Path) -> Result<Vec<GraphCommitRecord>> {
    match detect_storage_generation(db_dir)? {
        Some(StorageGeneration::V4Namespace) => {
            let manifest = read_committed_graph_snapshot(db_dir)?;
            let Some(entry) = manifest
                .datasets
                .iter()
                .find(|entry| entry.effective_table_id() == "__graph_tx")
                .cloned()
            else {
                return Ok(Vec::new());
            };
            let db_dir = db_dir.to_path_buf();
            return run_v4_async("v4 graph tx read", move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| {
                        NanoError::Storage(format!("initialize v4 graph tx runtime error: {}", err))
                    })?;
                runtime.block_on(async move { read_graph_commit_records(&db_dir, &entry).await })
            });
        }
        Some(StorageGeneration::NamespaceLineage) => {
            let manifest = read_committed_graph_snapshot(db_dir)?;
            let Some(entry) = manifest
                .datasets
                .iter()
                .find(|entry| entry.effective_table_id() == "__graph_tx")
                .cloned()
            else {
                return Ok(Vec::new());
            };
            let db_dir = db_dir.to_path_buf();
            return run_v4_async("namespace-lineage graph tx read", move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| {
                        NanoError::Storage(format!(
                            "initialize namespace-lineage graph tx runtime error: {}",
                            err
                        ))
                    })?;
                runtime.block_on(async move {
                    read_namespace_lineage_graph_commit_records(&db_dir, &entry).await
                })
            });
        }
        None => {}
    }
    let manifest = read_committed_graph_snapshot(db_dir)?;
    reconcile_logs_to_manifest(db_dir, manifest.db_version)?;
    Ok(read_wal_entries(db_dir)?
        .into_iter()
        .filter(|entry| entry.db_version <= manifest.db_version)
        .map(|entry| entry.graph_commit_record())
        .collect())
}

pub(crate) fn read_visible_graph_change_records(
    db_dir: &Path,
    from_graph_version_exclusive: u64,
    to_graph_version_inclusive: Option<u64>,
) -> Result<Vec<GraphChangeRecord>> {
    if matches!(
        detect_storage_generation(db_dir)?,
        Some(StorageGeneration::NamespaceLineage)
    ) {
        let rows = read_visible_change_rows_namespace_lineage(
            db_dir,
            from_graph_version_exclusive,
            to_graph_version_inclusive,
        )?;
        let mut seq_in_tx_by_tx = BTreeMap::<(u64, String), u32>::new();
        return Ok(rows
            .into_iter()
            .map(|row| {
                let key = (row.graph_version, row.tx_id.clone());
                let seq = seq_in_tx_by_tx.entry(key).or_insert(0);
                *seq = seq.saturating_add(1);
                GraphChangeRecord {
                    tx_id: row.tx_id.into(),
                    graph_version: row.graph_version.into(),
                    seq_in_tx: *seq,
                    op: row.change_kind,
                    entity_kind: row.entity_kind,
                    type_name: row.type_name,
                    entity_key: row.logical_key,
                    payload: row.row,
                    rowid_if_known: Some(row.rowid),
                    committed_at: row.committed_at,
                }
            })
            .collect());
    }
    if matches!(
        detect_storage_generation(db_dir)?,
        Some(StorageGeneration::V4Namespace)
    ) {
        let manifest = read_committed_graph_snapshot(db_dir)?;
        let upper = to_graph_version_inclusive
            .unwrap_or(manifest.db_version)
            .min(manifest.db_version);
        if upper <= from_graph_version_exclusive {
            return Ok(Vec::new());
        }
        let Some(entry) = manifest
            .datasets
            .iter()
            .find(|entry| entry.effective_table_id() == "__graph_changes")
            .cloned()
        else {
            return Ok(Vec::new());
        };
        let db_dir = db_dir.to_path_buf();
        return run_v4_async("v4 graph change read", move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| {
                    NanoError::Storage(format!("initialize v4 graph change runtime error: {}", err))
                })?;
            runtime.block_on(async move {
                let rows = read_graph_change_records(&db_dir, &entry).await?;
                Ok(rows
                    .into_iter()
                    .filter(|row| {
                        let version = row.graph_version.value();
                        version > from_graph_version_exclusive && version <= upper
                    })
                    .collect())
            })
        });
    }
    let manifest = read_committed_graph_snapshot(db_dir)?;
    reconcile_logs_to_manifest(db_dir, manifest.db_version)?;

    let upper = to_graph_version_inclusive
        .unwrap_or(manifest.db_version)
        .min(manifest.db_version);
    if upper <= from_graph_version_exclusive {
        return Ok(Vec::new());
    }

    let visible = read_wal_entries(db_dir)?
        .into_iter()
        .filter(|entry| {
            entry.db_version > from_graph_version_exclusive && entry.db_version <= upper
        })
        .collect::<Vec<_>>();
    Ok(flatten_graph_change_rows(visible))
}

fn flatten_graph_change_rows(entries: Vec<WalEntry>) -> Vec<GraphChangeRecord> {
    let mut rows: Vec<GraphChangeRecord> = entries
        .into_iter()
        .flat_map(|entry| entry.graph_change_records().into_iter())
        .collect();
    rows.sort_by(|a, b| {
        a.graph_version
            .value()
            .cmp(&b.graph_version.value())
            .then(a.seq_in_tx.cmp(&b.seq_in_tx))
            .then(a.tx_id.as_str().cmp(b.tx_id.as_str()))
    });
    rows
}

async fn collect_visible_lineage_shadow_cdc_entries_v4(
    db_dir: &Path,
    manifest: &GraphManifest,
    commits: &[GraphCommitRecord],
    seed_previous_versions: &BTreeMap<String, u64>,
    visible_changes: &[GraphChangeRecord],
) -> Result<LineageShadowCdcEntries> {
    let data_entries_by_table_id = manifest
        .datasets
        .iter()
        .filter(|entry| matches!(entry.kind.as_str(), "node" | "edge"))
        .map(|entry| (entry.effective_table_id().to_string(), entry.clone()))
        .collect::<BTreeMap<_, _>>();
    let authoritative_by_window = visible_changes.iter().cloned().fold(
        BTreeMap::<(u64, String, String), Vec<GraphChangeRecord>>::new(),
        |mut out, row| {
            out.entry((
                row.graph_version.value(),
                row.entity_kind.clone(),
                row.type_name.clone(),
            ))
            .or_default()
            .push(row);
            out
        },
    );

    let mut shadow_entries = visible_changes
        .iter()
        .filter(|row| row.op == "delete")
        .cloned()
        .map(graph_change_to_cdc_entry)
        .collect::<Vec<_>>();
    let mut authoritative_entries_compared = shadow_entries.clone();
    let mut skipped_windows = Vec::new();
    let mut previous_versions = seed_previous_versions.clone();

    for commit in commits {
        let graph_version = commit.graph_version.value();
        let mut next_versions = BTreeMap::<String, u64>::new();
        for table_version in &commit.table_versions {
            let Some(entry) = data_entries_by_table_id.get(table_version.table_id.as_str()) else {
                continue;
            };
            let table_id = entry.effective_table_id().to_string();
            let previous_version = previous_versions.get(&table_id).copied();
            next_versions.insert(table_id.clone(), table_version.version);

            let window_rows = authoritative_by_window
                .get(&(graph_version, entry.kind.clone(), entry.type_name.clone()))
                .cloned()
                .unwrap_or_default();
            let window_live_rows = window_rows
                .into_iter()
                .filter(|row| matches!(row.op.as_str(), "insert" | "update"))
                .collect::<Vec<_>>();
            if window_live_rows.is_empty() && previous_version == Some(table_version.version) {
                continue;
            }

            let Some(mut authoritative_lookup) =
                build_authoritative_lineage_lookup(&window_live_rows)
            else {
                skipped_windows.push(LineageShadowCdcSkip {
                    graph_version,
                    kind: entry.kind.clone(),
                    type_name: entry.type_name.clone(),
                    reason: "graph changes are missing row ids".to_string(),
                });
                continue;
            };
            let actual = match collect_actual_lineage_changes_for_commit_shadow(
                db_dir,
                entry,
                table_version.version,
                previous_version,
            )
            .await
            {
                Ok(Some(actual)) => actual,
                Ok(None) => {
                    skipped_windows.push(LineageShadowCdcSkip {
                        graph_version,
                        kind: entry.kind.clone(),
                        type_name: entry.type_name.clone(),
                        reason: "historical table versions are unavailable".to_string(),
                    });
                    continue;
                }
                Err(err) => {
                    skipped_windows.push(LineageShadowCdcSkip {
                        graph_version,
                        kind: entry.kind.clone(),
                        type_name: entry.type_name.clone(),
                        reason: err.to_string(),
                    });
                    continue;
                }
            };

            authoritative_entries_compared.extend(
                window_live_rows
                    .iter()
                    .cloned()
                    .map(graph_change_to_cdc_entry),
            );
            shadow_entries.extend(match_lineage_changes_to_graph_changes(
                &mut authoritative_lookup,
                &actual,
            ));
        }
        previous_versions = next_versions;
    }

    sort_cdc_entries(&mut shadow_entries);
    sort_cdc_entries(&mut authoritative_entries_compared);
    Ok(LineageShadowCdcEntries {
        shadow_entries,
        authoritative_entries_compared,
        skipped_windows,
    })
}

fn build_authoritative_lineage_lookup(
    rows: &[GraphChangeRecord],
) -> Option<BTreeMap<(String, u64, u64), Vec<GraphChangeRecord>>> {
    let mut out = BTreeMap::<(String, u64, u64), Vec<GraphChangeRecord>>::new();
    for row in rows {
        let entity_id = shadow_graph_change_entity_id(row)?;
        let rowid = row.rowid_if_known?;
        out.entry((row.op.clone(), entity_id, rowid))
            .or_default()
            .push(row.clone());
    }
    Some(out)
}

fn match_lineage_changes_to_graph_changes(
    authoritative_lookup: &mut BTreeMap<(String, u64, u64), Vec<GraphChangeRecord>>,
    actual: &ShadowLineageChangeSet,
) -> Vec<CdcLogEntry> {
    let mut out = Vec::new();
    for (entity_id, rowid) in &actual.inserts {
        if let Some(entry) =
            pop_authoritative_lineage_match(authoritative_lookup, "insert", *entity_id, *rowid)
        {
            out.push(graph_change_to_cdc_entry(entry));
        }
    }
    for (entity_id, rowid) in &actual.updates {
        if let Some(entry) =
            pop_authoritative_lineage_match(authoritative_lookup, "update", *entity_id, *rowid)
        {
            out.push(graph_change_to_cdc_entry(entry));
        }
    }
    out
}

fn pop_authoritative_lineage_match(
    authoritative_lookup: &mut BTreeMap<(String, u64, u64), Vec<GraphChangeRecord>>,
    op: &str,
    entity_id: u64,
    rowid: u64,
) -> Option<GraphChangeRecord> {
    let key = (op.to_string(), entity_id, rowid);
    let bucket = authoritative_lookup.get_mut(&key)?;
    if bucket.is_empty() {
        return None;
    }
    Some(bucket.remove(0))
}

fn sort_cdc_entries(rows: &mut [CdcLogEntry]) {
    rows.sort_by(|a, b| {
        a.db_version
            .cmp(&b.db_version)
            .then(a.seq_in_tx.cmp(&b.seq_in_tx))
            .then(a.tx_id.cmp(&b.tx_id))
    });
}

fn summarize_cdc_shadow_mismatch(authoritative: &[CdcLogEntry], shadow: &[CdcLogEntry]) -> String {
    let common_len = authoritative.len().min(shadow.len());
    for idx in 0..common_len {
        let expected = &authoritative[idx];
        let actual = &shadow[idx];
        if expected != actual {
            return format!(
                "first mismatch at row {} expected {} {} {} {} but shadow had {} {} {} {}",
                idx + 1,
                expected.db_version,
                expected.seq_in_tx,
                expected.op,
                expected.entity_key,
                actual.db_version,
                actual.seq_in_tx,
                actual.op,
                actual.entity_key
            );
        }
    }
    format!(
        "row count mismatch authoritative={} shadow={}",
        authoritative.len(),
        shadow.len()
    )
}

async fn collect_actual_lineage_changes_for_commit_shadow(
    db_dir: &Path,
    entry: &DatasetEntry,
    current_version: u64,
    previous_version: Option<u64>,
) -> Result<Option<ShadowLineageChangeSet>> {
    let locator = DatasetLocator {
        db_path: db_dir.to_path_buf(),
        table_id: entry.effective_table_id().to_string(),
        dataset_path: db_dir.join(&entry.dataset_path),
        dataset_version: current_version,
        row_count: entry.row_count,
        namespace_managed: true,
    };
    let dataset = match open_dataset_for_locator(&locator).await {
        Ok(dataset) => dataset,
        Err(err) => {
            return match err {
                NanoError::Lance(_) | NanoError::Storage(_) => Ok(None),
                _ => Err(err),
            };
        }
    };

    let mut changes = ShadowLineageChangeSet::default();
    if let Some(previous_version) = previous_version {
        let delta = dataset
            .delta()
            .compared_against_version(previous_version)
            .build()
            .map_err(|err| {
                NanoError::Lance(format!(
                    "build delta {} current {} vs previous {} error: {}",
                    entry.effective_table_id(),
                    current_version,
                    previous_version,
                    err
                ))
            })?;
        changes.inserts = collect_entity_rowids_from_lineage_stream_shadow(
            delta.get_inserted_rows().await.map_err(|err| {
                NanoError::Lance(format!(
                    "read inserted lineage rows {} current {} vs previous {} error: {}",
                    entry.effective_table_id(),
                    current_version,
                    previous_version,
                    err
                ))
            })?,
        )
        .await?;
        changes.updates = collect_entity_rowids_from_lineage_stream_shadow(
            delta.get_updated_rows().await.map_err(|err| {
                NanoError::Lance(format!(
                    "read updated lineage rows {} current {} vs previous {} error: {}",
                    entry.effective_table_id(),
                    current_version,
                    previous_version,
                    err
                ))
            })?,
        )
        .await?;
    } else {
        changes.inserts = collect_entity_rowids_from_live_dataset_shadow(&dataset).await?;
    }

    Ok(Some(changes))
}

async fn collect_entity_rowids_from_live_dataset_shadow(
    dataset: &Dataset,
) -> Result<BTreeSet<(u64, u64)>> {
    let mut scanner = dataset.scan();
    scanner
        .project(&[LANCE_INTERNAL_ID_FIELD, "_rowid"])
        .map_err(|err| NanoError::Lance(format!("project baseline lineage scan error: {}", err)))?;
    let stream = scanner
        .try_into_stream()
        .await
        .map_err(|err| NanoError::Lance(format!("scan baseline lineage dataset error: {}", err)))?;
    collect_entity_rowids_from_lineage_stream_shadow(stream).await
}

async fn collect_entity_rowids_from_lineage_stream_shadow(
    stream: DatasetRecordBatchStream,
) -> Result<BTreeSet<(u64, u64)>> {
    let batches = stream
        .try_collect::<Vec<_>>()
        .await
        .map_err(|err| NanoError::Lance(format!("collect lineage stream error: {}", err)))?;
    let mut out = BTreeSet::new();
    for batch in batches {
        let entity_ids = batch
            .column_by_name(LANCE_INTERNAL_ID_FIELD)
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "lineage batch missing {} column",
                    LANCE_INTERNAL_ID_FIELD
                ))
            })?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "lineage {} column is not UInt64",
                    LANCE_INTERNAL_ID_FIELD
                ))
            })?;
        let rowids = batch
            .column_by_name("_rowid")
            .ok_or_else(|| NanoError::Storage("lineage batch missing _rowid column".to_string()))?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| NanoError::Storage("lineage _rowid column is not UInt64".to_string()))?;
        for row in 0..batch.num_rows() {
            out.insert((entity_ids.value(row), rowids.value(row)));
        }
    }
    Ok(out)
}

fn shadow_graph_change_entity_id(record: &GraphChangeRecord) -> Option<u64> {
    let key = record.entity_key.strip_prefix("id=")?;
    let value = key.split(',').next()?;
    value.parse::<u64>().ok()
}

fn append_jsonl_row<T: Serialize>(path: &Path, row: &T) -> Result<(u64, u64)> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let start_offset = file.metadata()?.len();
    let json = serde_json::to_vec(row)
        .map_err(|e| NanoError::Manifest(format!("serialize JSONL row: {}", e)))?;
    file.write_all(&json)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    let end_offset = file.metadata()?.len();
    Ok((start_offset, end_offset))
}

fn rewrite_jsonl_rows<T: Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;

        for row in rows {
            let json = serde_json::to_vec(row)
                .map_err(|e| NanoError::Manifest(format!("serialize JSONL row: {}", e)))?;
            file.write_all(&json)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn read_jsonl_rows<T>(path: &Path) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();

    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: T = serde_json::from_str(&line).map_err(|e| {
            NanoError::Manifest(format!(
                "parse JSONL row {} in {}: {}",
                line_no + 1,
                path.display(),
                e
            ))
        })?;
        out.push(parsed);
    }

    Ok(out)
}

fn truncate_trailing_partial_jsonl(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let bytes = std::fs::read(path)?;
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        return Ok(());
    }

    let keep_len = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(keep_len as u64)?;
    file.sync_all()?;
    Ok(())
}

fn compute_wal_visible_prefix(path: &Path, manifest_db_version: u64) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }

    let bytes = std::fs::read(path)?;
    if bytes.is_empty() {
        return Ok(0);
    }

    let mut keep_len = 0u64;
    let mut prev_db_version = None;
    let mut offset = 0u64;

    for (line_no, chunk) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
        let next_offset = offset + chunk.len() as u64;
        let line = if chunk.last() == Some(&b'\n') {
            &chunk[..chunk.len().saturating_sub(1)]
        } else {
            chunk
        };

        let line_no = line_no + 1;
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            keep_len = next_offset;
            offset = next_offset;
            continue;
        }

        let line_str = std::str::from_utf8(line).map_err(|e| {
            NanoError::Manifest(format!(
                "invalid UTF-8 in WAL line {} ({}): {}",
                line_no,
                path.display(),
                e
            ))
        })?;
        let entry: WalEntry = serde_json::from_str(line_str).map_err(|e| {
            NanoError::Manifest(format!(
                "parse WAL line {} ({}): {}",
                line_no,
                path.display(),
                e
            ))
        })?;

        if let Some(prev) = prev_db_version
            && entry.db_version <= prev
        {
            return Err(NanoError::Manifest(format!(
                "non-monotonic db_version in WAL at line {} (prev {}, got {})",
                line_no, prev, entry.db_version
            )));
        }
        prev_db_version = Some(entry.db_version);

        if entry.db_version > manifest_db_version {
            break;
        }

        keep_len = next_offset;
        offset = next_offset;
    }

    Ok(keep_len)
}

fn truncate_file_to_len(path: &Path, keep_len: u64) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let current_len = std::fs::metadata(path)?.len();
    if keep_len >= current_len {
        return Ok(());
    }

    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(keep_len)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_manifest(db_version: u64) -> GraphManifest {
        let mut manifest = GraphManifest::new("abc".to_string());
        manifest.db_version = db_version;
        manifest.last_tx_id = format!("manifest-{}", db_version);
        manifest.committed_at = format!("170000000{}", db_version);
        manifest
    }

    fn sample_cdc(tx_id: &str, db_version: u64, seq_in_tx: u32, key: &str) -> CdcLogEntry {
        CdcLogEntry {
            tx_id: tx_id.to_string(),
            db_version,
            seq_in_tx,
            op: "insert".to_string(),
            entity_kind: "node".to_string(),
            type_name: "Person".to_string(),
            entity_key: key.to_string(),
            payload: serde_json::json!({ "key": key }),
            committed_at: format!("170000000{}", db_version),
        }
    }

    fn sample_tx_entry() -> TxCatalogEntry {
        let mut dataset_versions = BTreeMap::new();
        dataset_versions.insert("nodes/99c1bf00".to_string(), 3);
        dataset_versions.insert("edges/f7012952".to_string(), 1);
        TxCatalogEntry {
            tx_id: "tx-1".to_string(),
            db_version: 1,
            dataset_versions,
            committed_at: "1700000000".to_string(),
            op_summary: "test".to_string(),
        }
    }

    #[test]
    fn wal_roundtrip_projects_tx_and_cdc_views() {
        let dir = TempDir::new().unwrap();
        let entry = WalEntry {
            tx_id: "tx-1".to_string(),
            db_version: 1,
            dataset_versions: BTreeMap::from([("nodes/x".to_string(), 3)]),
            committed_at: "1700000001".to_string(),
            op_summary: "load:merge".to_string(),
            changes: vec![sample_cdc("tx-1", 1, 0, "Alice")],
        };

        let offsets = append_wal_entry(dir.path(), &entry).unwrap();
        assert!(offsets.1 > offsets.0);

        let wal = read_wal_entries(dir.path()).unwrap();
        assert_eq!(wal, vec![entry.clone()]);

        let tx = JsonlGraphStore
            .read_commits(dir.path())
            .unwrap()
            .into_iter()
            .map(|entry| graph_commit_to_tx_catalog_entry(&entry))
            .collect::<Vec<_>>();
        assert_eq!(
            tx,
            vec![TxCatalogEntry {
                tx_id: entry.tx_id.clone(),
                db_version: entry.db_version,
                dataset_versions: entry.dataset_versions.clone(),
                committed_at: entry.committed_at.clone(),
                op_summary: entry.op_summary.clone(),
            }]
        );

        let cdc = JsonlGraphStore
            .read_changes(dir.path())
            .unwrap()
            .into_iter()
            .map(graph_change_to_cdc_entry)
            .collect::<Vec<_>>();
        assert_eq!(cdc, entry.changes);
    }

    #[test]
    fn tx_catalog_append_is_projected_as_empty_change_wal_row() {
        let dir = TempDir::new().unwrap();
        let entry = sample_tx_entry();

        append_tx_catalog_entry(dir.path(), &entry).unwrap();

        let wal = read_wal_entries(dir.path()).unwrap();
        assert_eq!(wal.len(), 1);
        assert!(wal[0].changes.is_empty());

        let tx = JsonlGraphStore
            .read_commits(dir.path())
            .unwrap()
            .into_iter()
            .map(|row| graph_commit_to_tx_catalog_entry(&row))
            .collect::<Vec<_>>();
        assert_eq!(tx, vec![entry]);
    }

    #[test]
    fn commit_manifest_and_logs_appends_single_wal_row() {
        let dir = TempDir::new().unwrap();
        let manifest = sample_manifest(1);
        let cdc = vec![sample_cdc("manifest-1", 1, 0, "Alice")];

        commit_manifest_and_logs(dir.path(), &manifest, &cdc, "test_commit").unwrap();

        let wal = read_wal_entries(dir.path()).unwrap();
        assert_eq!(wal.len(), 1);
        assert_eq!(wal[0].db_version, 1);
        assert_eq!(wal[0].op_summary, "test_commit");
        assert_eq!(wal[0].changes, cdc);
    }

    #[test]
    fn visible_cdc_respects_manifest_gate() {
        let dir = TempDir::new().unwrap();

        let manifest1 = sample_manifest(1);
        let cdc1 = vec![sample_cdc("manifest-1", 1, 0, "Alice")];
        commit_manifest_and_logs(dir.path(), &manifest1, &cdc1, "tx1").unwrap();

        let future = WalEntry {
            tx_id: "future".to_string(),
            db_version: 2,
            dataset_versions: BTreeMap::new(),
            committed_at: "1700000002".to_string(),
            op_summary: "future".to_string(),
            changes: vec![sample_cdc("future", 2, 0, "Bob")],
        };
        append_wal_entry(dir.path(), &future).unwrap();

        let visible = read_visible_cdc_entries(dir.path(), 0, None).unwrap();
        assert_eq!(visible, cdc1);

        let wal_after = read_wal_entries(dir.path()).unwrap();
        assert_eq!(wal_after.len(), 1);
        assert_eq!(wal_after[0].db_version, 1);
    }

    #[test]
    fn visible_cdc_honors_version_window() {
        let dir = TempDir::new().unwrap();

        commit_manifest_and_logs(
            dir.path(),
            &sample_manifest(1),
            &[sample_cdc("manifest-1", 1, 0, "Alice")],
            "tx1",
        )
        .unwrap();
        commit_manifest_and_logs(
            dir.path(),
            &sample_manifest(2),
            &[sample_cdc("manifest-2", 2, 0, "Bob")],
            "tx2",
        )
        .unwrap();
        commit_manifest_and_logs(
            dir.path(),
            &sample_manifest(3),
            &[sample_cdc("manifest-3", 3, 0, "Charlie")],
            "tx3",
        )
        .unwrap();

        let rows_since_1 = read_visible_cdc_entries(dir.path(), 1, None).unwrap();
        assert_eq!(
            rows_since_1
                .iter()
                .map(|row| row.entity_key.as_str())
                .collect::<Vec<_>>(),
            vec!["Bob", "Charlie"]
        );

        let rows_range_2_only = read_visible_cdc_entries(dir.path(), 1, Some(2)).unwrap();
        assert_eq!(rows_range_2_only.len(), 1);
        assert_eq!(rows_range_2_only[0].entity_key, "Bob");
    }

    #[test]
    fn jsonl_graph_store_projects_commit_and_change_records() {
        let dir = TempDir::new().unwrap();
        let manifest = sample_manifest(1);
        let cdc = vec![sample_cdc("manifest-1", 1, 0, "Alice")];

        commit_manifest_and_logs(dir.path(), &manifest, &cdc, "test_commit").unwrap();

        let commits = JsonlGraphStore.read_commits(dir.path()).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].tx_id.as_str(), "manifest-1");
        assert_eq!(commits[0].graph_version.value(), 1);
        assert_eq!(commits[0].op_summary, "test_commit");

        let changes = JsonlGraphStore.read_changes(dir.path()).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].entity_key, "Alice");
        assert_eq!(changes[0].graph_version.value(), 1);
    }

    #[test]
    fn visible_graph_change_records_honor_version_window() {
        let dir = TempDir::new().unwrap();

        commit_manifest_and_logs(
            dir.path(),
            &sample_manifest(1),
            &[sample_cdc("manifest-1", 1, 0, "Alice")],
            "tx1",
        )
        .unwrap();
        commit_manifest_and_logs(
            dir.path(),
            &sample_manifest(2),
            &[sample_cdc("manifest-2", 2, 0, "Bob")],
            "tx2",
        )
        .unwrap();

        let rows = read_visible_graph_change_records(dir.path(), 1, Some(2)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entity_key, "Bob");
        assert_eq!(rows[0].graph_version.value(), 2);
    }

    #[test]
    fn reconcile_truncates_partial_last_line() {
        let dir = TempDir::new().unwrap();
        let manifest = sample_manifest(1);
        commit_manifest_and_logs(
            dir.path(),
            &manifest,
            &[sample_cdc("manifest-1", 1, 0, "Alice")],
            "tx1",
        )
        .unwrap();

        let path = wal_path(dir.path());
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"tx_id":"partial""#).unwrap();
        file.sync_all().unwrap();

        reconcile_logs_to_manifest(dir.path(), 1).unwrap();
        let wal = read_wal_entries(dir.path()).unwrap();
        assert_eq!(wal.len(), 1);
    }

    #[test]
    fn prune_logs_for_replay_window_rewrites_single_wal() {
        let dir = TempDir::new().unwrap();
        commit_manifest_and_logs(
            dir.path(),
            &sample_manifest(1),
            &[sample_cdc("manifest-1", 1, 0, "Alice")],
            "tx1",
        )
        .unwrap();
        commit_manifest_and_logs(
            dir.path(),
            &sample_manifest(2),
            &[sample_cdc("manifest-2", 2, 0, "Bob")],
            "tx2",
        )
        .unwrap();
        commit_manifest_and_logs(
            dir.path(),
            &sample_manifest(3),
            &[sample_cdc("manifest-3", 3, 0, "Charlie")],
            "tx3",
        )
        .unwrap();

        let stats = prune_logs_for_replay_window(dir.path(), 2).unwrap();
        assert_eq!(stats.tx_rows_removed, 1);
        assert_eq!(stats.tx_rows_kept, 2);
        assert_eq!(stats.cdc_rows_removed, 1);
        assert_eq!(stats.cdc_rows_kept, 2);

        let tx = read_tx_catalog_entries(dir.path()).unwrap();
        assert_eq!(tx.len(), 2);
        assert_eq!(tx[0].db_version, 2);
        assert_eq!(tx[1].db_version, 3);

        let cdc = read_visible_cdc_entries(dir.path(), 0, None).unwrap();
        assert_eq!(
            cdc.iter()
                .map(|row| row.entity_key.as_str())
                .collect::<Vec<_>>(),
            vec!["Bob", "Charlie"]
        );
    }
}
