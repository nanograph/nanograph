use super::maintenance::read_cdc_analytics_state;
use super::persist::json_rows_to_record_batch;
use super::*;
use crate::store::export::build_export_rows_at_path;
use crate::store::graph_mirror::{
    GRAPH_CHANGES_DATASET_DIR, GRAPH_COMMITS_DATASET_DIR, inspect_graph_mirror,
    read_graph_change_mirror, read_graph_commit_mirror, rebuild_graph_mirror_from_wal,
};
use crate::store::graph_types::{GraphChangeRecord, GraphCommitRecord, GraphTableVersion};
use crate::store::lance_io::write_lance_batch_with_mode_and_storage_version;
use crate::store::lance_io::{
    LANCE_INTERNAL_ID_FIELD, TableStore, V4NamespaceTableStore, open_dataset_for_locator,
};
use crate::store::manifest::DatasetEntry;
use crate::store::metadata::DatabaseMetadata;
use crate::store::migration::{MigrationStatus, execute_schema_migration};
use crate::store::namespace::{
    GRAPH_CHANGES_TABLE_ID, GRAPH_SNAPSHOT_TABLE_ID, GRAPH_TX_TABLE_ID,
    cleanup_namespace_orphan_versions, open_directory_namespace, resolve_table_location,
    write_namespace_batch,
};
use crate::store::namespace_commit::{build_graph_update_bundle, publish_graph_commit_bundle};
use crate::store::snapshot::{
    graph_snapshot_table_present, publish_committed_graph_snapshot, read_committed_graph_snapshot,
};
use crate::store::storage_generation::{StorageGeneration, storage_metadata_path};
use crate::store::txlog::{
    VisibleCdcSource, append_tx_catalog_entry, collect_visible_lineage_shadow_cdc_entries,
    read_tx_catalog_entries, read_visible_cdc_entries, read_visible_cdc_entries_with_source,
    read_visible_change_rows, read_visible_graph_change_records,
    read_visible_graph_commit_records,
};
use arrow_array::{Array, StringArray, UInt32Array, UInt64Array};
use arrow_schema::{Field, Schema};
use lance::Dataset;
use lance::dataset::WriteMode;
use lance_file::version::LanceFileVersion;
use lance_index::DatasetIndexExt;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use tempfile::TempDir;

fn test_schema_src() -> &'static str {
    r#"node Person {
    name: String @key
    age: I32?
}
node Company {
    name: String @key
}
edge Knows: Person -> Person {
    since: Date?
}
edge WorksAt: Person -> Company
"#
}

fn test_data_src() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Alice", "age": 30}}
{"type": "Person", "data": {"name": "Bob", "age": 25}}
{"type": "Person", "data": {"name": "Charlie", "age": 35}}
{"type": "Company", "data": {"name": "Acme"}}
{"edge": "Knows", "from": "Alice", "to": "Bob"}
{"edge": "Knows", "from": "Alice", "to": "Charlie"}
{"edge": "WorksAt", "from": "Alice", "to": "Acme"}
"#
}

fn assert_lineage_shadow_cdc_matches_authoritative(
    path: &std::path::Path,
    from_db_version_exclusive: u64,
    to_db_version_inclusive: Option<u64>,
) {
    let shadow = collect_visible_lineage_shadow_cdc_entries(
        path,
        from_db_version_exclusive,
        to_db_version_inclusive,
    )
    .unwrap();
    assert_eq!(
        shadow.shadow_entries, shadow.authoritative_entries_compared,
        "expected lineage shadow CDC to match authoritative CDC"
    );
}

async fn init_v4_db(path: &std::path::Path, schema_source: &str) -> Database {
    Database::init_with_generation(path, schema_source, StorageGeneration::V4Namespace)
        .await
        .unwrap()
}

fn duplicate_edge_data_src() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Alice", "age": 30}}
{"type": "Person", "data": {"name": "Bob", "age": 25}}
{"edge": "Knows", "from": "Alice", "to": "Bob"}
{"edge": "Knows", "from": "Alice", "to": "Bob"}
"#
}

fn keyed_schema_src() -> &'static str {
    r#"node Person {
    name: String @key
    age: I32?
}
node Company {
    name: String
}
edge Knows: Person -> Person
"#
}

fn keyed_data_initial() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Alice", "age": 30}}
{"type": "Person", "data": {"name": "Bob", "age": 25}}
{"type": "Company", "data": {"name": "Acme"}}
{"edge": "Knows", "from": "Alice", "to": "Bob"}
"#
}

fn keyed_data_upsert() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Alice", "age": 31}}
{"type": "Person", "data": {"name": "Charlie", "age": 40}}
{"edge": "Knows", "from": "Alice", "to": "Bob"}
{"edge": "Knows", "from": "Alice", "to": "Charlie"}
{"edge": "Knows", "from": "Alice", "to": "Charlie"}
"#
}

fn keyed_data_append_duplicate() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Alice", "age": 99}}
"#
}

fn timestamp_schema_src() -> &'static str {
    r#"node Person {
    name: String @key
    created_at: DateTime?
    updated_at: DateTime?
}
"#
}

fn timestamp_data_src() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Alice"}}
"#
}

fn id_named_key_schema_src() -> &'static str {
    r#"node Person {
    id: String @key
    name: String
    age: I32?
}
edge Knows: Person -> Person
"#
}

fn id_named_key_initial_data_src() -> &'static str {
    r#"{"type": "Person", "data": {"id": "usr_alice", "name": "Alice", "age": 30}}
{"type": "Person", "data": {"id": "usr_bob", "name": "Bob", "age": 25}}
{"edge": "Knows", "from": "usr_alice", "to": "usr_bob"}
"#
}

fn id_named_key_append_data_src() -> &'static str {
    r#"{"type": "Person", "data": {"id": "usr_charlie", "name": "Charlie", "age": 40}}
{"edge": "Knows", "from": "usr_charlie", "to": "usr_alice"}
"#
}

fn id_named_key_merge_data_src() -> &'static str {
    r#"{"type": "Person", "data": {"id": "usr_alice", "name": "Alice", "age": 31}}
{"type": "Person", "data": {"id": "usr_charlie", "name": "Charlie", "age": 40}}
{"edge": "Knows", "from": "usr_alice", "to": "usr_charlie"}
"#
}

fn append_data_new_person_with_edge_to_existing() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Diana", "age": 28}}
{"edge": "Knows", "from": "Diana", "to": "Alice"}
"#
}

fn unique_schema_src() -> &'static str {
    r#"node Person {
    name: String @key
    email: String @unique
}
"#
}

fn unique_data_initial() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Alice", "email": "alice@example.com"}}
{"type": "Person", "data": {"name": "Bob", "email": "bob@example.com"}}
"#
}

fn unique_data_existing_incoming_conflict() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Charlie", "email": "bob@example.com"}}
"#
}

fn unique_data_incoming_incoming_conflict() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Charlie", "email": "charlie@example.com"}}
{"type": "Person", "data": {"name": "Diana", "email": "charlie@example.com"}}
"#
}

fn id_named_unique_schema_src() -> &'static str {
    r#"node Person {
    id: String @unique
    name: String
}
"#
}

fn id_named_unique_duplicate_data() -> &'static str {
    r#"{"type": "Person", "data": {"id": "user-1", "name": "Alice"}}
{"type": "Person", "data": {"id": "user-1", "name": "Bob"}}
"#
}

fn nullable_unique_schema_src() -> &'static str {
    r#"node Person {
    name: String
    nick: String? @unique
}
"#
}

fn indexed_schema_src() -> &'static str {
    r#"node Person {
    name: String @key
    handle: String @index
    age: I32?
}
"#
}

fn indexed_data_src() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Alice", "handle": "a", "age": 30}}
{"type": "Person", "data": {"name": "Bob", "handle": "b", "age": 25}}
"#
}

fn vector_indexed_schema_src() -> &'static str {
    r#"node Doc {
    slug: String @key
    embedding: Vector(4) @index
    title: String
}
"#
}

fn vector_indexed_schema_v2_src() -> &'static str {
    r#"node Doc {
    slug: String @key
    embedding: Vector(4) @index
    title: String
    category: String?
}
"#
}

fn vector_indexed_data_src() -> &'static str {
    r#"{"type": "Doc", "data": {"slug": "a", "embedding": [1.0, 0.0, 0.0, 0.0], "title": "A"}}
{"type": "Doc", "data": {"slug": "b", "embedding": [0.0, 1.0, 0.0, 0.0], "title": "B"}}
{"type": "Doc", "data": {"slug": "c", "embedding": [0.0, 0.0, 1.0, 0.0], "title": "C"}}
"#
}

fn vector_nullable_keyed_schema_src() -> &'static str {
    r#"node Doc {
    slug: String @key
    embedding: Vector(4)?
    title: String
}
"#
}

fn vector_nullable_keyed_initial_data_src() -> &'static str {
    r#"{"type": "Doc", "data": {"slug": "a", "embedding": [1.0, 0.0, 0.0, 0.0], "title": "A"}}
"#
}

fn vector_nullable_keyed_append_data_src() -> &'static str {
    r#"{"type": "Doc", "data": {"slug": "b", "embedding": [0.0, 1.0, 0.0, 0.0], "title": "B"}}
"#
}

fn vector_nullable_keyed_merge_data_src() -> &'static str {
    r#"{"type": "Doc", "data": {"slug": "a", "embedding": [0.0, 0.0, 1.0, 0.0], "title": "A2"}}
{"type": "Doc", "data": {"slug": "b", "embedding": [0.0, 1.0, 0.0, 0.0], "title": "B"}}
"#
}

fn nullable_unique_ok_data() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Alice", "nick": null}}
{"type": "Person", "data": {"name": "Bob", "nick": null}}
"#
}

fn nullable_unique_duplicate_data() -> &'static str {
    r#"{"type": "Person", "data": {"name": "Alice", "nick": "ally"}}
{"type": "Person", "data": {"name": "Bob", "nick": "ally"}}
"#
}

fn person_id_by_name(batch: &RecordBatch, name: &str) -> u64 {
    let id_col = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    let name_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<arrow_array::StringArray>()
        .unwrap();
    (0..batch.num_rows())
        .find(|&i| name_col.value(i) == name)
        .map(|i| id_col.value(i))
        .unwrap()
}

fn edge_id_by_endpoints(batch: &RecordBatch, src_id: u64, dst_id: u64) -> u64 {
    let id_col = batch
        .column_by_name("id")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    let src_col = batch
        .column_by_name("src")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    let dst_col = batch
        .column_by_name("dst")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    (0..batch.num_rows())
        .find(|&i| src_col.value(i) == src_id && dst_col.value(i) == dst_id)
        .map(|i| id_col.value(i))
        .unwrap()
}

fn person_age_by_name(batch: &RecordBatch, name: &str) -> Option<i32> {
    let name_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<arrow_array::StringArray>()
        .unwrap();
    let age_col = batch
        .column(2)
        .as_any()
        .downcast_ref::<arrow_array::Int32Array>()
        .unwrap();
    (0..batch.num_rows()).find_map(|i| {
        if name_col.value(i) == name {
            Some(if age_col.is_null(i) {
                None
            } else {
                Some(age_col.value(i))
            })
        } else {
            None
        }
    })?
}

fn person_email_by_name(batch: &RecordBatch, name: &str) -> String {
    let name_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<arrow_array::StringArray>()
        .unwrap();
    let email_col = batch
        .column(2)
        .as_any()
        .downcast_ref::<arrow_array::StringArray>()
        .unwrap();
    (0..batch.num_rows())
        .find(|&i| name_col.value(i) == name)
        .map(|i| email_col.value(i).to_string())
        .unwrap()
}

fn person_internal_id_by_user_id(batch: &RecordBatch, user_id: &str) -> u64 {
    let id_col = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    let user_id_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<arrow_array::StringArray>()
        .unwrap();
    (0..batch.num_rows())
        .find(|&i| user_id_col.value(i) == user_id)
        .map(|i| id_col.value(i))
        .unwrap()
}

fn person_age_by_user_id(batch: &RecordBatch, user_id: &str) -> Option<i32> {
    let user_id_col = batch
        .column(1)
        .as_any()
        .downcast_ref::<arrow_array::StringArray>()
        .unwrap();
    let age_col = batch
        .column(3)
        .as_any()
        .downcast_ref::<arrow_array::Int32Array>()
        .unwrap();
    (0..batch.num_rows()).find_map(|i| {
        if user_id_col.value(i) == user_id {
            Some(if age_col.is_null(i) {
                None
            } else {
                Some(age_col.value(i))
            })
        } else {
            None
        }
    })?
}

fn doc_title_by_slug(batch: &RecordBatch, slug: &str) -> Option<String> {
    let slug_col = batch
        .column_by_name("slug")
        .and_then(|c| c.as_any().downcast_ref::<arrow_array::StringArray>())?;
    let title_col = batch
        .column_by_name("title")
        .and_then(|c| c.as_any().downcast_ref::<arrow_array::StringArray>())?;
    (0..batch.num_rows()).find_map(|i| {
        if slug_col.value(i) == slug {
            Some(title_col.value(i).to_string())
        } else {
            None
        }
    })
}

fn test_dir(name: &str) -> TempDir {
    tempfile::Builder::new()
        .prefix(&format!("nanograph_{}_", name))
        .tempdir()
        .unwrap()
}

fn graph_commit_table_versions(
    record: &crate::store::graph_types::GraphCommitRecord,
) -> BTreeMap<String, u64> {
    record
        .table_versions
        .iter()
        .map(|version| (version.table_id.as_str().to_string(), version.version))
        .collect()
}

fn write_data_file(dir: &TempDir, name: &str, data: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, data).unwrap();
    path
}

async fn manifest_file_for_table_version(
    db_path: &std::path::Path,
    table_id: &str,
    version: u64,
) -> std::path::PathBuf {
    let namespace = open_directory_namespace(db_path).await.unwrap();
    let location = resolve_table_location(namespace, table_id).await.unwrap();
    let versions_dir =
        std::path::PathBuf::from(location.strip_prefix("file://").unwrap_or(&location))
            .join("_versions");
    std::fs::read_dir(&versions_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .find_map(|entry| {
            let path = entry.path();
            let file_name = path.file_name()?.to_str()?;
            let scheme = lance_table::io::commit::ManifestNamingScheme::detect_scheme(file_name)?;
            let parsed = scheme.parse_version(file_name).or_else(|| {
                lance_table::io::commit::ManifestNamingScheme::parse_detached_version(file_name)
            })?;
            (parsed == version).then_some(path)
        })
        .unwrap_or_else(|| {
            panic!(
                "expected manifest file for {} version {} under {}",
                table_id,
                version,
                versions_dir.display()
            )
        })
}

#[test]
fn trim_surrounding_quotes_single_quote_does_not_panic() {
    assert_eq!(trim_surrounding_quotes("'"), "'");
    assert_eq!(trim_surrounding_quotes("\""), "\"");
}

fn dataset_version_for(manifest: &GraphManifest, kind: &str, type_name: &str) -> u64 {
    manifest
        .datasets
        .iter()
        .find(|entry| entry.kind == kind && entry.type_name == type_name)
        .map(|entry| entry.dataset_version)
        .unwrap()
}

fn dataset_rel_path_for(manifest: &GraphManifest, kind: &str, type_name: &str) -> String {
    manifest
        .datasets
        .iter()
        .find(|entry| entry.kind == kind && entry.type_name == type_name)
        .map(|entry| entry.dataset_path.clone())
        .unwrap()
}

fn graph_change_for_manifest(
    manifest: &GraphManifest,
    seq_in_tx: u32,
    op: &str,
    entity_kind: &str,
    type_name: &str,
    entity_key: &str,
    payload: serde_json::Value,
) -> GraphChangeRecord {
    GraphChangeRecord {
        tx_id: manifest.last_tx_id.clone().into(),
        graph_version: manifest.db_version.into(),
        seq_in_tx,
        op: op.to_string(),
        entity_kind: entity_kind.to_string(),
        type_name: type_name.to_string(),
        entity_key: entity_key.to_string(),
        payload,
        rowid_if_known: None,
        committed_at: manifest.committed_at.clone(),
    }
}

fn graph_commit_for_manifest(manifest: &GraphManifest, op_summary: &str) -> GraphCommitRecord {
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
        tx_props: std::collections::BTreeMap::new(),
    }
}

async fn dataset_storage_version(dataset_path: &std::path::Path) -> LanceFileVersion {
    let uri = dataset_path.to_string_lossy().to_string();
    let dataset = Dataset::open(&uri).await.unwrap();
    dataset
        .manifest
        .data_storage_format
        .lance_file_version()
        .unwrap()
}

async fn rowid_for_entity(db_path: &std::path::Path, kind: &str, type_name: &str, id: u64) -> u64 {
    let metadata = DatabaseMetadata::open(db_path).unwrap();
    let locator = match kind {
        "node" => metadata.node_dataset_locator(type_name).unwrap(),
        "edge" => metadata.edge_dataset_locator(type_name).unwrap(),
        other => panic!("unsupported kind {other}"),
    };
    let dataset = open_dataset_for_locator(&locator).await.unwrap();
    let mut scanner = dataset.scan();
    scanner.with_row_id();
    scanner
        .project(&[LANCE_INTERNAL_ID_FIELD, "_rowid"])
        .unwrap();
    scanner
        .filter(&format!("{LANCE_INTERNAL_ID_FIELD} = {id}"))
        .unwrap();
    let batch = scanner.try_into_batch().await.unwrap();
    let rowids = batch
        .column_by_name("_rowid")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    rowids.value(0)
}

fn graph_change_records_batch(records: &[GraphChangeRecord]) -> RecordBatch {
    RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("tx_id", arrow_schema::DataType::Utf8, false),
            Field::new("db_version", arrow_schema::DataType::UInt64, false),
            Field::new("seq_in_tx", arrow_schema::DataType::UInt32, false),
            Field::new("op", arrow_schema::DataType::Utf8, false),
            Field::new("entity_kind", arrow_schema::DataType::Utf8, false),
            Field::new("type_name", arrow_schema::DataType::Utf8, false),
            Field::new("entity_key", arrow_schema::DataType::Utf8, false),
            Field::new("payload_json", arrow_schema::DataType::Utf8, false),
            Field::new("rowid_if_known", arrow_schema::DataType::UInt64, true),
            Field::new("committed_at", arrow_schema::DataType::Utf8, false),
        ])),
        vec![
            Arc::new(StringArray::from(
                records
                    .iter()
                    .map(|record| record.tx_id.as_str().to_string())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                records
                    .iter()
                    .map(|record| record.graph_version.value())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                records
                    .iter()
                    .map(|record| record.seq_in_tx)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records
                    .iter()
                    .map(|record| record.op.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records
                    .iter()
                    .map(|record| record.entity_kind.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records
                    .iter()
                    .map(|record| record.type_name.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records
                    .iter()
                    .map(|record| record.entity_key.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records
                    .iter()
                    .map(|record| serde_json::to_string(&record.payload).unwrap())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                records
                    .iter()
                    .map(|record| record.rowid_if_known)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                records
                    .iter()
                    .map(|record| record.committed_at.clone())
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap()
}

async fn overwrite_committed_graph_changes(
    db_path: &std::path::Path,
    records: &[GraphChangeRecord],
) {
    let namespace = open_directory_namespace(db_path).await.unwrap();
    let version = write_namespace_batch(
        namespace,
        GRAPH_CHANGES_TABLE_ID,
        graph_change_records_batch(records),
        WriteMode::Overwrite,
        None,
    )
    .await
    .unwrap();

    let mut snapshot = read_committed_graph_snapshot(db_path).unwrap();
    let entry = snapshot
        .datasets
        .iter_mut()
        .find(|entry| entry.effective_table_id() == GRAPH_CHANGES_TABLE_ID)
        .unwrap();
    entry.dataset_version = version.version;
    entry.row_count = records.len() as u64;
    publish_committed_graph_snapshot(db_path, &snapshot).unwrap();
}

fn entity_id_from_key(entity_key: &str) -> u64 {
    entity_key
        .strip_prefix("id=")
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap()
}

async fn read_node_batch_for_db(db: &Database, type_name: &str) -> Option<RecordBatch> {
    let metadata = DatabaseMetadata::open(db.path()).unwrap();
    super::persist::read_sparse_node_batch(&metadata, type_name)
        .await
        .unwrap()
}

async fn read_edge_batch_for_db(db: &Database, type_name: &str) -> Option<RecordBatch> {
    let metadata = DatabaseMetadata::open(db.path()).unwrap();
    super::persist::read_sparse_edge_batch(&metadata, type_name)
        .await
        .unwrap()
}

async fn write_fixture_database_with_storage_version(
    db_path: &std::path::Path,
    schema_source: &str,
    data_source: &str,
    storage_version: LanceFileVersion,
) {
    let source_db = Database::open_in_memory(schema_source).await.unwrap();
    source_db
        .load_with_mode(data_source, LoadMode::Overwrite)
        .await
        .unwrap();
    let schema_ir = source_db.schema_ir.clone();
    let source_manifest = read_committed_graph_snapshot(source_db.path()).unwrap();
    let source_metadata = DatabaseMetadata::open(source_db.path()).unwrap();

    let fixture_db = Database::init(db_path, schema_source).await.unwrap();
    drop(fixture_db);

    let mut manifest = read_committed_graph_snapshot(db_path).unwrap();
    manifest.committed_at = now_unix_seconds_string();
    manifest.last_tx_id = format!("fixture-{}", storage_version);
    manifest.next_node_id = source_manifest.next_node_id;
    manifest.next_edge_id = source_manifest.next_edge_id;

    let mut datasets = Vec::new();
    for node_def in schema_ir.node_types() {
        if let Some(batch) =
            super::persist::read_sparse_node_batch(&source_metadata, &node_def.name)
                .await
                .unwrap()
        {
            let row_count = batch.num_rows() as u64;
            let dataset_rel_path = format!("nodes/{}", SchemaIR::dir_name(node_def.type_id));
            let dataset_path = db_path.join(&dataset_rel_path);
            let dataset_version = write_lance_batch_with_mode_and_storage_version(
                &dataset_path,
                batch,
                WriteMode::Overwrite,
                Some(storage_version),
            )
            .await
            .unwrap();
            datasets.push(DatasetEntry::new(
                node_def.type_id,
                node_def.name.clone(),
                "node",
                dataset_rel_path.clone(),
                dataset_rel_path,
                dataset_version,
                row_count,
            ));
        }
    }

    for edge_def in schema_ir.edge_types() {
        if let Some(batch) =
            super::persist::read_sparse_edge_batch(&source_metadata, &edge_def.name)
                .await
                .unwrap()
        {
            let row_count = batch.num_rows() as u64;
            let dataset_rel_path = format!("edges/{}", SchemaIR::dir_name(edge_def.type_id));
            let dataset_path = db_path.join(&dataset_rel_path);
            let dataset_version = write_lance_batch_with_mode_and_storage_version(
                &dataset_path,
                batch,
                WriteMode::Overwrite,
                Some(storage_version),
            )
            .await
            .unwrap();
            datasets.push(DatasetEntry::new(
                edge_def.type_id,
                edge_def.name.clone(),
                "edge",
                dataset_rel_path.clone(),
                dataset_rel_path,
                dataset_version,
                row_count,
            ));
        }
    }

    manifest.datasets = datasets;
    publish_committed_graph_snapshot(db_path, &manifest).unwrap();
}

#[tokio::test]
async fn test_init_creates_directory_structure() {
    let dir = test_dir("init");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();

    assert!(path.join("schema.pg").exists());
    assert!(path.join("schema.ir.json").exists());
    assert!(!path.join("graph.manifest.json").exists());
    assert!(path.join("nodes").exists());
    assert!(path.join("edges").exists());
    let manifest = read_committed_graph_snapshot(path).unwrap();
    assert!(
        manifest
            .datasets
            .iter()
            .any(|entry| entry.effective_table_id() == GRAPH_TX_TABLE_ID)
    );
    assert!(
        manifest
            .datasets
            .iter()
            .any(|entry| entry.effective_table_id() == "__graph_deletes")
    );
    assert!(
        manifest
            .datasets
            .iter()
            .all(|entry| entry.effective_table_id() != GRAPH_CHANGES_TABLE_ID)
    );

    assert_eq!(db.catalog.node_types.len(), 2);
    assert_eq!(db.catalog.edge_types.len(), 2);
}

#[tokio::test]
async fn test_v4_init_bootstraps_internal_tables_without_wal() {
    let dir = test_dir("v4_init_internal_tables");
    let path = dir.path();

    let _db = init_v4_db(path, test_schema_src()).await;

    assert!(storage_metadata_path(path).exists());
    assert!(!path.join("_wal.jsonl").exists());

    let manifest = read_committed_graph_snapshot(path).unwrap();
    assert!(
        manifest
            .datasets
            .iter()
            .any(|entry| entry.effective_table_id() == "__graph_tx")
    );
    assert!(
        manifest
            .datasets
            .iter()
            .any(|entry| entry.effective_table_id() == "__graph_changes")
    );
    assert!(
        manifest
            .datasets
            .iter()
            .any(|entry| entry.effective_table_id() == "__blob_store")
    );
    assert!(graph_snapshot_table_present(path).await.unwrap());
}

#[tokio::test]
async fn test_v4_unpublished_table_write_is_invisible_until_snapshot_publish() {
    let dir = test_dir("v4_snapshot_visibility");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();

    let metadata = DatabaseMetadata::open(path).unwrap();
    let locator = metadata.node_dataset_locator("Person").unwrap();
    let person_batches = crate::store::lance_io::read_lance_batches_for_locator(&locator)
        .await
        .unwrap();
    let person_schema = person_batches[0].schema();
    let extra_row = serde_json::json!({
        "id": 999_u64,
        "name": "Zoe",
        "age": 29
    })
    .as_object()
    .unwrap()
    .clone();
    let append_batch = json_rows_to_record_batch(person_schema.as_ref(), &[extra_row]).unwrap();

    let table_store = V4NamespaceTableStore::new(path);
    let unpublished_version = table_store
        .append(&path.join(&locator.table_id), append_batch)
        .await
        .unwrap()
        .version;

    let snapshot_before_publish = read_committed_graph_snapshot(path).unwrap();
    let person_entry_before_publish = snapshot_before_publish
        .datasets
        .iter()
        .find(|entry| entry.kind == "node" && entry.type_name == "Person")
        .unwrap();
    assert!(unpublished_version > person_entry_before_publish.dataset_version);

    let exported_before_publish = build_export_rows_at_path(path, false, true).await.unwrap();
    assert!(
        !exported_before_publish
            .iter()
            .any(|row| { row["type"] == "Person" && row["data"]["name"] == "Zoe" }),
        "unpublished row should stay invisible until snapshot publish"
    );

    let mut next_snapshot = snapshot_before_publish.clone();
    let person_entry = next_snapshot
        .datasets
        .iter_mut()
        .find(|entry| entry.kind == "node" && entry.type_name == "Person")
        .unwrap();
    person_entry.dataset_version = unpublished_version;
    person_entry.row_count += 1;
    next_snapshot.db_version += 1;
    next_snapshot.last_tx_id = format!("manual-snapshot-{}", next_snapshot.db_version);
    next_snapshot.committed_at = now_unix_seconds_string();
    publish_committed_graph_snapshot(path, &next_snapshot).unwrap();

    let exported_after_publish = build_export_rows_at_path(path, false, true).await.unwrap();
    assert!(
        exported_after_publish
            .iter()
            .any(|row| { row["type"] == "Person" && row["data"]["name"] == "Zoe" })
    );
}

#[tokio::test]
async fn test_v4_cleanup_removes_orphan_unpublished_versions() {
    let dir = test_dir("v4_cleanup_orphan_unpublished_versions");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();

    let metadata = DatabaseMetadata::open(path).unwrap();
    let locator = metadata.node_dataset_locator("Person").unwrap();
    let person_batches = crate::store::lance_io::read_lance_batches_for_locator(&locator)
        .await
        .unwrap();
    let person_schema = person_batches[0].schema();
    let extra_row = serde_json::json!({
        "id": 1_000_u64,
        "name": "Orphaned",
        "age": 31
    })
    .as_object()
    .unwrap()
    .clone();
    let append_batch = json_rows_to_record_batch(person_schema.as_ref(), &[extra_row]).unwrap();

    let table_store = V4NamespaceTableStore::new(path);
    let unpublished_version = table_store
        .append(&path.join(&locator.table_id), append_batch)
        .await
        .unwrap()
        .version;
    assert!(unpublished_version > locator.dataset_version);

    let dataset_uri = locator.dataset_path.to_string_lossy().to_string();
    let dataset_before = Dataset::open(&dataset_uri).await.unwrap();
    let versions_before = dataset_before.versions().await.unwrap();
    assert!(
        versions_before
            .iter()
            .any(|version| version.version == unpublished_version)
    );
    let versions_dir = locator.dataset_path.join("_versions");
    let unpublished_manifest_path = std::fs::read_dir(&versions_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .find_map(|entry| {
            let path = entry.path();
            let file_name = path.file_name()?.to_str()?;
            let scheme = lance_table::io::commit::ManifestNamingScheme::detect_scheme(file_name)?;
            let version = scheme.parse_version(file_name).or_else(|| {
                lance_table::io::commit::ManifestNamingScheme::parse_detached_version(file_name)
            })?;
            (version == unpublished_version).then_some(path)
        })
        .expect("expected unpublished manifest file");

    let snapshot = read_committed_graph_snapshot(path).unwrap();
    let removed = crate::store::namespace::cleanup_namespace_orphan_versions(path, &snapshot)
        .await
        .unwrap();
    assert!(removed > 0);
    assert!(!unpublished_manifest_path.exists());

    drop(db);
    let reopened = Database::open(path).await.unwrap();

    let exported = build_export_rows_at_path(reopened.path(), false, true)
        .await
        .unwrap();
    assert!(
        !exported
            .iter()
            .any(|row| { row["type"] == "Person" && row["data"]["name"] == "Orphaned" })
    );
}

#[tokio::test]
async fn test_v4_open_uses_namespace_snapshot_without_manifest_file() {
    let dir = test_dir("v4_namespace_snapshot_without_file");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();

    assert!(!path.join("graph.manifest.json").exists());

    let reopened = Database::open(path).await.unwrap();
    let exported = build_export_rows_at_path(reopened.path(), false, true)
        .await
        .unwrap();

    assert!(
        exported
            .iter()
            .any(|row| { row["type"] == "Person" && row["data"]["name"] == "Alice" })
    );
    assert!(graph_snapshot_table_present(path).await.unwrap());
}

#[tokio::test]
async fn test_v4_staged_internal_bundle_is_invisible_until_publish() {
    let dir = test_dir("v4_internal_bundle_visibility");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();
    drop(db);

    let committed_before = read_committed_graph_snapshot(path).unwrap();
    let tx_before = read_tx_catalog_entries(path).unwrap();
    let changes_before =
        read_visible_graph_change_records(path, 0, Some(committed_before.db_version)).unwrap();

    let mut next_snapshot = committed_before.clone();
    next_snapshot.db_version += 1;
    next_snapshot.last_tx_id = format!("bundle-test-{}", next_snapshot.db_version);
    next_snapshot.committed_at = now_unix_seconds_string();

    let graph_change = graph_change_for_manifest(
        &next_snapshot,
        0,
        "insert",
        "node",
        "Person",
        "bundle:person",
        serde_json::json!({"name": "Bundle Person", "age": 42}),
    );
    let graph_commit = graph_commit_for_manifest(&next_snapshot, "test:bundle-visibility");
    let bundle = build_graph_update_bundle(
        path,
        &graph_commit,
        std::slice::from_ref(&graph_change),
        &next_snapshot,
    )
    .unwrap();

    let committed_after_stage = read_committed_graph_snapshot(path).unwrap();
    let tx_after_stage = read_tx_catalog_entries(path).unwrap();
    let changes_after_stage =
        read_visible_graph_change_records(path, 0, Some(committed_after_stage.db_version))
            .unwrap();
    let exported_before_publish = build_export_rows_at_path(path, false, true).await.unwrap();

    assert_eq!(
        committed_after_stage.db_version,
        committed_before.db_version
    );
    assert_eq!(tx_after_stage.len(), tx_before.len());
    assert_eq!(changes_after_stage.len(), changes_before.len());
    assert!(
        !exported_before_publish
            .iter()
            .any(|row| { row["type"] == "Person" && row["data"]["name"] == "Bundle Person" })
    );

    publish_graph_commit_bundle(path, &bundle).unwrap();

    let committed_after_publish = read_committed_graph_snapshot(path).unwrap();
    let tx_after_publish = read_tx_catalog_entries(path).unwrap();
    let changes_after_publish =
        read_visible_graph_change_records(path, 0, Some(committed_after_publish.db_version))
            .unwrap();
    let visible_commits = read_visible_graph_commit_records(path).unwrap();

    assert_eq!(committed_after_publish.db_version, next_snapshot.db_version);
    assert_eq!(tx_after_publish.len(), tx_before.len() + 1);
    assert_eq!(changes_after_publish.len(), changes_before.len() + 1);
    assert_eq!(
        tx_after_publish.last().unwrap().db_version,
        next_snapshot.db_version
    );
    assert_eq!(
        changes_after_publish.last().unwrap().entity_key,
        "bundle:person"
    );
    assert_eq!(
        visible_commits.last().unwrap().graph_version.value(),
        next_snapshot.db_version
    );
}

#[tokio::test]
async fn test_v4_open_recovers_from_orphan_staged_internal_bundle_versions() {
    let dir = test_dir("v4_open_recovers_orphan_internal_bundle");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();
    drop(db);

    let committed_before = read_committed_graph_snapshot(path).unwrap();
    let tx_before = read_tx_catalog_entries(path).unwrap();
    let cdc_before = read_visible_cdc_entries(path, 0, Some(committed_before.db_version)).unwrap();

    let mut next_snapshot = committed_before.clone();
    next_snapshot.db_version += 1;
    next_snapshot.last_tx_id = format!("bundle-open-recover-{}", next_snapshot.db_version);
    next_snapshot.committed_at = now_unix_seconds_string();

    let graph_change = graph_change_for_manifest(
        &next_snapshot,
        0,
        "update",
        "node",
        "Person",
        "bundle:recover",
        serde_json::json!({"name": "Recovered Bundle Person", "age": 43}),
    );
    let graph_commit = graph_commit_for_manifest(&next_snapshot, "test:bundle-open-recover");
    let bundle = build_graph_update_bundle(
        path,
        &graph_commit,
        std::slice::from_ref(&graph_change),
        &next_snapshot,
    )
    .unwrap();

    let staged_internal_versions = bundle
        .published_versions
        .iter()
        .filter(|version| {
            matches!(
                version.table_id.as_str(),
                GRAPH_TX_TABLE_ID | GRAPH_CHANGES_TABLE_ID | GRAPH_SNAPSHOT_TABLE_ID
            )
        })
        .map(|version| (version.table_id.clone(), version.version))
        .collect::<Vec<_>>();
    let mut staged_internal_paths = Vec::new();
    for (table_id, version) in staged_internal_versions {
        staged_internal_paths.push(manifest_file_for_table_version(path, &table_id, version).await);
    }
    assert!(staged_internal_paths.iter().all(|path| path.exists()));

    let reopened = Database::open(path).await.unwrap();
    let committed_after = read_committed_graph_snapshot(path).unwrap();
    let tx_after = read_tx_catalog_entries(path).unwrap();
    let cdc_after = read_visible_cdc_entries(path, 0, Some(committed_after.db_version)).unwrap();
    let exported = build_export_rows_at_path(reopened.path(), false, true)
        .await
        .unwrap();

    assert_eq!(committed_after.db_version, committed_before.db_version);
    assert_eq!(tx_after.len(), tx_before.len());
    assert_eq!(cdc_after.len(), cdc_before.len());
    assert!(staged_internal_paths.iter().all(|path| !path.exists()));
    assert!(!exported.iter().any(|row| {
        row["type"] == "Person" && row["data"]["name"] == "Recovered Bundle Person"
    }));
}

#[tokio::test]
async fn test_v4_cleanup_removes_orphan_staged_internal_bundle_versions() {
    let dir = test_dir("v4_cleanup_internal_bundle_orphans");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();
    drop(db);

    let committed_before = read_committed_graph_snapshot(path).unwrap();
    let tx_before = read_tx_catalog_entries(path).unwrap();
    let cdc_before = read_visible_cdc_entries(path, 0, Some(committed_before.db_version)).unwrap();

    let mut next_snapshot = committed_before.clone();
    next_snapshot.db_version += 1;
    next_snapshot.last_tx_id = format!("bundle-orphan-{}", next_snapshot.db_version);
    next_snapshot.committed_at = now_unix_seconds_string();

    let graph_change = graph_change_for_manifest(
        &next_snapshot,
        0,
        "delete",
        "edge",
        "Knows",
        "bundle:orphan",
        serde_json::json!({"from": "Alice", "to": "Bob"}),
    );
    let graph_commit = graph_commit_for_manifest(&next_snapshot, "test:bundle-cleanup");
    let bundle = build_graph_update_bundle(
        path,
        &graph_commit,
        std::slice::from_ref(&graph_change),
        &next_snapshot,
    )
    .unwrap();

    let staged_internal_versions = bundle
        .published_versions
        .iter()
        .filter(|version| {
            matches!(
                version.table_id.as_str(),
                GRAPH_TX_TABLE_ID | GRAPH_CHANGES_TABLE_ID | GRAPH_SNAPSHOT_TABLE_ID
            )
        })
        .map(|version| (version.table_id.clone(), version.version))
        .collect::<Vec<_>>();
    assert_eq!(staged_internal_versions.len(), 3);
    let mut staged_internal_paths = Vec::new();
    for (table_id, version) in staged_internal_versions {
        staged_internal_paths.push(manifest_file_for_table_version(path, &table_id, version).await);
    }
    assert!(staged_internal_paths.iter().all(|path| path.exists()));

    let removed = cleanup_namespace_orphan_versions(path, &committed_before)
        .await
        .unwrap();
    assert!(removed >= 3);
    assert!(staged_internal_paths.iter().all(|path| !path.exists()));

    let reopened = Database::open(path).await.unwrap();
    let committed_after = read_committed_graph_snapshot(path).unwrap();
    let tx_after = read_tx_catalog_entries(path).unwrap();
    let cdc_after = read_visible_cdc_entries(path, 0, Some(committed_after.db_version)).unwrap();
    let exported = build_export_rows_at_path(reopened.path(), false, true)
        .await
        .unwrap();

    assert_eq!(committed_after.db_version, committed_before.db_version);
    assert_eq!(tx_after.len(), tx_before.len());
    assert_eq!(cdc_after.len(), cdc_before.len());
    assert!(
        !exported
            .iter()
            .any(|row| { row["type"] == "Person" && row["data"]["name"] == "Bundle Person" })
    );
}

#[tokio::test]
async fn test_new_datasets_use_lance_v2_2_storage_format() {
    let dir = test_dir("new_dataset_v2_2");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();

    let manifest = read_committed_graph_snapshot(path).unwrap();
    for entry in &manifest.datasets {
        let dataset_path = path.join(&entry.dataset_path);
        assert_eq!(
            dataset_storage_version(&dataset_path).await,
            LanceFileVersion::V2_2,
            "expected {} dataset {} to use Lance V2_2",
            entry.kind,
            entry.type_name
        );
    }
}

#[tokio::test]
#[ignore = "legacy raw-layout compatibility coverage"]
async fn test_lance_v2_0_datasets_remain_operational_in_v0_11_0() {
    let dir = test_dir("lance_v2_0_compat");
    let path = dir.path();

    write_fixture_database_with_storage_version(
        path,
        test_schema_src(),
        test_data_src(),
        LanceFileVersion::V2_0,
    )
    .await;

    let manifest_before = read_committed_graph_snapshot(path).unwrap();
    for entry in &manifest_before.datasets {
        let dataset_path = path.join(&entry.dataset_path);
        assert_eq!(
            dataset_storage_version(&dataset_path).await,
            LanceFileVersion::V2_0,
            "fixture dataset {} should start on Lance V2_0",
            entry.type_name
        );
    }

    let db = Database::open(path).await.unwrap();
    let people = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(people.num_rows(), 3);

    db.load_with_mode(test_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();
    db.delete_edges(
        "Knows",
        &DeletePredicate {
            property: "src".to_string(),
            op: DeleteOp::Eq,
            value: "0".to_string(),
        },
    )
    .await
    .unwrap();
    db.compact(CompactOptions::default()).await.unwrap();
    db.cleanup(CleanupOptions {
        retain_tx_versions: 1,
        retain_dataset_versions: 1,
    })
    .await
    .unwrap();

    let manifest_after = read_committed_graph_snapshot(path).unwrap();
    for entry in &manifest_after.datasets {
        let dataset_path = path.join(&entry.dataset_path);
        assert_eq!(
            dataset_storage_version(&dataset_path).await,
            LanceFileVersion::V2_0,
            "existing dataset {} should preserve Lance V2_0",
            entry.type_name
        );
    }

    let reopened = Database::open(path).await.unwrap();
    let people = read_node_batch_for_db(&reopened, "Person").await.unwrap();
    assert_eq!(people.num_rows(), 3);
}

#[tokio::test]
async fn test_open_fresh_db() {
    let dir = test_dir("open_fresh");
    let path = dir.path();

    Database::init(path, test_schema_src()).await.unwrap();
    let db = Database::open(path).await.unwrap();

    assert_eq!(db.catalog.node_types.len(), 2);
    assert_eq!(db.catalog.edge_types.len(), 2);
}

#[tokio::test]
async fn test_open_in_memory_loads_and_cleans_up_on_last_drop() {
    let db = Database::open_in_memory(test_schema_src()).await.unwrap();
    assert!(db.is_in_memory());

    let db_path = db.path().to_path_buf();
    assert!(db_path.exists());
    assert!(db_path.join("schema.pg").exists());

    db.load(test_data_src()).await.unwrap();
    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 3);

    let clone = db.clone();
    drop(db);
    assert!(db_path.exists());

    let clone_persons = read_node_batch_for_db(&clone, "Person").await.unwrap();
    assert_eq!(clone_persons.num_rows(), 3);

    drop(clone);
    assert!(!db_path.exists());
}

#[tokio::test]
async fn test_load_and_reopen_preserves_data() {
    let dir = test_dir("load_reopen");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 3);

    let db2 = Database::open(path).await.unwrap();
    let persons2 = read_node_batch_for_db(&db2, "Person").await.unwrap();
    assert_eq!(persons2.num_rows(), 3);

    let companies = read_node_batch_for_db(&db2, "Company").await.unwrap();
    assert_eq!(companies.num_rows(), 1);

    let knows = read_edge_batch_for_db(&db2, "Knows").await.unwrap();
    assert_eq!(knows.num_rows(), 2);
    let runtime2 = db2.current_runtime();
    assert!(runtime2.edge_dataset_path("Knows").is_some());
    assert!(runtime2.edge_dataset_version("Knows").is_some());
}

#[tokio::test]
async fn test_load_file_matches_string_load() {
    let string_dir = test_dir("load_string");
    let file_dir = test_dir("load_file");
    let input_dir = test_dir("load_file_input");

    let string_db = Database::init(string_dir.path(), test_schema_src())
        .await
        .unwrap();
    string_db
        .load_with_mode(test_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();

    let input_path = write_data_file(&input_dir, "graph.jsonl", test_data_src());
    let file_db = Database::init(file_dir.path(), test_schema_src())
        .await
        .unwrap();
    file_db
        .load_file_with_mode(&input_path, LoadMode::Overwrite)
        .await
        .unwrap();

    let string_persons = read_node_batch_for_db(&string_db, "Person").await.unwrap();
    let file_persons = read_node_batch_for_db(&file_db, "Person").await.unwrap();
    assert_eq!(string_persons, file_persons);

    let string_companies = read_node_batch_for_db(&string_db, "Company").await.unwrap();
    let file_companies = read_node_batch_for_db(&file_db, "Company").await.unwrap();
    assert_eq!(string_companies, file_companies);

    let string_knows = read_edge_batch_for_db(&string_db, "Knows").await.unwrap();
    let file_knows = read_edge_batch_for_db(&file_db, "Knows").await.unwrap();
    assert_eq!(string_knows, file_knows);
    let string_works_at = read_edge_batch_for_db(&string_db, "WorksAt").await.unwrap();
    let file_works_at = read_edge_batch_for_db(&file_db, "WorksAt").await.unwrap();
    assert_eq!(string_works_at, file_works_at);
}

#[tokio::test]
async fn test_load_file_handles_forward_reference_edges() {
    let dir = test_dir("load_file_forward_refs");
    let input_dir = test_dir("load_file_forward_refs_input");
    let path = dir.path();
    let data = r#"{"edge": "Knows", "from": "Alice", "to": "Bob"}
{"type": "Person", "data": {"name": "Alice", "age": 30}}
{"type": "Person", "data": {"name": "Bob", "age": 25}}
"#;

    let input_path = write_data_file(&input_dir, "forward_refs.jsonl", data);
    let db = Database::init(path, keyed_schema_src()).await.unwrap();
    db.load_file_with_mode(&input_path, LoadMode::Overwrite)
        .await
        .unwrap();

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 2);
    let knows = read_edge_batch_for_db(&db, "Knows").await.unwrap();
    assert_eq!(knows.num_rows(), 1);
}

#[tokio::test]
async fn test_load_appends_tx_catalog_row() {
    let dir = test_dir("tx_catalog_row");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();

    let rows = read_tx_catalog_entries(path).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].db_version, 1);
    assert_eq!(rows[0].tx_id, "manifest-1");
    assert_eq!(rows[0].op_summary, "load:merge");
    assert!(
        rows[0]
            .dataset_versions
            .keys()
            .any(|key| key.starts_with("nodes/"))
    );
}

#[tokio::test]
async fn test_load_overwrite_emits_insert_cdc_events() {
    let dir = test_dir("cdc_load_insert");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load_with_mode(test_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();

    let cdc = read_visible_cdc_entries(path, 0, None).unwrap();
    assert_eq!(cdc.len(), 7);
    assert!(cdc.iter().all(|e| e.db_version == 1));
    assert!(cdc.iter().all(|e| e.op == "insert"));
    assert!(
        cdc.iter()
            .any(|e| e.entity_kind == "node" && e.type_name == "Person")
    );
    assert!(
        cdc.iter()
            .any(|e| e.entity_kind == "edge" && e.type_name == "Knows")
    );
}

#[tokio::test]
async fn test_cdc_payload_preserves_vector_as_json_array() {
    let dir = test_dir("cdc_vector_payload");
    let path = dir.path();

    let db = Database::init(path, vector_indexed_schema_src())
        .await
        .unwrap();
    db.load_with_mode(vector_indexed_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();

    let cdc = read_visible_cdc_entries(path, 0, None).unwrap();
    let insert = cdc
        .iter()
        .find(|e| e.op == "insert" && e.entity_kind == "node" && e.type_name == "Doc")
        .expect("expected Doc insert CDC event");
    let embedding = insert
        .payload
        .get("embedding")
        .expect("embedding field present in CDC payload");
    assert!(
        embedding.is_array(),
        "expected embedding to be serialized as JSON array, got {}",
        embedding
    );
}

#[tokio::test]
async fn test_load_merge_emits_update_and_insert_cdc_events() {
    let dir = test_dir("cdc_load_merge");
    let path = dir.path();

    let db = init_v4_db(path, keyed_schema_src()).await;
    db.load_with_mode(keyed_data_initial(), LoadMode::Overwrite)
        .await
        .unwrap();
    db.load_with_mode(keyed_data_upsert(), LoadMode::Merge)
        .await
        .unwrap();

    let cdc = read_visible_graph_change_records(path, 1, Some(2)).unwrap();
    assert!(
        cdc.iter()
            .any(|e| e.op == "update" && e.entity_kind == "node" && e.type_name == "Person")
    );
    assert!(
        cdc.iter()
            .any(|e| e.op == "insert" && e.entity_kind == "node" && e.type_name == "Person")
    );

    let person_update = cdc
        .iter()
        .find(|e| e.op == "update" && e.entity_kind == "node" && e.type_name == "Person")
        .unwrap();
    assert!(person_update.payload.get("before").is_some());
    assert!(person_update.payload.get("after").is_some());
}

#[tokio::test]
async fn test_v5_changes_returns_lineage_native_rows_for_merge() {
    let dir = test_dir("v5_changes_merge");
    let path = dir.path();

    let db = Database::init(path, keyed_schema_src()).await.unwrap();
    db.load_with_mode(keyed_data_initial(), LoadMode::Overwrite)
        .await
        .unwrap();
    db.load_with_mode(keyed_data_upsert(), LoadMode::Merge)
        .await
        .unwrap();

    let rows = read_visible_change_rows(path, 1, Some(2)).unwrap();
    assert!(rows.len() >= 3);

    let insert = rows
        .iter()
        .find(|row| {
            row.change_kind == "insert"
                && row.entity_kind == "node"
                && row.type_name == "Person"
                && row.row["name"] == "Charlie"
        })
        .unwrap();
    let charlie_rowid = rowid_for_entity(path, "node", "Person", insert.entity_id).await;
    assert_eq!(insert.graph_version, 2);
    assert_eq!(insert.previous_graph_version, Some(1));
    assert_eq!(insert.rowid, charlie_rowid);

    let update = rows
        .iter()
        .find(|row| {
            row.change_kind == "update"
                && row.entity_kind == "node"
                && row.type_name == "Person"
                && row.row["name"] == "Alice"
        })
        .unwrap();
    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    let alice_id = person_id_by_name(&persons, "Alice");
    let alice_rowid = rowid_for_entity(path, "node", "Person", update.entity_id).await;
    assert_eq!(update.graph_version, 2);
    assert_eq!(update.previous_graph_version, Some(1));
    assert_eq!(update.rowid, alice_rowid);
    assert_eq!(update.entity_id, alice_id);
    assert!(
        rows.iter().any(|row| {
            row.change_kind == "insert"
                && row.entity_kind == "edge"
                && row.type_name == "Knows"
        }),
        "expected lineage-native rows to include the new Knows edge: {rows:?}"
    );

    let mut sorted = rows.clone();
    sorted.sort_by(|a, b| {
        a.graph_version
            .cmp(&b.graph_version)
            .then(a.entity_kind.cmp(&b.entity_kind))
            .then(a.type_name.cmp(&b.type_name))
            .then(a.rowid.cmp(&b.rowid))
            .then(a.logical_key.cmp(&b.logical_key))
            .then(a.change_kind.cmp(&b.change_kind))
    });
    assert_eq!(rows, sorted);
}

#[tokio::test]
#[ignore = "legacy v3 wal repair coverage"]
async fn test_open_repairs_trailing_partial_tx_catalog_row() {
    let dir = test_dir("tx_catalog_repair");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();
    drop(db);

    let tx_path = path.join("_wal.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&tx_path)
        .unwrap();
    use std::io::Write;
    file.write_all(br#"{"tx_id":"partial""#).unwrap();
    file.sync_all().unwrap();

    let reopened = Database::open(path).await.unwrap();
    let persons = read_node_batch_for_db(&reopened, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 3);

    let rows = read_tx_catalog_entries(path).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].db_version, 1);
}

#[tokio::test]
#[ignore = "legacy v3 wal repair coverage"]
async fn test_open_truncates_tx_catalog_rows_beyond_manifest_version() {
    let dir = test_dir("tx_catalog_manifest_gate");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();
    drop(db);

    let mut rows = read_tx_catalog_entries(path).unwrap();
    assert_eq!(rows.len(), 1);

    let mut future = rows.pop().unwrap();
    future.tx_id = "future-tx".to_string();
    future.db_version = 2;
    append_tx_catalog_entry(path, &future).unwrap();

    let rows_before = read_tx_catalog_entries(path).unwrap();
    assert_eq!(rows_before.len(), 2);

    let reopened = Database::open(path).await.unwrap();
    let persons = read_node_batch_for_db(&reopened, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 3);

    let rows_after = read_tx_catalog_entries(path).unwrap();
    assert_eq!(rows_after.len(), 1);
    assert_eq!(rows_after[0].db_version, 1);
}

#[tokio::test]
async fn test_load_deduplicates_edges() {
    let dir = test_dir("dedup_edges");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(duplicate_edge_data_src()).await.unwrap();

    let knows = read_edge_batch_for_db(&db, "Knows").await.unwrap();
    assert_eq!(knows.num_rows(), 1);
}

#[tokio::test]
async fn test_load_mode_overwrite_replaces_existing_data() {
    let dir = test_dir("mode_overwrite");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load_with_mode(test_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();
    db.load_with_mode(
        r#"{"type": "Person", "data": {"name": "OnlyOne", "age": 77}}"#,
        LoadMode::Overwrite,
    )
    .await
    .unwrap();

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 1);
    let names = persons
        .column_by_name("name")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(names.value(0), "OnlyOne");
}

#[tokio::test]
async fn test_load_mode_overwrite_supports_user_property_named_id_key() {
    let dir = test_dir("mode_overwrite_user_id_key");
    let path = dir.path();

    let db = Database::init(path, id_named_key_schema_src())
        .await
        .unwrap();
    db.load_with_mode(id_named_key_initial_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 2);
    let user_ids = persons
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(user_ids.value(0), "usr_alice");
    assert_eq!(user_ids.value(1), "usr_bob");

    let alice_id = person_internal_id_by_user_id(&persons, "usr_alice");
    let bob_id = person_internal_id_by_user_id(&persons, "usr_bob");
    let knows = read_edge_batch_for_db(&db, "Knows").await.unwrap();
    let src = knows
        .column_by_name("src")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let dst = knows
        .column_by_name("dst")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    assert_eq!(knows.num_rows(), 1);
    assert_eq!(src.value(0), alice_id);
    assert_eq!(dst.value(0), bob_id);
}

#[tokio::test]
async fn test_load_mode_append_adds_rows_and_can_reference_existing_nodes() {
    let dir = test_dir("mode_append");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load_with_mode(test_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();
    db.load_with_mode(
        append_data_new_person_with_edge_to_existing(),
        LoadMode::Append,
    )
    .await
    .unwrap();

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 4);
    let alice_id = person_id_by_name(&persons, "Alice");
    let diana_id = person_id_by_name(&persons, "Diana");
    let knows = read_edge_batch_for_db(&db, "Knows").await.unwrap();
    let src = knows
        .column_by_name("src")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let dst = knows
        .column_by_name("dst")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    assert!(
        (0..knows.num_rows()).any(|row| src.value(row) == diana_id && dst.value(row) == alice_id)
    );
}

#[tokio::test]
async fn test_load_mode_append_supports_user_property_named_id_key() {
    let dir = test_dir("mode_append_user_id_key");
    let path = dir.path();

    let db = Database::init(path, id_named_key_schema_src())
        .await
        .unwrap();
    db.load_with_mode(id_named_key_initial_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();
    db.load_with_mode(id_named_key_append_data_src(), LoadMode::Append)
        .await
        .unwrap();

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 3);

    let alice_id = person_internal_id_by_user_id(&persons, "usr_alice");
    let charlie_id = person_internal_id_by_user_id(&persons, "usr_charlie");
    assert_eq!(person_age_by_user_id(&persons, "usr_charlie"), Some(40));

    let knows = read_edge_batch_for_db(&db, "Knows").await.unwrap();
    let src = knows
        .column_by_name("src")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let dst = knows
        .column_by_name("dst")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    assert!(
        (0..knows.num_rows()).any(|row| src.value(row) == charlie_id && dst.value(row) == alice_id)
    );
}

#[tokio::test]
async fn test_load_mode_append_rejects_duplicate_key() {
    let dir = test_dir("mode_append_dupe");
    let path = dir.path();

    let db = Database::init(path, keyed_schema_src()).await.unwrap();
    db.load_with_mode(keyed_data_initial(), LoadMode::Overwrite)
        .await
        .unwrap();

    let err = db
        .load_with_mode(keyed_data_append_duplicate(), LoadMode::Append)
        .await
        .unwrap_err();
    match err {
        NanoError::UniqueConstraint { value, .. } => assert_eq!(value, "Alice"),
        other => panic!("expected UniqueConstraint, got {}", other),
    }
}

#[tokio::test]
async fn test_load_mode_merge_updates_and_preserves_existing_ids() {
    let dir = test_dir("mode_merge");
    let path = dir.path();

    let db = Database::init(path, keyed_schema_src()).await.unwrap();
    db.load_with_mode(keyed_data_initial(), LoadMode::Overwrite)
        .await
        .unwrap();

    let persons_before = read_node_batch_for_db(&db, "Person").await.unwrap();
    let alice_before = person_id_by_name(&persons_before, "Alice");

    db.load_with_mode(keyed_data_upsert(), LoadMode::Merge)
        .await
        .unwrap();

    let persons_after = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons_after.num_rows(), 3);
    assert_eq!(person_id_by_name(&persons_after, "Alice"), alice_before);
    assert_eq!(person_age_by_name(&persons_after, "Alice"), Some(31));
    assert!(person_age_by_name(&persons_after, "Charlie").is_some());
}

#[tokio::test]
async fn test_load_mode_merge_supports_user_property_named_id_key() {
    let dir = test_dir("mode_merge_user_id_key");
    let path = dir.path();

    let db = Database::init(path, id_named_key_schema_src())
        .await
        .unwrap();
    db.load_with_mode(id_named_key_initial_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();

    let persons_before = read_node_batch_for_db(&db, "Person").await.unwrap();
    let alice_before = person_internal_id_by_user_id(&persons_before, "usr_alice");

    db.load_with_mode(id_named_key_merge_data_src(), LoadMode::Merge)
        .await
        .unwrap();

    let persons_after = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons_after.num_rows(), 3);
    assert_eq!(
        person_internal_id_by_user_id(&persons_after, "usr_alice"),
        alice_before
    );
    assert_eq!(person_age_by_user_id(&persons_after, "usr_alice"), Some(31));
    assert_eq!(
        person_age_by_user_id(&persons_after, "usr_charlie"),
        Some(40)
    );

    let alice_id = person_internal_id_by_user_id(&persons_after, "usr_alice");
    let charlie_id = person_internal_id_by_user_id(&persons_after, "usr_charlie");
    let knows = read_edge_batch_for_db(&db, "Knows").await.unwrap();
    let src = knows
        .column_by_name("src")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let dst = knows
        .column_by_name("dst")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    assert!(
        (0..knows.num_rows()).any(|row| src.value(row) == alice_id && dst.value(row) == charlie_id)
    );
}

#[tokio::test]
async fn test_load_append_rewrites_only_changed_node_dataset_version() {
    let dir = test_dir("append_dataset_version_node");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load_with_mode(test_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();
    let manifest_before = read_committed_graph_snapshot(path).unwrap();
    let person_version_before = dataset_version_for(&manifest_before, "node", "Person");
    let company_version_before = dataset_version_for(&manifest_before, "node", "Company");
    let knows_version_before = dataset_version_for(&manifest_before, "edge", "Knows");
    let works_at_version_before = dataset_version_for(&manifest_before, "edge", "WorksAt");
    let person_path_before = dataset_rel_path_for(&manifest_before, "node", "Person");
    let company_path_before = dataset_rel_path_for(&manifest_before, "node", "Company");
    let knows_path_before = dataset_rel_path_for(&manifest_before, "edge", "Knows");

    db.load_with_mode(
        r#"{"type": "Person", "data": {"name": "Diana", "age": 28}}"#,
        LoadMode::Append,
    )
    .await
    .unwrap();

    let manifest_after = read_committed_graph_snapshot(path).unwrap();
    assert_eq!(
        dataset_rel_path_for(&manifest_after, "node", "Person"),
        person_path_before
    );
    assert_eq!(
        dataset_rel_path_for(&manifest_after, "node", "Company"),
        company_path_before
    );
    assert_eq!(
        dataset_rel_path_for(&manifest_after, "edge", "Knows"),
        knows_path_before
    );
    assert!(
        dataset_version_for(&manifest_after, "node", "Person") > person_version_before,
        "expected Person dataset version to advance on append"
    );
    assert_eq!(
        dataset_version_for(&manifest_after, "node", "Company"),
        company_version_before
    );
    assert_eq!(
        dataset_version_for(&manifest_after, "edge", "Knows"),
        knows_version_before
    );
    assert_eq!(
        dataset_version_for(&manifest_after, "edge", "WorksAt"),
        works_at_version_before
    );
}

#[tokio::test]
async fn test_load_append_rewrites_only_changed_edge_dataset_version() {
    let dir = test_dir("append_dataset_version_edge");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load_with_mode(test_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();
    let manifest_before = read_committed_graph_snapshot(path).unwrap();
    let person_version_before = dataset_version_for(&manifest_before, "node", "Person");
    let knows_version_before = dataset_version_for(&manifest_before, "edge", "Knows");
    let works_at_version_before = dataset_version_for(&manifest_before, "edge", "WorksAt");
    let knows_path_before = dataset_rel_path_for(&manifest_before, "edge", "Knows");

    db.load_with_mode(
        r#"{"edge": "Knows", "from": "Bob", "to": "Charlie"}"#,
        LoadMode::Append,
    )
    .await
    .unwrap();

    let manifest_after = read_committed_graph_snapshot(path).unwrap();
    assert_eq!(
        dataset_rel_path_for(&manifest_after, "edge", "Knows"),
        knows_path_before
    );
    assert_eq!(
        dataset_version_for(&manifest_after, "node", "Person"),
        person_version_before
    );
    assert!(
        dataset_version_for(&manifest_after, "edge", "Knows") > knows_version_before,
        "expected Knows dataset version to advance on append"
    );
    assert_eq!(
        dataset_version_for(&manifest_after, "edge", "WorksAt"),
        works_at_version_before
    );
}

#[tokio::test]
async fn test_load_merge_rewrites_only_changed_dataset_versions() {
    let dir = test_dir("merge_dataset_versions");
    let path = dir.path();

    let db = Database::init(path, keyed_schema_src()).await.unwrap();
    db.load_with_mode(keyed_data_initial(), LoadMode::Overwrite)
        .await
        .unwrap();
    let manifest_before = read_committed_graph_snapshot(path).unwrap();
    let person_version_before = dataset_version_for(&manifest_before, "node", "Person");
    let company_version_before = dataset_version_for(&manifest_before, "node", "Company");
    let knows_version_before = dataset_version_for(&manifest_before, "edge", "Knows");
    let person_path_before = dataset_rel_path_for(&manifest_before, "node", "Person");
    let knows_path_before = dataset_rel_path_for(&manifest_before, "edge", "Knows");

    db.load_with_mode(keyed_data_upsert(), LoadMode::Merge)
        .await
        .unwrap();

    let manifest_after = read_committed_graph_snapshot(path).unwrap();
    assert_eq!(
        dataset_rel_path_for(&manifest_after, "node", "Person"),
        person_path_before
    );
    assert_eq!(
        dataset_rel_path_for(&manifest_after, "edge", "Knows"),
        knows_path_before
    );
    assert!(
        dataset_version_for(&manifest_after, "node", "Person") > person_version_before,
        "expected Person dataset version to advance on merge"
    );
    assert_eq!(
        dataset_version_for(&manifest_after, "node", "Company"),
        company_version_before
    );
    assert!(
        dataset_version_for(&manifest_after, "edge", "Knows") > knows_version_before,
        "expected Knows dataset version to advance on merge"
    );
}

#[tokio::test]
async fn test_native_append_preserves_reserved_physical_names_on_disk() {
    let dir = test_dir("native_append_reserved_names");
    let path = dir.path();

    let db = Database::init(path, vector_nullable_keyed_schema_src())
        .await
        .unwrap();
    db.load_with_mode(
        vector_nullable_keyed_initial_data_src(),
        LoadMode::Overwrite,
    )
    .await
    .unwrap();

    let manifest_before = read_committed_graph_snapshot(path).unwrap();
    let dataset_rel_path = dataset_rel_path_for(&manifest_before, "node", "Doc");
    let dataset_path = path.join(&dataset_rel_path);
    let uri = dataset_path.to_string_lossy().to_string();
    let dataset = Dataset::open(&uri).await.unwrap();
    let fields_before: Vec<String> = dataset
        .schema()
        .fields
        .iter()
        .map(|field| field.name.clone())
        .collect();
    assert_eq!(fields_before[0], LANCE_INTERNAL_ID_FIELD);

    db.load_with_mode(vector_nullable_keyed_append_data_src(), LoadMode::Append)
        .await
        .unwrap();

    let manifest_after = read_committed_graph_snapshot(path).unwrap();
    let dataset_rel_path_after = dataset_rel_path_for(&manifest_after, "node", "Doc");
    assert_eq!(dataset_rel_path_after, dataset_rel_path);
    let reopened = Dataset::open(&uri).await.unwrap();
    let fields_after: Vec<String> = reopened
        .schema()
        .fields
        .iter()
        .map(|field| field.name.clone())
        .collect();
    assert_eq!(fields_after[0], LANCE_INTERNAL_ID_FIELD);
}

#[tokio::test]
async fn test_native_merge_preserves_reserved_physical_names_on_disk() {
    let dir = test_dir("native_merge_reserved_names");
    let path = dir.path();

    let db = Database::init(path, vector_nullable_keyed_schema_src())
        .await
        .unwrap();
    db.load_with_mode(
        vector_nullable_keyed_initial_data_src(),
        LoadMode::Overwrite,
    )
    .await
    .unwrap();

    let manifest_before = read_committed_graph_snapshot(path).unwrap();
    let dataset_rel_path = dataset_rel_path_for(&manifest_before, "node", "Doc");
    let dataset_path = path.join(&dataset_rel_path);
    let uri = dataset_path.to_string_lossy().to_string();
    let dataset = Dataset::open(&uri).await.unwrap();
    let fields_before: Vec<String> = dataset
        .schema()
        .fields
        .iter()
        .map(|field| field.name.clone())
        .collect();
    assert_eq!(fields_before[0], LANCE_INTERNAL_ID_FIELD);

    db.load_with_mode(vector_nullable_keyed_merge_data_src(), LoadMode::Merge)
        .await
        .unwrap();

    let manifest_after = read_committed_graph_snapshot(path).unwrap();
    let dataset_rel_path_after = dataset_rel_path_for(&manifest_after, "node", "Doc");
    assert_eq!(dataset_rel_path_after, dataset_rel_path);
    let reopened = Dataset::open(&uri).await.unwrap();
    let fields_after: Vec<String> = reopened
        .schema()
        .fields
        .iter()
        .map(|field| field.name.clone())
        .collect();
    assert_eq!(fields_after[0], LANCE_INTERNAL_ID_FIELD);
}

#[tokio::test]
async fn test_merge_preserves_vector_values_after_reopen() {
    let dir = test_dir("merge_vector_reopen");
    let path = dir.path();

    let db = Database::init(path, vector_nullable_keyed_schema_src())
        .await
        .unwrap();
    db.load_with_mode(
        vector_nullable_keyed_initial_data_src(),
        LoadMode::Overwrite,
    )
    .await
    .unwrap();
    db.load_with_mode(vector_nullable_keyed_merge_data_src(), LoadMode::Merge)
        .await
        .unwrap();

    let reopened = Database::open(path).await.unwrap();
    let docs = read_node_batch_for_db(&reopened, "Doc").await.unwrap();
    assert_eq!(docs.num_rows(), 2);
    assert_eq!(doc_title_by_slug(&docs, "a").as_deref(), Some("A2"));
    assert_eq!(doc_title_by_slug(&docs, "b").as_deref(), Some("B"));
}

#[tokio::test]
async fn test_append_preserves_vector_values_after_reopen() {
    let dir = test_dir("append_vector_reopen");
    let path = dir.path();

    let db = Database::init(path, vector_nullable_keyed_schema_src())
        .await
        .unwrap();
    db.load_with_mode(
        vector_nullable_keyed_initial_data_src(),
        LoadMode::Overwrite,
    )
    .await
    .unwrap();
    db.load_with_mode(vector_nullable_keyed_append_data_src(), LoadMode::Append)
        .await
        .unwrap();

    let reopened = Database::open(path).await.unwrap();
    let docs = read_node_batch_for_db(&reopened, "Doc").await.unwrap();
    assert_eq!(docs.num_rows(), 2);
    assert_eq!(doc_title_by_slug(&docs, "a").as_deref(), Some("A"));
    assert_eq!(doc_title_by_slug(&docs, "b").as_deref(), Some("B"));
}

#[tokio::test]
async fn test_delete_nodes_uses_lance_native_delete_path() {
    let dir = test_dir("delete_nodes_native");
    let path = dir.path();

    let db = Database::init(path, keyed_schema_src()).await.unwrap();
    db.load_with_mode(keyed_data_initial(), LoadMode::Overwrite)
        .await
        .unwrap();
    let manifest_before = read_committed_graph_snapshot(path).unwrap();
    let person_path_before = dataset_rel_path_for(&manifest_before, "node", "Person");
    let company_version_before = dataset_version_for(&manifest_before, "node", "Company");

    db.delete_nodes(
        "Person",
        &DeletePredicate {
            property: "name".to_string(),
            op: DeleteOp::Eq,
            value: "Bob".to_string(),
        },
    )
    .await
    .unwrap();

    let manifest_after = read_committed_graph_snapshot(path).unwrap();
    let person_path_after = dataset_rel_path_for(&manifest_after, "node", "Person");
    assert_eq!(person_path_before, person_path_after);
    assert_eq!(
        dataset_version_for(&manifest_after, "node", "Company"),
        company_version_before
    );
    assert!(
        manifest_after
            .datasets
            .iter()
            .all(|entry| !(entry.kind == "edge" && entry.type_name == "Knows")),
        "expected empty Knows dataset to be dropped from the manifest"
    );

    let person_uri = path.join(&person_path_after).to_string_lossy().to_string();
    let person_dataset = Dataset::open(&person_uri).await.unwrap();
    assert!(
        person_dataset.count_deleted_rows().await.unwrap() > 0,
        "expected native delete tombstones in Person dataset"
    );
}

#[tokio::test]
async fn test_compact_advances_dataset_versions_and_commits_manifest() {
    let dir = test_dir("compact_versions");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();
    db.delete_edges(
        "Knows",
        &DeletePredicate {
            property: "src".to_string(),
            op: DeleteOp::Eq,
            value: "0".to_string(),
        },
    )
    .await
    .unwrap();

    let manifest_before = read_committed_graph_snapshot(path).unwrap();
    let result = db
        .compact(CompactOptions {
            target_rows_per_fragment: 1_024,
            materialize_deletions: true,
            materialize_deletions_threshold: 0.0,
        })
        .await
        .unwrap();

    assert!(result.datasets_considered >= 1);
    let manifest_after = read_committed_graph_snapshot(path).unwrap();
    let version_advanced = manifest_after.datasets.iter().any(|entry| {
        manifest_before
            .datasets
            .iter()
            .find(|prev| prev.kind == entry.kind && prev.type_name == entry.type_name)
            .map(|prev| entry.dataset_version > prev.dataset_version)
            .unwrap_or(false)
    });

    if version_advanced {
        assert!(result.manifest_committed);
        assert!(manifest_after.db_version > manifest_before.db_version);
    } else {
        assert_eq!(result.datasets_compacted, 0);
        assert!(!result.manifest_committed);
        assert_eq!(manifest_after.db_version, manifest_before.db_version);
    }
}

#[tokio::test]
async fn test_cleanup_prunes_old_dataset_versions_but_keeps_manifest_visible_state() {
    let dir = test_dir("cleanup_versions");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();
    db.delete_edges(
        "Knows",
        &DeletePredicate {
            property: "src".to_string(),
            op: DeleteOp::Eq,
            value: "0".to_string(),
        },
    )
    .await
    .unwrap();
    db.compact(CompactOptions::default()).await.unwrap();

    let manifest_before = read_committed_graph_snapshot(path).unwrap();
    let result = db
        .cleanup(CleanupOptions {
            retain_tx_versions: 1,
            retain_dataset_versions: 1,
        })
        .await
        .unwrap();
    assert!(result.tx_rows_kept > 0);

    let manifest_after = read_committed_graph_snapshot(path).unwrap();
    assert_eq!(manifest_after.db_version, manifest_before.db_version);

    let reopened = Database::open(path).await.unwrap();
    let persons = read_node_batch_for_db(&reopened, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 3);
}

#[tokio::test]
async fn test_v4_cleanup_retains_versions_needed_for_lineage_backed_cdc() {
    let dir = test_dir("cleanup_lineage_cdc_versions");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load_with_mode(
        r#"{"type":"Person","data":{"name":"Alice","age":30}}"#,
        LoadMode::Overwrite,
    )
    .await
    .unwrap();
    db.load_with_mode(
        r#"{"type":"Person","data":{"name":"Alice","age":31}}"#,
        LoadMode::Merge,
    )
    .await
    .unwrap();
    db.load_with_mode(
        r#"{"type":"Person","data":{"name":"Alice","age":32}}"#,
        LoadMode::Merge,
    )
    .await
    .unwrap();

    db.cleanup(CleanupOptions {
        retain_tx_versions: 2,
        retain_dataset_versions: 1,
    })
    .await
    .unwrap();

    let cdc_rows = read_visible_cdc_entries(path, 1, None).unwrap();
    assert_eq!(cdc_rows.len(), 2);
    assert_eq!(
        cdc_rows
            .iter()
            .map(|row| row.payload["after"]["age"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![31, 32]
    );
}

#[tokio::test]
async fn test_doctor_reports_healthy_database() {
    let dir = test_dir("doctor_healthy");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();

    let report = db.doctor().await.unwrap();
    assert!(
        report.healthy,
        "expected healthy report: {:?}",
        report.issues
    );
    assert!(report.issues.is_empty());
    assert!(
        report.warnings.is_empty(),
        "expected no warnings for healthy database: {:?}",
        report.warnings
    );
    assert!(report.datasets_checked >= 1);
    assert_eq!(report.datasets.len(), report.datasets_checked);
    assert!(
        report
            .datasets
            .iter()
            .all(|dataset| dataset.storage_version == "2.2"),
        "expected doctor to report Lance 2.2 storage versions for new datasets"
    );
    assert!(
        report.lineage_shadow.is_none(),
        "expected default NamespaceLineage doctor report to omit V4 lineage-shadow details"
    );
}

#[tokio::test]
async fn test_v4_doctor_reports_graph_change_rowid_mismatches() {
    let dir = test_dir("doctor_v4_rowid_mismatch");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();
    drop(db);

    let mut changes = read_visible_graph_change_records(path, 0, None).unwrap();
    let alice_entity_key = {
        let alice_insert = changes
            .iter_mut()
            .find(|row| {
                row.op == "insert"
                    && row.entity_kind == "node"
                    && row.type_name == "Person"
                    && row.payload.get("name").and_then(|value| value.as_str()) == Some("Alice")
            })
            .unwrap();
        alice_insert.rowid_if_known = Some(alice_insert.rowid_if_known.unwrap() + 1);
        alice_insert.entity_key.clone()
    };

    overwrite_committed_graph_changes(path, &changes).await;

    let reopened = Database::open(path).await.unwrap();
    let report = reopened.doctor().await.unwrap();
    assert!(!report.healthy);
    assert!(
        report.issues.iter().any(|issue| {
            issue.contains("recorded rowid")
                && issue.contains("Person")
                && issue.contains(&alice_entity_key)
        }),
        "expected doctor to report rowid mismatch, got {:?}",
        report.issues
    );
}

#[tokio::test]
#[ignore = "legacy graph mirror coverage"]
async fn test_graph_mirror_matches_authoritative_wal_after_commit() {
    let dir = test_dir("graph_mirror_matches_wal");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();

    let tx_rows = read_tx_catalog_entries(path).unwrap();
    let cdc_rows = read_visible_cdc_entries(path, 0, None).unwrap();
    let mirror_commits = read_graph_commit_mirror(path).await.unwrap();
    let mirror_changes = read_graph_change_mirror(path).await.unwrap();

    assert_eq!(mirror_commits.len(), tx_rows.len());
    assert_eq!(mirror_changes.len(), cdc_rows.len());

    for (mirror, tx) in mirror_commits.iter().zip(tx_rows.iter()) {
        assert_eq!(mirror.tx_id.as_str(), tx.tx_id);
        assert_eq!(mirror.graph_version.value(), tx.db_version);
        assert_eq!(mirror.committed_at, tx.committed_at);
        assert_eq!(mirror.op_summary, tx.op_summary);
        assert_eq!(graph_commit_table_versions(mirror), tx.dataset_versions);
    }

    for (mirror, cdc) in mirror_changes.iter().zip(cdc_rows.iter()) {
        assert_eq!(mirror.tx_id.as_str(), cdc.tx_id);
        assert_eq!(mirror.graph_version.value(), cdc.db_version);
        assert_eq!(mirror.seq_in_tx, cdc.seq_in_tx);
        assert_eq!(mirror.op, cdc.op);
        assert_eq!(mirror.entity_kind, cdc.entity_kind);
        assert_eq!(mirror.type_name, cdc.type_name);
        assert_eq!(mirror.entity_key, cdc.entity_key);
        assert_eq!(mirror.payload, cdc.payload);
        assert_eq!(mirror.committed_at, cdc.committed_at);
    }
}

#[tokio::test]
#[ignore = "legacy graph mirror coverage"]
async fn test_rebuild_graph_mirror_recreates_missing_datasets() {
    let dir = test_dir("graph_mirror_rebuild");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();

    std::fs::remove_dir_all(path.join(GRAPH_COMMITS_DATASET_DIR)).unwrap();
    std::fs::remove_dir_all(path.join(GRAPH_CHANGES_DATASET_DIR)).unwrap();

    rebuild_graph_mirror_from_wal(path).await.unwrap();
    let status = inspect_graph_mirror(path).await.unwrap();
    assert!(status.commits_present);
    assert!(status.changes_present);
    assert_eq!(status.latest_commit_version, Some(1));
    assert_eq!(status.latest_change_version, Some(1));
}

#[tokio::test]
#[ignore = "legacy graph mirror coverage"]
async fn test_cleanup_rebuilds_graph_mirror_to_retained_window() {
    let dir = test_dir("graph_mirror_cleanup");
    let path = dir.path();

    let db = Database::init(path, keyed_schema_src()).await.unwrap();
    db.load_with_mode(keyed_data_initial(), LoadMode::Overwrite)
        .await
        .unwrap();
    db.load_with_mode(keyed_data_upsert(), LoadMode::Merge)
        .await
        .unwrap();

    let before = read_graph_commit_mirror(path).await.unwrap();
    assert_eq!(before.len(), 2);
    assert_eq!(before.last().map(|row| row.graph_version.value()), Some(2));

    db.cleanup(CleanupOptions {
        retain_tx_versions: 1,
        retain_dataset_versions: 1,
    })
    .await
    .unwrap();

    let tx_rows = read_tx_catalog_entries(path).unwrap();
    let mirror_commits = read_graph_commit_mirror(path).await.unwrap();
    let mirror_changes = read_graph_change_mirror(path).await.unwrap();

    assert_eq!(tx_rows.len(), 1);
    assert_eq!(mirror_commits.len(), 1);
    assert_eq!(
        mirror_commits[0].graph_version.value(),
        tx_rows[0].db_version
    );
    assert!(
        mirror_changes
            .iter()
            .all(|row| row.graph_version.value() == tx_rows[0].db_version)
    );
}

#[tokio::test]
#[ignore = "legacy graph mirror coverage"]
async fn test_mirror_write_failure_does_not_invalidate_commit_and_doctor_warns() {
    let dir = test_dir("graph_mirror_failure");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    std::fs::write(path.join(GRAPH_COMMITS_DATASET_DIR), "blocker").unwrap();

    db.load(test_data_src()).await.unwrap();

    let tx_rows = read_tx_catalog_entries(path).unwrap();
    assert_eq!(tx_rows.len(), 1);

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 3);

    let report = db.doctor().await.unwrap();
    assert!(report.healthy);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("graph mirror"))
    );
}

#[tokio::test]
async fn test_migration_rebuilds_scalar_indexes_for_indexed_properties() {
    let dir = test_dir("migration_scalar_indexed_props");
    let path = dir.path();

    let db = init_v4_db(path, indexed_schema_src()).await;
    db.load(indexed_data_src()).await.unwrap();
    drop(db);

    std::fs::write(
        path.join("schema.pg"),
        r#"node Person {
    name: String @key
    handle: String @index
    age: I32?
    city: String?
}
"#,
    )
    .unwrap();
    let result = execute_schema_migration(path, None, false, true)
        .await
        .unwrap();
    assert_eq!(result.status, MigrationStatus::Applied);

    let db = Database::open(path).await.unwrap();
    let person = db
        .schema_ir
        .node_types()
        .find(|n| n.name == "Person")
        .expect("person node type");
    let expected_index_name = crate::store::indexing::scalar_index_name(person.type_id, "handle");
    let expected_text_index_name =
        crate::store::indexing::text_index_name(person.type_id, "handle");
    let metadata = DatabaseMetadata::open(path).unwrap();
    let locator = metadata.node_dataset_locator("Person").unwrap();
    let dataset = open_dataset_for_locator(&locator).await.unwrap();
    let index_names: HashSet<String> = dataset
        .load_indices()
        .await
        .unwrap()
        .iter()
        .map(|idx| idx.name.clone())
        .collect();
    assert!(
        index_names.contains(&expected_index_name),
        "expected scalar index {} after migration",
        expected_index_name
    );
    assert!(
        index_names.contains(&expected_text_index_name),
        "expected text index {} after migration",
        expected_text_index_name
    );
}

#[tokio::test]
async fn test_v4_schema_migration_preserves_internal_tables() {
    let dir = test_dir("v4_schema_migration_internal_tables");
    let path = dir.path();

    let db = init_v4_db(path, indexed_schema_src()).await;
    db.load(indexed_data_src()).await.unwrap();
    drop(db);

    std::fs::write(
        path.join("schema.pg"),
        r#"node Person {
    name: String @key
    handle: String @index
    age: I32?
    city: String?
}
"#,
    )
    .unwrap();
    let result = execute_schema_migration(path, None, false, true)
        .await
        .unwrap();
    assert_eq!(result.status, MigrationStatus::Applied);

    let manifest = read_committed_graph_snapshot(path).unwrap();
    for table_id in [GRAPH_TX_TABLE_ID, GRAPH_CHANGES_TABLE_ID, "__blob_store"] {
        assert!(
            manifest
                .datasets
                .iter()
                .any(|entry| entry.effective_table_id() == table_id),
            "expected {} in committed v4 snapshot after schema migration",
            table_id
        );
    }

    let reopened = Database::open(path).await.unwrap();
    drop(reopened);

    let tx_rows = read_tx_catalog_entries(path).unwrap();
    assert_eq!(tx_rows.last().unwrap().op_summary, "schema_migration");
}

#[tokio::test]
async fn test_v4_graph_changes_capture_stable_row_ids_for_live_rows() {
    let dir = test_dir("v4_graph_changes_rowids");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();

    let manifest_after_insert = read_committed_graph_snapshot(path).unwrap();
    let insert_rows =
        read_visible_graph_change_records(path, 0, Some(manifest_after_insert.db_version)).unwrap();
    let alice_insert = insert_rows
        .iter()
        .find(|row| {
            row.op == "insert"
                && row.entity_kind == "node"
                && row.type_name == "Person"
                && row.payload.get("name").and_then(|value| value.as_str()) == Some("Alice")
        })
        .unwrap();
    let alice_entity_id = entity_id_from_key(&alice_insert.entity_key);
    let alice_rowid_before = rowid_for_entity(path, "node", "Person", alice_entity_id).await;
    assert_eq!(alice_insert.rowid_if_known, Some(alice_rowid_before));

    db.load_with_mode(
        r#"{"type":"Person","data":{"name":"Alice","age":31}}"#,
        LoadMode::Merge,
    )
    .await
    .unwrap();

    let manifest_after_update = read_committed_graph_snapshot(path).unwrap();
    let alice_rowid_after = rowid_for_entity(path, "node", "Person", alice_entity_id).await;
    let update_rows = read_visible_graph_change_records(
        path,
        manifest_after_insert.db_version,
        Some(manifest_after_update.db_version),
    )
    .unwrap();
    let alice_update = update_rows
        .iter()
        .find(|row| {
            row.op == "update"
                && row.entity_kind == "node"
                && row.type_name == "Person"
                && row
                    .payload
                    .get("after")
                    .and_then(|value| value.get("name"))
                    .and_then(|value| value.as_str())
                    == Some("Alice")
        })
        .unwrap();

    assert_eq!(alice_rowid_after, alice_rowid_before);
    assert_eq!(alice_update.rowid_if_known, Some(alice_rowid_before));
    assert_lineage_shadow_cdc_matches_authoritative(
        path,
        0,
        Some(manifest_after_update.db_version),
    );
    let report = db.doctor().await.unwrap();
    assert!(
        report.healthy,
        "expected healthy report after insert/update lineage flow: {report:?}"
    );
}

#[tokio::test]
async fn test_v4_doctor_reports_lineage_shadow_mismatches() {
    let dir = test_dir("doctor_v4_lineage_shadow_mismatch");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();
    db.load_with_mode(
        r#"{"type":"Person","data":{"name":"Alice","age":31}}"#,
        LoadMode::Merge,
    )
    .await
    .unwrap();
    drop(db);

    let mut changes = read_visible_graph_change_records(path, 0, None).unwrap();
    let alice_update = changes
        .iter_mut()
        .find(|row| {
            row.op == "update"
                && row.entity_kind == "node"
                && row.type_name == "Person"
                && row
                    .payload
                    .get("after")
                    .and_then(|value| value.get("name"))
                    .and_then(|value| value.as_str())
                    == Some("Alice")
        })
        .unwrap();
    alice_update.op = "insert".to_string();

    overwrite_committed_graph_changes(path, &changes).await;

    let reopened = Database::open(path).await.unwrap();
    let report = reopened.doctor().await.unwrap();
    assert!(!report.healthy);
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("lineage shadow mismatch") && issue.contains("Person")),
        "expected lineage shadow mismatch in doctor report, got {:?}",
        report.issues
    );
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("lineage shadow CDC mismatch")),
        "expected doctor to report CDC shadow mismatch, got {:?}",
        report.issues
    );
    let err = read_visible_cdc_entries_with_source(path, 0, None, VisibleCdcSource::LineageShadow)
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("lineage-backed CDC diverged from the committed payload log"),
        "expected strict lineage shadow reader mismatch error, got {err}"
    );
    let lineage_shadow = report
        .lineage_shadow
        .as_ref()
        .expect("expected v4 doctor report to include lineage shadow details");
    assert!(lineage_shadow.windows_mismatched >= 1);
    assert!(
        lineage_shadow.windows.iter().any(|window| {
            window.type_name == "Person" && window.status.starts_with("mismatch")
        })
    );
}

#[tokio::test]
async fn test_v4_lineage_shadow_cdc_source_errors_when_rowids_are_missing() {
    let dir = test_dir("v4_lineage_shadow_missing_rowids");
    let path = dir.path();

    let db = init_v4_db(path, test_schema_src()).await;
    db.load(test_data_src()).await.unwrap();
    drop(db);

    let mut changes = read_visible_graph_change_records(path, 0, None).unwrap();
    let alice_insert = changes
        .iter_mut()
        .find(|row| {
            row.op == "insert"
                && row.entity_kind == "node"
                && row.type_name == "Person"
                && row.payload.get("name").and_then(|value| value.as_str()) == Some("Alice")
        })
        .unwrap();
    alice_insert.rowid_if_known = None;
    overwrite_committed_graph_changes(path, &changes).await;

    let err = read_visible_cdc_entries_with_source(path, 0, None, VisibleCdcSource::LineageShadow)
        .unwrap_err();
    assert!(
        err.to_string().contains("lineage-backed CDC is incomplete"),
        "expected strict lineage shadow reader incompleteness error, got {err}"
    );
}

#[tokio::test]
async fn test_migration_rebuilds_vector_indexes_for_indexed_vector_properties() {
    let dir = test_dir("migration_vector_indexed_props");
    let path = dir.path();

    let db = Database::init(path, vector_indexed_schema_src())
        .await
        .unwrap();
    db.load(vector_indexed_data_src()).await.unwrap();
    drop(db);

    std::fs::write(path.join("schema.pg"), vector_indexed_schema_v2_src()).unwrap();
    let result = execute_schema_migration(path, None, false, true)
        .await
        .unwrap();
    assert_eq!(result.status, MigrationStatus::Applied);

    let db = Database::open(path).await.unwrap();
    let doc = db
        .schema_ir
        .node_types()
        .find(|n| n.name == "Doc")
        .expect("doc node type");
    let expected_index_name = crate::store::indexing::vector_index_name(doc.type_id, "embedding");
    let metadata = DatabaseMetadata::open(path).unwrap();
    let locator = metadata.node_dataset_locator("Doc").unwrap();
    let dataset = open_dataset_for_locator(&locator).await.unwrap();
    let index_names: HashSet<String> = dataset
        .load_indices()
        .await
        .unwrap()
        .iter()
        .map(|idx| idx.name.clone())
        .collect();
    assert!(
        index_names.contains(&expected_index_name),
        "expected vector index {} after migration",
        expected_index_name
    );
}

#[tokio::test]
async fn test_delete_nodes_cascades_edges() {
    let dir = test_dir("delete_cascade");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();
    let persons_before = read_node_batch_for_db(&db, "Person").await.unwrap();
    let alice_id = person_id_by_name(&persons_before, "Alice");
    let alice_rowid_before = rowid_for_entity(path, "node", "Person", alice_id).await;

    let result = db
        .delete_nodes(
            "Person",
            &DeletePredicate {
                property: "name".to_string(),
                op: DeleteOp::Eq,
                value: "Alice".to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.deleted_nodes, 1);
    assert_eq!(result.deleted_edges, 3);

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 2);
    let name_col = persons
        .column(1)
        .as_any()
        .downcast_ref::<arrow_array::StringArray>()
        .unwrap();
    let mut names: Vec<String> = (0..persons.num_rows())
        .map(|i| name_col.value(i).to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["Bob".to_string(), "Charlie".to_string()]);

    assert!(read_edge_batch_for_db(&db, "Knows").await.is_none());
    assert!(read_edge_batch_for_db(&db, "WorksAt").await.is_none());

    let tx_rows = read_tx_catalog_entries(path).unwrap();
    assert_eq!(tx_rows.len(), 2);
    assert_eq!(tx_rows[1].db_version, 2);
    assert_eq!(tx_rows[1].op_summary, "mutation:delete_nodes");

    let change_rows = read_visible_change_rows(path, 1, Some(2)).unwrap();
    assert!(
        change_rows
            .iter()
            .any(|row| row.change_kind == "delete" && row.entity_kind == "node" && row.type_name == "Person")
    );
    assert!(
        change_rows
            .iter()
            .any(|row| row.change_kind == "delete" && row.entity_kind == "edge")
    );
    let delete_rows = change_rows;
    let alice_delete = delete_rows
        .iter()
        .find(|row| {
            row.change_kind == "delete"
                && row.entity_kind == "node"
                && row.type_name == "Person"
                && row.entity_id == alice_id
        })
        .unwrap();
    assert_eq!(alice_delete.rowid, alice_rowid_before);
    assert_lineage_shadow_cdc_matches_authoritative(path, 1, Some(2));

    drop(db);
    let reopened = Database::open(path).await.unwrap();
    let persons2 = read_node_batch_for_db(&reopened, "Person").await.unwrap();
    assert_eq!(persons2.num_rows(), 2);
    assert!(read_edge_batch_for_db(&reopened, "Knows").await.is_none());
    assert!(read_edge_batch_for_db(&reopened, "WorksAt").await.is_none());
    let report = reopened.doctor().await.unwrap();
    assert!(
        report.healthy,
        "expected healthy report after delete cascade: {report:?}"
    );
}

#[tokio::test]
async fn test_delete_edges_commits_through_mutation_pipeline() {
    let dir = test_dir("delete_edges_pipeline");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    let alice_id = person_id_by_name(&persons, "Alice");
    let result = db
        .delete_edges(
            "Knows",
            &DeletePredicate {
                property: "src".to_string(),
                op: DeleteOp::Eq,
                value: alice_id.to_string(),
            },
        )
        .await
        .unwrap();

    assert_eq!(result.deleted_nodes, 0);
    assert_eq!(result.deleted_edges, 2);
    assert!(read_edge_batch_for_db(&db, "Knows").await.is_none());
    let works_at = read_edge_batch_for_db(&db, "WorksAt").await.unwrap();
    assert_eq!(works_at.num_rows(), 1);

    let tx_rows = read_tx_catalog_entries(path).unwrap();
    assert_eq!(tx_rows.len(), 2);
    assert_eq!(tx_rows[1].db_version, 2);
    assert_eq!(tx_rows[1].op_summary, "mutation:delete_edges");

    let cdc_rows = read_visible_cdc_entries(path, 1, Some(2)).unwrap();
    assert!(
        cdc_rows
            .iter()
            .any(|e| { e.op == "delete" && e.entity_kind == "edge" && e.type_name == "Knows" })
    );

    drop(db);
    let reopened = Database::open(path).await.unwrap();
    assert!(read_edge_batch_for_db(&reopened, "Knows").await.is_none());
    let works_at = read_edge_batch_for_db(&reopened, "WorksAt").await.unwrap();
    assert_eq!(works_at.num_rows(), 1);
}

#[tokio::test]
async fn test_delete_edges_uses_lance_native_delete_path() {
    let dir = test_dir("delete_edges_native");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();
    let manifest_before = read_committed_graph_snapshot(path).unwrap();
    let knows_path_before = dataset_rel_path_for(&manifest_before, "edge", "Knows");
    let works_at_version_before = dataset_version_for(&manifest_before, "edge", "WorksAt");

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    let alice_id = person_id_by_name(&persons, "Alice");
    let bob_id = person_id_by_name(&persons, "Bob");
    let knows_before = read_edge_batch_for_db(&db, "Knows").await.unwrap();
    let alice_bob_edge_id = edge_id_by_endpoints(&knows_before, alice_id, bob_id);
    let alice_bob_rowid_before = rowid_for_entity(path, "edge", "Knows", alice_bob_edge_id).await;
    let result = db
        .delete_edges(
            "Knows",
            &DeletePredicate {
                property: "dst".to_string(),
                op: DeleteOp::Eq,
                value: bob_id.to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.deleted_edges, 1);

    let manifest_after = read_committed_graph_snapshot(path).unwrap();
    let knows_path_after = dataset_rel_path_for(&manifest_after, "edge", "Knows");
    assert_eq!(knows_path_before, knows_path_after);
    assert_eq!(
        dataset_version_for(&manifest_after, "edge", "WorksAt"),
        works_at_version_before
    );

    let uri = path.join(&knows_path_after).to_string_lossy().to_string();
    let dataset = Dataset::open(&uri).await.unwrap();
    assert!(
        dataset.count_deleted_rows().await.unwrap() > 0,
        "expected native delete tombstones in Knows dataset"
    );
    let delete_rows = read_visible_change_rows(path, 1, Some(2)).unwrap();
    let deleted_edge_rows = delete_rows
        .iter()
        .filter(|row| {
            row.change_kind == "delete" && row.entity_kind == "edge" && row.type_name == "Knows"
        })
        .collect::<Vec<_>>();
    assert_eq!(deleted_edge_rows.len(), 1);
    let deleted_edge = deleted_edge_rows[0];
    assert_eq!(deleted_edge.rowid, alice_bob_rowid_before);
    assert_lineage_shadow_cdc_matches_authoritative(path, 1, Some(2));
    let report = db.doctor().await.unwrap();
    assert!(
        report.healthy,
        "expected healthy report after edge delete: {report:?}"
    );
}

#[tokio::test]
async fn test_load_reopen_query() {
    let dir = test_dir("load_query");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();
    drop(db);

    let db2 = Database::open(path).await.unwrap();
    assert!(
        db2.current_runtime()
            .edge_dataset_locator("Knows")
            .is_some()
    );

    let query = parse_query(
        r#"
query alice_friends() {
    match {
        $p: Person { name: "Alice" }
        $p knows $f
    }
    return { $f.name }
}
"#,
    )
    .unwrap()
    .queries
    .into_iter()
    .next()
    .unwrap();

    let rows = match db2.run_query(&query, &ParamMap::new()).await.unwrap() {
        RunResult::Query(rows) => rows.to_rust_json(),
        RunResult::Mutation(_) => panic!("expected query result"),
    };
    let mut names: Vec<String> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["Bob".to_string(), "Charlie".to_string()]);
}

#[tokio::test]
async fn test_traversal_query_works_with_locator_only_storage() {
    let dir = test_dir("locator_only_traversal");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();
    drop(db);

    let db2 = Database::open(path).await.unwrap();
    let runtime = db2.current_runtime();
    assert!(runtime.node_dataset_locator("Person").is_some());
    assert!(runtime.edge_dataset_locator("Knows").is_some());

    let query = parse_query(
        r#"
query alice_friends() {
    match {
        $p: Person { name: "Alice" }
        $p knows $f
    }
    return { $f.name }
}
"#,
    )
    .unwrap()
    .queries
    .into_iter()
    .next()
    .unwrap();

    let rows = match db2.run_query(&query, &ParamMap::new()).await.unwrap() {
        RunResult::Query(rows) => rows.to_rust_json(),
        RunResult::Mutation(_) => panic!("expected query result"),
    };
    let mut names: Vec<String> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["Bob".to_string(), "Charlie".to_string()]);
}

#[tokio::test]
async fn test_cloned_handles_observe_committed_storage_swaps() {
    let dir = test_dir("shared_handle_commit");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    let db2 = db.clone();

    db.load_with_mode(test_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();

    let persons = read_node_batch_for_db(&db2, "Person").await.unwrap();
    let companies = read_node_batch_for_db(&db2, "Company").await.unwrap();
    assert_eq!(persons.num_rows(), 3);
    assert_eq!(companies.num_rows(), 1);
    let knows = read_edge_batch_for_db(&db2, "Knows").await.unwrap();
    assert_eq!(knows.num_rows(), 2);
}

#[tokio::test]
async fn test_concurrent_mutations_on_cloned_handles_do_not_lose_updates() {
    let dir = test_dir("shared_handle_mutations");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    let db1 = db.clone();
    let db2 = db.clone();

    let alice = r#"{"type":"Person","data":{"name":"Alice","age":30}}"#;
    let bob = r#"{"type":"Person","data":{"name":"Bob","age":25}}"#;

    let (left, right) = tokio::join!(
        db1.apply_append_mutation(alice, "mutation:insert_node"),
        db2.apply_append_mutation(bob, "mutation:insert_node")
    );
    left.unwrap();
    right.unwrap();

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    let names = persons
        .column_by_name("name")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let mut actual: Vec<String> = (0..persons.num_rows())
        .map(|row| names.value(row).to_string())
        .collect();
    actual.sort();
    assert_eq!(actual, vec!["Alice".to_string(), "Bob".to_string()]);
}

#[tokio::test]
async fn test_prepared_reads_keep_old_snapshot_across_mutation() {
    let dir = test_dir("prepared_snapshot_stability");
    let path = dir.path();

    let db = Database::init(path, keyed_schema_src()).await.unwrap();
    db.load_with_mode(keyed_data_initial(), LoadMode::Overwrite)
        .await
        .unwrap();

    let query = parse_query(
        r#"
query people() {
    match { $p: Person }
    return { $p.name, $p.age }
}
"#,
    )
    .unwrap()
    .queries
    .into_iter()
    .next()
    .unwrap();
    let prepared_before = db.prepare_read_query(&query).unwrap();

    let db2 = db.clone();
    db2.load_with_mode(keyed_data_upsert(), LoadMode::Merge)
        .await
        .unwrap();

    let before_rows = prepared_before.execute(&ParamMap::new()).await.unwrap();
    let fresh_rows = db.prepare_read_query(&query).unwrap();
    let after_rows = fresh_rows.execute(&ParamMap::new()).await.unwrap();

    let mut before_json = before_rows.to_rust_json().as_array().unwrap().clone();
    before_json.sort_by(|left, right| {
        left["name"]
            .as_str()
            .unwrap()
            .cmp(right["name"].as_str().unwrap())
    });
    let mut after_json = after_rows.to_rust_json().as_array().unwrap().clone();
    after_json.sort_by(|left, right| {
        left["name"]
            .as_str()
            .unwrap()
            .cmp(right["name"].as_str().unwrap())
    });

    assert_eq!(
        before_json,
        vec![
            serde_json::json!({ "name": "Alice", "age": 30 }),
            serde_json::json!({ "name": "Bob", "age": 25 })
        ]
    );
    assert_eq!(
        after_json,
        vec![
            serde_json::json!({ "name": "Alice", "age": 31 }),
            serde_json::json!({ "name": "Bob", "age": 25 }),
            serde_json::json!({ "name": "Charlie", "age": 40 })
        ]
    );
}

#[tokio::test]
async fn test_prepared_reads_resolve_now_per_execute() {
    let db = Database::open_in_memory(timestamp_schema_src())
        .await
        .unwrap();
    db.load_with_mode(timestamp_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();

    let query = parse_query(
        r#"
query stamp() {
    match { $p: Person }
    return { now() as ts }
}
"#,
    )
    .unwrap()
    .queries
    .into_iter()
    .next()
    .unwrap();
    let prepared = db.prepare_read_query(&query).unwrap();

    let first = prepared
        .execute(&ParamMap::new())
        .await
        .unwrap()
        .to_rust_json();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let second = prepared
        .execute(&ParamMap::new())
        .await
        .unwrap()
        .to_rust_json();

    let first_ts = first[0]["ts"].as_str().unwrap().to_string();
    let second_ts = second[0]["ts"].as_str().unwrap().to_string();

    assert!(second_ts >= first_ts);
    assert_ne!(first_ts, second_ts);
}

#[tokio::test]
async fn test_mutation_queries_can_assign_now_to_datetime() {
    let db = Database::open_in_memory(timestamp_schema_src())
        .await
        .unwrap();
    db.load_with_mode(timestamp_data_src(), LoadMode::Overwrite)
        .await
        .unwrap();

    let mutation = parse_query(
        r#"
query touch() {
    update Person set { updated_at: now() } where name = "Alice"
}
"#,
    )
    .unwrap()
    .queries
    .into_iter()
    .next()
    .unwrap();
    let result = db.run_query(&mutation, &ParamMap::new()).await.unwrap();
    let RunResult::Mutation(result) = result else {
        panic!("expected mutation result");
    };
    assert_eq!(result.affected_nodes, 1);

    let query = parse_query(
        r#"
query person() {
    match { $p: Person { name: "Alice" } }
    return { $p.updated_at }
}
"#,
    )
    .unwrap()
    .queries
    .into_iter()
    .next()
    .unwrap();
    let rows = db.run_query(&query, &ParamMap::new()).await.unwrap();
    let RunResult::Query(rows) = rows else {
        panic!("expected query result");
    };
    let json = rows.to_rust_json();
    let updated_at = json[0]["updated_at"].as_str().unwrap();

    assert!(!updated_at.is_empty());
    assert!(updated_at.ends_with('Z'));
}

#[tokio::test]
async fn test_run_query_works_against_reopened_persisted_storage() {
    let dir = test_dir("query_reopened_persisted");
    let path = dir.path();

    let db = Database::init(path, keyed_schema_src()).await.unwrap();
    db.load_with_mode(keyed_data_initial(), LoadMode::Overwrite)
        .await
        .unwrap();
    let reopened = Database::open(path).await.unwrap();

    let query = parse_query(
        r#"
query people() {
    match { $p: Person }
    return { $p.name, $p.age }
}
"#,
    )
    .unwrap()
    .queries
    .into_iter()
    .next()
    .unwrap();

    let rows = match reopened.run_query(&query, &ParamMap::new()).await.unwrap() {
        RunResult::Query(rows) => rows.to_rust_json(),
        RunResult::Mutation(_) => panic!("expected query result"),
    };

    let mut people = rows.as_array().unwrap().clone();
    people.sort_by(|left, right| {
        left.get("name")
            .and_then(serde_json::Value::as_str)
            .cmp(&right.get("name").and_then(serde_json::Value::as_str))
    });

    assert_eq!(
        people,
        vec![
            serde_json::json!({ "name": "Alice", "age": 30 }),
            serde_json::json!({ "name": "Bob", "age": 25 }),
        ]
    );
}

#[tokio::test]
async fn test_keyed_load_upsert_preserves_ids_and_remaps_edges() {
    let dir = test_dir("keyed_upsert");
    let path = dir.path();

    let db = Database::init(path, keyed_schema_src()).await.unwrap();
    db.load(keyed_data_initial()).await.unwrap();

    let persons_before = read_node_batch_for_db(&db, "Person").await.unwrap();
    let alice_id_before = person_id_by_name(&persons_before, "Alice");
    let bob_id_before = person_id_by_name(&persons_before, "Bob");

    db.load(keyed_data_upsert()).await.unwrap();

    let persons_after = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons_after.num_rows(), 3);
    let alice_id_after = person_id_by_name(&persons_after, "Alice");
    let bob_id_after = person_id_by_name(&persons_after, "Bob");
    let charlie_id_after = person_id_by_name(&persons_after, "Charlie");

    assert_eq!(alice_id_after, alice_id_before);
    assert_eq!(bob_id_after, bob_id_before);
    assert_eq!(person_age_by_name(&persons_after, "Alice"), Some(31));

    let knows = read_edge_batch_for_db(&db, "Knows").await.unwrap();
    let src_col = knows
        .column_by_name("src")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    let dst_col = knows
        .column_by_name("dst")
        .unwrap()
        .as_any()
        .downcast_ref::<UInt64Array>()
        .unwrap();
    assert_eq!(knows.num_rows(), 2);
    assert!(
        src_col
            .iter()
            .zip(dst_col.iter())
            .any(|(src, dst)| src == Some(alice_id_after) && dst == Some(bob_id_after))
    );
    assert!(
        src_col
            .iter()
            .zip(dst_col.iter())
            .any(|(src, dst)| src == Some(alice_id_after) && dst == Some(charlie_id_after))
    );

    let companies_after = read_node_batch_for_db(&db, "Company").await.unwrap();
    assert_eq!(companies_after.num_rows(), 1);
    let company_name_col = companies_after
        .column(1)
        .as_any()
        .downcast_ref::<arrow_array::StringArray>()
        .unwrap();
    assert_eq!(company_name_col.value(0), "Acme");
}

#[tokio::test]
async fn test_unique_rejects_existing_incoming_conflict() {
    let dir = test_dir("unique_existing_incoming");
    let path = dir.path();

    let db = Database::init(path, unique_schema_src()).await.unwrap();
    db.load(unique_data_initial()).await.unwrap();

    let err = db
        .load(unique_data_existing_incoming_conflict())
        .await
        .unwrap_err();
    match err {
        NanoError::UniqueConstraint {
            type_name,
            property,
            value,
            ..
        } => {
            assert_eq!(type_name, "Person");
            assert_eq!(property, "email");
            assert_eq!(value, "bob@example.com");
        }
        other => panic!("expected UniqueConstraint, got {}", other),
    }

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 2);
    assert_eq!(person_email_by_name(&persons, "Bob"), "bob@example.com");
}

#[tokio::test]
async fn test_unique_rejects_incoming_incoming_conflict() {
    let dir = test_dir("unique_incoming_incoming");
    let path = dir.path();

    let db = Database::init(path, unique_schema_src()).await.unwrap();
    let err = db
        .load(unique_data_incoming_incoming_conflict())
        .await
        .unwrap_err();
    match err {
        NanoError::UniqueConstraint {
            type_name,
            property,
            value,
            ..
        } => {
            assert_eq!(type_name, "Person");
            assert_eq!(property, "email");
            assert_eq!(value, "charlie@example.com");
        }
        other => panic!("expected UniqueConstraint, got {}", other),
    }

    assert!(read_node_batch_for_db(&db, "Person").await.is_none());
}

#[tokio::test]
async fn test_unique_rejects_duplicate_user_property_named_id() {
    let dir = test_dir("unique_user_id");
    let path = dir.path();

    let db = Database::init(path, id_named_unique_schema_src())
        .await
        .unwrap();
    let err = db
        .load_with_mode(id_named_unique_duplicate_data(), LoadMode::Overwrite)
        .await
        .unwrap_err();
    match err {
        NanoError::UniqueConstraint {
            type_name,
            property,
            value,
            ..
        } => {
            assert_eq!(type_name, "Person");
            assert_eq!(property, "id");
            assert_eq!(value, "user-1");
        }
        other => panic!("expected UniqueConstraint, got {}", other),
    }
}

#[tokio::test]
async fn test_nullable_unique_allows_nulls_and_rejects_duplicate_non_null() {
    let dir = test_dir("nullable_unique");
    let path = dir.path();

    let db = Database::init(path, nullable_unique_schema_src())
        .await
        .unwrap();
    db.load(nullable_unique_ok_data()).await.unwrap();

    let persons = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons.num_rows(), 2);
    let nick_col = persons
        .column(2)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert!(nick_col.is_null(0));
    assert!(nick_col.is_null(1));

    let err = db.load(nullable_unique_duplicate_data()).await.unwrap_err();
    match err {
        NanoError::UniqueConstraint {
            type_name,
            property,
            value,
            ..
        } => {
            assert_eq!(type_name, "Person");
            assert_eq!(property, "nick");
            assert_eq!(value, "ally");
        }
        other => panic!("expected UniqueConstraint, got {}", other),
    }

    let persons_after_err = read_node_batch_for_db(&db, "Person").await.unwrap();
    assert_eq!(persons_after_err.num_rows(), 2);
}

#[tokio::test]
async fn test_cdc_analytics_materialization_writes_dataset_and_preserves_changes() {
    let dir = test_dir("cdc_analytics_materialize");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();

    let visible_before = read_visible_cdc_entries(path, 0, None).unwrap();
    let manifest_before = read_committed_graph_snapshot(path).unwrap();

    let result = db
        .materialize_cdc_analytics(CdcAnalyticsMaterializeOptions {
            min_new_rows: 0,
            force: true,
        })
        .await
        .unwrap();

    assert!(result.dataset_written);
    assert_eq!(result.source_rows, visible_before.len());
    assert_eq!(result.materialized_rows, visible_before.len());
    assert!(path.join(CDC_ANALYTICS_DATASET_DIR).exists());

    let state = read_cdc_analytics_state(path).unwrap();
    assert_eq!(state.rows_materialized, visible_before.len());
    assert_eq!(state.manifest_db_version, manifest_before.db_version);
    assert!(state.dataset_version.is_some());

    let visible_after = read_visible_cdc_entries(path, 0, None).unwrap();
    assert_eq!(visible_before, visible_after);
    let manifest_after = read_committed_graph_snapshot(path).unwrap();
    assert_eq!(manifest_after.db_version, manifest_before.db_version);
}

#[tokio::test]
async fn test_cdc_analytics_materialization_threshold_skip() {
    let dir = test_dir("cdc_analytics_threshold");
    let path = dir.path();

    let db = Database::init(path, test_schema_src()).await.unwrap();
    db.load(test_data_src()).await.unwrap();

    db.materialize_cdc_analytics(CdcAnalyticsMaterializeOptions {
        min_new_rows: 0,
        force: true,
    })
    .await
    .unwrap();

    let skipped = db
        .materialize_cdc_analytics(CdcAnalyticsMaterializeOptions {
            min_new_rows: 10_000,
            force: false,
        })
        .await
        .unwrap();

    assert!(skipped.skipped_by_threshold);
    assert!(!skipped.dataset_written);
    assert_eq!(skipped.new_rows_since_last_run, 0);
}
