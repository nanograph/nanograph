use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::path::Path;

use arrow_array::RecordBatch;
use lance::dataset::WriteMode;
use serde_json::{Map as JsonMap, Value as JsonValue};
use tracing::{debug, info};

use super::{
    Database, DatabaseWriteGuard, EmbedOptions, EmbedResult, LoadMode, MutationPlan, MutationSource,
};
use crate::catalog::schema_ir::SchemaIR;
use crate::error::{NanoError, Result};
use crate::json_output::array_value_to_json;
use crate::store::graph::GraphStorage;
use crate::store::indexing::{rebuild_node_scalar_indexes, rebuild_node_vector_indexes};
use crate::store::lance_io::{
    run_lance_delete_by_ids, run_lance_merge_insert_with_key, write_lance_batch,
    write_lance_batch_with_mode,
};
use crate::store::loader::{
    EmbedValueRequest, build_next_storage_for_load, build_next_storage_for_load_reader,
    collect_embed_specs, json_values_to_array, resolve_embedding_requests,
};
use crate::store::manifest::{DatasetEntry, GraphManifest, hash_string};
use crate::store::txlog::{CdcLogEntry, commit_manifest_and_logs};
use crate::types::ScalarType;

use super::cdc::{build_cdc_events_for_storage_transition, deleted_ids_from_cdc_events};

#[derive(Debug, Clone)]
struct SelectedEmbedProp {
    target_prop: String,
    source_prop: String,
    dim: usize,
    indexed: bool,
}

impl Database {
    /// Load JSONL data using compatibility defaults:
    /// - any `@key` in schema => `LoadMode::Merge`
    /// - no `@key` in schema => `LoadMode::Overwrite`
    pub async fn load(&self, data_source: &str) -> Result<()> {
        let mode = if self
            .schema_ir
            .node_types()
            .any(|node| node.properties.iter().any(|prop| prop.key))
        {
            LoadMode::Merge
        } else {
            LoadMode::Overwrite
        };
        self.load_with_mode(data_source, mode).await
    }

    /// Load JSONL data using explicit semantics.
    pub async fn load_with_mode(&self, data_source: &str, mode: LoadMode) -> Result<()> {
        let mut writer = self.lock_writer().await;
        self.load_with_mode_locked(data_source, mode, &mut writer)
            .await
    }

    /// Load JSONL data from a file using compatibility defaults.
    pub async fn load_file(&self, data_path: &Path) -> Result<()> {
        let mode = if self
            .schema_ir
            .node_types()
            .any(|node| node.properties.iter().any(|prop| prop.key))
        {
            LoadMode::Merge
        } else {
            LoadMode::Overwrite
        };
        self.load_file_with_mode(data_path, mode).await
    }

    /// Load JSONL data from a file using explicit semantics.
    pub async fn load_file_with_mode(&self, data_path: &Path, mode: LoadMode) -> Result<()> {
        let mut writer = self.lock_writer().await;
        self.load_file_with_mode_locked(data_path, mode, &mut writer)
            .await
    }

    async fn load_with_mode_locked(
        &self,
        data_source: &str,
        mode: LoadMode,
        writer: &mut DatabaseWriteGuard<'_>,
    ) -> Result<()> {
        info!("starting database load");
        self.apply_mutation_plan_locked(MutationPlan::for_load(data_source, mode), writer)
            .await?;
        let storage = self.snapshot();
        info!(
            mode = ?mode,
            node_types = storage.node_segments.len(),
            edge_types = storage.edge_segments.len(),
            "database load complete"
        );

        Ok(())
    }

    async fn load_file_with_mode_locked(
        &self,
        data_path: &Path,
        mode: LoadMode,
        writer: &mut DatabaseWriteGuard<'_>,
    ) -> Result<()> {
        info!(data_path = %data_path.display(), "starting database file load");
        self.apply_mutation_plan_locked(MutationPlan::for_load_file(data_path, mode), writer)
            .await?;
        let storage = self.snapshot();
        info!(
            mode = ?mode,
            node_types = storage.node_segments.len(),
            edge_types = storage.edge_segments.len(),
            "database file load complete"
        );

        Ok(())
    }

    /// Apply one append-only mutation payload through the unified mutation path.
    pub async fn apply_append_mutation(&self, data_source: &str, op_summary: &str) -> Result<()> {
        let mut writer = self.lock_writer().await;
        self.apply_append_mutation_locked(data_source, op_summary, &mut writer)
            .await
    }

    /// Apply one keyed-merge mutation payload through the unified mutation path.
    pub async fn apply_merge_mutation(&self, data_source: &str, op_summary: &str) -> Result<()> {
        let mut writer = self.lock_writer().await;
        self.apply_merge_mutation_locked(data_source, op_summary, &mut writer)
            .await
    }

    pub async fn embed(&self, options: EmbedOptions) -> Result<EmbedResult> {
        if options.property.is_some() && options.type_name.is_none() {
            return Err(NanoError::Execution(
                "--property requires --type for `nanograph embed`".to_string(),
            ));
        }

        let embed_specs = collect_embed_specs(&self.schema_ir)?;
        let selected_types = select_embed_types(
            &self.schema_ir,
            &embed_specs,
            options.type_name.as_deref(),
            options.property.as_deref(),
        )?;

        let reindexable_types: HashSet<String> = selected_types
            .iter()
            .filter(|(_, props)| props.iter().any(|prop| prop.indexed))
            .map(|(node_def, _)| node_def.name.clone())
            .collect();

        let current = self.snapshot();
        let mut next_storage = current.as_ref().clone();
        let mut rows_selected = 0usize;
        let mut embeddings_generated = 0usize;
        let mut remaining_limit = options.limit.unwrap_or(usize::MAX);
        let mut touched_types = HashSet::new();

        for (node_def, props) in &selected_types {
            if remaining_limit == 0 {
                break;
            }

            let Some(batch) = current.get_all_nodes(&node_def.name)? else {
                continue;
            };

            let mut rows = record_batch_to_json_rows(&batch);
            let mut requests = Vec::new();
            let mut assignments = Vec::new();

            for (row_idx, row) in rows.iter().enumerate() {
                if remaining_limit == 0 {
                    break;
                }

                let mut row_assignments = Vec::new();
                for prop in props {
                    let current_value = row.get(&prop.target_prop).unwrap_or(&JsonValue::Null);
                    let should_generate = if options.only_null {
                        current_value.is_null()
                    } else {
                        true
                    };
                    if !should_generate {
                        continue;
                    }

                    let source_text = row
                        .get(&prop.source_prop)
                        .and_then(JsonValue::as_str)
                        .ok_or_else(|| {
                            NanoError::Execution(format!(
                                "cannot embed {}.{}: source property {} must be a String",
                                node_def.name, prop.target_prop, prop.source_prop
                            ))
                        })?;

                    row_assignments.push((
                        prop.target_prop.clone(),
                        source_text.to_string(),
                        prop.dim,
                    ));
                }

                if row_assignments.is_empty() {
                    continue;
                }

                rows_selected += 1;
                remaining_limit = remaining_limit.saturating_sub(1);
                for (target_prop, source_text, dim) in row_assignments {
                    requests.push(EmbedValueRequest { source_text, dim });
                    assignments.push((row_idx, target_prop));
                    embeddings_generated += 1;
                }
            }

            if requests.is_empty() {
                continue;
            }

            touched_types.insert(node_def.name.clone());
            if options.dry_run {
                continue;
            }

            let vectors = resolve_embedding_requests(self.path(), &requests).await?;
            for ((row_idx, target_prop), vector) in assignments.into_iter().zip(vectors.into_iter())
            {
                rows[row_idx].insert(
                    target_prop,
                    serde_json::to_value(vector).map_err(|e| {
                        NanoError::Storage(format!("serialize embedding vector failed: {}", e))
                    })?,
                );
            }

            let rebuilt = json_rows_to_record_batch(batch.schema().as_ref(), &rows)?;
            next_storage.replace_node_batch(&node_def.name, rebuilt)?;
        }

        let properties_selected = selected_types.iter().map(|(_, props)| props.len()).sum();
        let reindexed_types = if options.dry_run {
            if options.reindex {
                reindexable_types.len()
            } else {
                reindexable_types
                    .iter()
                    .filter(|type_name| touched_types.contains(*type_name))
                    .count()
            }
        } else if !touched_types.is_empty() {
            let mut writer = self.lock_writer().await;
            self.apply_mutation_plan_locked(
                MutationPlan {
                    source: MutationSource::PreparedStorage(Box::new(next_storage)),
                    op_summary: "embed".to_string(),
                    cdc_events: Vec::new(),
                },
                &mut writer,
            )
            .await?;
            reindexable_types
                .iter()
                .filter(|type_name| touched_types.contains(*type_name))
                .count()
        } else if options.reindex {
            self.rebuild_vector_indexes_for_types(&reindexable_types)
                .await?
        } else {
            0
        };

        Ok(EmbedResult {
            node_types_considered: selected_types.len(),
            properties_selected,
            rows_selected,
            embeddings_generated,
            reindexed_types,
            dry_run: options.dry_run,
        })
    }

    pub(crate) async fn apply_append_mutation_locked(
        &self,
        data_source: &str,
        op_summary: &str,
        writer: &mut DatabaseWriteGuard<'_>,
    ) -> Result<()> {
        self.apply_mutation_plan_locked(
            MutationPlan::append_mutation(data_source, op_summary),
            writer,
        )
        .await
    }

    pub(crate) async fn apply_merge_mutation_locked(
        &self,
        data_source: &str,
        op_summary: &str,
        writer: &mut DatabaseWriteGuard<'_>,
    ) -> Result<()> {
        self.apply_mutation_plan_locked(
            MutationPlan::merge_mutation(data_source, op_summary),
            writer,
        )
        .await
    }

    pub(super) async fn apply_mutation_plan_locked(
        &self,
        plan: MutationPlan,
        _writer: &mut DatabaseWriteGuard<'_>,
    ) -> Result<()> {
        let MutationPlan {
            source,
            op_summary,
            cdc_events,
        } = plan;
        let previous_storage = self.snapshot();
        let mut next_storage = match source {
            MutationSource::LoadString { mode, data_source } => {
                build_next_storage_for_load(
                    &self.path,
                    previous_storage.as_ref(),
                    &self.schema_ir,
                    &data_source,
                    mode,
                )
                .await?
            }
            MutationSource::LoadFile { mode, data_path } => {
                let file = std::fs::File::open(&data_path)?;
                let reader = BufReader::new(file);
                build_next_storage_for_load_reader(
                    &self.path,
                    previous_storage.as_ref(),
                    &self.schema_ir,
                    reader,
                    mode,
                )
                .await?
            }
            MutationSource::PreparedStorage(storage) => *storage,
        };
        let effective_cdc_events = if cdc_events.is_empty() {
            build_cdc_events_for_storage_transition(
                previous_storage.as_ref(),
                &next_storage,
                &self.schema_ir,
            )?
        } else {
            cdc_events
        };
        self.persist_storage_with_cdc(&mut next_storage, &op_summary, &effective_cdc_events)
            .await?;
        self.replace_storage(next_storage);
        Ok(())
    }

    async fn persist_storage_with_cdc(
        &self,
        storage: &mut GraphStorage,
        op_summary: &str,
        cdc_events: &[CdcLogEntry],
    ) -> Result<()> {
        let previous_manifest = GraphManifest::read(&self.path)?;
        storage.clear_node_dataset_paths();
        let mut dataset_entries = Vec::new();
        let mut previous_entries_by_key: HashMap<String, DatasetEntry> = HashMap::new();
        for entry in &previous_manifest.datasets {
            previous_entries_by_key.insert(
                dataset_entity_key(&entry.kind, &entry.type_name),
                entry.clone(),
            );
        }

        let mut changed_entities: HashSet<String> = HashSet::new();
        let mut non_insert_entities: HashSet<String> = HashSet::new();
        let mut non_upsert_entities: HashSet<String> = HashSet::new();
        let mut non_delete_entities: HashSet<String> = HashSet::new();
        let mut insert_events_by_entity: HashMap<String, Vec<&CdcLogEntry>> = HashMap::new();
        let mut upsert_events_by_entity: HashMap<String, Vec<&CdcLogEntry>> = HashMap::new();
        let mut delete_events_by_entity: HashMap<String, Vec<&CdcLogEntry>> = HashMap::new();
        for event in cdc_events {
            let key = dataset_entity_key(&event.entity_kind, &event.type_name);
            changed_entities.insert(key.clone());
            if event.op == "insert" {
                insert_events_by_entity
                    .entry(key.clone())
                    .or_default()
                    .push(event);
                upsert_events_by_entity.entry(key).or_default().push(event);
                non_delete_entities
                    .insert(dataset_entity_key(&event.entity_kind, &event.type_name));
            } else if event.op == "update" {
                non_insert_entities.insert(key.clone());
                upsert_events_by_entity.entry(key).or_default().push(event);
                non_delete_entities
                    .insert(dataset_entity_key(&event.entity_kind, &event.type_name));
            } else if event.op == "delete" {
                non_insert_entities.insert(key.clone());
                non_upsert_entities.insert(key.clone());
                delete_events_by_entity.entry(key).or_default().push(event);
            } else {
                non_insert_entities.insert(key.clone());
                non_upsert_entities.insert(key);
                non_delete_entities
                    .insert(dataset_entity_key(&event.entity_kind, &event.type_name));
            }
        }
        let append_only_commit = op_summary == "load:append";
        let merge_commit = op_summary == "load:merge" || op_summary == "mutation:update_node";

        for node_def in self.schema_ir.node_types() {
            if let Some(batch) = storage.get_all_nodes(&node_def.name)? {
                let entity_key = dataset_entity_key("node", &node_def.name);
                let previous_entry = previous_entries_by_key.get(&entity_key).cloned();

                if !changed_entities.contains(&entity_key)
                    && let Some(prev) = previous_entry
                {
                    storage
                        .set_node_dataset_path(&node_def.name, self.path.join(&prev.dataset_path));
                    dataset_entries.push(prev);
                    continue;
                }

                let row_count = batch.num_rows() as u64;
                let dataset_rel_path = previous_entry
                    .as_ref()
                    .map(|entry| entry.dataset_path.clone())
                    .unwrap_or_else(|| format!("nodes/{}", SchemaIR::dir_name(node_def.type_id)));
                let dataset_path = self.path.join(&dataset_rel_path);
                let duplicate_field_names =
                    schema_has_duplicate_field_names(batch.schema().as_ref());
                let key_prop = node_def
                    .properties
                    .iter()
                    .find(|prop| prop.key)
                    .map(|prop| prop.name.as_str());
                let can_merge_upsert = merge_commit
                    && !duplicate_field_names
                    && previous_entry.is_some()
                    && key_prop.is_some()
                    && changed_entities.contains(&entity_key)
                    && !non_upsert_entities.contains(&entity_key);
                let can_append = append_only_commit
                    && !duplicate_field_names
                    && previous_entry.is_some()
                    && changed_entities.contains(&entity_key)
                    && !non_insert_entities.contains(&entity_key);
                let can_native_delete = previous_entry.is_some()
                    && changed_entities.contains(&entity_key)
                    && !non_delete_entities.contains(&entity_key);
                let dataset_version = if can_merge_upsert {
                    let upsert_events = upsert_events_by_entity
                        .get(&entity_key)
                        .map(|rows| rows.as_slice())
                        .unwrap_or(&[]);
                    match build_upsert_batch_from_cdc(batch.schema(), upsert_events)? {
                        Some(source_batch) if source_batch.num_rows() > 0 => {
                            let key_prop = key_prop.unwrap_or_default();
                            let pinned_version = previous_entry
                                .as_ref()
                                .map(|entry| entry.dataset_version)
                                .ok_or_else(|| {
                                    NanoError::Storage(format!(
                                        "missing previous dataset version for {}",
                                        node_def.name
                                    ))
                                })?;
                            debug!(
                                node_type = %node_def.name,
                                rows = source_batch.num_rows(),
                                key_prop = key_prop,
                                "merging node rows into existing Lance dataset"
                            );
                            run_lance_merge_insert_with_key(
                                &dataset_path,
                                pinned_version,
                                source_batch,
                                key_prop,
                            )
                            .await?
                        }
                        _ => previous_entry
                            .as_ref()
                            .map(|entry| entry.dataset_version)
                            .unwrap_or(0),
                    }
                } else if can_append {
                    let insert_events = insert_events_by_entity
                        .get(&entity_key)
                        .map(|rows| rows.as_slice())
                        .unwrap_or(&[]);
                    match build_append_batch_from_cdc(batch.schema(), insert_events)? {
                        Some(delta_batch) if delta_batch.num_rows() > 0 => {
                            debug!(
                                node_type = %node_def.name,
                                rows = delta_batch.num_rows(),
                                "appending node rows to existing Lance dataset"
                            );
                            write_lance_batch_with_mode(
                                &dataset_path,
                                delta_batch,
                                WriteMode::Append,
                            )
                            .await?
                        }
                        _ => previous_entry
                            .as_ref()
                            .map(|entry| entry.dataset_version)
                            .unwrap_or(0),
                    }
                } else if can_native_delete {
                    let delete_events = delete_events_by_entity
                        .get(&entity_key)
                        .map(|rows| rows.as_slice())
                        .unwrap_or(&[]);
                    let delete_ids = deleted_ids_from_cdc_events(delete_events)?;
                    let pinned_version = previous_entry
                        .as_ref()
                        .map(|entry| entry.dataset_version)
                        .ok_or_else(|| {
                            NanoError::Storage(format!(
                                "missing previous dataset version for {}",
                                node_def.name
                            ))
                        })?;
                    debug!(
                        node_type = %node_def.name,
                        rows = delete_ids.len(),
                        "deleting node rows from existing Lance dataset"
                    );
                    run_lance_delete_by_ids(&dataset_path, pinned_version, &delete_ids).await?
                } else {
                    debug!(
                        node_type = %node_def.name,
                        rows = row_count,
                        "writing node dataset"
                    );
                    write_lance_batch(&dataset_path, batch).await?
                };
                rebuild_node_scalar_indexes(&dataset_path, node_def).await?;
                rebuild_node_vector_indexes(&dataset_path, node_def).await?;
                storage.set_node_dataset_path(&node_def.name, dataset_path.clone());
                dataset_entries.push(DatasetEntry {
                    type_id: node_def.type_id,
                    type_name: node_def.name.clone(),
                    kind: "node".to_string(),
                    dataset_path: dataset_rel_path,
                    dataset_version,
                    row_count,
                });
            }
        }

        for edge_def in self.schema_ir.edge_types() {
            if let Some(batch) = storage.edge_batch_for_save(&edge_def.name)? {
                let entity_key = dataset_entity_key("edge", &edge_def.name);
                let previous_entry = previous_entries_by_key.get(&entity_key).cloned();

                if !changed_entities.contains(&entity_key)
                    && let Some(prev) = previous_entry
                {
                    dataset_entries.push(prev);
                    continue;
                }

                let row_count = batch.num_rows() as u64;
                let dataset_rel_path = previous_entry
                    .as_ref()
                    .map(|entry| entry.dataset_path.clone())
                    .unwrap_or_else(|| format!("edges/{}", SchemaIR::dir_name(edge_def.type_id)));
                let dataset_path = self.path.join(&dataset_rel_path);
                let duplicate_field_names =
                    schema_has_duplicate_field_names(batch.schema().as_ref());
                let can_append = append_only_commit
                    && !duplicate_field_names
                    && previous_entry.is_some()
                    && changed_entities.contains(&entity_key)
                    && !non_insert_entities.contains(&entity_key);
                let can_native_delete = previous_entry.is_some()
                    && changed_entities.contains(&entity_key)
                    && !non_delete_entities.contains(&entity_key);
                let dataset_version = if can_append {
                    let insert_events = insert_events_by_entity
                        .get(&entity_key)
                        .map(|rows| rows.as_slice())
                        .unwrap_or(&[]);
                    match build_append_batch_from_cdc(batch.schema(), insert_events)? {
                        Some(delta_batch) if delta_batch.num_rows() > 0 => {
                            debug!(
                                edge_type = %edge_def.name,
                                rows = delta_batch.num_rows(),
                                "appending edge rows to existing Lance dataset"
                            );
                            write_lance_batch_with_mode(
                                &dataset_path,
                                delta_batch,
                                WriteMode::Append,
                            )
                            .await?
                        }
                        _ => previous_entry
                            .as_ref()
                            .map(|entry| entry.dataset_version)
                            .unwrap_or(0),
                    }
                } else if can_native_delete {
                    let delete_events = delete_events_by_entity
                        .get(&entity_key)
                        .map(|rows| rows.as_slice())
                        .unwrap_or(&[]);
                    let delete_ids = deleted_ids_from_cdc_events(delete_events)?;
                    let pinned_version = previous_entry
                        .as_ref()
                        .map(|entry| entry.dataset_version)
                        .ok_or_else(|| {
                            NanoError::Storage(format!(
                                "missing previous dataset version for {}",
                                edge_def.name
                            ))
                        })?;
                    debug!(
                        edge_type = %edge_def.name,
                        rows = delete_ids.len(),
                        "deleting edge rows from existing Lance dataset"
                    );
                    run_lance_delete_by_ids(&dataset_path, pinned_version, &delete_ids).await?
                } else {
                    debug!(
                        edge_type = %edge_def.name,
                        rows = row_count,
                        "writing edge dataset"
                    );
                    write_lance_batch(&dataset_path, batch).await?
                };
                dataset_entries.push(DatasetEntry {
                    type_id: edge_def.type_id,
                    type_name: edge_def.name.clone(),
                    kind: "edge".to_string(),
                    dataset_path: dataset_rel_path,
                    dataset_version,
                    row_count,
                });
            }
        }

        let ir_json = serde_json::to_string_pretty(&self.schema_ir)
            .map_err(|e| NanoError::Manifest(format!("serialize IR error: {}", e)))?;
        let ir_hash = hash_string(&ir_json);

        let mut manifest = GraphManifest::new(ir_hash);
        manifest.db_version = previous_manifest.db_version.saturating_add(1);
        manifest.last_tx_id = format!("manifest-{}", manifest.db_version);
        manifest.committed_at = super::now_unix_seconds_string();
        manifest.next_node_id = storage.next_node_id();
        manifest.next_edge_id = storage.next_edge_id();
        let (next_type_id, next_prop_id) = super::next_schema_identity_counters(&self.schema_ir);
        manifest.next_type_id = next_type_id;
        manifest.next_prop_id = next_prop_id;
        manifest.schema_identity_version = previous_manifest.schema_identity_version.max(1);
        manifest.datasets = dataset_entries;

        let committed_cdc_events = finalize_cdc_entries_for_manifest(cdc_events, &manifest);
        commit_manifest_and_logs(&self.path, &manifest, &committed_cdc_events, op_summary)?;

        super::maintenance::cleanup_stale_dirs(&self.path, &manifest)?;
        Ok(())
    }

    async fn rebuild_vector_indexes_for_types(
        &self,
        type_names: &HashSet<String>,
    ) -> Result<usize> {
        if type_names.is_empty() {
            return Ok(0);
        }

        let storage = self.snapshot();
        let mut rebuilt = 0usize;
        for node_def in self.schema_ir.node_types() {
            if !type_names.contains(&node_def.name) {
                continue;
            }
            let Some(dataset_path) = storage.node_dataset_path(&node_def.name) else {
                continue;
            };
            rebuild_node_vector_indexes(dataset_path, node_def).await?;
            rebuilt += 1;
        }
        Ok(rebuilt)
    }
}

fn select_embed_types<'a>(
    schema_ir: &'a SchemaIR,
    embed_specs: &'a HashMap<String, Vec<crate::store::loader::EmbedSpec>>,
    type_name: Option<&str>,
    property_name: Option<&str>,
) -> Result<
    Vec<(
        &'a crate::catalog::schema_ir::NodeTypeDef,
        Vec<SelectedEmbedProp>,
    )>,
> {
    let mut selected = Vec::new();

    for node_def in schema_ir.node_types() {
        if let Some(expected) = type_name
            && node_def.name != expected
        {
            continue;
        }
        let Some(specs) = embed_specs.get(&node_def.name) else {
            continue;
        };
        let props: Vec<SelectedEmbedProp> = specs
            .iter()
            .filter(|spec| {
                property_name
                    .map(|name| spec.target_prop == name)
                    .unwrap_or(true)
            })
            .map(|spec| SelectedEmbedProp {
                target_prop: spec.target_prop.clone(),
                source_prop: spec.source_prop.clone(),
                dim: spec.dim,
                indexed: node_def
                    .properties
                    .iter()
                    .find(|prop| prop.name == spec.target_prop)
                    .map(|prop| {
                        prop.index
                            && matches!(
                                ScalarType::from_str_name(&prop.scalar_type),
                                Some(ScalarType::Vector(_))
                            )
                    })
                    .unwrap_or(false),
            })
            .collect();
        if !props.is_empty() {
            selected.push((node_def, props));
        }
    }

    if selected.is_empty() {
        return match (type_name, property_name) {
            (Some(type_name), Some(property_name)) => Err(NanoError::Execution(format!(
                "type {} has no @embed property {}",
                type_name, property_name
            ))),
            (Some(type_name), None) => Err(NanoError::Execution(format!(
                "type {} has no @embed properties",
                type_name
            ))),
            (None, Some(property_name)) => Err(NanoError::Execution(format!(
                "no @embed properties named {} found",
                property_name
            ))),
            (None, None) => Err(NanoError::Execution(
                "schema has no @embed properties".to_string(),
            )),
        };
    }

    Ok(selected)
}

fn record_batch_to_json_rows(batch: &RecordBatch) -> Vec<JsonMap<String, JsonValue>> {
    let mut rows = Vec::with_capacity(batch.num_rows());
    let schema = batch.schema();
    for row_idx in 0..batch.num_rows() {
        let mut row = JsonMap::new();
        for (col_idx, field) in schema.fields().iter().enumerate() {
            row.insert(
                field.name().clone(),
                array_value_to_json(batch.column(col_idx), row_idx),
            );
        }
        rows.push(row);
    }
    rows
}

fn json_rows_to_record_batch(
    schema: &arrow_schema::Schema,
    rows: &[JsonMap<String, JsonValue>],
) -> Result<RecordBatch> {
    let mut columns = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        let values: Vec<JsonValue> = rows
            .iter()
            .map(|row| row.get(field.name()).cloned().unwrap_or(JsonValue::Null))
            .collect();
        columns.push(json_values_to_array(
            &values,
            field.data_type(),
            field.is_nullable(),
        )?);
    }

    RecordBatch::try_new(std::sync::Arc::new(schema.clone()), columns)
        .map_err(|e| NanoError::Storage(format!("rebuild embed batch failed: {}", e)))
}

fn dataset_entity_key(kind: &str, type_name: &str) -> String {
    format!("{}:{}", kind, type_name)
}

fn build_append_batch_from_cdc(
    schema: std::sync::Arc<arrow_schema::Schema>,
    insert_events: &[&CdcLogEntry],
) -> Result<Option<RecordBatch>> {
    if insert_events.is_empty() {
        return Ok(None);
    }

    let mut values_by_column: Vec<Vec<serde_json::Value>> = schema
        .fields()
        .iter()
        .map(|_| Vec::with_capacity(insert_events.len()))
        .collect();

    for event in insert_events {
        let payload = event.payload.as_object().ok_or_else(|| {
            NanoError::Storage(format!(
                "CDC insert payload must be object for {} {}",
                event.entity_kind, event.type_name
            ))
        })?;
        for (idx, field) in schema.fields().iter().enumerate() {
            values_by_column[idx].push(
                payload
                    .get(field.name())
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            );
        }
    }

    let mut columns = Vec::with_capacity(schema.fields().len());
    for (idx, field) in schema.fields().iter().enumerate() {
        let arr = json_values_to_array(
            &values_by_column[idx],
            field.data_type(),
            field.is_nullable(),
        )?;
        columns.push(arr);
    }

    let batch = RecordBatch::try_new(schema, columns)
        .map_err(|e| NanoError::Storage(format!("append CDC batch build error: {}", e)))?;
    Ok(Some(batch))
}

fn build_upsert_batch_from_cdc(
    schema: std::sync::Arc<arrow_schema::Schema>,
    upsert_events: &[&CdcLogEntry],
) -> Result<Option<RecordBatch>> {
    if upsert_events.is_empty() {
        return Ok(None);
    }

    let mut values_by_column: Vec<Vec<serde_json::Value>> = schema
        .fields()
        .iter()
        .map(|_| Vec::with_capacity(upsert_events.len()))
        .collect();

    for event in upsert_events {
        let row = match event.op.as_str() {
            "insert" => event.payload.as_object(),
            "update" => event
                .payload
                .get("after")
                .and_then(|value| value.as_object()),
            op => {
                return Err(NanoError::Storage(format!(
                    "unsupported CDC op '{}' for upsert source",
                    op
                )));
            }
        }
        .ok_or_else(|| {
            NanoError::Storage(format!(
                "CDC {} payload missing object row for {} {}",
                event.op, event.entity_kind, event.type_name
            ))
        })?;

        for (idx, field) in schema.fields().iter().enumerate() {
            values_by_column[idx].push(
                row.get(field.name())
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            );
        }
    }

    let mut columns = Vec::with_capacity(schema.fields().len());
    for (idx, field) in schema.fields().iter().enumerate() {
        let arr = json_values_to_array(
            &values_by_column[idx],
            field.data_type(),
            field.is_nullable(),
        )?;
        columns.push(arr);
    }

    let batch = RecordBatch::try_new(schema, columns)
        .map_err(|e| NanoError::Storage(format!("upsert CDC batch build error: {}", e)))?;
    Ok(Some(batch))
}

fn finalize_cdc_entries_for_manifest(
    cdc_events: &[CdcLogEntry],
    manifest: &GraphManifest,
) -> Vec<CdcLogEntry> {
    cdc_events
        .iter()
        .enumerate()
        .map(|(seq, entry)| CdcLogEntry {
            tx_id: manifest.last_tx_id.clone(),
            db_version: manifest.db_version,
            seq_in_tx: seq.min(u32::MAX as usize) as u32,
            op: entry.op.clone(),
            entity_kind: entry.entity_kind.clone(),
            type_name: entry.type_name.clone(),
            entity_key: entry.entity_key.clone(),
            payload: entry.payload.clone(),
            committed_at: manifest.committed_at.clone(),
        })
        .collect()
}

fn schema_has_duplicate_field_names(schema: &arrow_schema::Schema) -> bool {
    let mut seen = HashSet::with_capacity(schema.fields().len());
    schema
        .fields()
        .iter()
        .any(|field| !seen.insert(field.name().clone()))
}
