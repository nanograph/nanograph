use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use arrow_array::builder::BooleanBuilder;
use arrow_array::{
    Array, ArrayRef, BooleanArray, Date32Array, Date64Array, FixedSizeListArray, Float32Array,
    Float64Array, Int32Array, Int64Array, ListArray, RecordBatch, StringArray, UInt32Array,
    UInt64Array,
};
use arrow_schema::DataType;
use tracing::instrument;

use super::{Database, DatabaseWriteGuard, DeleteOp, DeletePredicate, DeleteResult, MutationPlan};
use crate::catalog::schema_ir::SchemaIR;
use crate::error::{NanoError, Result};
use crate::store::graph::GraphStorage;
use crate::store::txlog::CdcLogEntry;

impl Database {
    /// Delete nodes of a given type matching a predicate, cascading incident edges.
    #[instrument(skip(self), fields(type_name = type_name, property = %predicate.property))]
    pub async fn delete_nodes(
        &self,
        type_name: &str,
        predicate: &DeletePredicate,
    ) -> Result<DeleteResult> {
        let mut writer = self.lock_writer().await;
        self.delete_nodes_locked(type_name, predicate, &mut writer)
            .await
    }

    pub(crate) async fn delete_nodes_locked(
        &self,
        type_name: &str,
        predicate: &DeletePredicate,
        writer: &mut DatabaseWriteGuard<'_>,
    ) -> Result<DeleteResult> {
        let current = self.snapshot();
        let target_batch = match current.get_all_nodes(type_name)? {
            Some(batch) => batch,
            None => return Ok(DeleteResult::default()),
        };

        let delete_mask = build_delete_mask_for_mutation(&target_batch, predicate)?;
        let deleted_node_ids = collect_deleted_node_ids(&target_batch, &delete_mask)?;
        if deleted_node_ids.is_empty() {
            return Ok(DeleteResult::default());
        }
        let deleted_node_set: HashSet<u64> = deleted_node_ids.into_iter().collect();

        let mut keep_builder = BooleanBuilder::with_capacity(target_batch.num_rows());
        for row in 0..target_batch.num_rows() {
            let delete = !delete_mask.is_null(row) && delete_mask.value(row);
            keep_builder.append_value(!delete);
        }
        let keep_mask = keep_builder.finish();
        let filtered_target = arrow_select::filter::filter_record_batch(&target_batch, &keep_mask)
            .map_err(|e| NanoError::Storage(format!("node delete filter error: {}", e)))?;

        let old_next_node_id = current.next_node_id();
        let old_next_edge_id = current.next_edge_id();
        let mut new_storage = GraphStorage::new(self.catalog.clone());

        for node_def in self.schema_ir.node_types() {
            if node_def.name == type_name {
                if filtered_target.num_rows() > 0 {
                    new_storage.load_node_batch(type_name, filtered_target.clone())?;
                }
                continue;
            }

            if let Some(batch) = current.get_all_nodes(&node_def.name)? {
                new_storage.load_node_batch(&node_def.name, batch)?;
            }
        }

        let mut deleted_edges = 0usize;
        for edge_def in self.schema_ir.edge_types() {
            if let Some(edge_batch) = current.edge_batch_for_save(&edge_def.name)? {
                let filtered = filter_edge_batch_by_deleted_nodes(&edge_batch, &deleted_node_set)?;
                deleted_edges += edge_batch.num_rows().saturating_sub(filtered.num_rows());
                if filtered.num_rows() > 0 {
                    new_storage.load_edge_batch(&edge_def.name, filtered)?;
                }
            }
        }

        if new_storage.next_node_id() < old_next_node_id {
            new_storage.set_next_node_id(old_next_node_id);
        }
        if new_storage.next_edge_id() < old_next_edge_id {
            new_storage.set_next_edge_id(old_next_edge_id);
        }
        new_storage.build_indices()?;
        self.apply_mutation_plan_locked(
            MutationPlan::prepared_storage(new_storage, "mutation:delete_nodes"),
            writer,
        )
        .await?;

        Ok(DeleteResult {
            deleted_nodes: deleted_node_set.len(),
            deleted_edges,
        })
    }

    /// Delete edges of a given type matching a predicate.
    #[instrument(skip(self), fields(type_name = type_name, property = %predicate.property))]
    pub async fn delete_edges(
        &self,
        type_name: &str,
        predicate: &DeletePredicate,
    ) -> Result<DeleteResult> {
        let mut writer = self.lock_writer().await;
        self.delete_edges_locked(type_name, predicate, &mut writer)
            .await
    }

    pub(crate) async fn delete_edges_locked(
        &self,
        type_name: &str,
        predicate: &DeletePredicate,
        writer: &mut DatabaseWriteGuard<'_>,
    ) -> Result<DeleteResult> {
        if !self.catalog.edge_types.contains_key(type_name) {
            return Err(NanoError::Storage(format!(
                "unknown edge type `{}`",
                type_name
            )));
        }

        let current = self.snapshot();
        let target_batch = match current.edge_batch_for_save(type_name)? {
            Some(batch) => batch,
            None => return Ok(DeleteResult::default()),
        };

        let delete_mask = build_delete_mask_for_mutation(&target_batch, predicate)?;
        let mut keep_builder = BooleanBuilder::with_capacity(target_batch.num_rows());
        for row in 0..target_batch.num_rows() {
            let delete = !delete_mask.is_null(row) && delete_mask.value(row);
            keep_builder.append_value(!delete);
        }
        let keep_mask = keep_builder.finish();
        let filtered_target = arrow_select::filter::filter_record_batch(&target_batch, &keep_mask)
            .map_err(|e| NanoError::Storage(format!("edge delete filter error: {}", e)))?;
        let deleted_edges = target_batch
            .num_rows()
            .saturating_sub(filtered_target.num_rows());
        if deleted_edges == 0 {
            return Ok(DeleteResult::default());
        }

        let old_next_node_id = current.next_node_id();
        let old_next_edge_id = current.next_edge_id();
        let mut new_storage = GraphStorage::new(self.catalog.clone());

        for node_def in self.schema_ir.node_types() {
            if let Some(batch) = current.get_all_nodes(&node_def.name)? {
                new_storage.load_node_batch(&node_def.name, batch)?;
            }
        }

        for edge_def in self.schema_ir.edge_types() {
            if edge_def.name == type_name {
                if filtered_target.num_rows() > 0 {
                    new_storage.load_edge_batch(type_name, filtered_target.clone())?;
                }
                continue;
            }

            if let Some(batch) = current.edge_batch_for_save(&edge_def.name)? {
                new_storage.load_edge_batch(&edge_def.name, batch)?;
            }
        }

        if new_storage.next_node_id() < old_next_node_id {
            new_storage.set_next_node_id(old_next_node_id);
        }
        if new_storage.next_edge_id() < old_next_edge_id {
            new_storage.set_next_edge_id(old_next_edge_id);
        }
        new_storage.build_indices()?;
        self.apply_mutation_plan_locked(
            MutationPlan::prepared_storage(new_storage, "mutation:delete_edges"),
            writer,
        )
        .await?;

        Ok(DeleteResult {
            deleted_nodes: 0,
            deleted_edges,
        })
    }
}

fn parse_predicate_array(value: &str, dt: &DataType, num_rows: usize) -> Result<ArrayRef> {
    let trim_quotes = trim_surrounding_quotes(value);
    let arr: ArrayRef = match dt {
        DataType::Utf8 => Arc::new(StringArray::from(vec![trim_quotes; num_rows])),
        DataType::Boolean => {
            let parsed = trim_quotes.parse::<bool>().map_err(|_| {
                NanoError::Storage(format!(
                    "invalid boolean literal '{}' for delete predicate",
                    value
                ))
            })?;
            Arc::new(BooleanArray::from(vec![parsed; num_rows]))
        }
        DataType::Int32 => {
            let parsed = trim_quotes.parse::<i32>().map_err(|_| {
                NanoError::Storage(format!(
                    "invalid i32 literal '{}' for delete predicate",
                    value
                ))
            })?;
            Arc::new(Int32Array::from(vec![parsed; num_rows]))
        }
        DataType::Int64 => {
            let parsed = trim_quotes.parse::<i64>().map_err(|_| {
                NanoError::Storage(format!(
                    "invalid i64 literal '{}' for delete predicate",
                    value
                ))
            })?;
            Arc::new(Int64Array::from(vec![parsed; num_rows]))
        }
        DataType::UInt32 => {
            let parsed = trim_quotes.parse::<u32>().map_err(|_| {
                NanoError::Storage(format!(
                    "invalid u32 literal '{}' for delete predicate",
                    value
                ))
            })?;
            Arc::new(UInt32Array::from(vec![parsed; num_rows]))
        }
        DataType::UInt64 => {
            let parsed = trim_quotes.parse::<u64>().map_err(|_| {
                NanoError::Storage(format!(
                    "invalid u64 literal '{}' for delete predicate",
                    value
                ))
            })?;
            Arc::new(UInt64Array::from(vec![parsed; num_rows]))
        }
        DataType::Float32 => {
            let parsed = trim_quotes.parse::<f32>().map_err(|_| {
                NanoError::Storage(format!(
                    "invalid f32 literal '{}' for delete predicate",
                    value
                ))
            })?;
            Arc::new(Float32Array::from(vec![parsed; num_rows]))
        }
        DataType::Float64 => {
            let parsed = trim_quotes.parse::<f64>().map_err(|_| {
                NanoError::Storage(format!(
                    "invalid f64 literal '{}' for delete predicate",
                    value
                ))
            })?;
            Arc::new(Float64Array::from(vec![parsed; num_rows]))
        }
        DataType::Date32 => {
            let base: ArrayRef = if let Ok(parsed) = trim_quotes.parse::<i32>() {
                Arc::new(Int32Array::from(vec![parsed; num_rows]))
            } else {
                Arc::new(StringArray::from(vec![trim_quotes; num_rows]))
            };
            arrow_cast::cast(&base, &DataType::Date32).map_err(|e| {
                NanoError::Storage(format!(
                    "invalid Date32 literal '{}' for delete predicate (expected ISO date string or days since epoch): {}",
                    value, e
                ))
            })?
        }
        DataType::Date64 => {
            let base: ArrayRef = if let Ok(parsed) = trim_quotes.parse::<i64>() {
                Arc::new(Int64Array::from(vec![parsed; num_rows]))
            } else {
                Arc::new(StringArray::from(vec![trim_quotes; num_rows]))
            };
            arrow_cast::cast(&base, &DataType::Date64).map_err(|e| {
                NanoError::Storage(format!(
                    "invalid Date64 literal '{}' for delete predicate (expected ISO datetime string or ms since epoch): {}",
                    value, e
                ))
            })?
        }
        _ => {
            return Err(NanoError::Storage(format!(
                "delete predicate on unsupported data type {:?}",
                dt
            )));
        }
    };

    Ok(arr)
}

pub(super) fn trim_surrounding_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let first = bytes[0];
        let last = bytes[s.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

fn compare_for_delete(left: &ArrayRef, right: &ArrayRef, op: DeleteOp) -> Result<BooleanArray> {
    use arrow_ord::cmp;

    match op {
        DeleteOp::Eq => cmp::eq(left, right),
        DeleteOp::Ne => cmp::neq(left, right),
        DeleteOp::Gt => cmp::gt(left, right),
        DeleteOp::Ge => cmp::gt_eq(left, right),
        DeleteOp::Lt => cmp::lt(left, right),
        DeleteOp::Le => cmp::lt_eq(left, right),
    }
    .map_err(|e| NanoError::Storage(format!("delete predicate compare error: {}", e)))
}

pub(super) fn build_delete_mask_for_mutation(
    batch: &RecordBatch,
    predicate: &DeletePredicate,
) -> Result<BooleanArray> {
    let left = batch
        .column_by_name(&predicate.property)
        .ok_or_else(|| {
            NanoError::Storage(format!(
                "property '{}' not found for delete predicate",
                predicate.property
            ))
        })?
        .clone();
    let right = parse_predicate_array(&predicate.value, left.data_type(), batch.num_rows())?;
    compare_for_delete(&left, &right, predicate.op)
}

fn collect_deleted_node_ids(batch: &RecordBatch, delete_mask: &BooleanArray) -> Result<Vec<u64>> {
    let id_arr = batch
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| NanoError::Storage("node id column is not UInt64".to_string()))?;
    let mut ids = Vec::new();
    for row in 0..batch.num_rows() {
        if !delete_mask.is_null(row) && delete_mask.value(row) {
            ids.push(id_arr.value(row));
        }
    }
    Ok(ids)
}

fn filter_edge_batch_by_deleted_nodes(
    batch: &RecordBatch,
    deleted_node_ids: &HashSet<u64>,
) -> Result<RecordBatch> {
    if deleted_node_ids.is_empty() {
        return Ok(batch.clone());
    }

    let src_arr = batch
        .column_by_name("src")
        .ok_or_else(|| NanoError::Storage("edge batch missing src column".to_string()))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| NanoError::Storage("edge src column is not UInt64".to_string()))?;
    let dst_arr = batch
        .column_by_name("dst")
        .ok_or_else(|| NanoError::Storage("edge batch missing dst column".to_string()))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| NanoError::Storage("edge dst column is not UInt64".to_string()))?;

    let mut keep_builder = BooleanBuilder::with_capacity(batch.num_rows());
    let mut kept_rows = 0usize;
    for row in 0..batch.num_rows() {
        let keep = !deleted_node_ids.contains(&src_arr.value(row))
            && !deleted_node_ids.contains(&dst_arr.value(row));
        keep_builder.append_value(keep);
        if keep {
            kept_rows += 1;
        }
    }
    if kept_rows == batch.num_rows() {
        return Ok(batch.clone());
    }
    if kept_rows == 0 {
        return Ok(RecordBatch::new_empty(batch.schema()));
    }

    let keep_mask = keep_builder.finish();
    arrow_select::filter::filter_record_batch(batch, &keep_mask)
        .map_err(|e| NanoError::Storage(format!("edge delete filter error: {}", e)))
}

pub(super) fn build_cdc_events_for_storage_transition(
    previous: &GraphStorage,
    next: &GraphStorage,
    schema_ir: &SchemaIR,
) -> Result<Vec<CdcLogEntry>> {
    let mut events = Vec::new();

    for node_def in schema_ir.node_types() {
        let before_rows = collect_rows_by_id(previous.get_all_nodes(&node_def.name)?)?;
        let after_rows = collect_rows_by_id(next.get_all_nodes(&node_def.name)?)?;
        append_entity_diff_events(
            &mut events,
            "node",
            &node_def.name,
            &before_rows,
            &after_rows,
        );
    }

    for edge_def in schema_ir.edge_types() {
        let before_rows = collect_rows_by_id(previous.edge_batch_for_save(&edge_def.name)?)?;
        let after_rows = collect_rows_by_id(next.edge_batch_for_save(&edge_def.name)?)?;
        append_entity_diff_events(
            &mut events,
            "edge",
            &edge_def.name,
            &before_rows,
            &after_rows,
        );
    }

    Ok(events)
}

fn collect_rows_by_id(
    batch: Option<RecordBatch>,
) -> Result<BTreeMap<u64, serde_json::Map<String, serde_json::Value>>> {
    let mut rows = BTreeMap::new();
    let Some(batch) = batch else {
        return Ok(rows);
    };

    let id_arr = batch
        .column_by_name("id")
        .ok_or_else(|| NanoError::Storage("batch missing id column for CDC".to_string()))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| NanoError::Storage("CDC id column is not UInt64".to_string()))?;

    for row in 0..batch.num_rows() {
        rows.insert(
            id_arr.value(row),
            record_batch_row_to_json_map(&batch, row)?,
        );
    }

    Ok(rows)
}

fn record_batch_row_to_json_map(
    batch: &RecordBatch,
    row: usize,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut map = serde_json::Map::new();
    for (col_idx, field) in batch.schema().fields().iter().enumerate() {
        let value = cdc_array_value_to_json(batch.column(col_idx), row);
        map.insert(field.name().clone(), value);
    }
    Ok(map)
}

fn cdc_array_value_to_json(array: &ArrayRef, row: usize) -> serde_json::Value {
    if array.is_null(row) {
        return serde_json::Value::Null;
    }

    match array.data_type() {
        DataType::Utf8 => array
            .as_any()
            .downcast_ref::<StringArray>()
            .map(|a| serde_json::Value::String(a.value(row).to_string()))
            .unwrap_or(serde_json::Value::Null),
        DataType::Boolean => array
            .as_any()
            .downcast_ref::<BooleanArray>()
            .map(|a| serde_json::Value::Bool(a.value(row)))
            .unwrap_or(serde_json::Value::Null),
        DataType::Int32 => array
            .as_any()
            .downcast_ref::<Int32Array>()
            .map(|a| serde_json::Value::Number((a.value(row) as i64).into()))
            .unwrap_or(serde_json::Value::Null),
        DataType::Int64 => array
            .as_any()
            .downcast_ref::<Int64Array>()
            .map(|a| serde_json::Value::Number(a.value(row).into()))
            .unwrap_or(serde_json::Value::Null),
        DataType::UInt32 => array
            .as_any()
            .downcast_ref::<UInt32Array>()
            .map(|a| serde_json::Value::Number((a.value(row) as u64).into()))
            .unwrap_or(serde_json::Value::Null),
        DataType::UInt64 => array
            .as_any()
            .downcast_ref::<UInt64Array>()
            .map(|a| serde_json::Value::Number(a.value(row).into()))
            .unwrap_or(serde_json::Value::Null),
        DataType::Float32 => array
            .as_any()
            .downcast_ref::<Float32Array>()
            .and_then(|a| {
                serde_json::Number::from_f64(a.value(row) as f64).map(serde_json::Value::Number)
            })
            .unwrap_or(serde_json::Value::Null),
        DataType::Float64 => array
            .as_any()
            .downcast_ref::<Float64Array>()
            .and_then(|a| serde_json::Number::from_f64(a.value(row)).map(serde_json::Value::Number))
            .unwrap_or(serde_json::Value::Null),
        DataType::Date32 => array
            .as_any()
            .downcast_ref::<Date32Array>()
            .map(|a| {
                let days = a.value(row);
                arrow_array::temporal_conversions::date32_to_datetime(days)
                    .map(|dt| serde_json::Value::String(dt.format("%Y-%m-%d").to_string()))
                    .unwrap_or_else(|| serde_json::Value::Number((days as i64).into()))
            })
            .unwrap_or(serde_json::Value::Null),
        DataType::Date64 => array
            .as_any()
            .downcast_ref::<Date64Array>()
            .map(|a| {
                let ms = a.value(row);
                arrow_array::temporal_conversions::date64_to_datetime(ms)
                    .map(|dt| {
                        serde_json::Value::String(dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
                    })
                    .unwrap_or_else(|| serde_json::Value::Number(ms.into()))
            })
            .unwrap_or(serde_json::Value::Null),
        DataType::List(_) => array
            .as_any()
            .downcast_ref::<ListArray>()
            .map(|a| {
                let values = a.value(row);
                serde_json::Value::Array(
                    (0..values.len())
                        .map(|idx| cdc_array_value_to_json(&values, idx))
                        .collect(),
                )
            })
            .unwrap_or(serde_json::Value::Null),
        DataType::FixedSizeList(_, _) => array
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .map(|a| {
                let values = a.value(row);
                serde_json::Value::Array(
                    (0..values.len())
                        .map(|idx| cdc_array_value_to_json(&values, idx))
                        .collect(),
                )
            })
            .unwrap_or(serde_json::Value::Null),
        _ => serde_json::Value::String(
            arrow_cast::display::array_value_to_string(array, row).unwrap_or_default(),
        ),
    }
}

fn append_entity_diff_events(
    out: &mut Vec<CdcLogEntry>,
    entity_kind: &str,
    type_name: &str,
    before_rows: &BTreeMap<u64, serde_json::Map<String, serde_json::Value>>,
    after_rows: &BTreeMap<u64, serde_json::Map<String, serde_json::Value>>,
) {
    for (id, before) in before_rows {
        if !after_rows.contains_key(id) {
            out.push(make_pending_cdc_event(
                "delete",
                entity_kind,
                type_name,
                *id,
                before,
                serde_json::Value::Object(before.clone()),
            ));
        }
    }

    for (id, after) in after_rows {
        if let Some(before) = before_rows.get(id)
            && before != after
        {
            out.push(make_pending_cdc_event(
                "update",
                entity_kind,
                type_name,
                *id,
                after,
                serde_json::json!({
                    "before": before,
                    "after": after,
                }),
            ));
        }
    }

    for (id, after) in after_rows {
        if !before_rows.contains_key(id) {
            out.push(make_pending_cdc_event(
                "insert",
                entity_kind,
                type_name,
                *id,
                after,
                serde_json::Value::Object(after.clone()),
            ));
        }
    }
}

fn make_pending_cdc_event(
    op: &str,
    entity_kind: &str,
    type_name: &str,
    id: u64,
    row: &serde_json::Map<String, serde_json::Value>,
    payload: serde_json::Value,
) -> CdcLogEntry {
    CdcLogEntry {
        tx_id: String::new(),
        db_version: 0,
        seq_in_tx: 0,
        op: op.to_string(),
        entity_kind: entity_kind.to_string(),
        type_name: type_name.to_string(),
        entity_key: cdc_entity_key(entity_kind, id, row),
        payload,
        committed_at: String::new(),
    }
}

fn cdc_entity_key(
    entity_kind: &str,
    id: u64,
    row: &serde_json::Map<String, serde_json::Value>,
) -> String {
    if entity_kind == "edge" {
        let src = row.get("src").and_then(|v| v.as_u64());
        let dst = row.get("dst").and_then(|v| v.as_u64());
        if let (Some(src), Some(dst)) = (src, dst) {
            return format!("id={},src={},dst={}", id, src, dst);
        }
    }
    format!("id={}", id)
}

pub(super) fn deleted_ids_from_cdc_events(events: &[&CdcLogEntry]) -> Result<Vec<u64>> {
    let mut ids = Vec::with_capacity(events.len());
    for event in events {
        let id = cdc_deleted_entity_id(event).ok_or_else(|| {
            NanoError::Storage(format!(
                "CDC delete payload missing id for {} {}",
                event.entity_kind, event.type_name
            ))
        })?;
        ids.push(id);
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

fn cdc_deleted_entity_id(event: &CdcLogEntry) -> Option<u64> {
    if let Some(id) = event
        .payload
        .as_object()
        .and_then(|row| row.get("id"))
        .and_then(|value| value.as_u64())
    {
        return Some(id);
    }

    event
        .entity_key
        .strip_prefix("id=")
        .and_then(|raw| raw.split(',').next())
        .and_then(|value| value.parse::<u64>().ok())
}
