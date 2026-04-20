use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use arrow_array::{Array, RecordBatch, StringArray, UInt64Array};
use arrow_schema::{DataType, Field, Schema};
use futures::StreamExt;
use lance::Dataset;
use lance::dataset::WriteMode;
use tracing::warn;

use crate::error::{NanoError, Result};
use crate::store::graph_types::{GraphChangeRecord, GraphCommitRecord};
use crate::store::lance_io::LANCE_INTERNAL_ID_FIELD;
use crate::store::lance_io::{
    append_lance_batch_at_version, write_lance_batch_with_mode_versioned,
};
use crate::store::manifest::{DatasetEntry, GraphManifest};
use crate::store::namespace::{
    GRAPH_CHANGES_TABLE_ID, GRAPH_SNAPSHOT_TABLE_ID, GRAPH_TX_TABLE_ID,
    NamespacePublishedVersion, StagedNamespaceTable, batch_publish_namespace_versions,
    dedup_namespace_published_versions, namespace_latest_version,
    namespace_location_to_dataset_uri, namespace_location_to_local_path,
    namespace_location_to_manifest_dataset_path,
    namespace_published_version_for_table, open_directory_namespace,
    resolve_or_declare_table_location,
};
use crate::store::v4_graph_log::{stage_graph_change_records, stage_graph_commit_record};

pub(crate) trait NamespaceCommitAdapter: Send + Sync {
    fn commit_graph_update(
        &self,
        db_dir: &Path,
        graph_commit: &GraphCommitRecord,
        graph_changes: &[GraphChangeRecord],
        manifest: &GraphManifest,
    ) -> Result<()>;
}

#[derive(Debug, Clone)]
pub(crate) struct GraphCommitBundle {
    pub(crate) next_snapshot: GraphManifest,
    pub(crate) published_versions: Vec<NamespacePublishedVersion>,
    pub(crate) internal_entries: Vec<DatasetEntry>,
}

impl GraphCommitBundle {
    fn validate(&self) -> Result<()> {
        if self
            .next_snapshot
            .datasets
            .iter()
            .any(|entry| entry.effective_table_id() == GRAPH_SNAPSHOT_TABLE_ID)
        {
            return Err(NanoError::Storage(format!(
                "graph commit bundle snapshot payload must not include {} as a manifest dataset",
                GRAPH_SNAPSHOT_TABLE_ID
            )));
        }

        let snapshot_entry_count = self
            .internal_entries
            .iter()
            .filter(|entry| entry.effective_table_id() == GRAPH_SNAPSHOT_TABLE_ID)
            .count();
        if snapshot_entry_count != 1 {
            return Err(NanoError::Storage(format!(
                "graph commit bundle must include exactly one {} entry, found {}",
                GRAPH_SNAPSHOT_TABLE_ID, snapshot_entry_count
            )));
        }

        let tx_entry_count = self
            .internal_entries
            .iter()
            .filter(|entry| entry.effective_table_id() == GRAPH_TX_TABLE_ID)
            .count();
        if tx_entry_count != 1 {
            return Err(NanoError::Storage(format!(
                "graph commit bundle must include exactly one {} entry, found {}",
                GRAPH_TX_TABLE_ID, tx_entry_count
            )));
        }

        let changes_entry_count = self
            .internal_entries
            .iter()
            .filter(|entry| entry.effective_table_id() == GRAPH_CHANGES_TABLE_ID)
            .count();
        if changes_entry_count > 1 {
            return Err(NanoError::Storage(format!(
                "graph commit bundle must include at most one {} entry, found {}",
                GRAPH_CHANGES_TABLE_ID, changes_entry_count
            )));
        }

        let snapshot_publish_count = self
            .published_versions
            .iter()
            .filter(|version| version.table_id == GRAPH_SNAPSHOT_TABLE_ID)
            .count();
        if snapshot_publish_count != 1 {
            return Err(NanoError::Storage(format!(
                "graph commit bundle must publish exactly one {} version, found {}",
                GRAPH_SNAPSHOT_TABLE_ID, snapshot_publish_count
            )));
        }

        let mut seen_versions = BTreeSet::new();
        for version in &self.published_versions {
            let key = (version.table_id.clone(), version.version);
            if !seen_versions.insert(key) {
                return Err(NanoError::Storage(format!(
                    "graph commit bundle contains duplicate publish entry {}@{}",
                    version.table_id, version.version
                )));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GraphCommitBundlePublisher;

impl GraphCommitBundlePublisher {
    pub(crate) async fn publish_bundle_async(
        &self,
        db_dir: &Path,
        bundle: &GraphCommitBundle,
    ) -> Result<()> {
        bundle.validate()?;
        batch_publish_namespace_versions(db_dir, &bundle.published_versions).await?;
        if let Err(err) = remove_legacy_graph_manifest_file(db_dir) {
            warn!(
                db_path = %db_dir.display(),
                "failed to remove legacy graph.manifest.json after namespace bundle publish: {}",
                err
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExplicitGraphCommitBundleNamespaceCommitAdapter;

impl NamespaceCommitAdapter for ExplicitGraphCommitBundleNamespaceCommitAdapter {
    fn commit_graph_update(
        &self,
        db_dir: &Path,
        graph_commit: &GraphCommitRecord,
        graph_changes: &[GraphChangeRecord],
        manifest: &GraphManifest,
    ) -> Result<()> {
        let db_dir = db_dir.to_path_buf();
        let graph_commit = graph_commit.clone();
        let graph_changes = graph_changes.to_vec();
        let manifest = manifest.clone();
        run_namespace_commit_task("commit v4 graph update", move || async move {
            let bundle =
                build_graph_update_bundle_async(&db_dir, &graph_commit, &graph_changes, &manifest)
                    .await?;
            GraphCommitBundlePublisher
                .publish_bundle_async(&db_dir, &bundle)
                .await
        })
    }
}

pub(crate) fn commit_with_namespace_adapter(
    db_dir: &Path,
    graph_commit: &GraphCommitRecord,
    graph_changes: &[GraphChangeRecord],
    manifest: &GraphManifest,
) -> Result<()> {
    ExplicitGraphCommitBundleNamespaceCommitAdapter.commit_graph_update(
        db_dir,
        graph_commit,
        graph_changes,
        manifest,
    )
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn build_graph_update_bundle(
    db_dir: &Path,
    graph_commit: &GraphCommitRecord,
    graph_changes: &[GraphChangeRecord],
    manifest: &GraphManifest,
) -> Result<GraphCommitBundle> {
    let db_dir = db_dir.to_path_buf();
    let graph_commit = graph_commit.clone();
    let graph_changes = graph_changes.to_vec();
    let manifest = manifest.clone();
    run_namespace_commit_task("stage v4 graph update bundle", move || async move {
        build_graph_update_bundle_async(&db_dir, &graph_commit, &graph_changes, &manifest).await
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn publish_graph_commit_bundle(db_dir: &Path, bundle: &GraphCommitBundle) -> Result<()> {
    let db_dir = db_dir.to_path_buf();
    let bundle = bundle.clone();
    run_namespace_commit_task("publish v4 graph commit bundle", move || async move {
        GraphCommitBundlePublisher
            .publish_bundle_async(&db_dir, &bundle)
            .await
    })
}

pub(crate) fn publish_snapshot_bundle(db_dir: &Path, snapshot: &GraphManifest) -> Result<()> {
    let db_dir = db_dir.to_path_buf();
    let snapshot = snapshot.clone();
    run_namespace_commit_task("publish v4 snapshot bundle", move || async move {
        let bundle = build_snapshot_bundle_async(&db_dir, &snapshot).await?;
        GraphCommitBundlePublisher
            .publish_bundle_async(&db_dir, &bundle)
            .await
    })
}

pub(crate) fn publish_snapshot_bundle_with_staged_entries(
    db_dir: &Path,
    snapshot: &GraphManifest,
    staged_entries: &[StagedNamespaceTable],
) -> Result<()> {
    let db_dir = db_dir.to_path_buf();
    let snapshot = snapshot.clone();
    let staged_entries = staged_entries.to_vec();
    run_namespace_commit_task("publish v4 snapshot bundle with staged entries", move || async move {
        let bundle =
            build_snapshot_bundle_with_staged_entries_async(&db_dir, &snapshot, &staged_entries)
                .await?;
        GraphCommitBundlePublisher
            .publish_bundle_async(&db_dir, &bundle)
            .await
    })
}

async fn build_graph_update_bundle_async(
    db_dir: &Path,
    graph_commit: &GraphCommitRecord,
    graph_changes: &[GraphChangeRecord],
    manifest: &GraphManifest,
) -> Result<GraphCommitBundle> {
    let mut next_snapshot = manifest.clone();
    let mut staged_entries = Vec::new();
    let resolved_changes = resolve_graph_change_row_ids(db_dir, manifest, graph_changes).await?;

    if !resolved_changes.is_empty() {
        staged_entries.push(
            stage_graph_change_records(
                db_dir,
                &next_snapshot,
                graph_commit.graph_version.value(),
                graph_commit.tx_id.as_str(),
                &graph_commit.op_summary,
                &resolved_changes,
            )
            .await?,
        );
    }

    staged_entries.push(stage_graph_commit_record(db_dir, &next_snapshot, graph_commit).await?);
    replace_staged_graph_log_entries(&mut next_snapshot, &staged_entries);

    build_snapshot_bundle_with_staged_entries_async(db_dir, &next_snapshot, &staged_entries).await
}

async fn resolve_graph_change_row_ids(
    db_dir: &Path,
    snapshot: &GraphManifest,
    graph_changes: &[GraphChangeRecord],
) -> Result<Vec<GraphChangeRecord>> {
    if graph_changes.is_empty() {
        return Ok(Vec::new());
    }

    let mut ids_by_table = HashMap::<String, BTreeSet<u64>>::new();
    for record in graph_changes {
        if !matches!(record.op.as_str(), "insert" | "update") {
            continue;
        }
        let Some(entity_id) = graph_change_entity_id(record) else {
            continue;
        };
        let Some(entry) = snapshot
            .datasets
            .iter()
            .find(|entry| entry.kind == record.entity_kind && entry.type_name == record.type_name)
        else {
            continue;
        };
        ids_by_table
            .entry(entry.effective_table_id().to_string())
            .or_default()
            .insert(entity_id);
    }

    if ids_by_table.is_empty() {
        return Ok(graph_changes.to_vec());
    }

    let mut rowids_by_table_and_entity = HashMap::<(String, u64), u64>::new();
    for (table_id, entity_ids) in ids_by_table {
        let Some(entry) = snapshot
            .datasets
            .iter()
            .find(|entry| entry.effective_table_id() == table_id)
        else {
            continue;
        };
        let resolved = resolve_row_ids_for_table(db_dir, entry, &entity_ids).await?;
        for (entity_id, row_id) in resolved {
            rowids_by_table_and_entity.insert((table_id.clone(), entity_id), row_id);
        }
    }

    Ok(graph_changes
        .iter()
        .cloned()
        .map(|mut record| {
            if let Some(entity_id) = graph_change_entity_id(&record)
                && let Some(entry) = snapshot.datasets.iter().find(|entry| {
                    entry.kind == record.entity_kind && entry.type_name == record.type_name
                })
            {
                record.rowid_if_known = rowids_by_table_and_entity
                    .get(&(entry.effective_table_id().to_string(), entity_id))
                    .copied();
            }
            record
        })
        .collect())
}

fn graph_change_entity_id(record: &GraphChangeRecord) -> Option<u64> {
    let key = record.entity_key.strip_prefix("id=")?;
    let value = key.split(',').next()?;
    value.parse::<u64>().ok()
}

async fn resolve_row_ids_for_table(
    db_dir: &Path,
    entry: &DatasetEntry,
    entity_ids: &BTreeSet<u64>,
) -> Result<HashMap<u64, u64>> {
    if entity_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let dataset_uri = db_dir
        .join(&entry.dataset_path)
        .to_string_lossy()
        .to_string();
    let dataset = Dataset::open(&dataset_uri)
        .await
        .map_err(|err| {
            NanoError::Lance(format!(
                "open dataset for row-id resolution {} error: {}",
                entry.effective_table_id(),
                err
            ))
        })?
        .checkout_version(entry.dataset_version)
        .await
        .map_err(|err| {
            NanoError::Lance(format!(
                "checkout dataset for row-id resolution {} version {} error: {}",
                entry.effective_table_id(),
                entry.dataset_version,
                err
            ))
        })?;

    let filter = entity_ids
        .iter()
        .map(|id| format!("{LANCE_INTERNAL_ID_FIELD} = {id}"))
        .collect::<Vec<_>>()
        .join(" OR ");

    let mut scanner = dataset.scan();
    scanner.with_row_id();
    scanner
        .project(&[LANCE_INTERNAL_ID_FIELD, "_rowid"])
        .map_err(|err| {
            NanoError::Lance(format!(
                "project row-id resolution scan {} error: {}",
                entry.effective_table_id(),
                err
            ))
        })?;
    scanner.filter(&filter).map_err(|err| {
        NanoError::Lance(format!(
            "filter row-id resolution scan {} error: {}",
            entry.effective_table_id(),
            err
        ))
    })?;

    let batches = scanner
        .try_into_stream()
        .await
        .map_err(|err| {
            NanoError::Lance(format!(
                "scan row-id resolution {} error: {}",
                entry.effective_table_id(),
                err
            ))
        })?
        .collect::<Vec<_>>()
        .await;

    let mut resolved = HashMap::new();
    for batch in batches {
        let batch = batch.map_err(|err| {
            NanoError::Lance(format!(
                "row-id resolution stream {} error: {}",
                entry.effective_table_id(),
                err
            ))
        })?;
        let ids = batch
            .column_by_name(LANCE_INTERNAL_ID_FIELD)
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "row-id resolution batch missing {} column",
                    LANCE_INTERNAL_ID_FIELD
                ))
            })?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "row-id resolution {} column is not UInt64",
                    LANCE_INTERNAL_ID_FIELD
                ))
            })?;
        let rowids = batch
            .column_by_name("_rowid")
            .ok_or_else(|| {
                NanoError::Storage("row-id resolution batch missing _rowid column".to_string())
            })?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| {
                NanoError::Storage("row-id resolution _rowid column is not UInt64".to_string())
            })?;
        for row in 0..batch.num_rows() {
            if ids.is_null(row) || rowids.is_null(row) {
                continue;
            }
            resolved.insert(ids.value(row), rowids.value(row));
        }
    }

    Ok(resolved)
}

async fn build_snapshot_bundle_async(
    db_dir: &Path,
    snapshot: &GraphManifest,
) -> Result<GraphCommitBundle> {
    build_snapshot_bundle_with_staged_entries_async(db_dir, snapshot, &[]).await
}

async fn build_snapshot_bundle_with_staged_entries_async(
    db_dir: &Path,
    snapshot: &GraphManifest,
    staged_entries: &[StagedNamespaceTable],
) -> Result<GraphCommitBundle> {
    let staged_snapshot = stage_graph_snapshot_entry(db_dir, snapshot).await?;
    let mut staged_entries_by_id = HashMap::new();
    for staged in staged_entries {
        staged_entries_by_id.insert(
            staged.entry.effective_table_id().to_string(),
            staged.clone(),
        );
    }

    let mut published_versions = Vec::new();
    for entry in &snapshot.datasets {
        let table_id = entry.effective_table_id();
        if let Some(staged) = staged_entries_by_id.get(table_id) {
            published_versions.push(staged.published_version.clone());
            continue;
        }
        if let Some(version) =
            namespace_published_version_for_table(db_dir, table_id, entry.dataset_version).await?
        {
            published_versions.push(version);
        }
    }
    published_versions.push(staged_snapshot.published_version.clone());
    dedup_namespace_published_versions(&mut published_versions);

    let mut internal_entries = snapshot
        .datasets
        .iter()
        .filter(|entry| {
            matches!(
                entry.effective_table_id(),
                GRAPH_TX_TABLE_ID | GRAPH_CHANGES_TABLE_ID
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    internal_entries.push(staged_snapshot.entry.clone());
    internal_entries.sort_by(|a, b| a.effective_table_id().cmp(b.effective_table_id()));

    let bundle = GraphCommitBundle {
        next_snapshot: snapshot.clone(),
        published_versions,
        internal_entries,
    };
    bundle.validate()?;
    Ok(bundle)
}

fn replace_staged_graph_log_entries(
    snapshot: &mut GraphManifest,
    staged_entries: &[StagedNamespaceTable],
) {
    let replaced_ids = staged_entries
        .iter()
        .map(|staged| staged.entry.effective_table_id().to_string())
        .collect::<BTreeSet<_>>();
    snapshot
        .datasets
        .retain(|entry| !replaced_ids.contains(entry.effective_table_id()));
    snapshot
        .datasets
        .extend(staged_entries.iter().map(|staged| staged.entry.clone()));
}

async fn stage_graph_snapshot_entry(
    db_dir: &Path,
    snapshot: &GraphManifest,
) -> Result<StagedNamespaceTable> {
    let namespace = open_directory_namespace(db_dir).await?;
    let location =
        resolve_or_declare_table_location(namespace.clone(), GRAPH_SNAPSHOT_TABLE_ID).await?;
    let payload = serde_json::to_string(snapshot).map_err(|err| {
        NanoError::Storage(format!("serialize namespace graph snapshot error: {}", err))
    })?;
    let batch = RecordBatch::try_new(
        graph_snapshot_schema(),
        vec![
            Arc::new(UInt64Array::from(vec![snapshot.db_version])),
            Arc::new(StringArray::from(vec![snapshot.last_tx_id.clone()])),
            Arc::new(StringArray::from(vec![snapshot.committed_at.clone()])),
            Arc::new(StringArray::from(vec![payload])),
        ],
    )
    .map_err(|err| {
        NanoError::Storage(format!(
            "build namespace graph snapshot batch error: {}",
            err
        ))
    })?;
    let location_path = normalize_namespace_location_path(db_dir, &location)?;
    match namespace_latest_version(namespace.clone(), GRAPH_SNAPSHOT_TABLE_ID).await {
        Ok(published) => {
            let versioned =
                append_lance_batch_at_version(&location_path, &published, batch).await?;
            let previous_rows = row_count_for_version(db_dir, &location, published.version).await?;
            staged_snapshot_entry(
                db_dir,
                &location,
                versioned.version,
                previous_rows.saturating_add(1),
            )
            .await
        }
        Err(_) => {
            let versioned =
                write_lance_batch_with_mode_versioned(&location_path, batch, WriteMode::Overwrite)
                    .await?;
            staged_snapshot_entry(db_dir, &location, versioned.version, 1).await
        }
    }
}

async fn staged_snapshot_entry(
    db_dir: &Path,
    location: &str,
    version: u64,
    row_count: u64,
) -> Result<StagedNamespaceTable> {
    let entry = DatasetEntry::internal(
        GRAPH_SNAPSHOT_TABLE_ID,
        manifest_dataset_path(db_dir, location, GRAPH_SNAPSHOT_TABLE_ID)?,
        version,
        row_count,
    );
    let published_version =
        namespace_published_version_for_table(db_dir, GRAPH_SNAPSHOT_TABLE_ID, version)
            .await?
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "staged graph snapshot {} version {} is not publishable",
                    GRAPH_SNAPSHOT_TABLE_ID, version
                ))
            })?;
    Ok(StagedNamespaceTable {
        entry,
        published_version,
    })
}

async fn row_count_for_version(db_dir: &Path, location: &str, version: u64) -> Result<u64> {
    let dataset_uri = namespace_location_to_dataset_uri(db_dir, location)?;
    let dataset = lance::Dataset::open(&dataset_uri)
        .await
        .map_err(|err| {
            NanoError::Lance(format!(
                "open snapshot dataset {} error: {}",
                GRAPH_SNAPSHOT_TABLE_ID, err
            ))
        })?
        .checkout_version(version)
        .await
        .map_err(|err| {
            NanoError::Lance(format!(
                "checkout snapshot dataset {} version {} error: {}",
                GRAPH_SNAPSHOT_TABLE_ID, version, err
            ))
        })?;
    dataset
        .count_rows(None)
        .await
        .map(|rows| rows as u64)
        .map_err(|err| {
            NanoError::Lance(format!(
                "count snapshot dataset {} version {} rows error: {}",
                GRAPH_SNAPSHOT_TABLE_ID, version, err
            ))
        })
}

fn normalize_namespace_location_path(db_dir: &Path, location: &str) -> Result<PathBuf> {
    namespace_location_to_local_path(db_dir, location)
}

fn manifest_dataset_path(db_path: &Path, location: &str, fallback: &str) -> Result<String> {
    namespace_location_to_manifest_dataset_path(db_path, location, fallback)
}

fn graph_snapshot_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("graph_version", DataType::UInt64, false),
        Field::new("tx_id", DataType::Utf8, false),
        Field::new("committed_at", DataType::Utf8, false),
        Field::new("manifest_json", DataType::Utf8, false),
    ]))
}

fn remove_legacy_graph_manifest_file(db_dir: &Path) -> Result<()> {
    let path = db_dir.join("graph.manifest.json");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn run_namespace_commit_task<T, Fut, F>(label: &str, work: F) -> Result<T>
where
    T: Send + 'static,
    Fut: std::future::Future<Output = Result<T>> + Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
{
    let label = label.to_string();
    let panic_label = label.clone();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| {
                NanoError::Storage(format!("initialize {} runtime error: {}", label, err))
            })?;
        runtime.block_on(work())
    })
    .join()
    .map_err(|_| NanoError::Storage(format!("{} worker thread panicked", panic_label)))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use lance_table::io::commit::ManifestNamingScheme;

    fn dummy_published_version(table_id: &str, version: u64) -> NamespacePublishedVersion {
        NamespacePublishedVersion {
            table_id: table_id.to_string(),
            version,
            manifest_path: format!("_versions/{}.manifest", version),
            manifest_size: Some(1),
            e_tag: None,
            naming_scheme: ManifestNamingScheme::V2,
        }
    }

    fn internal_entry(table_id: &str, version: u64) -> DatasetEntry {
        DatasetEntry::internal(table_id, table_id, version, 1)
    }

    #[test]
    fn bundle_validation_rejects_missing_snapshot_entry() {
        let bundle = GraphCommitBundle {
            next_snapshot: GraphManifest::new("abc".to_string()),
            published_versions: vec![dummy_published_version(GRAPH_TX_TABLE_ID, 1)],
            internal_entries: vec![internal_entry(GRAPH_TX_TABLE_ID, 1)],
        };
        let err = bundle.validate().unwrap_err().to_string();
        assert!(err.contains(GRAPH_SNAPSHOT_TABLE_ID));
    }

    #[test]
    fn bundle_validation_rejects_missing_tx_entry() {
        let bundle = GraphCommitBundle {
            next_snapshot: GraphManifest::new("abc".to_string()),
            published_versions: vec![dummy_published_version(GRAPH_SNAPSHOT_TABLE_ID, 1)],
            internal_entries: vec![internal_entry(GRAPH_SNAPSHOT_TABLE_ID, 1)],
        };
        let err = bundle.validate().unwrap_err().to_string();
        assert!(err.contains(GRAPH_TX_TABLE_ID));
    }

    #[test]
    fn bundle_validation_rejects_duplicate_published_pairs() {
        let bundle = GraphCommitBundle {
            next_snapshot: GraphManifest::new("abc".to_string()),
            published_versions: vec![
                dummy_published_version(GRAPH_TX_TABLE_ID, 1),
                dummy_published_version(GRAPH_TX_TABLE_ID, 1),
                dummy_published_version(GRAPH_SNAPSHOT_TABLE_ID, 2),
            ],
            internal_entries: vec![
                internal_entry(GRAPH_TX_TABLE_ID, 1),
                internal_entry(GRAPH_SNAPSHOT_TABLE_ID, 2),
            ],
        };
        let err = bundle.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate publish entry"));
    }
}
