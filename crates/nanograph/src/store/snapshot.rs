use std::path::Path;
use std::thread;

use arrow_array::{StringArray, UInt64Array};
use futures::StreamExt;
use lance::Dataset;

use crate::error::Result;
use crate::store::manifest::GraphManifest;
use crate::store::namespace::{
    GRAPH_SNAPSHOT_TABLE_ID, namespace_latest_version, namespace_location_to_dataset_uri,
    open_directory_namespace, resolve_table_location,
};
use crate::store::namespace_commit::publish_snapshot_bundle;
use crate::store::storage_generation::{StorageGeneration, detect_storage_generation};

pub(crate) trait GraphSnapshotStore: Send + Sync {
    fn read_snapshot(&self, db_dir: &Path) -> Result<GraphManifest>;
    fn publish_snapshot(&self, db_dir: &Path, snapshot: &GraphManifest) -> Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ManifestGraphSnapshotStore;

impl GraphSnapshotStore for ManifestGraphSnapshotStore {
    fn read_snapshot(&self, db_dir: &Path) -> Result<GraphManifest> {
        GraphManifest::read(db_dir)
    }

    fn publish_snapshot(&self, db_dir: &Path, snapshot: &GraphManifest) -> Result<()> {
        snapshot.write_atomic(db_dir)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NamespaceGraphSnapshotStore;

impl GraphSnapshotStore for NamespaceGraphSnapshotStore {
    fn read_snapshot(&self, db_dir: &Path) -> Result<GraphManifest> {
        read_namespace_snapshot_blocking(db_dir)
    }

    fn publish_snapshot(&self, db_dir: &Path, snapshot: &GraphManifest) -> Result<()> {
        publish_namespace_snapshot_blocking(db_dir, snapshot)
    }
}

pub fn read_committed_graph_snapshot(db_dir: &Path) -> Result<GraphManifest> {
    match detect_storage_generation(db_dir)? {
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage) => {
            NamespaceGraphSnapshotStore.read_snapshot(db_dir)
        }
        None => ManifestGraphSnapshotStore.read_snapshot(db_dir),
    }
}

pub fn publish_committed_graph_snapshot(db_dir: &Path, snapshot: &GraphManifest) -> Result<()> {
    match detect_storage_generation(db_dir)? {
        Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage) => {
            NamespaceGraphSnapshotStore.publish_snapshot(db_dir, snapshot)
        }
        None => ManifestGraphSnapshotStore.publish_snapshot(db_dir, snapshot),
    }
}

pub(crate) async fn graph_snapshot_table_present(db_dir: &Path) -> Result<bool> {
    let namespace = open_directory_namespace(db_dir).await?;
    Ok(resolve_table_location(namespace, GRAPH_SNAPSHOT_TABLE_ID)
        .await
        .is_ok())
}

async fn read_namespace_snapshot_async(db_dir: &Path) -> Result<GraphManifest> {
    let namespace = open_directory_namespace(db_dir).await?;
    let published_version = namespace_latest_version(namespace.clone(), GRAPH_SNAPSHOT_TABLE_ID)
        .await?
        .version;
    let location = resolve_table_location(namespace, GRAPH_SNAPSHOT_TABLE_ID).await?;
    let dataset_uri = normalize_namespace_location(db_dir, &location)?;
    let dataset = Dataset::open(&dataset_uri)
        .await
        .map_err(|err| {
            crate::error::NanoError::Lance(format!(
                "namespace snapshot dataset open error: {}",
                err
            ))
        })?
        .checkout_version(published_version)
        .await
        .map_err(|err| {
            crate::error::NanoError::Lance(format!(
                "namespace snapshot dataset checkout version {} error: {}",
                published_version, err
            ))
        })?;
    let mut scanner = dataset.scan();
    scanner
        .project(&["graph_version".to_string(), "manifest_json".to_string()])
        .map_err(|err| {
            crate::error::NanoError::Lance(format!(
                "project namespace snapshot dataset error: {}",
                err
            ))
        })?;
    let batches = scanner
        .try_into_stream()
        .await
        .map_err(|err| {
            crate::error::NanoError::Lance(format!(
                "scan namespace snapshot dataset error: {}",
                err
            ))
        })?
        .map(|batch| {
            batch.map_err(|err| {
                crate::error::NanoError::Lance(format!("namespace snapshot stream error: {}", err))
            })
        })
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?;

    let mut latest: Option<(u64, String)> = None;
    for batch in batches {
        let graph_versions = batch
            .column_by_name("graph_version")
            .ok_or_else(|| {
                crate::error::NanoError::Storage(
                    "graph snapshot batch missing graph_version column".to_string(),
                )
            })?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| {
                crate::error::NanoError::Storage(
                    "graph snapshot graph_version column is not UInt64".to_string(),
                )
            })?;
        let manifest_json = batch
            .column_by_name("manifest_json")
            .ok_or_else(|| {
                crate::error::NanoError::Storage(
                    "graph snapshot batch missing manifest_json column".to_string(),
                )
            })?
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| {
                crate::error::NanoError::Storage(
                    "graph snapshot manifest_json column is not Utf8".to_string(),
                )
            })?;
        for row in 0..batch.num_rows() {
            let graph_version = graph_versions.value(row);
            let payload = manifest_json.value(row).to_string();
            if latest
                .as_ref()
                .map(|(current, _)| graph_version >= *current)
                .unwrap_or(true)
            {
                latest = Some((graph_version, payload));
            }
        }
    }

    let (_, payload) = latest.ok_or_else(|| {
        crate::error::NanoError::Storage("graph snapshot table is empty".to_string())
    })?;
    serde_json::from_str(&payload).map_err(|err| {
        crate::error::NanoError::Storage(format!("parse namespace graph snapshot error: {}", err))
    })
}

fn normalize_namespace_location(db_dir: &Path, location: &str) -> Result<String> {
    namespace_location_to_dataset_uri(db_dir, location)
}

fn read_namespace_snapshot_blocking(db_dir: &Path) -> Result<GraphManifest> {
    let db_dir = db_dir.to_path_buf();
    run_v4_snapshot_task("read v4 namespace snapshot", move || async move {
        read_namespace_snapshot_async(&db_dir).await
    })
}

fn publish_namespace_snapshot_blocking(db_dir: &Path, snapshot: &GraphManifest) -> Result<()> {
    publish_snapshot_bundle(db_dir, snapshot)
}

fn run_v4_snapshot_task<T, Fut, F>(label: &str, work: F) -> Result<T>
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
                crate::error::NanoError::Storage(format!(
                    "initialize {} runtime error: {}",
                    label, err
                ))
            })?;
        runtime.block_on(work())
    })
    .join()
    .map_err(|_| {
        crate::error::NanoError::Storage(format!("{} worker thread panicked", panic_label))
    })?
}
