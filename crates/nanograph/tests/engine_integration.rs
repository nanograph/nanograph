use std::collections::HashSet;

use arrow_array::{
    Array, FixedSizeListArray, Float32Array, Float64Array, Int32Array, Int64Array, RecordBatch,
    StringArray, UInt64Array,
};
use lance::Dataset;
use lance_index::DatasetIndexExt;
use tokio::sync::Mutex;
use url::Url;

use nanograph::query::parser::parse_query;
use nanograph::query::typecheck::{CheckedQuery, typecheck_query, typecheck_query_decl};
use nanograph::schema::parser::parse_schema;
use nanograph::store::database::{Database, LoadMode};
use nanograph::store::export::build_export_rows_at_path;
use nanograph::store::manifest::GraphManifest;
use nanograph::store::snapshot::read_committed_graph_snapshot;
use nanograph::store::txlog::read_visible_cdc_entries;
use nanograph::store::{scalar_index_name, text_index_name, vector_index_name};
use nanograph::{MutationExecResult, ParamMap, build_catalog};

static EMBED_ENV_LOCK: Mutex<()> = Mutex::const_new(());

fn test_schema() -> &'static str {
    r#"
node Person {
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

fn indexed_test_schema() -> &'static str {
    r#"
node Person {
    name: String
    email: String @index
}
"#
}

fn list_membership_test_schema() -> &'static str {
    r#"
node Person {
    name: String @key
    tags: [String]?
}
"#
}

fn list_membership_test_data() -> &'static str {
    r#"{"type":"Person","data":{"name":"Alice","tags":["rust","db"]}}
{"type":"Person","data":{"name":"Bob","tags":["graph"]}}
{"type":"Person","data":{"name":"Cara","tags":["rust"]}}
{"type":"Person","data":{"name":"Dylan","tags":null}}
"#
}

fn indexed_test_data() -> &'static str {
    r#"{"type":"Person","data":{"name":"Alice","email":"alice@example.com"}}
{"type":"Person","data":{"name":"Bob","email":"bob@example.com"}}
"#
}

fn text_search_test_schema() -> &'static str {
    r#"
node Doc {
    slug: String @key
    title: String
    body: String @index
}
"#
}

fn text_search_test_data() -> &'static str {
    r#"{"type":"Doc","data":{"slug":"a","title":"Alpha","body":"graph database internals"}}
{"type":"Doc","data":{"slug":"b","title":"Beta","body":"graph storage basics"}}
{"type":"Doc","data":{"slug":"c","title":"Gamma","body":"distributed systems handbook"}}
"#
}

fn vector_test_schema() -> &'static str {
    r#"
node Doc {
    slug: String @key
    title: String
    embedding: Vector(4) @index
}
"#
}

fn vector_test_data() -> &'static str {
    r#"{"type":"Doc","data":{"slug":"a","title":"A","embedding":[1.0,0.0,0.0,0.0]}}
{"type":"Doc","data":{"slug":"b","title":"B","embedding":[0.0,1.0,0.0,0.0]}}
{"type":"Doc","data":{"slug":"c","title":"C","embedding":[-1.0,0.0,0.0,0.0]}}
{"type":"Doc","data":{"slug":"d","title":"D","embedding":[0.7,0.7,0.0,0.0]}}
"#
}

fn vector_filtered_test_schema() -> &'static str {
    r#"
node Doc {
    slug: String @key
    topic: String
    embedding: Vector(4) @index
}
"#
}

fn vector_filtered_test_data() -> &'static str {
    r#"{"type":"Doc","data":{"slug":"a","topic":"x","embedding":[1.0,0.0,0.0,0.0]}}
{"type":"Doc","data":{"slug":"b","topic":"y","embedding":[0.0,1.0,0.0,0.0]}}
{"type":"Doc","data":{"slug":"c","topic":"y","embedding":[-1.0,0.0,0.0,0.0]}}
"#
}

fn keyed_mutation_schema() -> &'static str {
    r#"
node Person {
    name: String @key
    age: I32?
}
edge Knows: Person -> Person
"#
}

fn keyed_mutation_data() -> &'static str {
    r#"{"type":"Person","data":{"name":"Alice","age":30}}
{"type":"Person","data":{"name":"Bob","age":25}}
{"edge":"Knows","from":"Alice","to":"Bob"}
"#
}

fn media_traversal_schema() -> &'static str {
    r#"
node PhotoAsset {
    slug: String @key
    uri: String @media_uri(mime)
    mime: String
    embedding: Vector(16) @embed(uri)
}

node Product {
    slug: String @key
    name: String
}

edge HasPhoto: Product -> PhotoAsset
"#
}

fn test_data() -> &'static str {
    r#"{"type":"Person","data":{"name":"Alice","age":30}}
{"type":"Person","data":{"name":"Bob","age":25}}
{"type":"Person","data":{"name":"Charlie","age":35}}
{"type":"Person","data":{"name":"Diana","age":28}}
{"type":"Company","data":{"name":"Acme"}}
{"type":"Company","data":{"name":"Globex"}}
{"edge":"Knows","from":"Alice","to":"Bob"}
{"edge":"Knows","from":"Alice","to":"Charlie"}
{"edge":"Knows","from":"Bob","to":"Diana"}
{"edge":"WorksAt","from":"Alice","to":"Acme"}
{"edge":"WorksAt","from":"Bob","to":"Globex"}
"#
}

async fn setup_storage() -> Database {
    let db = Database::open_in_memory(test_schema()).await.unwrap();
    db.load_with_mode(test_data(), LoadMode::Overwrite)
        .await
        .unwrap();
    db
}

fn manifest_dataset_version(manifest: &GraphManifest, kind: &str, type_name: &str) -> u64 {
    manifest
        .datasets
        .iter()
        .find(|entry| entry.kind == kind && entry.type_name == type_name)
        .map(|entry| entry.dataset_version)
        .unwrap()
}

fn manifest_dataset_rel_path(manifest: &GraphManifest, kind: &str, type_name: &str) -> String {
    manifest
        .datasets
        .iter()
        .find(|entry| entry.kind == kind && entry.type_name == type_name)
        .map(|entry| entry.dataset_path.clone())
        .unwrap()
}

fn dataset_uri_for_db(
    db_path: &std::path::Path,
    manifest: &GraphManifest,
    kind: &str,
    type_name: &str,
) -> String {
    db_path
        .join(manifest_dataset_rel_path(manifest, kind, type_name))
        .to_string_lossy()
        .to_string()
}

async fn run_query_test(query_str: &str, db: &Database) -> Vec<RecordBatch> {
    run_query_test_with_params(query_str, db, &ParamMap::new()).await
}

async fn run_query_test_with_params(
    query_str: &str,
    db: &Database,
    params: &ParamMap,
) -> Vec<RecordBatch> {
    let qf = parse_query(query_str).unwrap();
    let query = &qf.queries[0];
    match db.run_query(query, params).await.unwrap() {
        nanograph::RunResult::Query(result) => result.into_batches(),
        nanograph::RunResult::Mutation(_) => panic!("expected query result"),
    }
}

async fn run_db_query_test_with_params(
    query_str: &str,
    db: &Database,
    params: &ParamMap,
) -> Vec<RecordBatch> {
    let qf = parse_query(query_str).unwrap();
    let query = &qf.queries[0];
    match db.run_query(query, params).await.unwrap() {
        nanograph::RunResult::Query(result) => result.into_batches(),
        nanograph::RunResult::Mutation(_) => panic!("expected query result"),
    }
}

async fn run_db_mutation_test_with_params(
    query_str: &str,
    db: &Database,
    params: &ParamMap,
) -> MutationExecResult {
    let qf = parse_query(query_str).unwrap();
    let query = &qf.queries[0];
    let checked = typecheck_query_decl(db.catalog(), query).unwrap();
    assert!(matches!(checked, CheckedQuery::Mutation(_)));
    match db.run_query(query, params).await.unwrap() {
        nanograph::RunResult::Mutation(result) => MutationExecResult {
            affected_nodes: result.affected_nodes,
            affected_edges: result.affected_edges,
        },
        nanograph::RunResult::Query(_) => panic!("expected mutation result"),
    }
}

async fn export_rows_for_db(db: &Database) -> Vec<serde_json::Value> {
    build_export_rows_at_path(db.path(), true, true)
        .await
        .expect("export rows")
}

fn extract_string_column(batches: &[RecordBatch], col_name: &str) -> Vec<String> {
    let mut result = Vec::new();
    for batch in batches {
        let col_idx = batch.schema().index_of(col_name).unwrap();
        let col = batch.column(col_idx);
        let arr = col.as_any().downcast_ref::<StringArray>().unwrap();
        for i in 0..arr.len() {
            if !arr.is_null(i) {
                result.push(arr.value(i).to_string());
            }
        }
    }
    result
}

fn extract_string_pairs(
    batches: &[RecordBatch],
    left_col: &str,
    right_col: &str,
) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for batch in batches {
        let left_idx = batch.schema().index_of(left_col).unwrap();
        let right_idx = batch.schema().index_of(right_col).unwrap();
        let left = batch
            .column(left_idx)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let right = batch
            .column(right_idx)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        for i in 0..batch.num_rows() {
            if !left.is_null(i) && !right.is_null(i) {
                result.push((left.value(i).to_string(), right.value(i).to_string()));
            }
        }
    }
    result
}

fn extract_u64_column(batches: &[RecordBatch], col_name: &str) -> Vec<u64> {
    let mut result = Vec::new();
    for batch in batches {
        let col_idx = batch.schema().index_of(col_name).unwrap();
        let col = batch.column(col_idx);

        if let Some(arr) = col.as_any().downcast_ref::<UInt64Array>() {
            for i in 0..arr.len() {
                if !arr.is_null(i) {
                    result.push(arr.value(i));
                }
            }
            continue;
        }

        if let Some(arr) = col.as_any().downcast_ref::<Int64Array>() {
            for i in 0..arr.len() {
                if !arr.is_null(i) {
                    result.push(u64::try_from(arr.value(i)).unwrap());
                }
            }
            continue;
        }

        if let Some(arr) = col.as_any().downcast_ref::<Int32Array>() {
            for i in 0..arr.len() {
                if !arr.is_null(i) {
                    result.push(u64::try_from(arr.value(i)).unwrap());
                }
            }
            continue;
        }

        panic!("unsupported numeric type for column {col_name}");
    }
    result
}

fn extract_f64_column(batches: &[RecordBatch], col_name: &str) -> Vec<f64> {
    let mut result = Vec::new();
    for batch in batches {
        let col_idx = batch.schema().index_of(col_name).unwrap();
        let col = batch.column(col_idx);
        let arr = col
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("expected Float64 column");
        for i in 0..arr.len() {
            if !arr.is_null(i) {
                result.push(arr.value(i));
            }
        }
    }
    result
}

#[tokio::test]
async fn test_bind_by_property() {
    let storage = setup_storage().await;
    let mut params = ParamMap::new();
    params.insert(
        "name".to_string(),
        nanograph::query::ast::Literal::String("Alice".to_string()),
    );
    let results = run_query_test_with_params(
        r#"
query q($name: String) {
    match { $p: Person { name: $name } }
    return { $p.name, $p.age }
}
"#,
        &storage,
        &params,
    )
    .await;

    let names = extract_string_column(&results, "name");
    assert_eq!(names, vec!["Alice"]);
}

#[tokio::test]
async fn test_filter_age() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person
        $p.age > 30
    }
    return { $p.name, $p.age }
}
"#,
        &storage,
    )
    .await;

    let names = extract_string_column(&results, "name");
    assert_eq!(names, vec!["Charlie"]);
}

#[tokio::test]
async fn test_list_membership_inline_binding_filter_with_literal() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let db = Database::init(&db_path, list_membership_test_schema())
        .await
        .unwrap();
    db.load(list_membership_test_data()).await.unwrap();

    let results = run_db_query_test_with_params(
        r#"
query q() {
    match { $p: Person { tags: "rust" } }
    return { $p.name }
    order { $p.name asc }
}
"#,
        &db,
        &ParamMap::new(),
    )
    .await;

    let names = extract_string_column(&results, "name");
    assert_eq!(names, vec!["Alice", "Cara"]);
}

#[tokio::test]
async fn test_list_membership_inline_binding_filter_with_param() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let db = Database::init(&db_path, list_membership_test_schema())
        .await
        .unwrap();
    db.load(list_membership_test_data()).await.unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "tag".to_string(),
        nanograph::query::ast::Literal::String("graph".to_string()),
    );

    let results = run_db_query_test_with_params(
        r#"
query q($tag: String) {
    match { $p: Person { tags: $tag } }
    return { $p.name }
}
"#,
        &db,
        &params,
    )
    .await;

    let names = extract_string_column(&results, "name");
    assert_eq!(names, vec!["Bob"]);
}

#[tokio::test]
async fn test_list_membership_contains_filter_with_param() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let db = Database::init(&db_path, list_membership_test_schema())
        .await
        .unwrap();
    db.load(list_membership_test_data()).await.unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "tag".to_string(),
        nanograph::query::ast::Literal::String("rust".to_string()),
    );

    let results = run_db_query_test_with_params(
        r#"
query q($tag: String) {
    match {
        $p: Person
        $p.tags contains $tag
    }
    return { $p.name }
    order { $p.name asc }
}
"#,
        &db,
        &params,
    )
    .await;

    let names = extract_string_column(&results, "name");
    assert_eq!(names, vec!["Alice", "Cara"]);
}

#[tokio::test]
async fn test_inline_binding_filters_with_cross_join() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person { name: "Alice" }
        $q: Person { name: "Bob" }
    }
    return { $p.name as p_name, $q.name as q_name }
}
"#,
        &storage,
    )
    .await;

    let p_names = extract_string_column(&results, "p_name");
    let q_names = extract_string_column(&results, "q_name");
    assert_eq!(p_names, vec!["Alice"]);
    assert_eq!(q_names, vec!["Bob"]);
}

#[tokio::test]
async fn test_multi_scan_explicit_filter_on_single_binding() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person
        $q: Person
        $p.name = "Alice"
    }
    return { $p.name as p_name, $q.name as q_name }
}
"#,
        &storage,
    )
    .await;

    let p_names = extract_string_column(&results, "p_name");
    let mut q_names = extract_string_column(&results, "q_name");
    q_names.sort();

    assert_eq!(p_names.len(), 4);
    assert!(p_names.iter().all(|n| n == "Alice"));
    assert_eq!(q_names, vec!["Alice", "Bob", "Charlie", "Diana"]);
}
#[tokio::test]
async fn test_one_hop_traversal() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person { name: "Alice" }
        $p knows $f
    }
    return { $f.name }
}
"#,
        &storage,
    )
    .await;

    let mut names = extract_string_column(&results, "name");
    names.sort();
    assert_eq!(names, vec!["Bob", "Charlie"]);
}

#[tokio::test]
async fn test_indexed_point_lookup_after_persist_and_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let db = Database::init(&db_path, indexed_test_schema())
        .await
        .unwrap();
    db.load(indexed_test_data()).await.unwrap();

    let person = db
        .schema_ir
        .node_types()
        .find(|n| n.name == "Person")
        .unwrap();
    let manifest = read_committed_graph_snapshot(&db_path).unwrap();
    let dataset = Dataset::open(&dataset_uri_for_db(&db_path, &manifest, "node", "Person"))
        .await
        .unwrap();
    let index_names: HashSet<String> = dataset
        .load_indices()
        .await
        .unwrap()
        .iter()
        .map(|idx| idx.name.clone())
        .collect();
    let expected_index = scalar_index_name(person.type_id, "email");
    assert!(index_names.contains(&expected_index));

    drop(db);
    let reopened = Database::open(&db_path).await.unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "email".to_string(),
        nanograph::query::ast::Literal::String("alice@example.com".to_string()),
    );
    let results = run_db_query_test_with_params(
        r#"
query q($email: String) {
    match {
        $p: Person
        $p.email = $email
    }
    return { $p.name }
}
"#,
        &reopened,
        &params,
    )
    .await;

    let names = extract_string_column(&results, "name");
    assert_eq!(names, vec!["Alice"]);
}

#[tokio::test]
async fn test_nearest_query_on_indexed_vectors() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let db = Database::init(&db_path, vector_test_schema())
        .await
        .unwrap();
    db.load(vector_test_data()).await.unwrap();

    let doc = db.schema_ir.node_types().find(|n| n.name == "Doc").unwrap();
    let manifest = read_committed_graph_snapshot(&db_path).unwrap();
    let dataset = Dataset::open(&dataset_uri_for_db(&db_path, &manifest, "node", "Doc"))
        .await
        .unwrap();
    let index_names: HashSet<String> = dataset
        .load_indices()
        .await
        .unwrap()
        .iter()
        .map(|idx| idx.name.clone())
        .collect();
    let expected_index = vector_index_name(doc.type_id, "embedding");
    assert!(index_names.contains(&expected_index));

    let mut params = ParamMap::new();
    params.insert(
        "q".to_string(),
        nanograph::query::ast::Literal::List(vec![
            nanograph::query::ast::Literal::Float(1.0),
            nanograph::query::ast::Literal::Float(0.0),
            nanograph::query::ast::Literal::Float(0.0),
            nanograph::query::ast::Literal::Float(0.0),
        ]),
    );

    let results = run_db_query_test_with_params(
        r#"
query q($q: Vector(4)) {
    match {
        $d: Doc
    }
    return { $d.slug as slug, nearest($d.embedding, $q) as score }
    order { nearest($d.embedding, $q) }
    limit 2
}
"#,
        &db,
        &params,
    )
    .await;

    let slugs = extract_string_column(&results, "slug");
    assert_eq!(slugs, vec!["a", "d"]);
    let scores = extract_f64_column(&results, "score");
    assert_eq!(scores.len(), 2);
    assert!(scores[0] <= scores[1]);
    assert!(scores[0].abs() < 1e-9);
}

#[tokio::test]
async fn test_native_text_index_created_and_queries_work_after_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let db = Database::init(&db_path, text_search_test_schema())
        .await
        .unwrap();
    db.load(text_search_test_data()).await.unwrap();

    let doc = db.schema_ir.node_types().find(|n| n.name == "Doc").unwrap();
    let manifest = read_committed_graph_snapshot(&db_path).unwrap();
    let dataset = Dataset::open(&dataset_uri_for_db(&db_path, &manifest, "node", "Doc"))
        .await
        .unwrap();
    let index_names: HashSet<String> = dataset
        .load_indices()
        .await
        .unwrap()
        .iter()
        .map(|idx| idx.name.clone())
        .collect();
    let expected_text_index = text_index_name(doc.type_id, "body");
    assert!(index_names.contains(&expected_text_index));

    drop(db);
    let reopened = Database::open(&db_path).await.unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "q".to_string(),
        nanograph::query::ast::Literal::String("graph database".to_string()),
    );
    let results = run_db_query_test_with_params(
        r#"
query q($q: String) {
    match {
        $d: Doc
        search($d.body, $q)
    }
    return { $d.slug }
    order { $d.slug }
}
"#,
        &reopened,
        &params,
    )
    .await;
    assert_eq!(extract_string_column(&results, "slug"), vec!["a"]);

    let results = run_db_query_test_with_params(
        r#"
query q($q: String) {
    match {
        $d: Doc
        match_text($d.body, $q)
    }
    return { $d.slug }
}
"#,
        &reopened,
        &params,
    )
    .await;
    assert_eq!(extract_string_column(&results, "slug"), vec!["a"]);

    params.insert(
        "q".to_string(),
        nanograph::query::ast::Literal::String("databaze".to_string()),
    );
    let results = run_db_query_test_with_params(
        r#"
query q($q: String) {
    match {
        $d: Doc
        fuzzy($d.body, $q, 1)
    }
    return { $d.slug }
}
"#,
        &reopened,
        &params,
    )
    .await;
    assert_eq!(extract_string_column(&results, "slug"), vec!["a"]);

    params.insert(
        "q".to_string(),
        nanograph::query::ast::Literal::String("graph database".to_string()),
    );
    let results = run_db_query_test_with_params(
        r#"
query q($q: String) {
    match { $d: Doc }
    return { $d.slug as slug, bm25($d.body, $q) as score }
    order { bm25($d.body, $q) desc }
    limit 2
}
"#,
        &reopened,
        &params,
    )
    .await;
    let slugs = extract_string_column(&results, "slug");
    assert_eq!(slugs, vec!["a", "b"]);
    let scores = extract_f64_column(&results, "score");
    assert_eq!(scores.len(), 2);
    assert!(scores[0] >= scores[1]);
}

#[tokio::test]
async fn test_search_on_unindexed_text_field_falls_back_cleanly() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let db = Database::init(&db_path, text_search_test_schema())
        .await
        .unwrap();
    db.load(text_search_test_data()).await.unwrap();
    drop(db);

    let reopened = Database::open(&db_path).await.unwrap();
    let mut params = ParamMap::new();
    params.insert(
        "q".to_string(),
        nanograph::query::ast::Literal::String("alpha".to_string()),
    );
    let results = run_db_query_test_with_params(
        r#"
query q($q: String) {
    match {
        $d: Doc
        search($d.title, $q)
    }
    return { $d.slug }
}
"#,
        &reopened,
        &params,
    )
    .await;
    assert_eq!(extract_string_column(&results, "slug"), vec!["a"]);
}

#[tokio::test]
async fn test_vector_projection_on_lance_scan_does_not_panic() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let db = Database::init(&db_path, vector_test_schema())
        .await
        .unwrap();
    db.load(vector_test_data()).await.unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "slug".to_string(),
        nanograph::query::ast::Literal::String("a".to_string()),
    );

    let results = run_db_query_test_with_params(
        r#"
query q($slug: String) {
    match {
        $d: Doc { slug: $slug }
    }
    return { $d.slug as slug, $d.embedding as embedding }
    limit 1
}
"#,
        &db,
        &params,
    )
    .await;

    let slugs = extract_string_column(&results, "slug");
    assert_eq!(slugs, vec!["a"]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].num_rows(), 1);

    let emb_idx = results[0].schema().index_of("embedding").unwrap();
    let emb = results[0]
        .column(emb_idx)
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .expect("embedding should be a FixedSizeListArray");
    assert_eq!(emb.len(), 1);

    let first_embedding = emb.value(0);
    let values = first_embedding
        .as_any()
        .downcast_ref::<Float32Array>()
        .expect("embedding values should be Float32");
    assert_eq!(values.len(), 4);
    assert_eq!(values.value(0), 1.0);
}

#[tokio::test]
async fn test_nearest_filter_prefilter_returns_filtered_result_with_small_limit() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let db = Database::init(&db_path, vector_filtered_test_schema())
        .await
        .unwrap();
    db.load(vector_filtered_test_data()).await.unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "topic".to_string(),
        nanograph::query::ast::Literal::String("y".to_string()),
    );
    params.insert(
        "q".to_string(),
        nanograph::query::ast::Literal::List(vec![
            nanograph::query::ast::Literal::Float(1.0),
            nanograph::query::ast::Literal::Float(0.0),
            nanograph::query::ast::Literal::Float(0.0),
            nanograph::query::ast::Literal::Float(0.0),
        ]),
    );

    let results = run_db_query_test_with_params(
        r#"
query q($topic: String, $q: Vector(4)) {
    match {
        $d: Doc { topic: $topic }
    }
    return { $d.slug as slug }
    order { nearest($d.embedding, $q) }
    limit 1
}
"#,
        &db,
        &params,
    )
    .await;

    let slugs = extract_string_column(&results, "slug");
    assert_eq!(slugs, vec!["b"]);
}

#[tokio::test]
async fn test_text_query_can_rank_media_nodes_and_traverse_from_images() {
    let _guard = EMBED_ENV_LOCK.lock().await;
    let prev_mock = std::env::var_os("NANOGRAPH_EMBEDDINGS_MOCK");
    unsafe {
        std::env::set_var("NANOGRAPH_EMBEDDINGS_MOCK", "1");
    }

    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let space_path = dir.path().join("space.png");
    let beach_path = dir.path().join("beach.png");
    std::fs::write(
        &space_path,
        b"\x89PNG\r\n\x1a\nspace-image-for-mock-semantic-test",
    )
    .unwrap();
    std::fs::write(
        &beach_path,
        b"\x89PNG\r\n\x1a\nbeach-image-for-mock-semantic-test",
    )
    .unwrap();
    let space_uri = Url::from_file_path(&space_path).unwrap().to_string();
    let beach_uri = Url::from_file_path(&beach_path).unwrap().to_string();

    let db = Database::init(&db_path, media_traversal_schema())
        .await
        .unwrap();
    db.load(&format!(
        r#"{{"type":"PhotoAsset","data":{{"slug":"space","uri":"@uri:{space_uri}"}}}}
{{"type":"PhotoAsset","data":{{"slug":"beach","uri":"@uri:{beach_uri}"}}}}
{{"type":"Product","data":{{"slug":"rocket","name":"Rocket Poster"}}}}
{{"type":"Product","data":{{"slug":"sand","name":"Beach Poster"}}}}
{{"edge":"HasPhoto","from":"rocket","to":"space"}}
{{"edge":"HasPhoto","from":"sand","to":"beach"}}
"#,
    ))
    .await
    .unwrap();

    let embedding_rows = match db
        .run(
            r#"
query image_embedding($slug: String) {
    match { $img: PhotoAsset { slug: $slug } }
    return { $img.embedding as embedding }
}
"#,
            "image_embedding",
            &ParamMap::from([(
                "slug".to_string(),
                nanograph::query::ast::Literal::String("space".to_string()),
            )]),
        )
        .await
        .unwrap()
    {
        nanograph::RunResult::Query(rows) => rows.to_rust_json(),
        _ => panic!("expected query result"),
    };
    let embedding_values = embedding_rows
        .as_array()
        .unwrap()
        .first()
        .unwrap()
        .as_object()
        .unwrap()["embedding"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| nanograph::query::ast::Literal::Float(value.as_f64().unwrap()))
        .collect::<Vec<_>>();

    let results = run_db_query_test_with_params(
        r#"
query q($vq: Vector(16)) {
    match {
        $product: Product
        $product hasPhoto $img
    }
    return { $product.slug as slug, $img.slug as image_slug }
    order { nearest($img.embedding, $vq) }
    limit 1
}
"#,
        &db,
        &ParamMap::from([(
            "vq".to_string(),
            nanograph::query::ast::Literal::List(embedding_values),
        )]),
    )
    .await;

    assert_eq!(extract_string_column(&results, "slug"), vec!["rocket"]);
    assert_eq!(extract_string_column(&results, "image_slug"), vec!["space"]);

    match prev_mock {
        Some(value) => unsafe { std::env::set_var("NANOGRAPH_EMBEDDINGS_MOCK", value) },
        None => unsafe { std::env::remove_var("NANOGRAPH_EMBEDDINGS_MOCK") },
    }
}

#[tokio::test]
async fn test_file_media_load_imports_and_embeds_images() {
    let _guard = EMBED_ENV_LOCK.lock().await;
    let prev_mock = std::env::var_os("NANOGRAPH_EMBEDDINGS_MOCK");
    unsafe {
        std::env::set_var("NANOGRAPH_EMBEDDINGS_MOCK", "1");
    }

    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let fixture_dir = dir.path().join("fixtures");
    let image_dir = fixture_dir.join("images");
    std::fs::create_dir_all(&image_dir).unwrap();
    std::fs::write(
        image_dir.join("space.png"),
        b"\x89PNG\r\n\x1a\nspace-image-for-file-media-e2e",
    )
    .unwrap();
    std::fs::write(
        image_dir.join("beach.png"),
        b"\x89PNG\r\n\x1a\nbeach-image-for-file-media-e2e",
    )
    .unwrap();
    std::fs::write(
        fixture_dir.join("data.jsonl"),
        r#"{"type":"PhotoAsset","data":{"slug":"space","uri":"@file:images/space.png"}}
{"type":"PhotoAsset","data":{"slug":"beach","uri":"@file:images/beach.png"}}
{"type":"Product","data":{"slug":"rocket","name":"Rocket Poster"}}
{"type":"Product","data":{"slug":"sand","name":"Beach Poster"}}
{"edge":"HasPhoto","from":"rocket","to":"space"}
{"edge":"HasPhoto","from":"sand","to":"beach"}
"#,
    )
    .unwrap();

    let db = Database::init(&db_path, media_traversal_schema())
        .await
        .unwrap();
    db.load_file_with_mode(&fixture_dir.join("data.jsonl"), LoadMode::Overwrite)
        .await
        .unwrap();

    let rows = match db
        .run(
            r#"
query image_row($slug: String) {
    match { $img: PhotoAsset { slug: $slug } }
    return { $img.uri as uri, $img.mime as mime, $img.embedding as embedding }
}
"#,
            "image_row",
            &ParamMap::from([(
                "slug".to_string(),
                nanograph::query::ast::Literal::String("space".to_string()),
            )]),
        )
        .await
        .unwrap()
    {
        nanograph::RunResult::Query(rows) => rows.to_rust_json(),
        _ => panic!("expected query result"),
    };
    let row = rows
        .as_array()
        .unwrap()
        .first()
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(row["mime"].as_str(), Some("image/png"));
    assert!(
        row["uri"]
            .as_str()
            .unwrap()
            .starts_with("lanceblob://sha256/")
    );
    assert_eq!(row["embedding"].as_array().unwrap().len(), 16);

    let results = run_db_query_test_with_params(
        r#"
query q($slug: String) {
    match {
        $img: PhotoAsset { slug: $slug }
        $product: Product
        $product hasPhoto $img
    }
    return { $product.slug as slug, $img.mime as mime }
}
"#,
        &db,
        &ParamMap::from([(
            "slug".to_string(),
            nanograph::query::ast::Literal::String("space".to_string()),
        )]),
    )
    .await;

    assert_eq!(extract_string_column(&results, "slug"), vec!["rocket"]);
    assert_eq!(extract_string_column(&results, "mime"), vec!["image/png"]);

    match prev_mock {
        Some(value) => unsafe { std::env::set_var("NANOGRAPH_EMBEDDINGS_MOCK", value) },
        None => unsafe { std::env::remove_var("NANOGRAPH_EMBEDDINGS_MOCK") },
    }
}

#[tokio::test]
async fn test_two_hop_traversal() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person { name: "Alice" }
        $p knows $mid
        $mid knows $fof
    }
    return { $fof.name }
}
"#,
        &storage,
    )
    .await;

    let names = extract_string_column(&results, "name");
    // Alice->Bob->Diana, Alice->Charlie->nobody
    assert_eq!(names, vec!["Diana"]);
}

#[tokio::test]
async fn test_bounded_traversal_exact_two_hops() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person { name: "Alice" }
        $p knows{2,2} $x
    }
    return { $x.name }
}
"#,
        &storage,
    )
    .await;

    let names = extract_string_column(&results, "name");
    assert_eq!(names, vec!["Diana"]);
}

#[tokio::test]
async fn test_bounded_traversal_range_one_to_two_hops() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person { name: "Alice" }
        $p knows{1,2} $x
    }
    return { $x.name }
    order { $x.name asc }
}
"#,
        &storage,
    )
    .await;

    let names = extract_string_column(&results, "name");
    assert_eq!(names, vec!["Bob", "Charlie", "Diana"]);
}

#[tokio::test]
async fn test_unbounded_traversal_is_rejected() {
    let storage = setup_storage().await;
    let qf = parse_query(
        r#"
query q() {
    match {
        $p: Person { name: "Alice" }
        $p knows{1,} $x
    }
    return { $x.name }
}
"#,
    )
    .unwrap();
    let err = typecheck_query(storage.catalog(), &qf.queries[0]).unwrap_err();
    assert!(err.to_string().contains("unbounded traversal is disabled"));
}

#[tokio::test]
async fn test_negation_unemployed() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person
        not { $p worksAt $_ }
    }
    return { $p.name }
}
"#,
        &storage,
    )
    .await;

    let mut names = extract_string_column(&results, "name");
    names.sort();
    // Charlie and Diana don't work anywhere
    assert_eq!(names, vec!["Charlie", "Diana"]);
}

#[tokio::test]
async fn test_negation_inner_filter_applies() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person
        not {
            $p worksAt $c
            $c.name = "Acme"
        }
    }
    return { $p.name }
}
"#,
        &storage,
    )
    .await;

    let mut names = extract_string_column(&results, "name");
    names.sort();
    // Excludes only Alice (worksAt Acme). Bob worksAt Globex and should remain.
    assert_eq!(names, vec!["Bob", "Charlie", "Diana"]);
}

#[tokio::test]
async fn test_aggregation_friend_counts() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person
        $p knows $f
    }
    return {
        $p.name
        count($f) as friends
    }
    order { friends desc }
}
"#,
        &storage,
    )
    .await;

    let total_rows: usize = results.iter().map(RecordBatch::num_rows).sum();
    assert_eq!(total_rows, 2);

    let names = extract_string_column(&results, "name");
    let friends = extract_u64_column(&results, "friends");
    assert_eq!(names.len(), 2);
    assert_eq!(friends.len(), 2);

    let mut pairs = names.into_iter().zip(friends).collect::<Vec<_>>();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        pairs,
        vec![("Alice".to_string(), 2), ("Bob".to_string(), 1)]
    );
}

#[tokio::test]
async fn test_order_and_limit() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person
    }
    return { $p.name, $p.age }
    order { $p.age desc }
    limit 2
}
"#,
        &storage,
    )
    .await;

    let names = extract_string_column(&results, "name");
    assert_eq!(names.len(), 2);
    assert_eq!(names[0], "Charlie"); // age 35
    assert_eq!(names[1], "Alice"); // age 30
}

#[tokio::test]
async fn test_limit_without_order() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person
    }
    return { $p.name }
    order { $p.name asc }
    limit 2
}
"#,
        &storage,
    )
    .await;

    let names = extract_string_column(&results, "name");
    assert_eq!(names, vec!["Alice", "Bob"]);
}

#[tokio::test]
async fn test_limit_with_single_scan_filter_pushdown() {
    let storage = setup_storage().await;
    let results = run_query_test(
        r#"
query q() {
    match {
        $p: Person
        $p.age > 25
    }
    return { $p.name }
    limit 2
}
"#,
        &storage,
    )
    .await;

    let names = extract_string_column(&results, "name");
    assert_eq!(names, vec!["Alice", "Charlie"]);
}

#[tokio::test]
async fn test_single_scan_filter_pushdown_parity_with_nonpushdown_tautology() {
    let storage = setup_storage().await;
    let pushed = run_query_test(
        r#"
query q() {
    match {
        $p: Person
        $p.age > 25
    }
    return { $p.name }
    order { $p.name asc }
}
"#,
        &storage,
    )
    .await;
    let mixed = run_query_test(
        r#"
query q() {
    match {
        $p: Person
        $p.age > 25
        $p.name = $p.name
    }
    return { $p.name }
    order { $p.name asc }
}
"#,
        &storage,
    )
    .await;

    let pushed_names = extract_string_column(&pushed, "name");
    let mixed_names = extract_string_column(&mixed, "name");
    assert_eq!(pushed_names, mixed_names);
    assert_eq!(pushed_names, vec!["Alice", "Charlie", "Diana"]);
}

#[tokio::test]
async fn test_multi_scan_filter_pushdown_parity_with_nonpushdown_tautology() {
    let storage = setup_storage().await;
    let pushed = run_query_test(
        r#"
query q() {
    match {
        $p: Person
        $q: Person
        $p.name = "Alice"
    }
    return { $p.name as p_name, $q.name as q_name }
    order { $q.name asc }
}
"#,
        &storage,
    )
    .await;
    let mixed = run_query_test(
        r#"
query q() {
    match {
        $p: Person
        $q: Person
        $p.name = "Alice"
        $p.name = $p.name
    }
    return { $p.name as p_name, $q.name as q_name }
    order { $q.name asc }
}
"#,
        &storage,
    )
    .await;

    let pushed_pairs = extract_string_pairs(&pushed, "p_name", "q_name");
    let mixed_pairs = extract_string_pairs(&mixed, "p_name", "q_name");
    assert_eq!(pushed_pairs, mixed_pairs);
    assert_eq!(
        pushed_pairs,
        vec![
            ("Alice".to_string(), "Alice".to_string()),
            ("Alice".to_string(), "Bob".to_string()),
            ("Alice".to_string(), "Charlie".to_string()),
            ("Alice".to_string(), "Diana".to_string())
        ]
    );
}

#[tokio::test]
async fn test_insert_mutation_query() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let db = Database::init(&db_path, test_schema()).await.unwrap();
    db.load(test_data()).await.unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "name".to_string(),
        nanograph::query::ast::Literal::String("Eve".to_string()),
    );
    params.insert(
        "age".to_string(),
        nanograph::query::ast::Literal::Integer(29),
    );
    let result = run_db_mutation_test_with_params(
        r#"
query add_person($name: String, $age: I32) {
    insert Person {
        name: $name
        age: $age
    }
}
"#,
        &db,
        &params,
    )
    .await;
    assert_eq!(result.affected_nodes, 1);
    assert_eq!(result.affected_edges, 0);

    let names = extract_string_column(
        &run_db_query_test_with_params(
            r#"
query q() {
    match { $p: Person { name: "Eve" } }
    return { $p.name }
}
"#,
            &db,
            &ParamMap::new(),
        )
        .await,
        "name",
    );
    assert_eq!(names, vec!["Eve"]);
}

#[tokio::test]
async fn test_insert_edge_mutation_query() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let db = Database::init(&db_path, test_schema()).await.unwrap();
    db.load(test_data()).await.unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "from".to_string(),
        nanograph::query::ast::Literal::String("Bob".to_string()),
    );
    params.insert(
        "to".to_string(),
        nanograph::query::ast::Literal::String("Charlie".to_string()),
    );

    let result = run_db_mutation_test_with_params(
        r#"
query add_knows($from: String, $to: String) {
    insert Knows {
        from: $from
        to: $to
    }
}
"#,
        &db,
        &params,
    )
    .await;
    assert_eq!(result.affected_nodes, 0);
    assert_eq!(result.affected_edges, 1);
    let edge_count = extract_u64_column(
        &run_db_query_test_with_params(
            r#"
query q() {
    match {
        $p: Person
        $p knows $f
    }
    return { count($f) as edge_count }
}
"#,
            &db,
            &ParamMap::new(),
        )
        .await,
        "edge_count",
    );
    assert_eq!(edge_count, vec![4]);
}

#[tokio::test]
async fn test_update_mutation_query_preserves_id_and_edges() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let db = Database::init(&db_path, keyed_mutation_schema())
        .await
        .unwrap();
    db.load(keyed_mutation_data()).await.unwrap();
    let manifest_before = read_committed_graph_snapshot(&db_path).unwrap();

    let before_rows = export_rows_for_db(&db).await;
    let alice_id_before = before_rows
        .iter()
        .find(|row| {
            row.get("type").and_then(serde_json::Value::as_str) == Some("Person")
                && row["data"]["name"].as_str() == Some("Alice")
        })
        .and_then(|row| row["id"].as_u64())
        .unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "name".to_string(),
        nanograph::query::ast::Literal::String("Alice".to_string()),
    );
    params.insert(
        "age".to_string(),
        nanograph::query::ast::Literal::Integer(31),
    );
    let result = run_db_mutation_test_with_params(
        r#"
query update_person($name: String, $age: I32) {
    update Person set { age: $age } where name = $name
}
"#,
        &db,
        &params,
    )
    .await;
    assert_eq!(result.affected_nodes, 1);

    let after_rows = export_rows_for_db(&db).await;
    let alice_after = after_rows
        .iter()
        .find(|row| {
            row.get("type").and_then(serde_json::Value::as_str) == Some("Person")
                && row["data"]["name"].as_str() == Some("Alice")
        })
        .unwrap();
    assert_eq!(alice_after["id"].as_u64(), Some(alice_id_before));
    assert_eq!(alice_after["data"]["age"].as_i64(), Some(31));
    let edge_count = extract_u64_column(
        &run_db_query_test_with_params(
            r#"
query q() {
    match {
        $p: Person
        $p knows $f
    }
    return { count($f) as edge_count }
}
"#,
            &db,
            &ParamMap::new(),
        )
        .await,
        "edge_count",
    );
    assert_eq!(edge_count, vec![1]);
    let manifest_after = read_committed_graph_snapshot(&db_path).unwrap();
    assert!(
        manifest_dataset_version(&manifest_after, "node", "Person")
            > manifest_dataset_version(&manifest_before, "node", "Person")
    );
    assert_eq!(
        manifest_dataset_version(&manifest_after, "edge", "Knows"),
        manifest_dataset_version(&manifest_before, "edge", "Knows")
    );

    let cdc = read_visible_cdc_entries(&db_path, manifest_before.db_version, None).unwrap();
    assert!(
        cdc.iter().any(|row| {
            row.db_version == manifest_after.db_version
                && row.op == "update"
                && row.entity_kind == "node"
                && row.type_name == "Person"
        }),
        "expected node update CDC event in latest db version"
    );
}

#[tokio::test]
async fn test_delete_edge_mutation_query() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let db = Database::init(&db_path, test_schema()).await.unwrap();
    db.load(test_data()).await.unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "from".to_string(),
        nanograph::query::ast::Literal::String("Alice".to_string()),
    );
    let result = run_db_mutation_test_with_params(
        r#"
query delete_knows($from: String) {
    delete Knows where from = $from
}
"#,
        &db,
        &params,
    )
    .await;
    assert_eq!(result.affected_nodes, 0);
    assert_eq!(result.affected_edges, 2);
    let edge_count = extract_u64_column(
        &run_db_query_test_with_params(
            r#"
query q() {
    match {
        $p: Person
        $p knows $f
    }
    return { count($f) as edge_count }
}
"#,
            &db,
            &ParamMap::new(),
        )
        .await,
        "edge_count",
    );
    assert_eq!(edge_count, vec![1]);
}

#[tokio::test]
async fn test_delete_edge_mutation_query_uses_declared_key_for_endpoint_lookup() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let db = Database::init(
        &db_path,
        r#"
node Person {
    slug: String @key
    name: String
}
node Company {
    slug: String @key
    name: String
}
edge WorksAt: Person -> Company
"#,
    )
    .await
    .unwrap();
    db.load(
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Company","data":{"slug":"acme","name":"Acme"}}
{"edge":"WorksAt","from":"alice","to":"acme"}"#,
    )
    .await
    .unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "from".to_string(),
        nanograph::query::ast::Literal::String("alice".to_string()),
    );
    let result = run_db_mutation_test_with_params(
        r#"
query remove_work($from: String) {
    delete WorksAt where from = $from
}
"#,
        &db,
        &params,
    )
    .await;
    assert_eq!(result.affected_nodes, 0);
    assert_eq!(result.affected_edges, 1);
    let exported = export_rows_for_db(&db).await;
    let works_at_count = exported
        .iter()
        .filter(|row| row.get("edge").and_then(|value| value.as_str()) == Some("WorksAt"))
        .count();
    assert_eq!(works_at_count, 0);
}

#[tokio::test]
async fn test_delete_edge_mutation_query_missing_endpoint_is_noop() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let db = Database::init(
        &db_path,
        r#"
node Person {
    slug: String @key
    name: String
}
node Company {
    slug: String @key
    name: String
}
edge WorksAt: Person -> Company
"#,
    )
    .await
    .unwrap();
    db.load(
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Company","data":{"slug":"acme","name":"Acme"}}
{"edge":"WorksAt","from":"alice","to":"acme"}"#,
    )
    .await
    .unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "from".to_string(),
        nanograph::query::ast::Literal::String("nobody".to_string()),
    );
    let result = run_db_mutation_test_with_params(
        r#"
query remove_work($from: String) {
    delete WorksAt where from = $from
}
"#,
        &db,
        &params,
    )
    .await;
    assert_eq!(result.affected_nodes, 0);
    assert_eq!(result.affected_edges, 0);
    let exported = export_rows_for_db(&db).await;
    let works_at_count = exported
        .iter()
        .filter(|row| row.get("edge").and_then(|value| value.as_str()) == Some("WorksAt"))
        .count();
    assert_eq!(works_at_count, 1);
}

#[tokio::test]
async fn test_delete_mutation_query_cascades_edges() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let db = Database::init(&db_path, test_schema()).await.unwrap();
    db.load(test_data()).await.unwrap();

    let mut params = ParamMap::new();
    params.insert(
        "name".to_string(),
        nanograph::query::ast::Literal::String("Alice".to_string()),
    );
    let result = run_db_mutation_test_with_params(
        r#"
query delete_person($name: String) {
    delete Person where name = $name
}
"#,
        &db,
        &params,
    )
    .await;
    assert_eq!(result.affected_nodes, 1);
    assert_eq!(result.affected_edges, 3);

    let people = run_db_query_test_with_params(
        r#"
query q() {
    match { $p: Person }
    return { $p.name }
}
"#,
        &db,
        &ParamMap::new(),
    )
    .await;
    assert_eq!(extract_string_column(&people, "name").len(), 3);
    let knows_count = extract_u64_column(
        &run_db_query_test_with_params(
            r#"
query q() {
    match {
        $p: Person
        $p knows $f
    }
    return { count($f) as edge_count }
}
"#,
            &db,
            &ParamMap::new(),
        )
        .await,
        "edge_count",
    );
    assert_eq!(knows_count, vec![1]);
    let works_at_count = extract_u64_column(
        &run_db_query_test_with_params(
            r#"
query q() {
    match {
        $p: Person
        $p worksAt $c
    }
    return { count($c) as edge_count }
}
"#,
            &db,
            &ParamMap::new(),
        )
        .await,
        "edge_count",
    );
    assert_eq!(works_at_count, vec![1]);
}

#[tokio::test]
async fn test_type_error_unknown_type() {
    let storage = setup_storage().await;
    let catalog = storage.catalog();
    let qf = parse_query(
        r#"
query q() {
    match { $p: Foo }
    return { $p.name }
}
"#,
    )
    .unwrap();
    let err = typecheck_query(catalog, &qf.queries[0]).unwrap_err();
    assert!(err.to_string().contains("T1"));
}

#[tokio::test]
async fn test_type_error_bad_endpoints() {
    let storage = setup_storage().await;
    let catalog = storage.catalog();
    let qf = parse_query(
        r#"
query q() {
    match {
        $c: Company
        $c knows $f
    }
    return { $c.name }
}
"#,
    )
    .unwrap();
    let err = typecheck_query(catalog, &qf.queries[0]).unwrap_err();
    assert!(err.to_string().contains("T5"));
}

#[tokio::test]
async fn test_schema_typecheck_all_valid() {
    // Test that all valid queries from test.gq parse and typecheck
    let schema = parse_schema(test_schema()).unwrap();
    let catalog = build_catalog(&schema).unwrap();

    let query_src = std::fs::read_to_string("tests/fixtures/test.gq").unwrap();
    let qf = parse_query(&query_src).unwrap();

    for q in &qf.queries {
        let result = typecheck_query(&catalog, q);
        assert!(
            result.is_ok(),
            "query {} failed typecheck: {:?}",
            q.name,
            result.err()
        );
    }
}

fn fan_in_schema() -> &'static str {
    r#"
node A { slug: String @key }
node B { slug: String @key }
node C { slug: String @key }

edge AToB: A -> B
edge CToB: C -> B
"#
}

fn fan_in_data() -> &'static str {
    r#"{"type":"A","data":{"slug":"a1"}}
{"type":"A","data":{"slug":"a2"}}
{"type":"B","data":{"slug":"b1"}}
{"type":"B","data":{"slug":"b2"}}
{"type":"C","data":{"slug":"c1"}}
{"type":"C","data":{"slug":"c2"}}
{"edge":"AToB","from":"a1","to":"b1"}
{"edge":"AToB","from":"a2","to":"b2"}
{"edge":"CToB","from":"c1","to":"b1"}
{"edge":"CToB","from":"c2","to":"b2"}
"#
}

// Regression test for issue #8: two edges landing on the same destination variable
// previously failed at execution with `number of columns(N) must match number of fields(M)`
// because the cycle-closing lowering reused a single `__temp_b` name across both
// traversals, producing duplicate destination columns in the Expand schema.
#[tokio::test]
async fn test_fan_in_two_edges_same_destination() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let db = Database::init(&db_path, fan_in_schema()).await.unwrap();
    db.load(fan_in_data()).await.unwrap();

    let results = run_db_query_test_with_params(
        r#"
query fan_in() {
    match {
        $a: A
        $b: B
        $c: C
        $a aToB $b
        $c cToB $b
    }
    return { $a.slug as a_slug, $c.slug as c_slug }
    order { $a.slug asc }
}
"#,
        &db,
        &ParamMap::new(),
    )
    .await;

    let pairs = extract_string_pairs(&results, "a_slug", "c_slug");
    assert_eq!(
        pairs,
        vec![
            ("a1".to_string(), "c1".to_string()),
            ("a2".to_string(), "c2".to_string()),
        ]
    );
}

// Regression test for issue #8 (not{} variant): a `not { }` clause containing a
// traversal whose destination is an outer-bound variable hit the same column-count
// bug because the inner pipeline's temp var name collided with whatever the outer
// match emitted for the same destination.
#[tokio::test]
async fn test_fan_in_with_negation_on_shared_destination() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let db = Database::init(&db_path, fan_in_schema()).await.unwrap();
    db.load(fan_in_data()).await.unwrap();

    // Find A nodes that point at a B which is NOT pointed at by any C.
    // With the fixture, every B has a corresponding C, so this should return zero rows
    // — the test is about the query *executing* without an Arrow schema error.
    let results = run_db_query_test_with_params(
        r#"
query fan_in_neg() {
    match {
        $a: A
        $b: B
        $a aToB $b
        not {
            $c: C
            $c cToB $b
        }
    }
    return { $a.slug as a_slug }
}
"#,
        &db,
        &ParamMap::new(),
    )
    .await;

    let names = extract_string_column(&results, "a_slug");
    assert!(
        names.is_empty(),
        "expected zero rows (every B has a matching C), got {names:?}"
    );

    // Insert an orphan B with no incoming CToB edge and verify the query now finds it.
    db.load(
        r#"{"type":"A","data":{"slug":"a3"}}
{"type":"B","data":{"slug":"b3"}}
{"edge":"AToB","from":"a3","to":"b3"}
"#,
    )
    .await
    .unwrap();

    let results = run_db_query_test_with_params(
        r#"
query fan_in_neg() {
    match {
        $a: A
        $b: B
        $a aToB $b
        not {
            $c: C
            $c cToB $b
        }
    }
    return { $a.slug as a_slug }
}
"#,
        &db,
        &ParamMap::new(),
    )
    .await;

    let names = extract_string_column(&results, "a_slug");
    assert_eq!(names, vec!["a3"]);
}
