use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{RecordBatch, RecordBatchIterator};
use async_trait::async_trait;
use futures::StreamExt;
use lance::Dataset;
use lance::dataset::{
    InsertBuilder, MergeInsertBuilder, WhenMatched, WhenNotMatched, WriteMode, WriteParams,
};
use lance_file::version::LanceFileVersion;
use tracing::info;

use crate::error::{NanoError, Result};
use crate::store::graph_types::{GraphTableId, GraphTableVersion};
use crate::store::metadata::DatasetLocator;
use crate::store::namespace::{
    local_path_to_file_uri, namespace_latest_version, namespace_location_to_dataset_uri,
    namespace_location_to_local_path, open_directory_namespace, resolve_or_declare_table_location,
    resolve_table_location,
};

pub(crate) const LANCE_INTERNAL_ID_FIELD: &str = "__ng_id";
pub(crate) const LANCE_INTERNAL_SRC_FIELD: &str = "__ng_src";
pub(crate) const LANCE_INTERNAL_DST_FIELD: &str = "__ng_dst";
const DEFAULT_NEW_DATASET_STORAGE_VERSION: LanceFileVersion = LanceFileVersion::V2_2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LanceDatasetKind {
    Node,
    Edge,
    Plain,
}

pub(crate) fn logical_node_field_to_lance(field_name: &str) -> &str {
    match field_name {
        "id" => LANCE_INTERNAL_ID_FIELD,
        other => other,
    }
}

pub(crate) fn graph_table_id_for_path(path: &Path) -> GraphTableId {
    GraphTableId(path.to_string_lossy().to_string())
}

#[async_trait]
pub(crate) trait TableStore: Send + Sync {
    async fn overwrite(&self, path: &Path, batch: RecordBatch) -> Result<GraphTableVersion>;
    async fn append(&self, path: &Path, batch: RecordBatch) -> Result<GraphTableVersion>;
    async fn merge_insert_with_key(
        &self,
        path: &Path,
        pinned_version: &GraphTableVersion,
        source_batch: RecordBatch,
        key_prop: &str,
    ) -> Result<GraphTableVersion>;
    async fn delete_by_ids(
        &self,
        path: &Path,
        pinned_version: &GraphTableVersion,
        ids: &[u64],
    ) -> Result<GraphTableVersion>;
    async fn latest_version(&self, path: &Path) -> Result<GraphTableVersion>;
    async fn read_all_batches(&self, version: &GraphTableVersion) -> Result<Vec<RecordBatch>>;
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct V3TableStore;

impl V3TableStore {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[derive(Debug, Clone)]
pub(crate) struct V4NamespaceTableStore {
    db_path: PathBuf,
}

impl V4NamespaceTableStore {
    pub(crate) fn new(db_path: &Path) -> Self {
        Self {
            db_path: db_path.to_path_buf(),
        }
    }

    fn table_id_for_path(&self, path: &Path) -> Result<String> {
        path.strip_prefix(&self.db_path)
            .map(|relative| relative.to_string_lossy().to_string())
            .map_err(|_| {
                NanoError::Storage(format!(
                    "dataset path {} is outside database root {}",
                    path.display(),
                    self.db_path.display()
                ))
            })
    }
}

pub(crate) fn logical_edge_field_to_lance(field_name: &str) -> &str {
    match field_name {
        "id" => LANCE_INTERNAL_ID_FIELD,
        "src" => LANCE_INTERNAL_SRC_FIELD,
        "dst" => LANCE_INTERNAL_DST_FIELD,
        other => other,
    }
}

fn lance_dataset_kind(path: &Path) -> LanceDatasetKind {
    match path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
    {
        Some("nodes") => LanceDatasetKind::Node,
        Some("edges") => LanceDatasetKind::Edge,
        _ => LanceDatasetKind::Plain,
    }
}

fn lance_dataset_kind_for_table_id(table_id: &str) -> LanceDatasetKind {
    if table_id.starts_with("nodes/") {
        LanceDatasetKind::Node
    } else if table_id.starts_with("edges/") {
        LanceDatasetKind::Edge
    } else {
        LanceDatasetKind::Plain
    }
}

fn lance_dataset_kind_for_locator(locator: &DatasetLocator) -> LanceDatasetKind {
    if locator.namespace_managed {
        lance_dataset_kind_for_table_id(&locator.table_id)
    } else {
        lance_dataset_kind(&locator.dataset_path)
    }
}

fn rename_batch_fields(batch: &RecordBatch, renames: &[(usize, &str)]) -> Result<RecordBatch> {
    let mut fields: Vec<arrow_schema::Field> = batch
        .schema()
        .fields()
        .iter()
        .map(|field| field.as_ref().clone())
        .collect();
    for (index, new_name) in renames {
        if *index >= fields.len() {
            return Err(NanoError::Storage(format!(
                "cannot rename field {} in schema with {} field(s)",
                index,
                fields.len()
            )));
        }
        fields[*index] = fields[*index].clone().with_name(*new_name);
    }

    RecordBatch::try_new(
        Arc::new(arrow_schema::Schema::new(fields)),
        batch.columns().to_vec(),
    )
    .map_err(|e| NanoError::Storage(format!("rename batch schema error: {}", e)))
}

pub(crate) fn logical_batch_to_lance(
    batch: &RecordBatch,
    kind: LanceDatasetKind,
) -> Result<RecordBatch> {
    match kind {
        LanceDatasetKind::Node => rename_batch_fields(batch, &[(0, LANCE_INTERNAL_ID_FIELD)]),
        LanceDatasetKind::Edge => rename_batch_fields(
            batch,
            &[
                (0, LANCE_INTERNAL_ID_FIELD),
                (1, LANCE_INTERNAL_SRC_FIELD),
                (2, LANCE_INTERNAL_DST_FIELD),
            ],
        ),
        LanceDatasetKind::Plain => Ok(batch.clone()),
    }
}

fn lance_batch_to_logical(batch: &RecordBatch, kind: LanceDatasetKind) -> Result<RecordBatch> {
    match kind {
        LanceDatasetKind::Node => {
            if batch.schema().field(0).name() == LANCE_INTERNAL_ID_FIELD {
                rename_batch_fields(batch, &[(0, "id")])
            } else {
                Ok(batch.clone())
            }
        }
        LanceDatasetKind::Edge => {
            if batch.schema().fields().len() >= 3
                && batch.schema().field(0).name() == LANCE_INTERNAL_ID_FIELD
                && batch.schema().field(1).name() == LANCE_INTERNAL_SRC_FIELD
                && batch.schema().field(2).name() == LANCE_INTERNAL_DST_FIELD
            {
                rename_batch_fields(batch, &[(0, "id"), (1, "src"), (2, "dst")])
            } else {
                Ok(batch.clone())
            }
        }
        LanceDatasetKind::Plain => Ok(batch.clone()),
    }
}

fn project_logical_batch(batch: &RecordBatch, columns: &[&str]) -> Result<RecordBatch> {
    let projected_fields = columns
        .iter()
        .map(|column| {
            let index = batch.schema().index_of(column).map_err(|e| {
                NanoError::Storage(format!(
                    "projection column {} missing from batch: {}",
                    column, e
                ))
            })?;
            Ok((
                batch.schema().field(index).as_ref().clone(),
                batch.column(index).clone(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    let (fields, arrays): (Vec<_>, Vec<_>) = projected_fields.into_iter().unzip();
    RecordBatch::try_new(Arc::new(arrow_schema::Schema::new(fields)), arrays)
        .map_err(|e| NanoError::Storage(format!("project logical batch error: {}", e)))
}

pub(crate) async fn write_lance_batch(path: &Path, batch: RecordBatch) -> Result<u64> {
    Ok(
        write_lance_batch_with_mode_versioned(path, batch, WriteMode::Overwrite)
            .await?
            .version,
    )
}

#[allow(dead_code)]
pub(crate) async fn write_lance_batch_with_mode(
    path: &Path,
    batch: RecordBatch,
    mode: WriteMode,
) -> Result<u64> {
    Ok(write_lance_batch_with_mode_versioned(path, batch, mode)
        .await?
        .version)
}

pub(crate) async fn write_lance_batch_with_mode_versioned(
    path: &Path,
    batch: RecordBatch,
    mode: WriteMode,
) -> Result<GraphTableVersion> {
    write_lance_batch_with_mode_for_kind_versioned(path, batch, mode, lance_dataset_kind(path))
        .await
}

pub(crate) async fn write_lance_batch_with_mode_for_kind_versioned(
    path: &Path,
    batch: RecordBatch,
    mode: WriteMode,
    kind: LanceDatasetKind,
) -> Result<GraphTableVersion> {
    write_lance_batch_with_mode_for_kind_versioned_and_properties(path, batch, mode, kind, None)
        .await
}

pub(crate) async fn write_lance_batch_with_mode_for_kind_versioned_and_properties(
    path: &Path,
    batch: RecordBatch,
    mode: WriteMode,
    kind: LanceDatasetKind,
    transaction_properties: Option<HashMap<String, String>>,
) -> Result<GraphTableVersion> {
    let has_manifest_dir = path.join("_versions").exists();
    let storage_version = if has_manifest_dir || matches!(mode, WriteMode::Append) {
        None
    } else {
        Some(DEFAULT_NEW_DATASET_STORAGE_VERSION)
    };
    write_lance_batch_with_mode_and_storage_version_for_kind_versioned(
        path,
        batch,
        mode,
        storage_version,
        kind,
        transaction_properties,
    )
    .await
}

#[allow(dead_code)]
pub(crate) async fn write_lance_batch_with_mode_and_storage_version(
    path: &Path,
    batch: RecordBatch,
    mode: WriteMode,
    storage_version: Option<LanceFileVersion>,
) -> Result<u64> {
    Ok(write_lance_batch_with_mode_and_storage_version_versioned(
        path,
        batch,
        mode,
        storage_version,
    )
    .await?
    .version)
}

async fn write_lance_batch_with_mode_and_storage_version_versioned(
    path: &Path,
    batch: RecordBatch,
    mode: WriteMode,
    storage_version: Option<LanceFileVersion>,
) -> Result<GraphTableVersion> {
    write_lance_batch_with_mode_and_storage_version_for_kind_versioned(
        path,
        batch,
        mode,
        storage_version,
        lance_dataset_kind(path),
        None,
    )
    .await
}

async fn write_lance_batch_with_mode_and_storage_version_for_kind_versioned(
    path: &Path,
    batch: RecordBatch,
    mode: WriteMode,
    storage_version: Option<LanceFileVersion>,
    kind: LanceDatasetKind,
    transaction_properties: Option<HashMap<String, String>>,
) -> Result<GraphTableVersion> {
    info!(
        dataset_path = %path.display(),
        rows = batch.num_rows(),
        mode = ?mode,
        "writing Lance dataset"
    );
    let batch = logical_batch_to_lance(&batch, kind)?;
    let schema = batch.schema();
    let uri = local_path_to_file_uri(path)?;

    let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
    let write_params = match storage_version {
        Some(version) => {
            let mut params = WriteParams::with_storage_version(version);
            params.mode = mode;
            params.enable_stable_row_ids = true;
            if let Some(properties) = transaction_properties {
                params.transaction_properties = Some(Arc::new(properties));
            }
            params
        }
        None => {
            let mut params = WriteParams {
                mode,
                enable_stable_row_ids: true,
                ..Default::default()
            };
            if let Some(properties) = transaction_properties {
                params.transaction_properties = Some(Arc::new(properties));
            }
            params
        }
    };

    let dataset = Dataset::write(reader, &uri, Some(write_params))
        .await
        .map_err(|e| NanoError::Lance(format!("write error: {}", e)))?;

    Ok(GraphTableVersion::new(
        graph_table_id_for_path(path),
        dataset.version().version,
    ))
}

pub(crate) async fn append_lance_batch_at_version(
    path: &Path,
    pinned_version: &GraphTableVersion,
    batch: RecordBatch,
) -> Result<GraphTableVersion> {
    append_lance_batch_at_version_for_kind(path, pinned_version, batch, lance_dataset_kind(path))
        .await
}

pub(crate) async fn append_lance_batch_at_version_for_kind(
    path: &Path,
    pinned_version: &GraphTableVersion,
    batch: RecordBatch,
    kind: LanceDatasetKind,
) -> Result<GraphTableVersion> {
    append_lance_batch_at_version_for_kind_with_properties(
        path,
        pinned_version,
        batch,
        kind,
        None,
    )
    .await
}

pub(crate) async fn append_lance_batch_at_version_for_kind_with_properties(
    path: &Path,
    pinned_version: &GraphTableVersion,
    batch: RecordBatch,
    kind: LanceDatasetKind,
    transaction_properties: Option<HashMap<String, String>>,
) -> Result<GraphTableVersion> {
    let batch = logical_batch_to_lance(&batch, kind)?;
    let uri = local_path_to_file_uri(path)?;
    let dataset = Dataset::open(&uri)
        .await
        .map_err(|e| NanoError::Lance(format!("append open error: {}", e)))?;
    let dataset = dataset
        .checkout_version(pinned_version.version)
        .await
        .map_err(|e| {
            NanoError::Lance(format!(
                "append checkout version {} error: {}",
                pinned_version.version, e
            ))
        })?;
    let mut params = WriteParams {
        mode: WriteMode::Append,
        enable_stable_row_ids: true,
        ..Default::default()
    };
    if let Some(properties) = transaction_properties {
        params.transaction_properties = Some(Arc::new(properties));
    }
    let appended = InsertBuilder::new(Arc::new(dataset))
        .with_params(&params)
        .execute(vec![batch])
        .await
        .map_err(|e| NanoError::Lance(format!("append error: {}", e)))?;
    Ok(GraphTableVersion::new(
        graph_table_id_for_path(path),
        appended.version().version,
    ))
}

#[allow(dead_code)]
pub(crate) async fn run_lance_merge_insert_with_key(
    dataset_path: &Path,
    pinned_version: u64,
    source_batch: RecordBatch,
    key_prop: &str,
) -> Result<u64> {
    Ok(run_lance_merge_insert_with_key_versioned(
        dataset_path,
        &GraphTableVersion::new(graph_table_id_for_path(dataset_path), pinned_version),
        source_batch,
        key_prop,
    )
    .await?
    .version)
}

async fn run_lance_merge_insert_with_key_versioned(
    dataset_path: &Path,
    pinned_version: &GraphTableVersion,
    source_batch: RecordBatch,
    key_prop: &str,
) -> Result<GraphTableVersion> {
    run_lance_merge_insert_with_key_versioned_for_kind(
        dataset_path,
        pinned_version,
        source_batch,
        key_prop,
        lance_dataset_kind(dataset_path),
    )
    .await
}

async fn run_lance_merge_insert_with_key_versioned_for_kind(
    dataset_path: &Path,
    pinned_version: &GraphTableVersion,
    source_batch: RecordBatch,
    key_prop: &str,
    kind: LanceDatasetKind,
) -> Result<GraphTableVersion> {
    let uri = local_path_to_file_uri(dataset_path)?;
    let dataset = Dataset::open(&uri)
        .await
        .map_err(|e| NanoError::Lance(format!("merge open error: {}", e)))?;
    let dataset = dataset
        .checkout_version(pinned_version.version)
        .await
        .map_err(|e| {
            NanoError::Lance(format!(
                "merge checkout version {} error: {}",
                pinned_version.version, e
            ))
        })?;

    let mut builder = MergeInsertBuilder::try_new(Arc::new(dataset), vec![key_prop.to_string()])
        .map_err(|e| NanoError::Lance(format!("merge builder error: {}", e)))?;
    builder
        .when_matched(WhenMatched::UpdateAll)
        .when_not_matched(WhenNotMatched::InsertAll)
        .conflict_retries(0);

    let source_batch = logical_batch_to_lance(&source_batch, kind)?;
    let source_schema = source_batch.schema();
    let source = Box::new(RecordBatchIterator::new(
        vec![Ok(source_batch)].into_iter(),
        source_schema,
    ));
    let job = builder
        .try_build()
        .map_err(|e| NanoError::Lance(format!("merge build error: {}", e)))?;
    let (merged_dataset, _) = job
        .execute_reader(source)
        .await
        .map_err(|e| NanoError::Lance(format!("merge execute error: {}", e)))?;

    Ok(GraphTableVersion::new(
        graph_table_id_for_path(dataset_path),
        merged_dataset.version().version,
    ))
}

#[allow(dead_code)]
pub(crate) async fn run_lance_delete_by_ids(
    dataset_path: &Path,
    pinned_version: u64,
    ids: &[u64],
) -> Result<u64> {
    Ok(run_lance_delete_by_ids_versioned(
        dataset_path,
        &GraphTableVersion::new(graph_table_id_for_path(dataset_path), pinned_version),
        ids,
    )
    .await?
    .version)
}

async fn run_lance_delete_by_ids_versioned(
    dataset_path: &Path,
    pinned_version: &GraphTableVersion,
    ids: &[u64],
) -> Result<GraphTableVersion> {
    if ids.is_empty() {
        return Ok(pinned_version.clone());
    }

    let uri = local_path_to_file_uri(dataset_path)?;
    let dataset = Dataset::open(&uri)
        .await
        .map_err(|e| NanoError::Lance(format!("delete open error: {}", e)))?;
    let mut dataset = dataset
        .checkout_version(pinned_version.version)
        .await
        .map_err(|e| {
            NanoError::Lance(format!(
                "delete checkout version {} error: {}",
                pinned_version.version, e
            ))
        })?;

    let predicate = ids
        .iter()
        .map(|id| format!("{} = {}", LANCE_INTERNAL_ID_FIELD, id))
        .collect::<Vec<_>>()
        .join(" OR ");
    dataset
        .delete(&predicate)
        .await
        .map_err(|e| NanoError::Lance(format!("delete execute error: {}", e)))?;

    Ok(GraphTableVersion::new(
        graph_table_id_for_path(dataset_path),
        dataset.version().version,
    ))
}

pub(crate) async fn latest_lance_dataset_version(path: &Path) -> Result<u64> {
    Ok(latest_lance_dataset_graph_version(path).await?.version)
}

async fn latest_lance_dataset_graph_version(path: &Path) -> Result<GraphTableVersion> {
    let uri = local_path_to_file_uri(path)?;
    let dataset = Dataset::open(&uri)
        .await
        .map_err(|e| NanoError::Lance(format!("open error: {}", e)))?;
    let version = dataset
        .latest_version_id()
        .await
        .map_err(|e| NanoError::Lance(format!("latest version error: {}", e)))?;
    Ok(GraphTableVersion::new(
        graph_table_id_for_path(path),
        version,
    ))
}

pub(crate) async fn cleanup_unpublished_manifest_versions(
    db_dir: &Path,
    dataset_location: &str,
    published_version: Option<u64>,
) -> Result<usize> {
    let dataset_uri = namespace_location_to_dataset_uri(db_dir, dataset_location)?;
    match Dataset::open(&dataset_uri).await {
        Ok(_dataset) => {}
        Err(_err) if published_version.is_none() => return Ok(0),
        Err(err) => return Err(NanoError::Lance(format!("cleanup open error: {}", err))),
    };
    let versions_dir = namespace_location_to_local_path(db_dir, dataset_location)?.join("_versions");
    let unpublished = std::fs::read_dir(versions_dir)?
    .filter_map(|entry| entry.ok())
    .filter_map(|entry| {
        let path = entry.path();
        let file_name = path.file_name()?.to_str()?;
        let version = lance_table::io::commit::ManifestNamingScheme::detect_scheme(file_name)
            .and_then(|scheme| scheme.parse_version(file_name))
            .or_else(|| {
                lance_table::io::commit::ManifestNamingScheme::parse_detached_version(file_name)
            })?;
        let should_remove = match published_version {
            Some(published_version) => version > published_version,
            None => true,
        };
        should_remove.then_some((path, version))
    })
    .collect::<Vec<_>>();
    if unpublished.is_empty() {
        return Ok(0);
    }

    for (path, _version) in &unpublished {
        std::fs::remove_file(path)?;
    }

    Ok(unpublished.len())
}

pub(crate) async fn open_dataset_for_locator(locator: &DatasetLocator) -> Result<Dataset> {
    if locator.namespace_managed {
        let namespace = open_directory_namespace(&locator.db_path).await?;
        let location = resolve_table_location(namespace, &locator.table_id).await?;
        let dataset_uri = namespace_location_to_dataset_uri(&locator.db_path, &location)?;
        let dataset = Dataset::open(&dataset_uri)
            .await
            .map_err(|e| {
                NanoError::Lance(format!(
                    "namespace dataset {} open error: {}",
                    locator.table_id, e
                ))
            })?;
        return dataset
            .checkout_version(locator.dataset_version)
            .await
            .map_err(|e| {
                NanoError::Lance(format!(
                    "namespace dataset {} version {} load error: {}",
                    locator.table_id, locator.dataset_version, e
                ))
            });
    } else {
        let uri = local_path_to_file_uri(&locator.dataset_path)?;
        let dataset = Dataset::open(&uri)
            .await
            .map_err(|e| NanoError::Lance(format!("open error: {}", e)))?;
        dataset
            .checkout_version(locator.dataset_version)
            .await
            .map_err(|e| {
                NanoError::Lance(format!(
                    "checkout version {} error: {}",
                    locator.dataset_version, e
                ))
            })
    }
}

pub(crate) async fn read_lance_batches_for_locator(
    locator: &DatasetLocator,
) -> Result<Vec<RecordBatch>> {
    let dataset = open_dataset_for_locator(locator).await?;
    let kind = lance_dataset_kind_for_locator(locator);
    let projected_columns: Vec<String> = dataset
        .schema()
        .fields
        .iter()
        .map(|field| field.name.clone())
        .collect();
    let mut scanner = dataset.scan();
    scanner
        .project(&projected_columns)
        .map_err(|e| NanoError::Lance(format!("projection error: {}", e)))?;
    scanner
        .try_into_stream()
        .await
        .map_err(|e| NanoError::Lance(format!("scan error: {}", e)))?
        .map(|batch| batch.map_err(|e| NanoError::Lance(format!("stream error: {}", e))))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|batch| lance_batch_to_logical(&batch, kind))
        .collect()
}

pub(crate) async fn read_lance_projected_batches_for_locator(
    locator: &DatasetLocator,
    columns: &[&str],
) -> Result<Vec<RecordBatch>> {
    if locator.namespace_managed {
        return read_lance_batches_for_locator(locator)
            .await?
            .into_iter()
            .map(|batch| project_logical_batch(&batch, columns))
            .collect();
    }

    let dataset = open_dataset_for_locator(locator).await?;
    let kind = lance_dataset_kind_for_locator(locator);
    let projected_columns: Vec<String> = columns
        .iter()
        .map(|column| match kind {
            LanceDatasetKind::Node => logical_node_field_to_lance(column),
            LanceDatasetKind::Edge => logical_edge_field_to_lance(column),
            LanceDatasetKind::Plain => column,
        })
        .map(ToString::to_string)
        .collect();

    let mut scanner = dataset.scan();
    scanner
        .project(&projected_columns)
        .map_err(|e| NanoError::Lance(format!("projection error: {}", e)))?;
    scanner
        .try_into_stream()
        .await
        .map_err(|e| NanoError::Lance(format!("scan error: {}", e)))?
        .map(|batch| batch.map_err(|e| NanoError::Lance(format!("stream error: {}", e))))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|batch| lance_batch_to_logical(&batch, kind))
        .collect()
}

async fn read_lance_batches_versioned(version: &GraphTableVersion) -> Result<Vec<RecordBatch>> {
    info!(
        dataset_path = %version.table_id.as_str(),
        dataset_version = version.version,
        "reading Lance dataset"
    );
    let path = Path::new(version.table_id.as_str());
    let uri = local_path_to_file_uri(path)?;
    let dataset = Dataset::open(&uri)
        .await
        .map_err(|e| NanoError::Lance(format!("open error: {}", e)))?;
    let dataset = dataset
        .checkout_version(version.version)
        .await
        .map_err(|e| {
            NanoError::Lance(format!("checkout version {} error: {}", version.version, e))
        })?;

    let kind = lance_dataset_kind(path);
    let projected_columns: Vec<String> = dataset
        .schema()
        .fields
        .iter()
        .map(|field| field.name.clone())
        .collect();
    let mut scanner = dataset.scan();
    scanner
        .project(&projected_columns)
        .map_err(|e| NanoError::Lance(format!("projection error: {}", e)))?;
    let batches: Vec<RecordBatch> = scanner
        .try_into_stream()
        .await
        .map_err(|e| NanoError::Lance(format!("scan error: {}", e)))?
        .map(|batch| batch.map_err(|e| NanoError::Lance(format!("stream error: {}", e))))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|batch| lance_batch_to_logical(&batch, kind))
        .collect::<Result<Vec<_>>>()?;

    Ok(batches)
}

pub(crate) async fn read_lance_projected_batches(
    path: &Path,
    version: u64,
    columns: &[&str],
) -> Result<Vec<RecordBatch>> {
    info!(
        dataset_path = %path.display(),
        dataset_version = version,
        projection = ?columns,
        "reading projected Lance dataset"
    );
    let uri = local_path_to_file_uri(path)?;
    let dataset = Dataset::open(&uri)
        .await
        .map_err(|e| NanoError::Lance(format!("open error: {}", e)))?;
    let dataset = dataset
        .checkout_version(version)
        .await
        .map_err(|e| NanoError::Lance(format!("checkout version {} error: {}", version, e)))?;

    let kind = lance_dataset_kind(path);
    let projected_columns: Vec<String> = columns
        .iter()
        .map(|column| match kind {
            LanceDatasetKind::Node => logical_node_field_to_lance(column),
            LanceDatasetKind::Edge => logical_edge_field_to_lance(column),
            LanceDatasetKind::Plain => column,
        })
        .map(ToString::to_string)
        .collect();

    let mut scanner = dataset.scan();
    scanner
        .project(&projected_columns)
        .map_err(|e| NanoError::Lance(format!("projection error: {}", e)))?;
    let batches: Vec<RecordBatch> = scanner
        .try_into_stream()
        .await
        .map_err(|e| NanoError::Lance(format!("scan error: {}", e)))?
        .map(|batch| batch.map_err(|e| NanoError::Lance(format!("stream error: {}", e))))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|batch| lance_batch_to_logical(&batch, kind))
        .collect::<Result<Vec<_>>>()?;

    Ok(batches)
}

#[async_trait]
impl TableStore for V3TableStore {
    async fn overwrite(&self, path: &Path, batch: RecordBatch) -> Result<GraphTableVersion> {
        write_lance_batch_with_mode_versioned(path, batch, WriteMode::Overwrite).await
    }

    async fn append(&self, path: &Path, batch: RecordBatch) -> Result<GraphTableVersion> {
        write_lance_batch_with_mode_versioned(path, batch, WriteMode::Append).await
    }

    async fn merge_insert_with_key(
        &self,
        path: &Path,
        pinned_version: &GraphTableVersion,
        source_batch: RecordBatch,
        key_prop: &str,
    ) -> Result<GraphTableVersion> {
        run_lance_merge_insert_with_key_versioned(path, pinned_version, source_batch, key_prop)
            .await
    }

    async fn delete_by_ids(
        &self,
        path: &Path,
        pinned_version: &GraphTableVersion,
        ids: &[u64],
    ) -> Result<GraphTableVersion> {
        run_lance_delete_by_ids_versioned(path, pinned_version, ids).await
    }

    async fn latest_version(&self, path: &Path) -> Result<GraphTableVersion> {
        latest_lance_dataset_graph_version(path).await
    }

    async fn read_all_batches(&self, version: &GraphTableVersion) -> Result<Vec<RecordBatch>> {
        read_lance_batches_versioned(version).await
    }
}

#[async_trait]
impl TableStore for V4NamespaceTableStore {
    async fn overwrite(&self, path: &Path, batch: RecordBatch) -> Result<GraphTableVersion> {
        let namespace = open_directory_namespace(&self.db_path).await?;
        let table_id = self.table_id_for_path(path)?;
        let location = resolve_or_declare_table_location(namespace, &table_id).await?;
        let location_path = namespace_location_to_local_path(&self.db_path, &location)?;
        write_lance_batch_with_mode_for_kind_versioned(
            &location_path,
            batch,
            WriteMode::Overwrite,
            lance_dataset_kind_for_table_id(&table_id),
        )
        .await
    }

    async fn append(&self, path: &Path, batch: RecordBatch) -> Result<GraphTableVersion> {
        let namespace = open_directory_namespace(&self.db_path).await?;
        let table_id = self.table_id_for_path(path)?;
        let published = namespace_latest_version(namespace.clone(), &table_id).await?;
        let location = resolve_or_declare_table_location(namespace, &table_id).await?;
        cleanup_unpublished_manifest_versions(&self.db_path, &location, Some(published.version))
            .await?;
        let location_path = namespace_location_to_local_path(&self.db_path, &location)?;
        append_lance_batch_at_version_for_kind(
            &location_path,
            &published,
            batch,
            lance_dataset_kind_for_table_id(&table_id),
        )
        .await
    }

    async fn merge_insert_with_key(
        &self,
        path: &Path,
        pinned_version: &GraphTableVersion,
        source_batch: RecordBatch,
        key_prop: &str,
    ) -> Result<GraphTableVersion> {
        let table_id = self.table_id_for_path(path)?;
        let namespace = open_directory_namespace(&self.db_path).await?;
        let location = resolve_or_declare_table_location(namespace, &table_id).await?;
        cleanup_unpublished_manifest_versions(&self.db_path, &location, Some(pinned_version.version))
            .await?;
        let location_path = namespace_location_to_local_path(&self.db_path, &location)?;
        run_lance_merge_insert_with_key_versioned_for_kind(
            &location_path,
            pinned_version,
            source_batch,
            key_prop,
            lance_dataset_kind_for_table_id(&table_id),
        )
        .await
    }

    async fn delete_by_ids(
        &self,
        path: &Path,
        pinned_version: &GraphTableVersion,
        ids: &[u64],
    ) -> Result<GraphTableVersion> {
        let table_id = self.table_id_for_path(path)?;
        let namespace = open_directory_namespace(&self.db_path).await?;
        let location = resolve_or_declare_table_location(namespace, &table_id).await?;
        cleanup_unpublished_manifest_versions(&self.db_path, &location, Some(pinned_version.version))
            .await?;
        let location_path = namespace_location_to_local_path(&self.db_path, &location)?;
        run_lance_delete_by_ids_versioned(&location_path, pinned_version, ids).await
    }

    async fn latest_version(&self, path: &Path) -> Result<GraphTableVersion> {
        let namespace = open_directory_namespace(&self.db_path).await?;
        let table_id = self.table_id_for_path(path)?;
        namespace_latest_version(namespace, &table_id).await
    }

    async fn read_all_batches(&self, version: &GraphTableVersion) -> Result<Vec<RecordBatch>> {
        read_lance_batches_versioned(version).await
    }
}
