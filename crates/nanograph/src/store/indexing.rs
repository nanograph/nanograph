use std::path::Path;
use std::sync::OnceLock;

use lance::Dataset;
use lance::index::vector::VectorIndexParams;
use lance_index::scalar::ScalarIndexParams;
use lance_index::scalar::inverted::InvertedIndexParams;
use lance_index::{DatasetIndexExt, IndexType};
use lance_linalg::distance::MetricType;
use tokio::sync::Mutex;
use tracing::debug;

use crate::catalog::schema_ir::NodeTypeDef;
use crate::error::{NanoError, Result};
use crate::store::namespace::local_path_to_file_uri;
use crate::types::ScalarType;

const SCALAR_INDEX_SUFFIX: &str = "_btree_idx";
const TEXT_INDEX_SUFFIX: &str = "_fts_idx";
const VECTOR_INDEX_SUFFIX: &str = "_ivfpq_idx";
const VECTOR_INDEX_MAX_PARTITIONS: usize = 256;
const VECTOR_INDEX_PQ_BITS: u8 = 8;
const VECTOR_INDEX_PQ_MIN_ROWS: usize = 1 << VECTOR_INDEX_PQ_BITS;
const VECTOR_INDEX_PQ_TRAIN_MAX_ITERS: usize = 50;
static SCALAR_INDEX_BUILD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static VECTOR_INDEX_BUILD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn scalar_index_name(type_id: u32, property: &str) -> String {
    format!("nano_{:08x}_{}{}", type_id, property, SCALAR_INDEX_SUFFIX)
}

pub fn vector_index_name(type_id: u32, property: &str) -> String {
    format!("nano_{:08x}_{}{}", type_id, property, VECTOR_INDEX_SUFFIX)
}

pub fn text_index_name(type_id: u32, property: &str) -> String {
    format!("nano_{:08x}_{}{}", type_id, property, TEXT_INDEX_SUFFIX)
}

pub(crate) async fn rebuild_node_scalar_indexes(
    dataset_path: &Path,
    node_def: &NodeTypeDef,
) -> Result<()> {
    let indexed_props: Vec<&str> = node_def
        .properties
        .iter()
        .filter(|prop| {
            prop.index
                && !prop.list
                && !matches!(
                    ScalarType::from_str_name(&prop.scalar_type),
                    Some(ScalarType::Vector(_))
                )
        })
        .map(|prop| prop.name.as_str())
        .collect();
    if indexed_props.is_empty() {
        return Ok(());
    }

    // Lance scalar index builds use a shared memory pool. Building multiple indexes
    // concurrently across tests/process tasks can exhaust that pool for tiny workloads.
    // Serialize builds to keep resource usage predictable.
    let build_lock = SCALAR_INDEX_BUILD_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = build_lock.lock().await;

    let uri = local_path_to_file_uri(dataset_path)?;
    let mut dataset = Dataset::open(&uri)
        .await
        .map_err(|e| NanoError::Lance(format!("open error: {}", e)))?;

    let index_params = ScalarIndexParams::default();
    for prop in indexed_props {
        let index_name = scalar_index_name(node_def.type_id, prop);
        dataset
            .create_index(
                &[prop],
                IndexType::Scalar,
                Some(index_name.clone()),
                &index_params,
                true,
            )
            .await
            .map_err(|e| {
                NanoError::Lance(format!(
                    "create scalar index `{}` on {}.{} failed: {}",
                    index_name, node_def.name, prop, e
                ))
            })?;
        debug!(
            node_type = %node_def.name,
            property = %prop,
            index_name = %index_name,
            "created/replaced scalar index"
        );
    }

    Ok(())
}

pub(crate) async fn rebuild_node_text_indexes(
    dataset_path: &Path,
    node_def: &NodeTypeDef,
) -> Result<()> {
    let indexed_props: Vec<&str> = node_def
        .properties
        .iter()
        .filter(|prop| prop.index && !prop.list)
        .filter_map(|prop| match ScalarType::from_str_name(&prop.scalar_type) {
            Some(ScalarType::String) => Some(prop.name.as_str()),
            _ => None,
        })
        .collect();
    if indexed_props.is_empty() {
        return Ok(());
    }

    let build_lock = SCALAR_INDEX_BUILD_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = build_lock.lock().await;

    let uri = local_path_to_file_uri(dataset_path)?;
    let mut dataset = Dataset::open(&uri)
        .await
        .map_err(|e| NanoError::Lance(format!("open error: {}", e)))?;

    let index_params = InvertedIndexParams::default()
        .with_position(true)
        .stem(false)
        .remove_stop_words(false)
        .ascii_folding(false);
    for prop in indexed_props {
        let index_name = text_index_name(node_def.type_id, prop);
        dataset
            .create_index(
                &[prop],
                IndexType::Inverted,
                Some(index_name.clone()),
                &index_params,
                true,
            )
            .await
            .map_err(|e| {
                NanoError::Lance(format!(
                    "create text index `{}` on {}.{} failed: {}",
                    index_name, node_def.name, prop, e
                ))
            })?;
        debug!(
            node_type = %node_def.name,
            property = %prop,
            index_name = %index_name,
            with_position = true,
            stem = false,
            remove_stop_words = false,
            "created/replaced text index"
        );
    }

    Ok(())
}

fn indexed_vector_properties(node_def: &NodeTypeDef) -> Vec<(&str, usize)> {
    node_def
        .properties
        .iter()
        .filter(|prop| prop.index && !prop.list)
        .filter_map(|prop| match ScalarType::from_str_name(&prop.scalar_type) {
            Some(ScalarType::Vector(dim)) if dim > 0 => Some((prop.name.as_str(), dim as usize)),
            _ => None,
        })
        .collect()
}

fn choose_ivf_partitions(row_count: usize) -> usize {
    if row_count <= 1024 {
        return 1;
    }
    let approx = (row_count as f64).sqrt().round() as usize;
    approx.clamp(1, VECTOR_INDEX_MAX_PARTITIONS)
}

fn choose_pq_sub_vectors(dim: usize) -> usize {
    for candidate in [32, 16, 8, 4, 2, 1] {
        if candidate <= dim && dim.is_multiple_of(candidate) {
            return candidate;
        }
    }
    1
}

pub(crate) async fn rebuild_node_vector_indexes(
    dataset_path: &Path,
    node_def: &NodeTypeDef,
) -> Result<()> {
    let indexed_props = indexed_vector_properties(node_def);
    if indexed_props.is_empty() {
        return Ok(());
    }

    // Serialize vector index builds to avoid resource spikes in test workloads.
    let build_lock = VECTOR_INDEX_BUILD_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = build_lock.lock().await;

    let uri = local_path_to_file_uri(dataset_path)?;
    let mut dataset = Dataset::open(&uri)
        .await
        .map_err(|e| NanoError::Lance(format!("open error: {}", e)))?;
    let row_count = dataset
        .count_rows(None)
        .await
        .map_err(|e| NanoError::Lance(format!("count rows error: {}", e)))?;

    if row_count == 0 {
        return Ok(());
    }

    let num_partitions = choose_ivf_partitions(row_count);
    for (prop, dim) in indexed_props {
        let num_sub_vectors = choose_pq_sub_vectors(dim);
        let index_name = vector_index_name(node_def.type_id, prop);
        let (index_params, index_kind) = if row_count >= VECTOR_INDEX_PQ_MIN_ROWS {
            (
                VectorIndexParams::ivf_pq(
                    num_partitions,
                    VECTOR_INDEX_PQ_BITS,
                    num_sub_vectors,
                    MetricType::Cosine,
                    VECTOR_INDEX_PQ_TRAIN_MAX_ITERS,
                ),
                "IVF_PQ",
            )
        } else {
            // PQ training needs enough rows; for tiny datasets use IVF_FLAT and
            // keep the same cosine semantics.
            (
                VectorIndexParams::ivf_flat(num_partitions, MetricType::Cosine),
                "IVF_FLAT",
            )
        };
        dataset
            .create_index(
                &[prop],
                IndexType::Vector,
                Some(index_name.clone()),
                &index_params,
                true,
            )
            .await
            .map_err(|e| {
                NanoError::Lance(format!(
                    "create vector index `{}` on {}.{} failed: {}",
                    index_name, node_def.name, prop, e
                ))
            })?;
        debug!(
            node_type = %node_def.name,
            property = %prop,
            index_name = %index_name,
            partitions = num_partitions,
            sub_vectors = num_sub_vectors,
            index_kind = index_kind,
            metric = "cosine",
            "created/replaced vector index"
        );
    }

    Ok(())
}
