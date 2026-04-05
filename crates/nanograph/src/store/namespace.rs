use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use lance::Dataset;
use lance::dataset::builder::DatasetBuilder;
use lance::dataset::{MergeInsertBuilder, WhenMatched, WhenNotMatched, WriteMode, WriteParams};
use lance_file::version::LanceFileVersion;
use lance_io::object_store::{ObjectStore, ObjectStoreParams, ObjectStoreRegistry};
use lance_namespace::LanceNamespace;
use lance_namespace::models::{
    DeclareTableRequest, DescribeTableRequest, ListTableVersionsRequest,
};
use lance_namespace_impls::dir::manifest::{ManifestEntry, ObjectType};
use lance_namespace_impls::{DirectoryNamespaceBuilder, ManifestNamespace};
use lance_table::io::commit::ManifestNamingScheme;

use crate::error::{NanoError, Result};
use crate::store::graph_types::GraphTableVersion;
use crate::store::lance_io::{
    LANCE_INTERNAL_ID_FIELD, cleanup_unpublished_manifest_versions, logical_batch_to_lance,
};
use crate::store::manifest::{DatasetEntry, GraphManifest};

const DEFAULT_NEW_DATASET_STORAGE_VERSION: LanceFileVersion = LanceFileVersion::V2_2;

pub(crate) const GRAPH_TX_TABLE_ID: &str = "__graph_tx";
pub(crate) const GRAPH_CHANGES_TABLE_ID: &str = "__graph_changes";
pub(crate) const GRAPH_DELETES_TABLE_ID: &str = "__graph_deletes";
pub(crate) const GRAPH_SNAPSHOT_TABLE_ID: &str = "__graph_snapshot";
pub(crate) const BLOB_STORE_TABLE_ID: &str = "__blob_store";

#[derive(Debug, Clone)]
pub(crate) struct NamespacePublishedVersion {
    pub(crate) table_id: String,
    pub(crate) version: u64,
    pub(crate) manifest_path: String,
    pub(crate) manifest_size: Option<u64>,
    pub(crate) e_tag: Option<String>,
    pub(crate) naming_scheme: ManifestNamingScheme,
}

#[derive(Debug, Clone)]
pub(crate) struct StagedNamespaceTable {
    pub(crate) entry: DatasetEntry,
    pub(crate) published_version: NamespacePublishedVersion,
}

pub(crate) fn table_id_parts(table_id: &str) -> Vec<String> {
    table_id
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| part.to_string())
        .collect()
}

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

pub(crate) async fn resolve_table_location(
    namespace: Arc<dyn LanceNamespace>,
    table_id: &str,
) -> Result<String> {
    let response = namespace
        .describe_table(DescribeTableRequest {
            id: Some(table_id_parts(table_id)),
            ..Default::default()
        })
        .await
        .map_err(|err| {
            NanoError::Lance(format!(
                "describe namespace table {} error: {}",
                table_id, err
            ))
        })?;
    response.location.ok_or_else(|| {
        NanoError::Storage(format!(
            "namespace table {} has no resolved location",
            table_id
        ))
    })
}

pub(crate) async fn resolve_or_declare_table_location(
    namespace: Arc<dyn LanceNamespace>,
    table_id: &str,
) -> Result<String> {
    match resolve_table_location(namespace.clone(), table_id).await {
        Ok(location) => Ok(location),
        Err(_) => namespace
            .declare_table(DeclareTableRequest {
                id: Some(table_id_parts(table_id)),
                ..Default::default()
            })
            .await
            .map_err(|err| {
                NanoError::Lance(format!(
                    "declare namespace table {} error: {}",
                    table_id, err
                ))
            })?
            .location
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "declared namespace table {} returned no location",
                    table_id
                ))
            }),
    }
}

fn namespace_write_params(
    _namespace: Arc<dyn LanceNamespace>,
    _table_id: &str,
    mode: WriteMode,
    transaction_properties: Option<HashMap<String, String>>,
) -> WriteParams {
    let mut params = WriteParams::default();
    params.mode = mode;
    params.enable_stable_row_ids = true;
    if let Some(properties) = transaction_properties {
        params.transaction_properties = Some(Arc::new(properties));
    }
    params
}

fn namespace_create_params(
    namespace: Arc<dyn LanceNamespace>,
    table_id: &str,
    mode: WriteMode,
    transaction_properties: Option<HashMap<String, String>>,
) -> WriteParams {
    let mut params = namespace_write_params(namespace, table_id, mode, transaction_properties);
    params.data_storage_version = Some(DEFAULT_NEW_DATASET_STORAGE_VERSION);
    params
}

pub(crate) async fn write_namespace_batch(
    namespace: Arc<dyn LanceNamespace>,
    table_id: &str,
    batch: arrow_array::RecordBatch,
    mode: WriteMode,
    transaction_properties: Option<HashMap<String, String>>,
) -> Result<GraphTableVersion> {
    let kind = if table_id.starts_with("nodes/") {
        super::lance_io::LanceDatasetKind::Node
    } else if table_id.starts_with("edges/") {
        super::lance_io::LanceDatasetKind::Edge
    } else {
        super::lance_io::LanceDatasetKind::Plain
    };
    let batch = logical_batch_to_lance(&batch, kind)?;
    let table_exists = resolve_table_location(namespace.clone(), table_id)
        .await
        .is_ok();
    let effective_mode = if table_exists {
        mode
    } else {
        WriteMode::Create
    };
    let schema = batch.schema();
    let reader = arrow_array::RecordBatchIterator::new(vec![Ok(batch)], schema);
    match effective_mode {
        WriteMode::Create => {
            let params = namespace_create_params(
                namespace.clone(),
                table_id,
                WriteMode::Create,
                transaction_properties,
            );
            let dataset = Dataset::write_into_namespace(
                reader,
                namespace,
                table_id_parts(table_id),
                Some(params),
            )
            .await
            .map_err(|err| {
                NanoError::Lance(format!("namespace create {} error: {}", table_id, err))
            })?;
            Ok(GraphTableVersion::new(table_id, dataset.version().version))
        }
        WriteMode::Append => {
            let mut dataset = load_namespace_dataset(namespace.clone(), table_id, None).await?;
            dataset
                .append(
                    reader,
                    Some(namespace_write_params(
                        namespace,
                        table_id,
                        WriteMode::Append,
                        transaction_properties,
                    )),
                )
                .await
                .map_err(|err| {
                    NanoError::Lance(format!("namespace append {} error: {}", table_id, err))
                })?;
            Ok(GraphTableVersion::new(table_id, dataset.version().version))
        }
        WriteMode::Overwrite => {
            let params = namespace_write_params(
                namespace.clone(),
                table_id,
                WriteMode::Overwrite,
                transaction_properties,
            );
            let dataset = Dataset::write_into_namespace(
                reader,
                namespace,
                table_id_parts(table_id),
                Some(params),
            )
            .await
            .map_err(|err| {
                NanoError::Lance(format!("namespace overwrite {} error: {}", table_id, err))
            })?;
            Ok(GraphTableVersion::new(table_id, dataset.version().version))
        }
    }
}

pub(crate) async fn namespace_latest_version(
    namespace: Arc<dyn LanceNamespace>,
    table_id: &str,
) -> Result<GraphTableVersion> {
    namespace_latest_version_optional(namespace, table_id)
        .await?
        .ok_or_else(|| {
            NanoError::Storage(format!(
                "namespace table {} has no recorded versions",
                table_id
            ))
        })
}

pub(crate) async fn namespace_latest_version_optional(
    namespace: Arc<dyn LanceNamespace>,
    table_id: &str,
) -> Result<Option<GraphTableVersion>> {
    let response = namespace
        .list_table_versions(ListTableVersionsRequest {
            id: Some(table_id_parts(table_id)),
            descending: Some(true),
            limit: Some(1),
            ..Default::default()
        })
        .await
        .map_err(|err| {
            NanoError::Lance(format!(
                "namespace list versions {} error: {}",
                table_id, err
            ))
        })?;
    Ok(response
        .versions
        .first()
        .map(|version| GraphTableVersion::new(table_id, version.version as u64)))
}

pub(crate) async fn cleanup_namespace_orphan_versions(
    db_path: &Path,
    snapshot: &GraphManifest,
) -> Result<usize> {
    let namespace = open_directory_namespace(db_path).await?;
    let mut removed = 0usize;
    for entry in &snapshot.datasets {
        let table_id = entry.effective_table_id();
        let location = resolve_table_location(namespace.clone(), table_id).await?;
        let published_version = Some(entry.dataset_version);
        removed += cleanup_unpublished_manifest_versions(&location, published_version).await?;
    }
    if let Ok(location) = resolve_table_location(namespace.clone(), GRAPH_SNAPSHOT_TABLE_ID).await {
        let published_snapshot_version =
            namespace_latest_version(namespace, GRAPH_SNAPSHOT_TABLE_ID)
                .await?
                .version;
        removed +=
            cleanup_unpublished_manifest_versions(&location, Some(published_snapshot_version))
                .await?;
    }
    Ok(removed)
}

pub(crate) async fn cleanup_namespace_snapshot_orphan_versions(db_path: &Path) -> Result<usize> {
    let namespace = open_directory_namespace(db_path).await?;
    let Ok(location) = resolve_table_location(namespace.clone(), GRAPH_SNAPSHOT_TABLE_ID).await
    else {
        return Ok(0);
    };
    let published_snapshot_version = namespace_latest_version(namespace, GRAPH_SNAPSHOT_TABLE_ID)
        .await?
        .version;
    cleanup_unpublished_manifest_versions(&location, Some(published_snapshot_version)).await
}

pub(crate) async fn namespace_published_version_for_table(
    db_path: &Path,
    table_id: &str,
    version: u64,
) -> Result<Option<NamespacePublishedVersion>> {
    let namespace = open_directory_namespace(db_path).await?;
    if let Ok(current) = namespace_latest_version(namespace.clone(), table_id).await
        && current.version >= version
    {
        return Ok(None);
    }
    let location = resolve_or_declare_table_location(namespace, table_id).await?;
    let dataset = Dataset::open(&location)
        .await
        .map_err(|err| {
            NanoError::Lance(format!("open staged dataset {} error: {}", table_id, err))
        })?
        .checkout_version(version)
        .await
        .map_err(|err| {
            NanoError::Lance(format!(
                "checkout staged dataset {} version {} error: {}",
                table_id, version, err
            ))
        })?;
    let manifest_location = dataset.manifest_location().clone();
    Ok(Some(NamespacePublishedVersion {
        table_id: table_id.to_string(),
        version,
        manifest_path: manifest_location.path.to_string(),
        manifest_size: manifest_location.size,
        e_tag: manifest_location.e_tag,
        naming_scheme: manifest_location.naming_scheme,
    }))
}

pub(crate) fn dedup_namespace_published_versions(versions: &mut Vec<NamespacePublishedVersion>) {
    versions.sort_by(|a, b| a.table_id.cmp(&b.table_id).then(a.version.cmp(&b.version)));
    versions.dedup_by(|a, b| a.table_id == b.table_id && a.version == b.version);
}

pub(crate) async fn open_manifest_namespace(db_path: &Path) -> Result<ManifestNamespace> {
    let root = db_path.to_string_lossy().to_string();
    let registry = Arc::new(ObjectStoreRegistry::default());
    let (object_store, base_path) =
        ObjectStore::from_uri_and_params(registry, &root, &ObjectStoreParams::default())
            .await
            .map_err(|err| {
                NanoError::Lance(format!(
                    "open object store for manifest namespace {} error: {}",
                    db_path.display(),
                    err
                ))
            })?;
    ManifestNamespace::from_directory(
        root,
        None,
        None,
        object_store,
        base_path,
        false,
        true,
        None,
        true,
    )
    .await
    .map_err(|err| {
        NanoError::Lance(format!(
            "open manifest namespace {} error: {}",
            db_path.display(),
            err
        ))
    })
}

pub(crate) async fn batch_publish_namespace_versions(
    db_path: &Path,
    versions: &[NamespacePublishedVersion],
) -> Result<()> {
    if versions.is_empty() {
        return Ok(());
    }

    let manifest_ns = open_manifest_namespace(db_path).await?;
    let entries = versions
        .iter()
        .map(|version| {
            let table_object_id =
                ManifestNamespace::str_object_id(&table_id_parts(&version.table_id));
            let object_id = ManifestNamespace::build_version_object_id(
                &table_object_id,
                version.version as i64,
            );
            let metadata = serde_json::json!({
                "manifest_path": version.manifest_path,
                "manifest_size": version.manifest_size.map(|size| size as i64),
                "e_tag": version.e_tag,
                "naming_scheme": match version.naming_scheme {
                    ManifestNamingScheme::V1 => "V1",
                    ManifestNamingScheme::V2 => "V2",
                },
            })
            .to_string();
            ManifestEntry {
                object_id,
                object_type: ObjectType::TableVersion,
                location: None,
                metadata: Some(metadata),
            }
        })
        .collect::<Vec<_>>();
    manifest_ns
        .insert_into_manifest_with_metadata(entries, None)
        .await
        .map_err(|err| {
            NanoError::Lance(format!(
                "batch publish namespace versions for {} error: {}",
                db_path.display(),
                err
            ))
        })
}

async fn load_namespace_dataset(
    namespace: Arc<dyn LanceNamespace>,
    table_id: &str,
    version: Option<u64>,
) -> Result<Dataset> {
    let mut builder = DatasetBuilder::from_namespace(namespace, table_id_parts(table_id))
        .await
        .map_err(|err| {
            NanoError::Lance(format!(
                "namespace dataset builder {} error: {}",
                table_id, err
            ))
        })?;
    if let Some(version) = version {
        builder = builder.with_version(version);
    }
    builder.load().await.map_err(|err| {
        NanoError::Lance(format!(
            "namespace dataset {}{} load error: {}",
            table_id,
            version
                .map(|version| format!(" version {}", version))
                .unwrap_or_default(),
            err
        ))
    })
}

#[allow(dead_code)]
pub(crate) async fn namespace_merge_insert_with_key(
    namespace: Arc<dyn LanceNamespace>,
    table_id: &str,
    pinned_version: &GraphTableVersion,
    source_batch: arrow_array::RecordBatch,
    key_prop: &str,
) -> Result<GraphTableVersion> {
    let kind = if table_id.starts_with("nodes/") {
        super::lance_io::LanceDatasetKind::Node
    } else if table_id.starts_with("edges/") {
        super::lance_io::LanceDatasetKind::Edge
    } else {
        super::lance_io::LanceDatasetKind::Plain
    };
    let dataset = load_namespace_dataset(namespace, table_id, Some(pinned_version.version)).await?;
    let mut builder = MergeInsertBuilder::try_new(Arc::new(dataset), vec![key_prop.to_string()])
        .map_err(|err| NanoError::Lance(format!("merge builder {} error: {}", table_id, err)))?;
    builder
        .when_matched(WhenMatched::UpdateAll)
        .when_not_matched(WhenNotMatched::InsertAll)
        .conflict_retries(0);
    let source_batch = logical_batch_to_lance(&source_batch, kind)?;
    let source_schema = source_batch.schema();
    let source = Box::new(arrow_array::RecordBatchIterator::new(
        vec![Ok(source_batch)].into_iter(),
        source_schema,
    ));
    let job = builder
        .try_build()
        .map_err(|err| NanoError::Lance(format!("merge build {} error: {}", table_id, err)))?;
    let (dataset, _) = job
        .execute_reader(source)
        .await
        .map_err(|err| NanoError::Lance(format!("merge execute {} error: {}", table_id, err)))?;
    Ok(GraphTableVersion::new(table_id, dataset.version().version))
}

#[allow(dead_code)]
pub(crate) async fn namespace_delete_by_ids(
    namespace: Arc<dyn LanceNamespace>,
    table_id: &str,
    pinned_version: &GraphTableVersion,
    ids: &[u64],
) -> Result<GraphTableVersion> {
    if ids.is_empty() {
        return Ok(pinned_version.clone());
    }
    let mut dataset =
        load_namespace_dataset(namespace, table_id, Some(pinned_version.version)).await?;
    let id_column = if table_id.starts_with("edges/") || table_id.starts_with("nodes/") {
        LANCE_INTERNAL_ID_FIELD
    } else {
        "id"
    };
    let predicate = ids
        .iter()
        .map(|id| format!("{} = {}", id_column, id))
        .collect::<Vec<_>>()
        .join(" OR ");
    dataset
        .delete(&predicate)
        .await
        .map_err(|err| NanoError::Lance(format!("delete {} error: {}", table_id, err)))?;
    Ok(GraphTableVersion::new(table_id, dataset.version().version))
}

#[allow(dead_code)]
pub(crate) fn tx_properties(
    graph_version: u64,
    tx_id: &str,
    op_summary: &str,
) -> HashMap<String, String> {
    HashMap::from([
        ("graph_version".to_string(), graph_version.to_string()),
        ("tx_id".to_string(), tx_id.to_string()),
        ("op_summary".to_string(), op_summary.to_string()),
    ])
}
