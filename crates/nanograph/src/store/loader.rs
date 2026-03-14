use std::io::{BufRead, Cursor};
use std::path::Path;

use crate::catalog::schema_ir::SchemaIR;
use crate::error::{NanoError, Result};

use super::database::LoadMode;
use super::graph::GraphStorage;

mod constraints;
mod embeddings;
mod jsonl;
mod merge;

pub(crate) use embeddings::{
    EmbedSpec, EmbedValueRequest, collect_embed_specs, resolve_embedding_requests,
};
pub(crate) use jsonl::{
    json_values_to_array, load_jsonl_reader_with_name_seed_at_path, parse_date32_literal,
    parse_date64_literal,
};

/// Build the next storage snapshot for a `Database::load` operation.
///
/// Behavior:
/// - `overwrite`: full replacement with incoming data.
/// - `append`: append incoming nodes/edges to existing data.
/// - `merge`: keyed node merge with stable IDs and edge endpoint remap.
/// - Always enforces node-level `@unique`/`@key` uniqueness before returning.
pub(crate) async fn build_next_storage_for_load(
    db_path: &Path,
    existing: &GraphStorage,
    schema_ir: &SchemaIR,
    data_source: &str,
    mode: LoadMode,
) -> Result<GraphStorage> {
    let reader = Cursor::new(data_source.as_bytes());
    build_next_storage_for_load_reader(db_path, existing, schema_ir, reader, mode).await
}

pub(crate) async fn build_next_storage_for_load_reader<R: BufRead>(
    db_path: &Path,
    existing: &GraphStorage,
    schema_ir: &SchemaIR,
    reader: R,
    mode: LoadMode,
) -> Result<GraphStorage> {
    let annotations = constraints::load_node_constraint_annotations(schema_ir)?;
    let key_props = annotations.key_props;
    if mode == LoadMode::Merge && key_props.is_empty() {
        return Err(NanoError::Storage(
            "load mode 'merge' requires at least one node @key property in schema".to_string(),
        ));
    }

    let mut incoming_storage = GraphStorage::new(existing.catalog.clone());
    if matches!(mode, LoadMode::Merge | LoadMode::Append) {
        incoming_storage.set_next_node_id(existing.next_node_id());
    }
    let key_seed = match mode {
        LoadMode::Overwrite => None,
        LoadMode::Merge => Some(constraints::build_name_seed_for_keyed_load(
            existing, &key_props,
        )?),
        LoadMode::Append => Some(constraints::build_name_seed_for_append(
            existing, &key_props,
        )?),
    };

    if embeddings::has_embedding_specs(schema_ir) {
        let materialized_path =
            embeddings::materialize_embeddings_for_load_to_tempfile(db_path, schema_ir, reader)
                .await?;
        let file = std::fs::File::open(&materialized_path)?;
        let buffered = std::io::BufReader::new(file);
        load_jsonl_reader_with_name_seed_at_path(
            &mut incoming_storage,
            db_path,
            buffered,
            &key_props,
            key_seed.as_ref(),
        )?;
        let _ = std::fs::remove_file(materialized_path);
    } else {
        load_jsonl_reader_with_name_seed_at_path(
            &mut incoming_storage,
            db_path,
            reader,
            &key_props,
            key_seed.as_ref(),
        )?;
    }

    let mut next_storage = match mode {
        LoadMode::Overwrite => incoming_storage,
        LoadMode::Merge => {
            merge::merge_storage_with_node_keys(
                db_path,
                existing,
                &incoming_storage,
                schema_ir,
                &key_props,
            )
            .await?
        }
        LoadMode::Append => merge::append_storage(existing, &incoming_storage, schema_ir)?,
    };

    constraints::enforce_node_unique_constraints(&next_storage, &annotations.unique_props)?;
    next_storage.build_indices()?;
    Ok(next_storage)
}
