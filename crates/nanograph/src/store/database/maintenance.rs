use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{Array, RecordBatch, StringArray, UInt32Array, UInt64Array};
use arrow_schema::DataType;
use futures::TryStreamExt;
use lance::Dataset;
use lance::dataset::cleanup::CleanupPolicyBuilder;
use lance::dataset::optimize::{CompactionOptions as LanceCompactionOptions, compact_files};
use lance::dataset::scanner::DatasetRecordBatchStream;
use tracing::warn;

use super::persist::{read_sparse_edge_batch, read_sparse_node_batch};
use super::{
    CDC_ANALYTICS_DATASET_DIR, CDC_ANALYTICS_STATE_FILE, CdcAnalyticsMaterializeOptions,
    CdcAnalyticsMaterializeResult, CdcAnalyticsState, CleanupOptions, CleanupResult,
    CompactOptions, CompactResult, Database, DoctorLineageShadowReport,
    DoctorLineageShadowWindowReport, DoctorReport, now_unix_seconds_string,
};
use crate::catalog::schema_ir::SchemaIR;
use crate::error::{NanoError, Result};
use crate::store::graph_mirror::{inspect_graph_mirror, rebuild_graph_mirror_from_wal};
use crate::store::graph_types::{GraphChangeRecord, GraphCommitRecord, GraphDeleteRecord, GraphTouchedTableWindow};
use crate::store::lance_io::{
    LANCE_INTERNAL_ID_FIELD, open_dataset_for_locator, write_lance_batch,
};
use crate::store::manifest::GraphManifest;
use crate::store::metadata::{DatabaseMetadata, DatasetLocator};
use crate::store::namespace::{
    BLOB_STORE_TABLE_ID, GRAPH_CHANGES_TABLE_ID, GRAPH_DELETES_TABLE_ID, GRAPH_SNAPSHOT_TABLE_ID,
    GRAPH_TX_TABLE_ID,
    cleanup_namespace_orphan_versions, open_directory_namespace, resolve_table_location,
};
use crate::store::namespace_commit::publish_snapshot_bundle_with_staged_entries;
use crate::store::namespace_lineage_graph_log::{
    rewrite_graph_delete_records, rewrite_namespace_lineage_graph_commit_records,
};
use crate::store::snapshot::{graph_snapshot_table_present, read_committed_graph_snapshot};
use crate::store::storage_generation::{StorageGeneration, detect_storage_generation};
use crate::store::txlog::{
    CdcLogEntry, collect_visible_lineage_shadow_cdc_entries, commit_manifest_and_logs,
    commit_graph_records_and_manifest_namespace_lineage, prune_logs_for_replay_window,
    read_cdc_log_entries, read_tx_catalog_entries, read_visible_change_rows,
    read_visible_graph_delete_records_namespace_lineage,
    read_visible_cdc_entries, read_visible_graph_change_records, read_visible_graph_commit_records,
    reconcile_logs_to_manifest,
};
use crate::store::v4_graph_log::{rewrite_graph_change_records, rewrite_graph_commit_records};

/// Compact all manifest-tracked Lance datasets and commit updated dataset versions.
pub async fn compact_database(db_path: &Path, options: CompactOptions) -> Result<CompactResult> {
    let storage_generation = detect_storage_generation(db_path)?;
    let previous_manifest = read_committed_graph_snapshot(db_path)?;
    if matches!(
        storage_generation,
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage)
    ) {
        cleanup_namespace_orphan_versions(db_path, &previous_manifest).await?;
    }
    reconcile_logs_to_manifest(db_path, previous_manifest.db_version)?;
    let mut next_manifest = previous_manifest.clone();
    let mut result = CompactResult {
        datasets_considered: next_manifest.datasets.len(),
        ..Default::default()
    };

    for entry in &mut next_manifest.datasets {
        let dataset_path = db_path.join(&entry.dataset_path);
        let uri = dataset_path.to_string_lossy().to_string();
        let dataset = Dataset::open(&uri)
            .await
            .map_err(|e| NanoError::Lance(format!("open error: {}", e)))?;
        let mut dataset = dataset
            .checkout_version(entry.dataset_version)
            .await
            .map_err(|e| {
                NanoError::Lance(format!(
                    "checkout version {} error: {}",
                    entry.dataset_version, e
                ))
            })?;
        let before_version = dataset.version().version;

        let compact_opts = LanceCompactionOptions {
            target_rows_per_fragment: options.target_rows_per_fragment,
            materialize_deletions: options.materialize_deletions,
            materialize_deletions_threshold: options.materialize_deletions_threshold,
            ..Default::default()
        };

        let metrics = compact_files(&mut dataset, compact_opts, None)
            .await
            .map_err(|e| NanoError::Lance(format!("compact error: {}", e)))?;
        result.fragments_removed += metrics.fragments_removed;
        result.fragments_added += metrics.fragments_added;
        result.files_removed += metrics.files_removed;
        result.files_added += metrics.files_added;

        let after_version = dataset.version().version;
        if after_version != before_version {
            entry.dataset_version = after_version;
            result.datasets_compacted += 1;
        }
    }

    if result.datasets_compacted > 0 {
        next_manifest.db_version = previous_manifest.db_version.saturating_add(1);
        next_manifest.last_tx_id = format!("manifest-{}", next_manifest.db_version);
        next_manifest.committed_at = now_unix_seconds_string();
        match storage_generation {
            Some(StorageGeneration::NamespaceLineage) => {
                let graph_commit = GraphCommitRecord {
                    tx_id: next_manifest.last_tx_id.clone().into(),
                    graph_version: next_manifest.db_version.into(),
                    table_versions: next_manifest
                        .datasets
                        .iter()
                        .map(|entry| {
                            crate::store::graph_types::GraphTableVersion::new(
                                entry.effective_table_id().to_string(),
                                entry.dataset_version,
                            )
                        })
                        .collect(),
                    committed_at: next_manifest.committed_at.clone(),
                    op_summary: "maintenance:compact".to_string(),
                    schema_identity_version: next_manifest.schema_identity_version,
                    touched_tables: build_namespace_lineage_touched_table_windows(
                        &previous_manifest,
                        &next_manifest,
                    ),
                    tx_props: BTreeMap::from([
                        ("graph_version".to_string(), next_manifest.db_version.to_string()),
                        ("tx_id".to_string(), next_manifest.last_tx_id.clone()),
                        ("op_summary".to_string(), "maintenance:compact".to_string()),
                    ]),
                };
                commit_graph_records_and_manifest_namespace_lineage(
                    db_path,
                    &graph_commit,
                    &[],
                    &next_manifest,
                )?;
            }
            _ => {
                commit_manifest_and_logs(db_path, &next_manifest, &[], "maintenance:compact")?;
                if !matches!(storage_generation, Some(StorageGeneration::V4Namespace))
                    && let Err(err) = rebuild_graph_mirror_from_wal(db_path).await
                {
                    warn!(
                        error = %err,
                        db_path = %db_path.display(),
                        "graph mirror rebuild failed after compaction commit"
                    );
                }
            }
        }
        result.manifest_committed = true;
    }

    Ok(result)
}

/// Prune tx/CDC logs and old Lance dataset versions while preserving manifest-visible state.
pub async fn cleanup_database(db_path: &Path, options: CleanupOptions) -> Result<CleanupResult> {
    let storage_generation = detect_storage_generation(db_path)?;
    if options.retain_tx_versions == 0 {
        return Err(NanoError::Storage(
            "retain_tx_versions must be >= 1".to_string(),
        ));
    }
    if matches!(storage_generation, Some(StorageGeneration::NamespaceLineage)) {
        return cleanup_database_namespace_lineage(db_path, options).await;
    }
    if options.retain_dataset_versions == 0 {
        return Err(NanoError::Storage(
            "retain_dataset_versions must be >= 1".to_string(),
        ));
    }

    let manifest = read_committed_graph_snapshot(db_path)?;
    if matches!(storage_generation, Some(StorageGeneration::V4Namespace)) {
        cleanup_namespace_orphan_versions(db_path, &manifest).await?;
    }
    reconcile_logs_to_manifest(db_path, manifest.db_version)?;
    let v4_all_commits = if matches!(storage_generation, Some(StorageGeneration::V4Namespace)) {
        Some(read_visible_graph_commit_records(db_path)?)
    } else {
        None
    };
    let v4_retained_window = v4_all_commits
        .as_ref()
        .map(|commits| select_retained_v4_graph_window(commits, options.retain_tx_versions));
    let v4_lineage_required_versions = v4_all_commits
        .as_ref()
        .map(|commits| compute_v4_lineage_required_versions(&manifest, commits, options.retain_tx_versions));
    let log_prune = prune_logs_for_replay_window(db_path, options.retain_tx_versions)?;
    let mut result = if matches!(storage_generation, Some(StorageGeneration::V4Namespace)) {
        let retained = v4_retained_window.as_ref().expect("v4 retained window");
        let retained_changes =
            read_visible_graph_change_records(
                db_path,
                retained.lower_bound_exclusive,
                Some(manifest.db_version),
            )?;
        CleanupResult {
            tx_rows_removed: read_tx_catalog_entries(db_path)?
                .len()
                .saturating_sub(retained.logical_commits.len()),
            tx_rows_kept: retained.logical_commits.len(),
            cdc_rows_removed: read_visible_graph_change_records(db_path, 0, Some(manifest.db_version))?
                .len()
                .saturating_sub(retained_changes.len()),
            cdc_rows_kept: retained_changes.len(),
            ..Default::default()
        }
    } else {
        CleanupResult {
            tx_rows_removed: log_prune.tx_rows_removed,
            tx_rows_kept: log_prune.tx_rows_kept,
            cdc_rows_removed: log_prune.cdc_rows_removed,
            cdc_rows_kept: log_prune.cdc_rows_kept,
            ..Default::default()
        }
    };

    for entry in &manifest.datasets {
        let dataset_path = db_path.join(&entry.dataset_path);
        let uri = dataset_path.to_string_lossy().to_string();
        let dataset = Dataset::open(&uri)
            .await
            .map_err(|e| NanoError::Lance(format!("open error: {}", e)))?;
        dataset
            .checkout_version(entry.dataset_version)
            .await
            .map_err(|e| {
                NanoError::Lance(format!(
                    "checkout version {} error: {}",
                    entry.dataset_version, e
                ))
            })?;

        let versions = dataset
            .versions()
            .await
            .map_err(|e| NanoError::Lance(format!("list versions error: {}", e)))?;
        let effective_retain_n = versions
            .iter()
            .position(|v| v.version == entry.dataset_version)
            .map(|idx| {
                let needed_for_manifest = versions.len().saturating_sub(idx);
                options.retain_dataset_versions.max(needed_for_manifest)
            })
            .unwrap_or(options.retain_dataset_versions);
        let effective_retain_n = if matches!(storage_generation, Some(StorageGeneration::V4Namespace))
            && matches!(entry.kind.as_str(), "node" | "edge")
        {
            v4_lineage_required_versions
                .as_ref()
                .and_then(|required| required.get(entry.effective_table_id()))
                .and_then(|oldest_required| {
                    versions
                        .iter()
                        .position(|version| version.version == *oldest_required)
                        .map(|idx| versions.len().saturating_sub(idx))
                })
                .map(|needed_for_lineage| effective_retain_n.max(needed_for_lineage))
                .unwrap_or(effective_retain_n)
        } else {
            effective_retain_n
        };
        let policy = CleanupPolicyBuilder::default()
            .retain_n_versions(&dataset, effective_retain_n)
            .await
            .map_err(|e| NanoError::Lance(format!("cleanup policy error: {}", e)))?
            .build();
        let stats = dataset
            .cleanup_with_policy(policy)
            .await
            .map_err(|e| NanoError::Lance(format!("cleanup error: {}", e)))?;
        if stats.old_versions > 0 {
            result.datasets_cleaned += 1;
        }
        result.dataset_old_versions_removed += stats.old_versions;
        result.dataset_bytes_removed += stats.bytes_removed;
    }

    if matches!(storage_generation, Some(StorageGeneration::V4Namespace)) {
        let retained = v4_retained_window.as_ref().expect("v4 retained window");
        let retained_changes =
            read_visible_graph_change_records(
                db_path,
                retained.lower_bound_exclusive,
                Some(manifest.db_version),
            )?;
        let staged_tx = rewrite_graph_commit_records(db_path, &retained.staged_commits).await?;
        let staged_changes = rewrite_graph_change_records(db_path, &retained_changes).await?;
        let mut next_snapshot = manifest.clone();
        let replaced_ids = [GRAPH_TX_TABLE_ID, GRAPH_CHANGES_TABLE_ID]
            .into_iter()
            .collect::<HashSet<_>>();
        next_snapshot
            .datasets
            .retain(|entry| !replaced_ids.contains(entry.effective_table_id()));
        next_snapshot
            .datasets
            .push(staged_tx.entry.clone());
        next_snapshot
            .datasets
            .push(staged_changes.entry.clone());
        publish_snapshot_bundle_with_staged_entries(
            db_path,
            &next_snapshot,
            &[staged_tx, staged_changes],
        )?;
    }

    if !matches!(storage_generation, Some(StorageGeneration::V4Namespace))
        && let Err(err) = rebuild_graph_mirror_from_wal(db_path).await
    {
        warn!(
            error = %err,
            db_path = %db_path.display(),
            "graph mirror rebuild failed after cleanup"
        );
    }

    Ok(result)
}

async fn cleanup_database_namespace_lineage(
    db_path: &Path,
    options: CleanupOptions,
) -> Result<CleanupResult> {
    let manifest = read_committed_graph_snapshot(db_path)?;
    cleanup_namespace_orphan_versions(db_path, &manifest).await?;

    let all_commits = read_visible_graph_commit_records(db_path)?;
    let retained = select_retained_namespace_lineage_graph_window(
        &all_commits,
        options.retain_tx_versions,
    );
    let retained_deletes = read_visible_graph_delete_records_namespace_lineage(
        db_path,
        retained.lower_bound_exclusive,
        Some(manifest.db_version),
    )?;
    let total_change_rows = read_visible_change_rows(db_path, 0, Some(manifest.db_version))?;
    let retained_change_rows = read_visible_change_rows(
        db_path,
        retained.lower_bound_exclusive,
        Some(manifest.db_version),
    )?;
    let required_versions =
        compute_namespace_lineage_required_versions(&retained.logical_commits);

    let mut result = CleanupResult {
        tx_rows_removed: all_commits.len().saturating_sub(retained.logical_commits.len()),
        tx_rows_kept: retained.logical_commits.len(),
        cdc_rows_removed: total_change_rows.len().saturating_sub(retained_change_rows.len()),
        cdc_rows_kept: retained_change_rows.len(),
        ..Default::default()
    };

    for entry in &manifest.datasets {
        let dataset_path = db_path.join(&entry.dataset_path);
        let uri = dataset_path.to_string_lossy().to_string();
        let dataset = Dataset::open(&uri)
            .await
            .map_err(|e| NanoError::Lance(format!("open error: {}", e)))?;
        dataset
            .checkout_version(entry.dataset_version)
            .await
            .map_err(|e| {
                NanoError::Lance(format!(
                    "checkout version {} error: {}",
                    entry.dataset_version, e
                ))
            })?;

        let versions = dataset
            .versions()
            .await
            .map_err(|e| NanoError::Lance(format!("list versions error: {}", e)))?;
        let effective_retain_n = if matches!(entry.kind.as_str(), "node" | "edge") {
            required_versions
                .get(entry.effective_table_id())
                .and_then(|oldest_required| {
                    versions
                        .iter()
                        .position(|version| version.version == *oldest_required)
                        .map(|idx| versions.len().saturating_sub(idx))
                })
                .unwrap_or(1)
        } else {
            1
        };

        let policy = CleanupPolicyBuilder::default()
            .retain_n_versions(&dataset, effective_retain_n.max(1))
            .await
            .map_err(|e| NanoError::Lance(format!("cleanup policy error: {}", e)))?
            .build();
        let stats = dataset
            .cleanup_with_policy(policy)
            .await
            .map_err(|e| NanoError::Lance(format!("cleanup error: {}", e)))?;
        if stats.old_versions > 0 {
            result.datasets_cleaned += 1;
        }
        result.dataset_old_versions_removed += stats.old_versions;
        result.dataset_bytes_removed += stats.bytes_removed;
    }

    let staged_tx = rewrite_namespace_lineage_graph_commit_records(
        db_path,
        &retained.logical_commits,
    )
    .await?;
    let staged_deletes = rewrite_graph_delete_records(db_path, &retained_deletes).await?;
    let mut next_snapshot = manifest.clone();
    let replaced_ids = [GRAPH_TX_TABLE_ID, GRAPH_DELETES_TABLE_ID]
        .into_iter()
        .collect::<HashSet<_>>();
    next_snapshot
        .datasets
        .retain(|entry| !replaced_ids.contains(entry.effective_table_id()));
    next_snapshot.datasets.push(staged_tx.entry.clone());
    next_snapshot.datasets.push(staged_deletes.entry.clone());
    publish_snapshot_bundle_with_staged_entries(
        db_path,
        &next_snapshot,
        &[staged_tx, staged_deletes],
    )?;
    cleanup_namespace_orphan_versions(db_path, &next_snapshot).await?;

    Ok(result)
}

struct RetainedV4GraphWindow {
    staged_commits: Vec<GraphCommitRecord>,
    logical_commits: Vec<GraphCommitRecord>,
    lower_bound_exclusive: u64,
}

struct RetainedNamespaceLineageGraphWindow {
    logical_commits: Vec<GraphCommitRecord>,
    lower_bound_exclusive: u64,
}

fn select_retained_v4_graph_window(
    commits: &[GraphCommitRecord],
    retain_tx_versions: u64,
) -> RetainedV4GraphWindow {
    let retain_count = (retain_tx_versions as usize).min(commits.len());
    if retain_count == 0 {
        return RetainedV4GraphWindow {
            staged_commits: Vec::new(),
            logical_commits: Vec::new(),
            lower_bound_exclusive: 0,
        };
    }
    let logical_start = commits.len().saturating_sub(retain_count);
    let logical_commits = commits[logical_start..].to_vec();
    let mut staged_commits = logical_commits.clone();
    if let Some(predecessor) = logical_start.checked_sub(1).and_then(|idx| commits.get(idx)) {
        staged_commits.insert(0, predecessor.clone());
    }
    let lower_bound_exclusive = logical_commits
        .first()
        .map(|commit| commit.graph_version.value().saturating_sub(1))
        .unwrap_or(0);
    RetainedV4GraphWindow {
        staged_commits,
        logical_commits,
        lower_bound_exclusive,
    }
}

fn select_retained_namespace_lineage_graph_window(
    commits: &[GraphCommitRecord],
    retain_tx_versions: u64,
) -> RetainedNamespaceLineageGraphWindow {
    let retain_count = (retain_tx_versions as usize).min(commits.len());
    if retain_count == 0 {
        return RetainedNamespaceLineageGraphWindow {
            logical_commits: Vec::new(),
            lower_bound_exclusive: 0,
        };
    }
    let logical_start = commits.len().saturating_sub(retain_count);
    let logical_commits = commits[logical_start..].to_vec();
    let lower_bound_exclusive = logical_commits
        .first()
        .map(|commit| commit.graph_version.value().saturating_sub(1))
        .unwrap_or(0);
    RetainedNamespaceLineageGraphWindow {
        logical_commits,
        lower_bound_exclusive,
    }
}

fn compute_v4_lineage_required_versions(
    manifest: &GraphManifest,
    commits: &[GraphCommitRecord],
    retain_tx_versions: u64,
) -> BTreeMap<String, u64> {
    let retain_count = (retain_tx_versions as usize).min(commits.len());
    if retain_count == 0 {
        return BTreeMap::new();
    }
    let retain_start = commits.len().saturating_sub(retain_count);
    let predecessor = retain_start.checked_sub(1).and_then(|idx| commits.get(idx));
    let retained_commits = &commits[retain_start..];
    let mut out = BTreeMap::new();

    for entry in manifest
        .datasets
        .iter()
        .filter(|entry| matches!(entry.kind.as_str(), "node" | "edge"))
    {
        let table_id = entry.effective_table_id().to_string();
        let mut previous_version =
            predecessor.and_then(|commit| graph_commit_table_version(commit, &table_id));
        let mut oldest_required = None::<u64>;

        for commit in retained_commits {
            let Some(current_version) = graph_commit_table_version(commit, &table_id) else {
                continue;
            };
            if previous_version != Some(current_version) {
                oldest_required = Some(
                    oldest_required
                        .map(|version| version.min(current_version))
                        .unwrap_or(current_version),
                );
                if let Some(previous_version) = previous_version {
                    oldest_required = Some(
                        oldest_required
                            .map(|version| version.min(previous_version))
                            .unwrap_or(previous_version),
                    );
                }
            }
            previous_version = Some(current_version);
        }

        if let Some(oldest_required) = oldest_required {
            out.insert(table_id, oldest_required);
        }
    }

    out
}

fn compute_namespace_lineage_required_versions(
    commits: &[GraphCommitRecord],
) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for commit in commits {
        for window in &commit.touched_tables {
            if window.before_version > 0 {
                out.entry(window.table_id.as_str().to_string())
                    .and_modify(|version: &mut u64| *version = (*version).min(window.before_version))
                    .or_insert(window.before_version);
            }
            if window.after_version > 0 {
                out.entry(window.table_id.as_str().to_string())
                    .and_modify(|version: &mut u64| *version = (*version).min(window.after_version))
                    .or_insert(window.after_version);
            }
        }
    }
    out
}

fn build_namespace_lineage_touched_table_windows(
    previous_manifest: &GraphManifest,
    next_manifest: &GraphManifest,
) -> Vec<GraphTouchedTableWindow> {
    let previous_by_key = previous_manifest
        .datasets
        .iter()
        .filter(|entry| matches!(entry.kind.as_str(), "node" | "edge"))
        .map(|entry| (format!("{}:{}", entry.kind, entry.type_name), entry))
        .collect::<HashMap<_, _>>();
    let mut windows = next_manifest
        .datasets
        .iter()
        .filter(|entry| matches!(entry.kind.as_str(), "node" | "edge"))
        .filter_map(|entry| {
            let previous = previous_by_key.get(&format!("{}:{}", entry.kind, entry.type_name));
            let before_version = previous.map(|entry| entry.dataset_version).unwrap_or(0);
            if before_version == entry.dataset_version {
                return None;
            }
            Some(GraphTouchedTableWindow::new(
                entry.effective_table_id().to_string(),
                entry.kind.clone(),
                entry.type_name.clone(),
                before_version,
                entry.dataset_version,
            ))
        })
        .collect::<Vec<_>>();
    windows.sort_by(|a, b| {
        a.entity_kind
            .cmp(&b.entity_kind)
            .then(a.type_name.cmp(&b.type_name))
            .then(a.table_id.as_str().cmp(b.table_id.as_str()))
    });
    windows
}

fn graph_commit_table_version(commit: &GraphCommitRecord, table_id: &str) -> Option<u64> {
    commit
        .table_versions
        .iter()
        .find(|version| version.table_id.as_str() == table_id)
        .map(|version| version.version)
}

impl Database {
    pub async fn compact(&self, options: CompactOptions) -> Result<CompactResult> {
        let _writer = self.lock_writer().await;
        compact_database(&self.path, options).await
    }

    pub async fn cleanup(&self, options: CleanupOptions) -> Result<CleanupResult> {
        let _writer = self.lock_writer().await;
        cleanup_database(&self.path, options).await
    }

    /// Materialize visible CDC rows into a derived Lance dataset for analytics workloads.
    ///
    /// JSONL remains the authoritative CDC source; this dataset is best-effort acceleration.
    pub async fn materialize_cdc_analytics(
        &self,
        options: CdcAnalyticsMaterializeOptions,
    ) -> Result<CdcAnalyticsMaterializeResult> {
        let manifest = read_committed_graph_snapshot(&self.path)?;
        reconcile_logs_to_manifest(&self.path, manifest.db_version)?;

        let rows = read_visible_cdc_entries(&self.path, 0, Some(manifest.db_version))?;
        let source_rows = rows.len();
        let state = read_cdc_analytics_state(&self.path)?;
        let previous_rows = state.rows_materialized;
        let shrank = source_rows < previous_rows || manifest.db_version < state.manifest_db_version;
        let new_rows_since_last_run = if shrank {
            source_rows
        } else {
            source_rows.saturating_sub(previous_rows)
        };

        if !options.force
            && !shrank
            && options.min_new_rows > 0
            && new_rows_since_last_run < options.min_new_rows
        {
            return Ok(CdcAnalyticsMaterializeResult {
                source_rows,
                previously_materialized_rows: previous_rows,
                new_rows_since_last_run,
                materialized_rows: previous_rows.min(source_rows),
                dataset_written: false,
                skipped_by_threshold: true,
                dataset_version: state.dataset_version,
            });
        }

        let dataset_path = cdc_analytics_dataset_path(&self.path);
        let mut dataset_written = false;
        let dataset_version = if rows.is_empty() {
            if dataset_path.exists() {
                std::fs::remove_dir_all(&dataset_path)?;
                dataset_written = true;
            }
            None
        } else {
            let batch = cdc_rows_to_analytics_batch(&rows)?;
            let version = write_lance_batch(&dataset_path, batch).await?;
            dataset_written = true;
            Some(version)
        };

        write_cdc_analytics_state(
            &self.path,
            &CdcAnalyticsState {
                rows_materialized: source_rows,
                manifest_db_version: manifest.db_version,
                dataset_version,
                updated_at_unix: now_unix_seconds_string(),
            },
        )?;

        Ok(CdcAnalyticsMaterializeResult {
            source_rows,
            previously_materialized_rows: previous_rows,
            new_rows_since_last_run,
            materialized_rows: source_rows,
            dataset_written,
            skipped_by_threshold: false,
            dataset_version,
        })
    }

    /// Validate manifest/log/dataset consistency and in-memory graph integrity.
    pub async fn doctor(&self) -> Result<DoctorReport> {
        let storage_generation = detect_storage_generation(&self.path)?;
        let manifest = read_committed_graph_snapshot(&self.path)?;
        reconcile_logs_to_manifest(&self.path, manifest.db_version)?;
        let metadata = DatabaseMetadata::open(&self.path)?;
        let mut issues = Vec::new();
        let mut warnings = Vec::new();
        let mut datasets = Vec::new();
        let mut lineage_shadow = None;

        let tx_rows = read_tx_catalog_entries(&self.path)?;
        for (idx, window) in tx_rows.windows(2).enumerate() {
            if window[0].db_version >= window[1].db_version {
                issues.push(format!(
                    "non-monotonic tx db_version at rows {} and {}",
                    idx + 1,
                    idx + 2
                ));
            }
        }
        if let Some(last) = tx_rows.last() {
            if last.db_version > manifest.db_version {
                issues.push(format!(
                    "tx catalog db_version {} exceeds manifest db_version {}",
                    last.db_version, manifest.db_version
                ));
            } else if last.db_version < manifest.db_version {
                warnings.push(format!(
                    "tx catalog trimmed to db_version {} while manifest is {}",
                    last.db_version, manifest.db_version
                ));
            }
        }

        let mut datasets_checked = 0usize;
        for entry in &manifest.datasets {
            let dataset_path = self.path.join(&entry.dataset_path);
            if !dataset_path.exists() {
                issues.push(format!("dataset path missing: {}", dataset_path.display()));
                continue;
            }
            let uri = dataset_path.to_string_lossy().to_string();
            match Dataset::open(&uri).await {
                Ok(dataset) => match dataset.checkout_version(entry.dataset_version).await {
                    Ok(dataset) => {
                        datasets_checked += 1;
                        let storage_version = dataset
                            .manifest
                            .data_storage_format
                            .lance_file_version()
                            .map(|version| version.to_string())
                            .unwrap_or_else(|_| "unknown".to_string());
                        datasets.push(crate::store::database::DoctorDatasetReport {
                            kind: entry.kind.clone(),
                            type_name: entry.type_name.clone(),
                            dataset_path: entry.dataset_path.clone(),
                            dataset_version: entry.dataset_version,
                            storage_version,
                        });
                    }
                    Err(e) => {
                        issues.push(format!(
                            "dataset {} missing pinned version {}: {}",
                            entry.dataset_path, entry.dataset_version, e
                        ));
                    }
                },
                Err(e) => {
                    issues.push(format!(
                        "failed to open dataset {}: {}",
                        entry.dataset_path, e
                    ));
                }
            }
        }

        for edge_def in self.schema_ir.edge_types() {
            let src_nodes = collect_existing_ids(
                read_sparse_node_batch(&metadata, &edge_def.src_type_name).await?,
            )?;
            let dst_nodes = collect_existing_ids(
                read_sparse_node_batch(&metadata, &edge_def.dst_type_name).await?,
            )?;
            if let Some(edge_batch) = read_sparse_edge_batch(&metadata, &edge_def.name).await? {
                let src_arr = edge_batch
                    .column_by_name("src")
                    .ok_or_else(|| NanoError::Storage("edge batch missing src column".to_string()))?
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or_else(|| {
                        NanoError::Storage("edge src column is not UInt64".to_string())
                    })?;
                let dst_arr = edge_batch
                    .column_by_name("dst")
                    .ok_or_else(|| NanoError::Storage("edge batch missing dst column".to_string()))?
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or_else(|| {
                        NanoError::Storage("edge dst column is not UInt64".to_string())
                    })?;
                let mut orphan_count = 0usize;
                for row in 0..edge_batch.num_rows() {
                    if !src_nodes.contains(&src_arr.value(row))
                        || !dst_nodes.contains(&dst_arr.value(row))
                    {
                        orphan_count += 1;
                    }
                }
                if orphan_count > 0 {
                    issues.push(format!(
                        "edge type {} has {} orphan endpoint row(s)",
                        edge_def.name, orphan_count
                    ));
                }
            }
        }

        if matches!(storage_generation, Some(StorageGeneration::NamespaceLineage)) {
            let namespace = open_directory_namespace(&self.path).await?;
            for required in [
                GRAPH_TX_TABLE_ID,
                GRAPH_DELETES_TABLE_ID,
                BLOB_STORE_TABLE_ID,
                GRAPH_SNAPSHOT_TABLE_ID,
            ] {
                if let Err(err) = resolve_table_location(namespace.clone(), required).await {
                    warnings.push(format!(
                        "namespace-lineage internal table {} is missing: {}",
                        required, err
                    ));
                }
            }
            if !graph_snapshot_table_present(&self.path).await? {
                warnings.push("namespace-lineage graph snapshot table is missing".to_string());
            }
            validate_namespace_lineage_state(&metadata, &manifest, &mut issues, &mut warnings)
                .await?;
        } else if matches!(storage_generation, Some(StorageGeneration::V4Namespace)) {
            let namespace = open_directory_namespace(&self.path).await?;
            for required in [
                GRAPH_TX_TABLE_ID,
                GRAPH_CHANGES_TABLE_ID,
                BLOB_STORE_TABLE_ID,
                GRAPH_SNAPSHOT_TABLE_ID,
            ] {
                if let Err(err) = resolve_table_location(namespace.clone(), required).await {
                    warnings.push(format!(
                        "v4 internal table {} is missing: {}",
                        required, err
                    ));
                }
            }
            if !graph_snapshot_table_present(&self.path).await? {
                warnings.push("v4 graph snapshot table is missing".to_string());
            }
            lineage_shadow = Some(
                validate_v4_lineage_state(&metadata, &manifest, &mut issues, &mut warnings).await?,
            );
        } else {
            match inspect_graph_mirror(&self.path).await {
                Ok(status) => {
                    if manifest.db_version > 0
                        && (!status.commits_present || !status.changes_present)
                    {
                        warnings.push(
                            "graph mirror tables are missing or empty; rebuildable from authoritative WAL"
                                .to_string(),
                        );
                    } else {
                        if let Some(mirrored) = status.latest_commit_version
                            && mirrored < manifest.db_version
                        {
                            warnings.push(format!(
                                "graph commit mirror is stale at db_version {} while manifest is {}",
                                mirrored, manifest.db_version
                            ));
                        }
                        if let Some(mirrored) = status.latest_change_version
                            && mirrored < manifest.db_version
                        {
                            warnings.push(format!(
                                "graph change mirror is stale at db_version {} while manifest is {}",
                                mirrored, manifest.db_version
                            ));
                        }
                    }
                }
                Err(err) => warnings.push(format!(
                    "graph mirror tables are unreadable and should be rebuilt from WAL: {}",
                    err
                )),
            }
        }

        let cdc_rows = match storage_generation {
            Some(StorageGeneration::NamespaceLineage) => match read_visible_change_rows(
                &self.path,
                0,
                Some(manifest.db_version),
            ) {
                Ok(rows) => rows.len(),
                Err(err) => {
                    issues.push(format!("NamespaceLineage CDC read failed: {}", err));
                    0
                }
            },
            _ => match read_cdc_log_entries(&self.path) {
                Ok(rows) => rows.len(),
                Err(err) if matches!(storage_generation, Some(StorageGeneration::V4Namespace)) => {
                    issues.push(format!("v4 lineage-backed CDC read failed: {}", err));
                    read_visible_graph_change_records(&self.path, 0, Some(manifest.db_version))
                        .map(|rows| rows.len())
                        .unwrap_or(0)
                }
                Err(err) => return Err(err),
            },
        };
        let healthy = issues.is_empty();
        Ok(DoctorReport {
            healthy,
            issues,
            warnings,
            manifest_db_version: manifest.db_version,
            datasets_checked,
            datasets,
            tx_rows: tx_rows.len(),
            cdc_rows,
            lineage_shadow,
        })
    }
}

fn collect_existing_ids(batch: Option<RecordBatch>) -> Result<HashSet<u64>> {
    let mut ids = HashSet::new();
    let Some(batch) = batch else {
        return Ok(ids);
    };
    let id_arr = batch
        .column_by_name("id")
        .ok_or_else(|| NanoError::Storage("batch missing id column".to_string()))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| NanoError::Storage("id column is not UInt64".to_string()))?;
    for row in 0..batch.num_rows() {
        ids.insert(id_arr.value(row));
    }
    Ok(ids)
}

#[derive(Debug, Clone, Copy)]
struct LiveRowLineage {
    rowid: u64,
    created_at_version: u64,
    last_updated_at_version: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LineageChangeSet {
    inserts: BTreeSet<(u64, u64)>,
    updates: BTreeSet<(u64, u64)>,
}

#[derive(Debug, Clone, Default)]
struct ExpectedLineageChanges {
    inserts: BTreeSet<(u64, u64)>,
    updates: BTreeSet<(u64, u64)>,
    missing_rowid_entities: Vec<String>,
}

async fn validate_namespace_lineage_state(
    metadata: &DatabaseMetadata,
    manifest: &GraphManifest,
    issues: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let commits = read_visible_graph_commit_records(metadata.path())?;
    for commit in &commits {
        for window in &commit.touched_tables {
            if window.after_version == 0 {
                issues.push(format!(
                    "namespace-lineage tx {} has invalid touched-table after_version for {} {}",
                    commit.tx_id.as_str(),
                    window.entity_kind,
                    window.type_name
                ));
            }
            if window.before_version > window.after_version && window.after_version > 0 {
                issues.push(format!(
                    "namespace-lineage tx {} has inverted touched-table window {} {} {}>{}",
                    commit.tx_id.as_str(),
                    window.entity_kind,
                    window.type_name,
                    window.before_version,
                    window.after_version
                ));
            }
        }
    }

    let deletes =
        read_visible_graph_delete_records_namespace_lineage(
            metadata.path(),
            0,
            Some(manifest.db_version),
        )?;
    validate_namespace_lineage_delete_records(&deletes, issues, warnings);

    if let Err(err) = read_visible_change_rows(metadata.path(), 0, Some(manifest.db_version)) {
        issues.push(format!(
            "NamespaceLineage CDC reconstruction failed for retained window: {}",
            err
        ));
    }
    Ok(())
}

fn validate_namespace_lineage_delete_records(
    deletes: &[GraphDeleteRecord],
    issues: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let mut seen = BTreeSet::new();
    for row in deletes {
        if row.logical_key.is_empty() {
            issues.push(format!(
                "namespace-lineage delete tombstone is missing logical_key for {} {} id={}",
                row.entity_kind, row.type_name, row.entity_id
            ));
        }
        if !row.row.is_object() {
            issues.push(format!(
                "namespace-lineage delete tombstone row payload is not an object for {} {} id={}",
                row.entity_kind, row.type_name, row.entity_id
            ));
        }
        let key = (
            row.graph_version.value(),
            row.table_id.as_str().to_string(),
            row.rowid,
        );
        if !seen.insert(key) {
            warnings.push(format!(
                "duplicate namespace-lineage delete tombstone for {} {} rowid={} graph_version={}",
                row.entity_kind,
                row.type_name,
                row.rowid,
                row.graph_version.value()
            ));
        }
    }
}

async fn validate_v4_lineage_state(
    metadata: &DatabaseMetadata,
    manifest: &GraphManifest,
    issues: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<DoctorLineageShadowReport> {
    for entry in manifest
        .datasets
        .iter()
        .filter(|entry| matches!(entry.kind.as_str(), "node" | "edge"))
    {
        let Some(locator) = metadata.dataset_locator(&entry.kind, &entry.type_name) else {
            issues.push(format!(
                "missing dataset locator for {} {} while validating v4 lineage",
                entry.kind, entry.type_name
            ));
            continue;
        };
        validate_v4_dataset_lineage_columns(&locator, issues).await?;
    }

    let visible_changes =
        read_visible_graph_change_records(metadata.path(), 0, Some(manifest.db_version))?;
    validate_v4_graph_change_rowids(metadata, &visible_changes, issues, warnings).await?;
    let report = validate_v4_graph_changes_against_lance_lineage(
        metadata,
        manifest,
        &visible_changes,
        issues,
        warnings,
    )
    .await?;
    validate_v4_lineage_shadow_cdc_window(metadata.path(), manifest.db_version, issues, warnings)?;
    Ok(report)
}

fn validate_v4_lineage_shadow_cdc_window(
    db_path: &Path,
    upper_graph_version: u64,
    issues: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let shadow = collect_visible_lineage_shadow_cdc_entries(db_path, 0, Some(upper_graph_version))?;
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
        warnings.push(format!(
            "v4 lineage shadow CDC skipped {} window(s): {}",
            shadow.skipped_windows.len(),
            sample
        ));
    }
    if shadow.shadow_entries == shadow.authoritative_entries_compared {
        return Ok(());
    }

    let detail = first_cdc_shadow_mismatch(
        &shadow.authoritative_entries_compared,
        &shadow.shadow_entries,
    );
    issues.push(format!("v4 lineage shadow CDC mismatch: {}", detail));
    Ok(())
}

fn first_cdc_shadow_mismatch(authoritative: &[CdcLogEntry], shadow: &[CdcLogEntry]) -> String {
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
    if authoritative.len() != shadow.len() {
        return format!(
            "row count mismatch authoritative={} shadow={}",
            authoritative.len(),
            shadow.len()
        );
    }
    format!(
        "authoritative entries={} shadow entries={}",
        authoritative.len(),
        shadow.len()
    )
}

async fn validate_v4_dataset_lineage_columns(
    locator: &DatasetLocator,
    issues: &mut Vec<String>,
) -> Result<()> {
    let dataset = open_dataset_for_locator(locator).await?;
    let mut scanner = dataset.scan();
    if let Err(err) = scanner.project(&[
        LANCE_INTERNAL_ID_FIELD,
        "_rowid",
        "_row_created_at_version",
        "_row_last_updated_at_version",
    ]) {
        issues.push(format!(
            "v4 dataset {} version {} does not expose required lineage columns: {}",
            locator.table_id, locator.dataset_version, err
        ));
        return Ok(());
    }
    scanner.limit(Some(1), None).map_err(|err| {
        NanoError::Lance(format!(
            "limit lineage validation scan {} error: {}",
            locator.table_id, err
        ))
    })?;
    let batch = scanner.try_into_batch().await.map_err(|err| {
        NanoError::Lance(format!(
            "execute lineage validation scan {} error: {}",
            locator.table_id, err
        ))
    })?;
    if batch.column_by_name("_rowid").is_none()
        || batch.column_by_name("_row_created_at_version").is_none()
        || batch
            .column_by_name("_row_last_updated_at_version")
            .is_none()
    {
        issues.push(format!(
            "v4 dataset {} version {} is missing projected lineage columns",
            locator.table_id, locator.dataset_version
        ));
        return Ok(());
    }
    if batch.num_rows() == 0 {
        return Ok(());
    }

    let created = batch
        .column_by_name("_row_created_at_version")
        .ok_or_else(|| {
            NanoError::Storage(
                "lineage validation batch missing _row_created_at_version".to_string(),
            )
        })?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| {
            NanoError::Storage(
                "lineage validation _row_created_at_version column is not UInt64".to_string(),
            )
        })?;
    let updated = batch
        .column_by_name("_row_last_updated_at_version")
        .ok_or_else(|| {
            NanoError::Storage(
                "lineage validation batch missing _row_last_updated_at_version".to_string(),
            )
        })?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| {
            NanoError::Storage(
                "lineage validation _row_last_updated_at_version column is not UInt64".to_string(),
            )
        })?;
    let first_created = created.value(0);
    let first_updated = updated.value(0);
    if first_created == 0 || first_updated == 0 || first_updated < first_created {
        issues.push(format!(
            "v4 dataset {} version {} returned invalid lineage values created={} updated={}",
            locator.table_id, locator.dataset_version, first_created, first_updated
        ));
    }
    Ok(())
}

async fn validate_v4_graph_change_rowids(
    metadata: &DatabaseMetadata,
    visible_changes: &[GraphChangeRecord],
    issues: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let mut latest_live_changes = BTreeMap::<(String, String, String), GraphChangeRecord>::new();
    let mut last_known_live_rowids = BTreeMap::<(String, String, String), u64>::new();
    for record in visible_changes {
        let key = (
            record.entity_kind.clone(),
            record.type_name.clone(),
            record.entity_key.clone(),
        );
        latest_live_changes.insert(key.clone(), record.clone());
        match record.op.as_str() {
            "delete" => match record.rowid_if_known {
                Some(rowid) => match last_known_live_rowids.get(&key) {
                    Some(previous_rowid) if *previous_rowid == rowid => {}
                    Some(previous_rowid) => issues.push(format!(
                        "v4 delete graph change {} {} {} recorded rowid {} but prior live rowid was {}",
                        record.entity_kind,
                        record.type_name,
                        record.entity_key,
                        rowid,
                        previous_rowid
                    )),
                    None => issues.push(format!(
                        "v4 delete graph change {} {} {} has no prior live rowid history",
                        record.entity_kind, record.type_name, record.entity_key
                    )),
                },
                None => issues.push(format!(
                    "v4 delete graph change {} {} {} is missing rowid_if_known",
                    record.entity_kind, record.type_name, record.entity_key
                )),
            },
            "insert" | "update" => {
                if let Some(rowid) = record.rowid_if_known {
                    if record.op == "update"
                        && let Some(previous_rowid) = last_known_live_rowids.get(&key)
                        && *previous_rowid != rowid
                    {
                        issues.push(format!(
                            "v4 update graph change {} {} {} changed stable rowid from {} to {}",
                            record.entity_kind,
                            record.type_name,
                            record.entity_key,
                            previous_rowid,
                            rowid
                        ));
                    }
                    last_known_live_rowids.insert(key, rowid);
                }
            }
            _ => {}
        }
    }

    let mut entity_ids_by_locator = BTreeMap::<(String, String), BTreeSet<u64>>::new();
    for ((kind, type_name, _entity_key), record) in &latest_live_changes {
        if record.op == "delete" {
            continue;
        }
        let Some(entity_id) = graph_change_entity_id(record) else {
            warnings.push(format!(
                "v4 graph change {} {} has unparsable entity key {}",
                kind, type_name, record.entity_key
            ));
            continue;
        };
        entity_ids_by_locator
            .entry((kind.clone(), type_name.clone()))
            .or_default()
            .insert(entity_id);
    }

    let mut live_rows = BTreeMap::<(String, String, u64), LiveRowLineage>::new();
    for ((kind, type_name), entity_ids) in entity_ids_by_locator {
        let Some(locator) = metadata.dataset_locator(&kind, &type_name) else {
            issues.push(format!(
                "missing dataset locator for {} {} while validating graph change row ids",
                kind, type_name
            ));
            continue;
        };
        let resolved = resolve_live_row_lineage_for_locator(&locator, &entity_ids).await?;
        for (entity_id, lineage) in resolved {
            live_rows.insert((kind.clone(), type_name.clone(), entity_id), lineage);
        }
    }

    for ((kind, type_name, _entity_key), record) in latest_live_changes {
        if record.op == "delete" {
            continue;
        }
        let Some(entity_id) = graph_change_entity_id(&record) else {
            continue;
        };
        let Some(lineage) = live_rows.get(&(kind.clone(), type_name.clone(), entity_id)) else {
            issues.push(format!(
                "latest live graph change for {} {} {} is not present in the committed snapshot",
                kind, type_name, record.entity_key
            ));
            continue;
        };

        if lineage.created_at_version == 0
            || lineage.last_updated_at_version == 0
            || lineage.last_updated_at_version < lineage.created_at_version
        {
            issues.push(format!(
                "live row lineage for {} {} {} is invalid: created={} updated={}",
                kind,
                type_name,
                record.entity_key,
                lineage.created_at_version,
                lineage.last_updated_at_version
            ));
        }

        match record.rowid_if_known {
            Some(rowid) if rowid != lineage.rowid => issues.push(format!(
                "v4 graph change {} {} {} recorded rowid {} but committed snapshot rowid is {}",
                kind, type_name, record.entity_key, rowid, lineage.rowid
            )),
            None => warnings.push(format!(
                "v4 graph change {} {} {} has no rowid_if_known; likely committed before lineage capture",
                kind, type_name, record.entity_key
            )),
            _ => {}
        }
    }

    Ok(())
}

async fn validate_v4_graph_changes_against_lance_lineage(
    metadata: &DatabaseMetadata,
    manifest: &GraphManifest,
    visible_changes: &[GraphChangeRecord],
    issues: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<DoctorLineageShadowReport> {
    let commits = read_visible_graph_commit_records(metadata.path())?;
    let mut report = DoctorLineageShadowReport::default();
    if commits.is_empty() {
        return Ok(report);
    }

    let data_entries_by_table_id = manifest
        .datasets
        .iter()
        .filter(|entry| matches!(entry.kind.as_str(), "node" | "edge"))
        .map(|entry| (entry.effective_table_id().to_string(), entry.clone()))
        .collect::<BTreeMap<_, _>>();
    let expected_by_graph_version_and_table = build_expected_lineage_changes(visible_changes);
    let mut previous_versions = BTreeMap::<String, u64>::new();

    for commit in commits {
        let graph_version = commit.graph_version.value();
        let mut next_versions = BTreeMap::<String, u64>::new();
        for table_version in &commit.table_versions {
            let Some(entry) = data_entries_by_table_id.get(table_version.table_id.as_str()) else {
                continue;
            };
            let table_id = entry.effective_table_id().to_string();
            next_versions.insert(table_id.clone(), table_version.version);

            let expected = expected_by_graph_version_and_table
                .get(&(graph_version, entry.kind.clone(), entry.type_name.clone()))
                .cloned()
                .unwrap_or_default();
            let previous_version = previous_versions.get(&table_id).copied();
            report.windows_considered += 1;
            let mut window = DoctorLineageShadowWindowReport {
                kind: entry.kind.clone(),
                type_name: entry.type_name.clone(),
                graph_version,
                previous_table_version: previous_version,
                current_table_version: table_version.version,
                expected_inserts: expected.inserts.len(),
                expected_updates: expected.updates.len(),
                actual_inserts: None,
                actual_updates: None,
                status: "verified".to_string(),
                detail: None,
            };
            if !expected.missing_rowid_entities.is_empty() {
                let detail = format!(
                    "skipping v4 lineage shadow verification for {} {} at graph_version {} because graph changes are missing row ids for {}",
                    entry.kind,
                    entry.type_name,
                    graph_version,
                    expected.missing_rowid_entities.join(", ")
                );
                warnings.push(detail.clone());
                report.windows_skipped += 1;
                report.missing_rowid_windows += 1;
                window.status = "skipped_missing_rowids".to_string();
                window.detail = Some(detail);
                report.windows.push(window);
                continue;
            }
            if previous_version == Some(table_version.version) {
                if !expected.inserts.is_empty() || !expected.updates.is_empty() {
                    let detail = format!(
                        "v4 lineage shadow mismatch for {} {} at graph_version {}: graph changes recorded inserts/updates without a table version change",
                        entry.kind, entry.type_name, graph_version
                    );
                    issues.push(detail.clone());
                    report.windows_mismatched += 1;
                    window.status = "mismatch_no_table_version_change".to_string();
                    window.detail = Some(detail);
                } else {
                    report.windows_verified += 1;
                    window.status = "verified_no_table_version_change".to_string();
                }
                report.windows.push(window);
                continue;
            }

            let actual = match collect_actual_lineage_changes_for_commit(
                metadata,
                entry,
                table_version.version,
                previous_version,
            )
            .await
            {
                Ok(Some(actual)) => actual,
                Ok(None) => {
                    report.windows_skipped += 1;
                    window.status = "skipped_unavailable_history".to_string();
                    window.detail = Some(
                        "historical table versions needed for lineage shadow verification are unavailable"
                            .to_string(),
                    );
                    report.windows.push(window);
                    continue;
                }
                Err(err) => {
                    let detail = format!(
                        "skipping v4 lineage shadow verification for {} {} at graph_version {}: {}",
                        entry.kind, entry.type_name, graph_version, err
                    );
                    warnings.push(detail.clone());
                    report.windows_skipped += 1;
                    window.status = "skipped_unavailable_history".to_string();
                    window.detail = Some(detail);
                    report.windows.push(window);
                    continue;
                }
            };
            window.actual_inserts = Some(actual.inserts.len());
            window.actual_updates = Some(actual.updates.len());

            if previous_version.is_none()
                && expected.inserts.is_empty()
                && expected.updates.is_empty()
            {
                report.windows_verified += 1;
                window.status = "verified_bootstrap_no_changes".to_string();
                report.windows.push(window);
                continue;
            }

            if previous_version.is_none() && !expected.updates.is_empty() {
                let detail = format!(
                    "v4 lineage shadow mismatch for {} {} at graph_version {}: graph changes recorded updates before any prior committed table version existed",
                    entry.kind, entry.type_name, graph_version
                );
                issues.push(detail.clone());
                report.windows_mismatched += 1;
                window.status = "mismatch_missing_prior_version".to_string();
                window.detail = Some(detail);
                report.windows.push(window);
                continue;
            }

            if actual.inserts != expected.inserts || actual.updates != expected.updates {
                let detail = format!(
                    "v4 lineage shadow mismatch for {} {} at graph_version {}: expected inserts {:?} updates {:?}, actual inserts {:?} updates {:?}",
                    entry.kind,
                    entry.type_name,
                    graph_version,
                    expected.inserts,
                    expected.updates,
                    actual.inserts,
                    actual.updates
                );
                issues.push(detail.clone());
                report.windows_mismatched += 1;
                window.status = "mismatch_delta".to_string();
                window.detail = Some(detail);
            } else {
                report.windows_verified += 1;
            }
            report.windows.push(window);
        }
        previous_versions = next_versions;
    }

    Ok(report)
}

fn build_expected_lineage_changes(
    visible_changes: &[GraphChangeRecord],
) -> BTreeMap<(u64, String, String), ExpectedLineageChanges> {
    let mut out = BTreeMap::<(u64, String, String), ExpectedLineageChanges>::new();
    for record in visible_changes {
        let Some(entity_id) = graph_change_entity_id(record) else {
            continue;
        };
        let key = (
            record.graph_version.value(),
            record.entity_kind.clone(),
            record.type_name.clone(),
        );
        let entry = out.entry(key).or_default();
        match record.op.as_str() {
            "insert" => match record.rowid_if_known {
                Some(rowid) => {
                    entry.inserts.insert((entity_id, rowid));
                }
                None => entry.missing_rowid_entities.push(record.entity_key.clone()),
            },
            "update" => match record.rowid_if_known {
                Some(rowid) => {
                    entry.updates.insert((entity_id, rowid));
                }
                None => entry.missing_rowid_entities.push(record.entity_key.clone()),
            },
            _ => {}
        }
    }
    out
}

async fn collect_actual_lineage_changes_for_commit(
    metadata: &DatabaseMetadata,
    entry: &crate::store::manifest::DatasetEntry,
    current_version: u64,
    previous_version: Option<u64>,
) -> Result<Option<LineageChangeSet>> {
    let locator = DatasetLocator {
        db_path: metadata.path().to_path_buf(),
        table_id: entry.effective_table_id().to_string(),
        dataset_path: metadata.path().join(&entry.dataset_path),
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

    let mut changes = LineageChangeSet::default();
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
        changes.inserts = collect_entity_rowids_from_lineage_stream(
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
        changes.updates = collect_entity_rowids_from_lineage_stream(
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
        changes.inserts = collect_entity_rowids_from_live_dataset(&dataset).await?;
    }

    Ok(Some(changes))
}

async fn collect_entity_rowids_from_live_dataset(
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
    collect_entity_rowids_from_lineage_stream(stream).await
}

async fn collect_entity_rowids_from_lineage_stream(
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

fn graph_change_entity_id(record: &GraphChangeRecord) -> Option<u64> {
    let key = record.entity_key.strip_prefix("id=")?;
    let value = key.split(',').next()?;
    value.parse::<u64>().ok()
}

async fn resolve_live_row_lineage_for_locator(
    locator: &DatasetLocator,
    entity_ids: &BTreeSet<u64>,
) -> Result<BTreeMap<u64, LiveRowLineage>> {
    if entity_ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    let dataset = open_dataset_for_locator(locator).await?;
    let filter = entity_ids
        .iter()
        .map(|id| format!("{LANCE_INTERNAL_ID_FIELD} = {id}"))
        .collect::<Vec<_>>()
        .join(" OR ");

    let mut scanner = dataset.scan();
    scanner
        .project(&[
            LANCE_INTERNAL_ID_FIELD,
            "_rowid",
            "_row_created_at_version",
            "_row_last_updated_at_version",
        ])
        .map_err(|err| {
            NanoError::Lance(format!(
                "project live lineage scan {} error: {}",
                locator.table_id, err
            ))
        })?;
    scanner.filter(&filter).map_err(|err| {
        NanoError::Lance(format!(
            "filter live lineage scan {} error: {}",
            locator.table_id, err
        ))
    })?;
    let batch = scanner.try_into_batch().await.map_err(|err| {
        NanoError::Lance(format!(
            "execute live lineage scan {} error: {}",
            locator.table_id, err
        ))
    })?;

    let entity_ids = batch
        .column_by_name(LANCE_INTERNAL_ID_FIELD)
        .ok_or_else(|| {
            NanoError::Storage(format!(
                "live lineage batch missing {} column",
                LANCE_INTERNAL_ID_FIELD
            ))
        })?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| {
            NanoError::Storage(format!(
                "live lineage {} column is not UInt64",
                LANCE_INTERNAL_ID_FIELD
            ))
        })?;
    let rowids = batch
        .column_by_name("_rowid")
        .ok_or_else(|| NanoError::Storage("live lineage batch missing _rowid column".to_string()))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| {
            NanoError::Storage("live lineage _rowid column is not UInt64".to_string())
        })?;
    let created = batch
        .column_by_name("_row_created_at_version")
        .ok_or_else(|| {
            NanoError::Storage(
                "live lineage batch missing _row_created_at_version column".to_string(),
            )
        })?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| {
            NanoError::Storage(
                "live lineage _row_created_at_version column is not UInt64".to_string(),
            )
        })?;
    let updated = batch
        .column_by_name("_row_last_updated_at_version")
        .ok_or_else(|| {
            NanoError::Storage(
                "live lineage batch missing _row_last_updated_at_version column".to_string(),
            )
        })?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| {
            NanoError::Storage(
                "live lineage _row_last_updated_at_version column is not UInt64".to_string(),
            )
        })?;

    let mut out = BTreeMap::new();
    for row in 0..batch.num_rows() {
        out.insert(
            entity_ids.value(row),
            LiveRowLineage {
                rowid: rowids.value(row),
                created_at_version: created.value(row),
                last_updated_at_version: updated.value(row),
            },
        );
    }
    Ok(out)
}

/// Remove Lance dirs under nodes/ and edges/ that are not in the manifest.
pub(super) fn cleanup_stale_dirs(db_path: &Path, manifest: &GraphManifest) -> Result<()> {
    if matches!(
        detect_storage_generation(db_path)?,
        Some(StorageGeneration::V4Namespace)
    ) {
        let _ = manifest;
        return Ok(());
    }
    let valid_node_dirs: HashSet<String> = manifest
        .datasets
        .iter()
        .filter(|d| d.kind == "node")
        .map(|d| SchemaIR::dir_name(d.type_id))
        .collect();
    let valid_edge_dirs: HashSet<String> = manifest
        .datasets
        .iter()
        .filter(|d| d.kind == "edge")
        .map(|d| SchemaIR::dir_name(d.type_id))
        .collect();

    for (subdir, valid) in [("nodes", &valid_node_dirs), ("edges", &valid_edge_dirs)] {
        let dir = db_path.join(subdir);
        if dir.exists() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                if let Some(name) = entry.file_name().to_str()
                    && !valid.contains(name)
                {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
    }

    Ok(())
}

fn cdc_analytics_dataset_path(db_path: &Path) -> PathBuf {
    db_path.join(CDC_ANALYTICS_DATASET_DIR)
}

fn cdc_analytics_state_path(db_path: &Path) -> PathBuf {
    db_path.join(CDC_ANALYTICS_STATE_FILE)
}

pub(super) fn read_cdc_analytics_state(db_path: &Path) -> Result<CdcAnalyticsState> {
    let path = cdc_analytics_state_path(db_path);
    if !path.exists() {
        return Ok(CdcAnalyticsState::default());
    }

    let raw = std::fs::read_to_string(&path)?;
    let state: CdcAnalyticsState = serde_json::from_str(&raw).map_err(|e| {
        NanoError::Manifest(format!(
            "parse CDC analytics state {}: {}",
            path.display(),
            e
        ))
    })?;
    Ok(state)
}

fn write_cdc_analytics_state(db_path: &Path, state: &CdcAnalyticsState) -> Result<()> {
    let path = cdc_analytics_state_path(db_path);
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| NanoError::Manifest(format!("serialize CDC analytics state: {}", e)))?;
    std::fs::write(path, json)?;
    Ok(())
}

fn cdc_rows_to_analytics_batch(rows: &[CdcLogEntry]) -> Result<RecordBatch> {
    use arrow_schema::{Field, Schema};

    let payload_json: Vec<String> = rows
        .iter()
        .map(|row| {
            serde_json::to_string(&row.payload)
                .map_err(|e| NanoError::Manifest(format!("serialize CDC payload: {}", e)))
        })
        .collect::<Result<Vec<_>>>()?;

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

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(
                rows.iter().map(|row| row.tx_id.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|row| row.db_version).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                rows.iter().map(|row| row.seq_in_tx).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|row| row.op.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.entity_kind.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.type_name.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.entity_key.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(payload_json)),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.committed_at.clone())
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(|e| NanoError::Storage(format!("build CDC analytics batch: {}", e)))
}
