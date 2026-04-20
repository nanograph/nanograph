use super::*;
use crate::metadata::{build_describe_payload, build_export_rows, build_version_payload};
use arrow_array::{
    ArrayRef, BooleanArray, Date32Array, Date64Array, Int32Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use nanograph::store::metadata::DatabaseMetadata;
use nanograph::store::txlog::{
    VisibleCdcSource, read_visible_cdc_entries, read_visible_cdc_entries_with_source,
    read_visible_change_rows,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

fn write_file(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
}

fn version_dataset_entries(payload: &serde_json::Value) -> Vec<&serde_json::Value> {
    payload["db"]["dataset_versions"]
        .as_array()
        .unwrap()
        .iter()
        .collect()
}

fn version_data_dataset_path(
    payload: &serde_json::Value,
    kind: &str,
    type_name: &str,
) -> String {
    version_dataset_entries(payload)
        .into_iter()
        .find(|entry| entry["kind"] == kind && entry["type_name"] == type_name)
        .and_then(|entry| entry["dataset_path"].as_str())
        .unwrap()
        .to_string()
}

#[test]
fn dotenv_loader_sets_missing_and_skips_existing_keys() {
    let dir = TempDir::new().unwrap();
    let dotenv_path = dir.path().join(".env");
    write_file(
        &dotenv_path,
        "OPENAI_API_KEY=from_file\nNANOGRAPH_EMBEDDINGS_MOCK=1\n",
    );

    let env = RefCell::new(HashMap::from([(
        "OPENAI_API_KEY".to_string(),
        "preset".to_string(),
    )]));
    let stats = load_dotenv_from_path_with(
        &dotenv_path,
        |key| env.borrow().contains_key(key),
        |key, value| {
            env.borrow_mut().insert(key.to_string(), value.to_string());
        },
    )
    .unwrap();

    assert_eq!(stats.loaded, 1);
    assert_eq!(stats.skipped_existing, 1);
    assert_eq!(
        env.borrow().get("OPENAI_API_KEY").map(String::as_str),
        Some("preset")
    );
    assert_eq!(
        env.borrow()
            .get("NANOGRAPH_EMBEDDINGS_MOCK")
            .map(String::as_str),
        Some("1")
    );
}

#[test]
fn dotenv_loader_is_noop_when_file_missing() {
    let dir = TempDir::new().unwrap();
    let env = RefCell::new(HashMap::<String, String>::new());
    let stats = load_dotenv_from_dir_with(
        dir.path(),
        |key| env.borrow().contains_key(key),
        |key, value| {
            env.borrow_mut().insert(key.to_string(), value.to_string());
        },
    )
    .unwrap();
    assert!(stats.is_none());
    assert!(env.borrow().is_empty());
}

#[test]
fn project_dotenv_loader_prefers_env_nano_before_env() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join(".env.nano"), "OPENAI_API_KEY=from_nano\n");
    write_file(&dir.path().join(".env"), "OPENAI_API_KEY=from_env\n");

    let env = RefCell::new(HashMap::<String, String>::new());
    let results = load_project_dotenv_from_dir_with(
        dir.path(),
        |key| env.borrow().contains_key(key),
        |key, value| {
            env.borrow_mut().insert(key.to_string(), value.to_string());
        },
    );

    assert_eq!(results.len(), 2);
    assert_eq!(
        env.borrow().get("OPENAI_API_KEY").map(String::as_str),
        Some("from_nano")
    );
}

#[test]
fn scaffold_project_files_creates_shared_config_and_env_template() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("demo.nano");
    let schema_path = dir.path().join("schema.pg");
    write_file(
        &schema_path,
        r#"node Person {
    name: String @key
}"#,
    );

    let generated = scaffold_project_files(dir.path(), &db_path, &schema_path).unwrap();
    assert_eq!(generated.len(), 2);

    let config = std::fs::read_to_string(dir.path().join("nanograph.toml")).unwrap();
    assert!(config.contains("[db]"));
    assert!(config.contains("default_path = \"demo.nano\""));
    assert!(config.contains("[schema]"));
    assert!(config.contains("default_path = \"schema.pg\""));
    assert!(config.contains("[embedding]"));
    assert!(config.contains("provider = \"openai\""));
    assert!(config.contains("[cli]"));
    assert!(config.contains("table_max_column_width = 80"));
    assert!(config.contains("table_cell_layout = \"truncate\""));

    let dotenv = std::fs::read_to_string(dir.path().join(".env.nano")).unwrap();
    assert!(dotenv.contains("OPENAI_API_KEY=sk-..."));
    assert!(dotenv.contains("GEMINI_API_KEY=..."));
    assert!(dotenv.contains("Do not commit this file."));

    let generated_again = scaffold_project_files(dir.path(), &db_path, &schema_path).unwrap();
    assert!(generated_again.is_empty());
}

#[test]
fn infer_init_project_dir_prefers_shared_parent_of_db_and_schema() {
    let cwd = Path::new("/workspace");
    let project_dir = infer_init_project_dir(
        cwd,
        Path::new("/tmp/demo/db"),
        Path::new("/tmp/demo/schema.pg"),
    );
    assert_eq!(project_dir, PathBuf::from("/tmp/demo"));
}

#[test]
fn query_execution_preamble_renders_description_and_instruction() {
    let query = nanograph::query::ast::QueryDecl {
        name: "semantic_search".to_string(),
        description: Some("Find semantically similar documents.".to_string()),
        instruction: Some(
            "Use for conceptual search. Prefer keyword_search for exact terms.".to_string(),
        ),
        params: Vec::new(),
        match_clause: Vec::new(),
        return_clause: Vec::new(),
        order_clause: Vec::new(),
        limit: None,
        mutation: None,
    };

    assert_eq!(
        query_execution_preamble(&query, "table", false).as_deref(),
        Some(
            "Query: semantic_search\nDescription: Find semantically similar documents.\nInstruction: Use for conceptual search. Prefer keyword_search for exact terms.\n\n"
        )
    );
    assert_eq!(
        query_execution_preamble(&query, "kv", false).as_deref(),
        Some(
            "Query: semantic_search\nDescription: Find semantically similar documents.\nInstruction: Use for conceptual search. Prefer keyword_search for exact terms.\n\n"
        )
    );
}

#[test]
fn query_execution_preamble_skips_machine_formats() {
    let query = nanograph::query::ast::QueryDecl {
        name: "semantic_search".to_string(),
        description: Some("Find semantically similar documents.".to_string()),
        instruction: None,
        params: Vec::new(),
        match_clause: Vec::new(),
        return_clause: Vec::new(),
        order_clause: Vec::new(),
        limit: None,
        mutation: None,
    };

    assert!(query_execution_preamble(&query, "json", false).is_none());
    assert!(query_execution_preamble(&query, "table", true).is_none());
}

#[test]
fn query_metadata_json_includes_description_and_instruction() {
    let query = nanograph::query::ast::QueryDecl {
        name: "semantic_search".to_string(),
        description: Some("Find semantically similar documents.".to_string()),
        instruction: Some(
            "Use for conceptual search. Prefer keyword_search for exact terms.".to_string(),
        ),
        params: Vec::new(),
        match_clause: Vec::new(),
        return_clause: Vec::new(),
        order_clause: Vec::new(),
        limit: None,
        mutation: None,
    };

    assert_eq!(
        query_metadata_json(&query),
        serde_json::json!({
            "name": "semantic_search",
            "description": "Find semantically similar documents.",
            "instruction": "Use for conceptual search. Prefer keyword_search for exact terms."
        })
    );
}

#[test]
fn format_kv_rows_renders_typed_scalars_and_nested_values() {
    let rows = vec![serde_json::json!({
        "type": "Character",
        "slug": "luke-skywalker",
        "name": "Luke Skywalker",
        "age": 23,
        "active": true,
        "homeworld": serde_json::Value::Null,
        "tags": ["jedi", "pilot"],
        "meta": {
            "rank": "commander"
        }
    })];

    let formatted = format_kv_rows(&rows, false);
    assert!(formatted.starts_with("Character: luke-skywalker\n"));
    assert!(formatted.contains("type     : Character\n"));
    assert!(formatted.contains("slug     : luke-skywalker\n"));
    assert!(formatted.contains("name     : Luke Skywalker\n"));
    assert!(formatted.contains("age      : 23\n"));
    assert!(formatted.contains("active   : true\n"));
    assert!(formatted.contains("homeworld: null\n"));
    assert!(formatted.contains("tags     :\n  - jedi\n  - pilot\n"));
    assert!(formatted.contains("meta     :\n  rank: commander\n"));
}

#[test]
fn format_kv_rows_separates_rows_with_headers_and_divider() {
    let rows = vec![
        serde_json::json!({
            "slug": "luke-skywalker",
            "name": "Luke Skywalker",
        }),
        serde_json::json!({
            "name": "Leia Organa",
            "role": "General",
        }),
    ];

    let formatted = format_kv_rows(&rows, false);
    assert!(formatted.contains("Row 1: luke-skywalker\n"));
    assert!(formatted.contains("slug: luke-skywalker\n"));
    assert!(formatted.contains("\n────────────────────────\n\nRow 2: Leia Organa\n"));
    assert!(formatted.contains("role: General\n"));
}

#[test]
fn default_log_filter_matches_build_mode() {
    assert_eq!(default_log_filter(), "error");
}

#[test]
fn parse_load_mode_from_cli() {
    let cli = Cli::parse_from([
        "nanograph",
        "load",
        "--db",
        "/tmp/db",
        "--data",
        "/tmp/data.jsonl",
        "--mode",
        "append",
    ]);
    match cli.command {
        Commands::Load { db, mode, .. } => {
            assert_eq!(db, Some(PathBuf::from("/tmp/db")));
            assert_eq!(mode, LoadModeArg::Append);
        }
        _ => panic!("expected load command"),
    }
}

#[test]
fn parse_changes_range_from_cli() {
    let cli = Cli::parse_from([
        "nanograph",
        "changes",
        "--db",
        "/tmp/db",
        "--from",
        "2",
        "--to",
        "4",
        "--format",
        "json",
        "--cdc-source",
        "lineage-shadow",
    ]);
    match cli.command {
        Commands::Changes {
            db,
            from_version,
            to_version,
            format,
            cdc_source,
            ..
        } => {
            assert_eq!(db, Some(PathBuf::from("/tmp/db")));
            assert_eq!(from_version, Some(2));
            assert_eq!(to_version, Some(4));
            assert_eq!(format.as_deref(), Some("json"));
            assert_eq!(cdc_source, ChangesSourceArg::LineageShadow);
        }
        _ => panic!("expected changes command"),
    }
}

#[test]
fn parse_maintenance_commands_from_cli() {
    let compact = Cli::parse_from([
        "nanograph",
        "compact",
        "--db",
        "/tmp/db",
        "--target-rows-per-fragment",
        "1000",
    ]);
    match compact.command {
        Commands::Compact {
            db,
            target_rows_per_fragment,
            ..
        } => {
            assert_eq!(db, Some(PathBuf::from("/tmp/db")));
            assert_eq!(target_rows_per_fragment, 1000);
        }
        _ => panic!("expected compact command"),
    }

    let cleanup = Cli::parse_from([
        "nanograph",
        "cleanup",
        "--db",
        "/tmp/db",
        "--retain-tx-versions",
        "4",
        "--retain-dataset-versions",
        "3",
    ]);
    match cleanup.command {
        Commands::Cleanup {
            db,
            retain_tx_versions,
            retain_dataset_versions,
            ..
        } => {
            assert_eq!(db, Some(PathBuf::from("/tmp/db")));
            assert_eq!(retain_tx_versions, 4);
            assert_eq!(retain_dataset_versions, 3);
        }
        _ => panic!("expected cleanup command"),
    }

    let doctor = Cli::parse_from(["nanograph", "doctor", "--db", "/tmp/db"]);
    match doctor.command {
        Commands::Doctor { db, .. } => assert_eq!(db, Some(PathBuf::from("/tmp/db"))),
        _ => panic!("expected doctor command"),
    }

    let materialize = Cli::parse_from([
        "nanograph",
        "cdc-materialize",
        "--db",
        "/tmp/db",
        "--min-new-rows",
        "50",
        "--force",
    ]);
    match materialize.command {
        Commands::CdcMaterialize {
            db,
            min_new_rows,
            force,
            ..
        } => {
            assert_eq!(db, Some(PathBuf::from("/tmp/db")));
            assert_eq!(min_new_rows, 50);
            assert!(force);
        }
        _ => panic!("expected cdc-materialize command"),
    }
}

#[test]
fn parse_metadata_commands_from_cli() {
    let version = Cli::parse_from(["nanograph", "--quiet", "version", "--db", "/tmp/db"]);
    assert!(version.quiet);
    match version.command {
        Commands::Version { db } => assert_eq!(db, Some(PathBuf::from("/tmp/db"))),
        _ => panic!("expected version command"),
    }

    let describe = Cli::parse_from([
        "nanograph",
        "describe",
        "--db",
        "/tmp/db",
        "--format",
        "json",
        "--verbose",
    ]);
    match describe.command {
        Commands::Describe {
            db,
            format,
            type_name,
            verbose,
        } => {
            assert_eq!(db, Some(PathBuf::from("/tmp/db")));
            assert_eq!(format.as_deref(), Some("json"));
            assert!(type_name.is_none());
            assert!(verbose);
        }
        _ => panic!("expected describe command"),
    }

    let export = Cli::parse_from([
        "nanograph",
        "export",
        "--db",
        "/tmp/db",
        "--format",
        "jsonl",
        "--no-embeddings",
    ]);
    match export.command {
        Commands::Export {
            db,
            format,
            no_embeddings,
        } => {
            assert_eq!(db, Some(PathBuf::from("/tmp/db")));
            assert_eq!(format.as_deref(), Some("jsonl"));
            assert!(no_embeddings);
        }
        _ => panic!("expected export command"),
    }

    let schema_diff = Cli::parse_from([
        "nanograph",
        "schema-diff",
        "--from",
        "/tmp/old.pg",
        "--to",
        "/tmp/new.pg",
        "--format",
        "json",
    ]);
    match schema_diff.command {
        Commands::SchemaDiff {
            from_schema,
            to_schema,
            format,
        } => {
            assert_eq!(from_schema, PathBuf::from("/tmp/old.pg"));
            assert_eq!(to_schema, PathBuf::from("/tmp/new.pg"));
            assert_eq!(format.as_deref(), Some("json"));
        }
        _ => panic!("expected schema-diff command"),
    }
}

#[test]
fn resolve_changes_window_supports_since_and_range_modes() {
    let since = resolve_changes_window(Some(5), None, None).unwrap();
    assert_eq!(
        since,
        ChangesWindow {
            from_db_version_exclusive: 5,
            to_db_version_inclusive: None
        }
    );

    let range = resolve_changes_window(None, Some(2), Some(4)).unwrap();
    assert_eq!(
        range,
        ChangesWindow {
            from_db_version_exclusive: 1,
            to_db_version_inclusive: Some(4)
        }
    );
}

#[test]
fn resolve_changes_window_rejects_invalid_ranges() {
    assert!(resolve_changes_window(Some(1), Some(1), Some(2)).is_err());
    assert!(resolve_changes_window(None, Some(4), Some(3)).is_err());
    assert!(resolve_changes_window(None, Some(2), None).is_err());
    assert!(resolve_changes_window(None, None, Some(2)).is_err());
}

#[tokio::test]
async fn load_mode_merge_requires_keyed_schema() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");

    write_file(
        &schema_path,
        r#"node Person {
    name: String
}"#,
    );
    write_file(&data_path, r#"{"type":"Person","data":{"name":"Alice"}}"#);

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    let err = cmd_load(&db_path, &data_path, LoadModeArg::Merge, false, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("requires at least one node @key"));
}

#[tokio::test]
async fn load_mode_append_and_merge_behave_as_expected() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_initial = dir.path().join("initial.jsonl");
    let data_append = dir.path().join("append.jsonl");
    let data_merge = dir.path().join("merge.jsonl");

    write_file(
        &schema_path,
        r#"node Person {
    name: String @key
    age: I32?
}"#,
    );
    write_file(
        &data_initial,
        r#"{"type":"Person","data":{"name":"Alice","age":30}}"#,
    );
    write_file(
        &data_append,
        r#"{"type":"Person","data":{"name":"Bob","age":22}}"#,
    );
    write_file(
        &data_merge,
        r#"{"type":"Person","data":{"name":"Alice","age":31}}"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(
        &db_path,
        &data_initial,
        LoadModeArg::Overwrite,
        false,
        false,
    )
    .await
    .unwrap();
    cmd_load(&db_path, &data_append, LoadModeArg::Append, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_merge, LoadModeArg::Merge, false, false)
        .await
        .unwrap();

    let rows = build_export_rows(&db_path, false, true).await.unwrap();
    let people: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|row| row.get("type").and_then(serde_json::Value::as_str) == Some("Person"))
        .collect();
    assert_eq!(people.len(), 2);
    let alice_age = people.iter().find_map(|row| {
        (row["data"]["name"].as_str() == Some("Alice"))
            .then_some(row["data"]["age"].as_i64())
            .flatten()
    });
    let has_bob = people
        .iter()
        .any(|row| row["data"]["name"].as_str() == Some("Bob"));
    assert_eq!(alice_age, Some(31));
    assert!(has_bob);
}

#[test]
fn lint_and_run_allow_db_to_be_resolved_later() {
    let lint = Cli::try_parse_from(["nanograph", "lint", "--query", "/tmp/q.gq"]).unwrap();
    match lint.command {
        Commands::Lint { db, query, .. } => {
            assert!(db.is_none());
            assert_eq!(query, PathBuf::from("/tmp/q.gq"));
        }
        _ => panic!("expected lint command"),
    }

    let run = Cli::try_parse_from(["nanograph", "run", "search", "--param", "q=hello"]).unwrap();
    match run.command {
        Commands::Run {
            alias,
            args,
            db,
            query,
            name,
            ..
        } => {
            assert_eq!(alias.as_deref(), Some("search"));
            assert!(args.is_empty());
            assert!(db.is_none());
            assert!(query.is_none());
            assert!(name.is_none());
        }
        _ => panic!("expected run command"),
    }
}

#[test]
fn parse_run_alias_with_positional_args() {
    let run = Cli::try_parse_from([
        "nanograph",
        "run",
        "search",
        "vector databases",
        "--param",
        "limit=5",
    ])
    .unwrap();
    match run.command {
        Commands::Run {
            alias,
            args,
            params,
            ..
        } => {
            assert_eq!(alias.as_deref(), Some("search"));
            assert_eq!(args, vec!["vector databases".to_string()]);
            assert_eq!(params, vec![("limit".to_string(), "5".to_string())]);
        }
        _ => panic!("expected run command"),
    }
}

#[test]
fn merge_run_params_maps_alias_positionals_and_preserves_explicit_overrides() {
    let merged = merge_run_params(
        Some("search"),
        &[String::from("q"), String::from("limit")],
        vec!["vector databases".to_string()],
        vec![
            ("q".to_string(), "override".to_string()),
            ("format".to_string(), "json".to_string()),
        ],
    )
    .unwrap();
    assert_eq!(
        merged,
        vec![
            ("q".to_string(), "vector databases".to_string()),
            ("q".to_string(), "override".to_string()),
            ("format".to_string(), "json".to_string()),
        ]
    );
}

#[test]
fn merge_run_params_rejects_positional_args_without_alias_mapping() {
    let err = merge_run_params(
        Some("search"),
        &[],
        vec!["vector databases".to_string()],
        Vec::new(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("does not declare args"));
}

#[test]
fn build_param_map_parses_date_and_datetime_types() {
    let query_params = vec![
        nanograph::query::ast::Param {
            name: "d".to_string(),
            type_name: "Date".to_string(),
            nullable: false,
        },
        nanograph::query::ast::Param {
            name: "dt".to_string(),
            type_name: "DateTime".to_string(),
            nullable: false,
        },
    ];
    let raw = vec![
        ("d".to_string(), "2026-02-14".to_string()),
        ("dt".to_string(), "2026-02-14T10:00:00Z".to_string()),
    ];

    let params = build_param_map(&query_params, &raw).unwrap();
    assert!(matches!(
        params.get("d"),
        Some(Literal::Date(v)) if v == "2026-02-14"
    ));
    assert!(matches!(
        params.get("dt"),
        Some(Literal::DateTime(v)) if v == "2026-02-14T10:00:00Z"
    ));
}

#[test]
fn build_param_map_parses_vector_type() {
    let query_params = vec![nanograph::query::ast::Param {
        name: "q".to_string(),
        type_name: "Vector(3)".to_string(),
        nullable: false,
    }];
    let raw = vec![("q".to_string(), "[0.1, 0.2, 0.3]".to_string())];

    let params = build_param_map(&query_params, &raw).unwrap();
    match params.get("q") {
        Some(Literal::List(items)) => {
            assert_eq!(items.len(), 3);
            assert!(matches!(items[0], Literal::Float(_)));
            assert!(matches!(items[1], Literal::Float(_)));
            assert!(matches!(items[2], Literal::Float(_)));
        }
        other => panic!("expected vector list literal, got {:?}", other),
    }
}

#[test]
fn build_param_map_parses_u32_and_u64_types() {
    let query_params = vec![
        nanograph::query::ast::Param {
            name: "u32v".to_string(),
            type_name: "U32".to_string(),
            nullable: false,
        },
        nanograph::query::ast::Param {
            name: "u64v".to_string(),
            type_name: "U64".to_string(),
            nullable: false,
        },
    ];
    let raw = vec![
        ("u32v".to_string(), "42".to_string()),
        ("u64v".to_string(), "9001".to_string()),
    ];

    let params = build_param_map(&query_params, &raw).unwrap();
    assert!(matches!(params.get("u32v"), Some(Literal::Integer(42))));
    assert!(matches!(params.get("u64v"), Some(Literal::Integer(9001))));
}

#[test]
fn build_param_map_rejects_u64_values_outside_literal_range() {
    let query_params = vec![nanograph::query::ast::Param {
        name: "u64v".to_string(),
        type_name: "U64".to_string(),
        nullable: false,
    }];
    let too_large = format!("{}", (i64::MAX as u128) + 1);
    let raw = vec![("u64v".to_string(), too_large)];

    let err = build_param_map(&query_params, &raw).unwrap_err();
    assert!(err.to_string().contains("exceeds supported range"));
}

#[test]
fn parse_param_single_quote_value_does_not_panic() {
    let (key, value) = parse_param("x='").unwrap();
    assert_eq!(key, "x");
    assert_eq!(value, "'");
}

#[test]
fn parse_delete_predicate_single_quote_value_does_not_panic() {
    let pred = parse_delete_predicate("slug='").unwrap();
    assert_eq!(pred.property, "slug");
    assert_eq!(pred.op, DeleteOp::Eq);
    assert_eq!(pred.value, "'");
}

#[test]
fn array_value_to_json_formats_temporal_types_as_iso_strings() {
    use nanograph::json_output::array_value_to_json;
    let date: ArrayRef = Arc::new(Date32Array::from(vec![Some(20498)]));
    let dt: ArrayRef = Arc::new(Date64Array::from(vec![Some(1771063200000)]));

    assert_eq!(
        array_value_to_json(&date, 0),
        serde_json::Value::String("2026-02-14".to_string())
    );
    assert_eq!(
        array_value_to_json(&dt, 0),
        serde_json::Value::String("2026-02-14T10:00:00.000Z".to_string())
    );
}

#[test]
fn format_jsonl_preserves_typed_values() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("age", DataType::Int32, false),
        Field::new("active", DataType::Boolean, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(vec![Some("Luke")])) as ArrayRef,
            Arc::new(Int32Array::from(vec![Some(23)])) as ArrayRef,
            Arc::new(BooleanArray::from(vec![Some(true)])) as ArrayRef,
        ],
    )
    .unwrap();

    let output = format_jsonl(&[batch]).unwrap();
    let line = output.trim_end();
    let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({
            "name": "Luke",
            "age": 23,
            "active": true
        })
    );
}

#[test]
fn format_jsonl_with_query_emits_metadata_header_then_rows() {
    let schema = Arc::new(Schema::new(vec![Field::new("name", DataType::Utf8, false)]));
    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::new(StringArray::from(vec![Some("Luke")])) as ArrayRef],
    )
    .unwrap();
    let query = nanograph::query::ast::QueryDecl {
        name: "semantic_search".to_string(),
        description: Some("Find semantically similar documents.".to_string()),
        instruction: Some(
            "Use for conceptual search. Prefer keyword_search for exact terms.".to_string(),
        ),
        params: Vec::new(),
        match_clause: Vec::new(),
        return_clause: Vec::new(),
        order_clause: Vec::new(),
        limit: None,
        mutation: None,
    };

    let output = format_jsonl_with_query(&query, &[batch]).unwrap();
    let mut lines = output.lines();
    let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let row: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(
        header,
        serde_json::json!({
            "$nanograph": {
                "query": {
                    "name": "semantic_search",
                    "description": "Find semantically similar documents.",
                    "instruction": "Use for conceptual search. Prefer keyword_search for exact terms."
                }
            }
        })
    );
    assert_eq!(row, serde_json::json!({ "name": "Luke" }));
    assert!(lines.next().is_none());
}

#[test]
fn print_json_with_query_wraps_rows_with_query_metadata() {
    let schema = Arc::new(Schema::new(vec![Field::new("name", DataType::Utf8, false)]));
    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::new(StringArray::from(vec![Some("Luke")])) as ArrayRef],
    )
    .unwrap();
    let query = nanograph::query::ast::QueryDecl {
        name: "semantic_search".to_string(),
        description: Some("Find semantically similar documents.".to_string()),
        instruction: Some(
            "Use for conceptual search. Prefer keyword_search for exact terms.".to_string(),
        ),
        params: Vec::new(),
        match_clause: Vec::new(),
        return_clause: Vec::new(),
        order_clause: Vec::new(),
        limit: None,
        mutation: None,
    };

    let rows = nanograph::json_output::record_batches_to_json_rows(&[batch]);
    let payload = serde_json::json!({
        "$nanograph": {
            "query": query_metadata_json(&query),
        },
        "rows": rows,
    });
    assert_eq!(
        payload,
        serde_json::json!({
            "$nanograph": {
                "query": {
                    "name": "semantic_search",
                    "description": "Find semantically similar documents.",
                    "instruction": "Use for conceptual search. Prefer keyword_search for exact terms."
                }
            },
            "rows": [
                { "name": "Luke" }
            ]
        })
    );
}

#[test]
fn format_csv_escapes_values_and_prints_header_once() {
    let schema = Arc::new(Schema::new(vec![Field::new("note", DataType::Utf8, false)]));
    let batch_a = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(vec![Some("alpha,beta")])) as ArrayRef],
    )
    .unwrap();
    let batch_b = RecordBatch::try_new(
        schema,
        vec![Arc::new(StringArray::from(vec![
            Some("line 1\nline 2"),
            Some("quote \"here\""),
        ])) as ArrayRef],
    )
    .unwrap();

    assert_eq!(
        format_csv(&[batch_a, batch_b]),
        concat!(
            "note\n",
            "\"alpha,beta\"\n",
            "\"line 1\nline 2\"\n",
            "\"quote \"\"here\"\"\"\n",
        )
    );
}

#[test]
fn format_table_renders_headers_rows_and_empty_state() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("age", DataType::Int32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(vec![Some("Luke"), Some("Leia")])) as ArrayRef,
            Arc::new(Int32Array::from(vec![Some(23), Some(23)])) as ArrayRef,
        ],
    )
    .unwrap();

    let formatted = format_table(&[batch], 80, TableCellLayout::Truncate);
    assert!(formatted.contains("name"));
    assert!(formatted.contains("age"));
    assert!(formatted.contains("Luke"));
    assert!(formatted.contains("Leia"));

    assert_eq!(
        format_table(&[], 80, TableCellLayout::Truncate),
        "(empty result)\n"
    );
}

#[test]
fn format_table_truncates_and_flattens_long_text_cells() {
    let schema = Arc::new(Schema::new(vec![Field::new(
        "content",
        DataType::Utf8,
        false,
    )]));
    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::new(StringArray::from(vec![Some(
            "line 1\n\nline 2 with much more text than should fit in a compact table cell",
        )])) as ArrayRef],
    )
    .unwrap();

    let formatted = format_table(&[batch], 24, TableCellLayout::Truncate);
    assert!(formatted.contains("line 1 line 2 with mu..."));
    assert!(!formatted.contains("line 1\n\nline 2"));
}

#[test]
fn format_table_wrap_layout_preserves_wrapped_preview() {
    let schema = Arc::new(Schema::new(vec![Field::new(
        "content",
        DataType::Utf8,
        false,
    )]));
    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::new(StringArray::from(vec![Some(
            "line 1 line 2 with much more text than should wrap across rows",
        )])) as ArrayRef],
    )
    .unwrap();

    let formatted = format_table(&[batch], 18, TableCellLayout::Wrap);
    assert!(formatted.contains("line 1 line 2"));
    assert!(formatted.contains("much more text"));
    assert!(!formatted.contains("..."));
}

#[test]
fn format_elapsed_uses_ms_then_seconds() {
    assert_eq!(format_elapsed(std::time::Duration::from_millis(2)), "2ms");
    assert_eq!(
        format_elapsed(std::time::Duration::from_millis(1250)),
        "1.25s"
    );
}

#[tokio::test]
async fn run_mutation_insert_in_db_mode() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let query_path = dir.path().join("mut.gq");

    write_file(
        &schema_path,
        r#"node Person {
    name: String
    age: I32?
}"#,
    );
    write_file(
        &query_path,
        r#"
query add_person($name: String, $age: I32) {
    insert Person {
        name: $name
        age: $age
    }
}
"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_run(
        db_path.clone(),
        &ResolvedRunConfig {
            query_path: query_path.clone(),
            query_name: "add_person".to_string(),
            format: "table".to_string(),
            positional_param_names: Vec::new(),
        },
        80,
        TableCellLayout::Truncate,
        vec![
            ("name".to_string(), "Eve".to_string()),
            ("age".to_string(), "29".to_string()),
        ],
        false,
        false,
    )
    .await
    .unwrap();

    let rows = build_export_rows(&db_path, false, true).await.unwrap();
    assert!(rows.iter().any(|row| {
        row.get("type").and_then(serde_json::Value::as_str) == Some("Person")
            && row["data"]["name"].as_str() == Some("Eve")
    }));

    let cdc_rows = read_visible_cdc_entries(&db_path, 0, None).unwrap();
    assert_eq!(cdc_rows.len(), 1);
    assert_eq!(cdc_rows[0].op, "insert");
    assert_eq!(cdc_rows[0].type_name, "Person");
}

#[tokio::test]
async fn maintenance_commands_work_on_real_db() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");

    write_file(
        &schema_path,
        r#"node Person {
    name: String @key
}"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"name":"Alice"}}
{"type":"Person","data":{"name":"Bob"}}"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    cmd_compact(&db_path, 1_024, true, 0.1, false, false)
        .await
        .unwrap();
    cmd_cleanup(&db_path, 1, 1, false, false).await.unwrap();
    cmd_cdc_materialize(&db_path, 0, true, false, false)
        .await
        .unwrap();
    cmd_doctor(&db_path, None, false, false, false)
        .await
        .unwrap();

    assert!(db_path.join("__cdc_analytics").exists());

    let db = Database::open(&db_path).await.unwrap();
    let report = db.doctor().await.unwrap();
    assert!(report.tx_rows <= 1);
}

#[tokio::test]
async fn version_describe_export_helpers_work_on_real_db() {
    let _guard = ENV_LOCK.lock().await;
    let previous_mock = std::env::var_os("NANOGRAPH_EMBEDDINGS_MOCK");
    unsafe { std::env::set_var("NANOGRAPH_EMBEDDINGS_MOCK", "1") };

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");

    write_file(
        &schema_path,
        r#"node Person {
    name: String @key
    summary: String
    embedding: Vector(3) @embed(summary)
}
edge Knows: Person -> Person"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"name":"Alice","summary":"Alpha","embedding":[1.0,0.0,0.0]}}
{"type":"Person","data":{"name":"Bob","summary":"Beta","embedding":[0.0,1.0,0.0]}}
{"edge":"Knows","from":"Alice","to":"Bob"}"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let version = build_version_payload(Some(&db_path)).unwrap();
    assert_eq!(
        version["db"]["db_version"].as_u64(),
        Some(1),
        "expected one committed load version"
    );
    let dataset_versions = version_dataset_entries(&version);
    assert_eq!(
        version["db"]["dataset_count"].as_u64(),
        Some(dataset_versions.len() as u64)
    );
    let user_datasets = dataset_versions
        .iter()
        .filter(|entry| matches!(entry["kind"].as_str(), Some("node" | "edge")))
        .count();
    assert_eq!(user_datasets, 2);

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let describe = build_describe_payload(&db_path, &metadata, None).unwrap();
    assert_eq!(describe["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(describe["edges"].as_array().unwrap().len(), 1);
    assert_eq!(describe["nodes"][0]["rows"].as_u64(), Some(2));
    assert_eq!(describe["edges"][0]["rows"].as_u64(), Some(1));
    assert_eq!(describe["nodes"][0]["key_property"].as_str(), Some("name"));
    assert_eq!(
        describe["edges"][0]["endpoint_keys"]["src"].as_str(),
        Some("name")
    );
    let embedding_prop = describe["nodes"][0]["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "embedding")
        .expect("embedding property present in describe payload");
    assert_eq!(embedding_prop["embed_source"].as_str(), Some("summary"));

    let rows = build_export_rows(&db_path, false, true).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .any(|row| row["type"] == "Person" && row["data"]["name"] == "Alice")
    );
    assert!(
        rows.iter()
            .any(|row| row["edge"] == "Knows" && row["from"] == "Alice" && row["to"] == "Bob")
    );

    match previous_mock {
        Some(previous) => unsafe { std::env::set_var("NANOGRAPH_EMBEDDINGS_MOCK", previous) },
        None => unsafe { std::env::remove_var("NANOGRAPH_EMBEDDINGS_MOCK") },
    }
}

#[tokio::test]
async fn cli_media_base64_load_embed_and_traversal_work_end_to_end() {
    let _guard = ENV_LOCK.lock().await;
    let previous_mock = std::env::var_os("NANOGRAPH_EMBEDDINGS_MOCK");
    unsafe { std::env::set_var("NANOGRAPH_EMBEDDINGS_MOCK", "1") };

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");

    write_file(
        &schema_path,
        r#"node PhotoAsset {
    slug: String @key
    uri: String @media_uri(mime)
    mime: String
    embedding: Vector(16) @embed(uri) @index
}

node Product {
    slug: String @key
    name: String
}

edge HasPhoto: Product -> PhotoAsset"#,
    );

    let space_base64 = "iVBORw0KGgpzcGFjZS1pbWFnZS1mb3ItY2xpLWJhc2U2NC1lMmU=";
    let beach_base64 = "iVBORw0KGgpiZWFjaC1pbWFnZS1mb3ItY2xpLWJhc2U2NC1lMmU=";
    write_file(
        &data_path,
        &format!(
            r#"{{"type":"PhotoAsset","data":{{"slug":"space","uri":"@base64:{space_base64}","embedding":[1.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0]}}}}
{{"type":"PhotoAsset","data":{{"slug":"beach","uri":"@base64:{beach_base64}","embedding":[0.0,1.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0]}}}}
{{"type":"Product","data":{{"slug":"rocket","name":"Rocket Poster"}}}}
{{"type":"Product","data":{{"slug":"sand","name":"Beach Poster"}}}}
{{"edge":"HasPhoto","from":"rocket","to":"space"}}
{{"edge":"HasPhoto","from":"sand","to":"beach"}}
"#
        ),
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();
    cmd_embed(
        &db_path,
        EmbedOptions {
            type_name: Some("PhotoAsset".to_string()),
            property: Some("embedding".to_string()),
            only_null: false,
            limit: None,
            reindex: true,
            dry_run: false,
        },
        false,
        false,
    )
    .await
    .unwrap();

    let db = Database::open(&db_path).await.unwrap();
    let image_rows = match db
        .run(
            r#"
query image_embedding($slug: String) {
    match { $img: PhotoAsset { slug: $slug } }
    return { $img.embedding as embedding, $img.mime as mime, $img.uri as uri }
}
"#,
            "image_embedding",
            &ParamMap::from([("slug".to_string(), Literal::String("space".to_string()))]),
        )
        .await
        .unwrap()
    {
        nanograph::RunResult::Query(rows) => rows.to_rust_json(),
        _ => panic!("expected query result"),
    };
    let image = image_rows
        .as_array()
        .unwrap()
        .first()
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(image["mime"].as_str(), Some("image/png"));
    assert!(
        image["uri"]
            .as_str()
            .unwrap()
            .starts_with("lanceblob://sha256/")
    );
    let embedding_values = image["embedding"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| Literal::Float(value.as_f64().unwrap()))
        .collect::<Vec<_>>();

    let result_rows = match db
        .run(
            r#"
query products_from_image_vector($vq: Vector(16)) {
    match {
        $product: Product
        $product hasPhoto $img
    }
    return { $product.slug as product, $img.slug as image }
    order { nearest($img.embedding, $vq) }
    limit 1
}
"#,
            "products_from_image_vector",
            &ParamMap::from([("vq".to_string(), Literal::List(embedding_values))]),
        )
        .await
        .unwrap()
    {
        nanograph::RunResult::Query(rows) => rows.to_rust_json(),
        _ => panic!("expected query result"),
    };
    let row = result_rows
        .as_array()
        .unwrap()
        .first()
        .unwrap()
        .as_object()
        .unwrap();
    assert_eq!(row["product"].as_str(), Some("rocket"));
    assert_eq!(row["image"].as_str(), Some("space"));

    match previous_mock {
        Some(value) => unsafe { std::env::set_var("NANOGRAPH_EMBEDDINGS_MOCK", value) },
        None => unsafe { std::env::remove_var("NANOGRAPH_EMBEDDINGS_MOCK") },
    }
}

#[tokio::test]
async fn describe_type_filter_and_metadata_fields_are_present() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");

    write_file(
        &schema_path,
        r#"node Task @description("Tracked work item") @instruction("Query by slug") {
    slug: String @key @description("Stable external identifier")
    title: String
}
edge DependsOn: Task -> Task @description("Hard dependency") @instruction("Use only for blockers")
"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let task = build_describe_payload(&db_path, &metadata, Some("Task")).unwrap();
    assert_eq!(task["nodes"].as_array().unwrap().len(), 1);
    assert!(task["edges"].as_array().unwrap().is_empty());
    assert_eq!(
        task["nodes"][0]["description"].as_str(),
        Some("Tracked work item")
    );
    assert_eq!(
        task["nodes"][0]["instruction"].as_str(),
        Some("Query by slug")
    );
    assert_eq!(
        task["nodes"][0]["properties"][0]["description"].as_str(),
        Some("Stable external identifier")
    );
    assert_eq!(
        task["nodes"][0]["outgoing_edges"][0]["name"].as_str(),
        Some("DependsOn")
    );

    let edge = build_describe_payload(&db_path, &metadata, Some("DependsOn")).unwrap();
    assert!(edge["nodes"].as_array().unwrap().is_empty());
    assert_eq!(edge["edges"].as_array().unwrap().len(), 1);
    assert_eq!(
        edge["edges"][0]["endpoint_keys"]["src"].as_str(),
        Some("slug")
    );
}

#[tokio::test]
async fn describe_uses_manifest_metadata_without_opening_datasets() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
}"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Person","data":{"slug":"bob","name":"Bob"}}"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let manifest = build_version_payload(Some(&db_path)).unwrap();
    let dataset_path = version_data_dataset_path(&manifest, "node", "Person");
    std::fs::remove_dir_all(db_path.join(dataset_path)).unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let describe = build_describe_payload(&db_path, &metadata, Some("Person")).unwrap();
    assert_eq!(describe["nodes"][0]["rows"].as_u64(), Some(2));
}

#[tokio::test]
async fn lint_uses_schema_file_without_opening_datasets() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");
    let query_path = dir.path().join("query.gq");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
}"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}"#,
    );
    write_file(
        &query_path,
        r#"
query people() {
    match { $p: Person }
    return { $p.slug }
}
"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let manifest = build_version_payload(Some(&db_path)).unwrap();
    let dataset_path = version_data_dataset_path(&manifest, "node", "Person");
    std::fs::remove_dir_all(db_path.join(dataset_path)).unwrap();

    cmd_lint(Some(db_path), &query_path, None, true, true)
        .await
        .unwrap();
}

#[tokio::test]
async fn changes_no_embeddings_still_requires_visible_cdc_history() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    summary: String
    embedding: Vector(3) @embed(summary)
}"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"slug":"alice","summary":"Alpha","embedding":[1.0,0.0,0.0]}}"#,
    );

    Database::init_with_generation(&db_path, &std::fs::read_to_string(&schema_path).unwrap(), StorageGeneration::V4Namespace)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let manifest = build_version_payload(Some(&db_path)).unwrap();
    let dataset_path = version_data_dataset_path(&manifest, "node", "Person");
    std::fs::remove_dir_all(db_path.join(dataset_path)).unwrap();

    let err = cmd_changes(
        &db_path,
        None,
        None,
        None,
        "json",
        true,
        true,
        VisibleCdcSource::Authoritative,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("lineage-backed CDC is incomplete")
            || err
                .to_string()
                .contains("historical table versions are unavailable")
    );
}

#[tokio::test]
async fn changes_lineage_shadow_matches_authoritative_on_v4_database() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");
    let merge_path = dir.path().join("merge.jsonl");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
    age: I32?
}
edge Knows: Person -> Person"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice","age":30}}
{"type":"Person","data":{"slug":"bob","name":"Bob","age":25}}
{"edge":"Knows","from":"alice","to":"bob"}"#,
    );
    write_file(
        &merge_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice","age":31}}"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &merge_path, LoadModeArg::Merge, false, false)
        .await
        .unwrap();

    let authoritative = read_visible_cdc_entries_with_source(
        &db_path,
        0,
        None,
        nanograph::store::txlog::VisibleCdcSource::Authoritative,
    )
    .unwrap();
    let lineage_shadow = read_visible_cdc_entries_with_source(
        &db_path,
        0,
        None,
        nanograph::store::txlog::VisibleCdcSource::LineageShadow,
    )
    .unwrap();
    assert_eq!(authoritative, lineage_shadow);

    cmd_changes(
        &db_path,
        None,
        None,
        None,
        "json",
        true,
        false,
        VisibleCdcSource::LineageShadow,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn simple_run_query_uses_lance_first_without_full_graph_open() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");
    let query_path = dir.path().join("query.gq");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
}
edge Knows: Person -> Person"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Person","data":{"slug":"bob","name":"Bob"}}
{"edge":"Knows","from":"alice","to":"bob"}"#,
    );
    write_file(
        &query_path,
        r#"
query people() {
    match { $p: Person }
    return { $p.name }
    order { $p.name asc }
}
"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let edge_dataset_path = metadata
        .manifest()
        .datasets
        .iter()
        .find(|entry| entry.kind == "edge")
        .map(|entry| entry.dataset_path.clone())
        .unwrap();
    std::fs::remove_dir_all(db_path.join(edge_dataset_path)).unwrap();

    let query_src = std::fs::read_to_string(&query_path).unwrap();
    let queries = parse_query_or_report(&query_path, &query_src).unwrap();
    let results = execute_run_query_batches(&db_path, &queries.queries[0], &ParamMap::new())
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    let names = results[0]
        .column_by_name("name")
        .unwrap()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(names.value(0), "Alice");
    assert_eq!(names.value(1), "Bob");
}

#[tokio::test]
async fn load_append_uses_sparse_restore_without_opening_unrelated_datasets() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let initial_path = dir.path().join("initial.jsonl");
    let append_path = dir.path().join("append.jsonl");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
}
node Company {
    slug: String @key
    name: String
}
edge Knows: Person -> Person
edge WorksAt: Person -> Company"#,
    );
    write_file(
        &initial_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Company","data":{"slug":"acme","name":"Acme"}}
{"edge":"Knows","from":"alice","to":"alice"}
{"edge":"WorksAt","from":"alice","to":"acme"}"#,
    );
    write_file(
        &append_path,
        r#"{"type":"Person","data":{"slug":"bob","name":"Bob"}}
{"edge":"WorksAt","from":"bob","to":"acme"}"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(
        &db_path,
        &initial_path,
        LoadModeArg::Overwrite,
        false,
        false,
    )
    .await
    .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let knows_dataset_path = metadata
        .manifest()
        .datasets
        .iter()
        .find(|entry| entry.kind == "edge" && entry.type_name == "Knows")
        .map(|entry| entry.dataset_path.clone())
        .unwrap();
    std::fs::remove_dir_all(db_path.join(knows_dataset_path)).unwrap();

    cmd_load(&db_path, &append_path, LoadModeArg::Append, false, false)
        .await
        .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let people = build_describe_payload(&db_path, &metadata, Some("Person")).unwrap();
    let works_at = build_describe_payload(&db_path, &metadata, Some("WorksAt")).unwrap();
    assert_eq!(people["nodes"][0]["rows"].as_u64(), Some(2));
    assert_eq!(works_at["edges"][0]["rows"].as_u64(), Some(2));
}

#[tokio::test]
async fn load_merge_uses_sparse_restore_without_opening_unrelated_datasets() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let initial_path = dir.path().join("initial.jsonl");
    let merge_path = dir.path().join("merge.jsonl");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
}
node Company {
    slug: String @key
    name: String
}
edge Knows: Person -> Person
edge WorksAt: Person -> Company"#,
    );
    write_file(
        &initial_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Company","data":{"slug":"acme","name":"Acme"}}
{"edge":"Knows","from":"alice","to":"alice"}
{"edge":"WorksAt","from":"alice","to":"acme"}"#,
    );
    write_file(
        &merge_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice Cooper"}}
{"type":"Person","data":{"slug":"bob","name":"Bob"}}"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(
        &db_path,
        &initial_path,
        LoadModeArg::Overwrite,
        false,
        false,
    )
    .await
    .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let works_at_dataset_path = metadata
        .manifest()
        .datasets
        .iter()
        .find(|entry| entry.kind == "edge" && entry.type_name == "WorksAt")
        .map(|entry| entry.dataset_path.clone())
        .unwrap();
    std::fs::remove_dir_all(db_path.join(works_at_dataset_path)).unwrap();

    cmd_load(&db_path, &merge_path, LoadModeArg::Merge, false, false)
        .await
        .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let people = build_describe_payload(&db_path, &metadata, Some("Person")).unwrap();
    assert_eq!(people["nodes"][0]["rows"].as_u64(), Some(2));
}

#[tokio::test]
async fn run_insert_node_uses_sparse_mutation_without_full_graph_open() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");
    let query_path = dir.path().join("insert.gq");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
}
node Company {
    slug: String @key
    name: String
}
edge WorksAt: Person -> Company"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Company","data":{"slug":"acme","name":"Acme"}}
{"edge":"WorksAt","from":"alice","to":"acme"}"#,
    );
    write_file(
        &query_path,
        r#"
query add_person($slug: String, $name: String) {
    insert Person {
        slug: $slug
        name: $name
    }
}
"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let works_at_dataset_path = metadata
        .manifest()
        .datasets
        .iter()
        .find(|entry| entry.kind == "edge" && entry.type_name == "WorksAt")
        .map(|entry| entry.dataset_path.clone())
        .unwrap();
    std::fs::remove_dir_all(db_path.join(works_at_dataset_path)).unwrap();

    let query_src = std::fs::read_to_string(&query_path).unwrap();
    let queries = parse_query_or_report(&query_path, &query_src).unwrap();
    let results = execute_run_query_batches(
        &db_path,
        &queries.queries[0],
        &ParamMap::from([
            ("slug".to_string(), Literal::String("bob".to_string())),
            ("name".to_string(), Literal::String("Bob".to_string())),
        ]),
    )
    .await
    .unwrap();
    let affected_nodes = results[0]
        .column_by_name("affected_nodes")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    assert_eq!(affected_nodes.value(0), 1);

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let people = build_describe_payload(&db_path, &metadata, Some("Person")).unwrap();
    assert_eq!(people["nodes"][0]["rows"].as_u64(), Some(2));
}

#[tokio::test]
async fn run_update_node_uses_sparse_mutation_without_full_graph_open() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");
    let query_path = dir.path().join("update.gq");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
}
node Company {
    slug: String @key
    name: String
}
edge WorksAt: Person -> Company"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Company","data":{"slug":"acme","name":"Acme"}}
{"edge":"WorksAt","from":"alice","to":"acme"}"#,
    );
    write_file(
        &query_path,
        r#"
query rename_person($slug: String, $name: String) {
    update Person set {
        name: $name
    } where slug = $slug
}
"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let works_at_dataset_path = metadata
        .manifest()
        .datasets
        .iter()
        .find(|entry| entry.kind == "edge" && entry.type_name == "WorksAt")
        .map(|entry| entry.dataset_path.clone())
        .unwrap();
    std::fs::remove_dir_all(db_path.join(works_at_dataset_path)).unwrap();

    let query_src = std::fs::read_to_string(&query_path).unwrap();
    let queries = parse_query_or_report(&query_path, &query_src).unwrap();
    let results = execute_run_query_batches(
        &db_path,
        &queries.queries[0],
        &ParamMap::from([
            ("slug".to_string(), Literal::String("alice".to_string())),
            (
                "name".to_string(),
                Literal::String("Alice Cooper".to_string()),
            ),
        ]),
    )
    .await
    .unwrap();
    let affected_nodes = results[0]
        .column_by_name("affected_nodes")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    assert_eq!(affected_nodes.value(0), 1);

    let change_rows = read_visible_change_rows(&db_path, 1, None).unwrap();
    let update_row = change_rows.last().unwrap();
    assert_eq!(update_row.change_kind, "update");
    assert_eq!(update_row.type_name, "Person");
    assert_eq!(update_row.row["name"], "Alice Cooper");
}

#[tokio::test]
async fn run_insert_edge_uses_sparse_mutation_without_full_graph_open() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");
    let query_path = dir.path().join("insert-edge.gq");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
}
node Company {
    slug: String @key
    name: String
}
edge Knows: Person -> Person
edge WorksAt: Person -> Company"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Person","data":{"slug":"bob","name":"Bob"}}
{"type":"Company","data":{"slug":"acme","name":"Acme"}}
{"edge":"Knows","from":"alice","to":"bob"}
{"edge":"WorksAt","from":"alice","to":"acme"}"#,
    );
    write_file(
        &query_path,
        r#"
query add_work($from: String, $to: String) {
    insert WorksAt {
        from: $from
        to: $to
    }
}
"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let knows_dataset_path = metadata
        .manifest()
        .datasets
        .iter()
        .find(|entry| entry.kind == "edge" && entry.type_name == "Knows")
        .map(|entry| entry.dataset_path.clone())
        .unwrap();
    std::fs::remove_dir_all(db_path.join(knows_dataset_path)).unwrap();

    let query_src = std::fs::read_to_string(&query_path).unwrap();
    let queries = parse_query_or_report(&query_path, &query_src).unwrap();
    let results = execute_run_query_batches(
        &db_path,
        &queries.queries[0],
        &ParamMap::from([
            ("from".to_string(), Literal::String("bob".to_string())),
            ("to".to_string(), Literal::String("acme".to_string())),
        ]),
    )
    .await
    .unwrap();
    let affected_edges = results[0]
        .column_by_name("affected_edges")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    assert_eq!(affected_edges.value(0), 1);

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let works_at = build_describe_payload(&db_path, &metadata, Some("WorksAt")).unwrap();
    assert_eq!(works_at["edges"][0]["rows"].as_u64(), Some(2));
}

#[tokio::test]
async fn run_delete_edge_uses_sparse_mutation_without_full_graph_open() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");
    let query_path = dir.path().join("delete-edge.gq");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
}
node Company {
    slug: String @key
    name: String
}
edge Knows: Person -> Person
edge WorksAt: Person -> Company"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Person","data":{"slug":"bob","name":"Bob"}}
{"type":"Company","data":{"slug":"acme","name":"Acme"}}
{"edge":"Knows","from":"alice","to":"bob"}
{"edge":"WorksAt","from":"alice","to":"acme"}"#,
    );
    write_file(
        &query_path,
        r#"
query remove_work($from: String) {
    delete WorksAt where from = $from
}
"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let knows_dataset_path = metadata
        .manifest()
        .datasets
        .iter()
        .find(|entry| entry.kind == "edge" && entry.type_name == "Knows")
        .map(|entry| entry.dataset_path.clone())
        .unwrap();
    std::fs::remove_dir_all(db_path.join(knows_dataset_path)).unwrap();

    let query_src = std::fs::read_to_string(&query_path).unwrap();
    let queries = parse_query_or_report(&query_path, &query_src).unwrap();
    let results = execute_run_query_batches(
        &db_path,
        &queries.queries[0],
        &ParamMap::from([("from".to_string(), Literal::String("alice".to_string()))]),
    )
    .await
    .unwrap();
    let affected_edges = results[0]
        .column_by_name("affected_edges")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    assert_eq!(affected_edges.value(0), 1);

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let works_at = build_describe_payload(&db_path, &metadata, Some("WorksAt")).unwrap();
    assert_eq!(works_at["edges"][0]["rows"].as_u64(), Some(0));

    let change_rows = read_visible_change_rows(&db_path, 1, None).unwrap();
    let delete_row = change_rows.last().unwrap();
    assert_eq!(delete_row.change_kind, "delete");
    assert_eq!(delete_row.type_name, "WorksAt");
}

#[tokio::test]
async fn run_delete_edge_missing_endpoint_returns_zero_affected_without_error() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");
    let query_path = dir.path().join("delete-edge.gq");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
}
node Company {
    slug: String @key
    name: String
}
edge Knows: Person -> Person
edge WorksAt: Person -> Company"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Person","data":{"slug":"bob","name":"Bob"}}
{"type":"Company","data":{"slug":"acme","name":"Acme"}}
{"edge":"Knows","from":"alice","to":"bob"}
{"edge":"WorksAt","from":"alice","to":"acme"}"#,
    );
    write_file(
        &query_path,
        r#"
query remove_work($from: String) {
    delete WorksAt where from = $from
}
"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let knows_dataset_path = metadata
        .manifest()
        .datasets
        .iter()
        .find(|entry| entry.kind == "edge" && entry.type_name == "Knows")
        .map(|entry| entry.dataset_path.clone())
        .unwrap();
    std::fs::remove_dir_all(db_path.join(knows_dataset_path)).unwrap();

    let query_src = std::fs::read_to_string(&query_path).unwrap();
    let queries = parse_query_or_report(&query_path, &query_src).unwrap();
    let results = execute_run_query_batches(
        &db_path,
        &queries.queries[0],
        &ParamMap::from([("from".to_string(), Literal::String("nobody".to_string()))]),
    )
    .await
    .unwrap();
    let affected_edges = results[0]
        .column_by_name("affected_edges")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    assert_eq!(affected_edges.value(0), 0);

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let works_at = build_describe_payload(&db_path, &metadata, Some("WorksAt")).unwrap();
    assert_eq!(works_at["edges"][0]["rows"].as_u64(), Some(1));
}

#[tokio::test]
async fn run_delete_node_without_incident_edges_uses_sparse_mutation_without_full_graph_open() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let schema_path = dir.path().join("schema.pg");
    let data_path = dir.path().join("data.jsonl");
    let query_path = dir.path().join("delete-node.gq");

    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
    name: String
}
node Company {
    slug: String @key
    name: String
}
node Project {
    slug: String @key
    name: String
}
edge WorksAt: Person -> Company"#,
    );
    write_file(
        &data_path,
        r#"{"type":"Person","data":{"slug":"alice","name":"Alice"}}
{"type":"Company","data":{"slug":"acme","name":"Acme"}}
{"type":"Project","data":{"slug":"alpha","name":"Alpha"}}
{"edge":"WorksAt","from":"alice","to":"acme"}"#,
    );
    write_file(
        &query_path,
        r#"
query remove_project($slug: String) {
    delete Project where slug = $slug
}
"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let works_at_dataset_path = metadata
        .manifest()
        .datasets
        .iter()
        .find(|entry| entry.kind == "edge" && entry.type_name == "WorksAt")
        .map(|entry| entry.dataset_path.clone())
        .unwrap();
    std::fs::remove_dir_all(db_path.join(works_at_dataset_path)).unwrap();

    let query_src = std::fs::read_to_string(&query_path).unwrap();
    let queries = parse_query_or_report(&query_path, &query_src).unwrap();
    let results = execute_run_query_batches(
        &db_path,
        &queries.queries[0],
        &ParamMap::from([("slug".to_string(), Literal::String("alpha".to_string()))]),
    )
    .await
    .unwrap();
    let affected_nodes = results[0]
        .column_by_name("affected_nodes")
        .unwrap()
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    assert_eq!(affected_nodes.value(0), 1);

    let metadata = DatabaseMetadata::open(&db_path).unwrap();
    let projects = build_describe_payload(&db_path, &metadata, Some("Project")).unwrap();
    assert_eq!(projects["nodes"][0]["rows"].as_u64(), Some(0));

    let change_rows = read_visible_change_rows(&db_path, 1, None).unwrap();
    let delete_row = change_rows.last().unwrap();
    assert_eq!(delete_row.change_kind, "delete");
    assert_eq!(delete_row.type_name, "Project");
}

#[tokio::test]
async fn export_uses_key_properties_for_edge_endpoints_and_round_trips() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let roundtrip_db_path = dir.path().join("roundtrip-db");
    let schema_path = dir.path().join("schema.pg");
    let export_path = dir.path().join("export.jsonl");
    let data_path = dir.path().join("data.jsonl");

    write_file(
        &schema_path,
        r#"node ActionItem {
    slug: String @key
    title: String
}
node Person {
    slug: String @key
    name: String
}
edge MadeBy: ActionItem -> Person"#,
    );
    write_file(
        &data_path,
        r#"{"type":"ActionItem","data":{"slug":"dec-build-mcp","title":"Build MCP"}}
{"type":"Person","data":{"slug":"act-andrew","name":"Andrew"}}
{"edge":"MadeBy","from":"dec-build-mcp","to":"act-andrew"}"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let rows = build_export_rows(&db_path, false, true).await.unwrap();
    let edge = rows
        .iter()
        .find(|row| row["edge"] == "MadeBy")
        .expect("made by edge row");
    assert_eq!(edge["from"].as_str(), Some("dec-build-mcp"));
    assert_eq!(edge["to"].as_str(), Some("act-andrew"));
    assert!(edge.get("id").is_none());
    assert!(edge.get("src").is_none());
    assert!(edge.get("dst").is_none());

    let export_jsonl = rows
        .iter()
        .map(|row| serde_json::to_string(row).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    write_file(&export_path, &(export_jsonl + "\n"));

    cmd_init(&roundtrip_db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(
        &roundtrip_db_path,
        &export_path,
        LoadModeArg::Overwrite,
        false,
        false,
    )
    .await
    .unwrap();

    let roundtrip_rows = build_export_rows(&roundtrip_db_path, false, true)
        .await
        .unwrap();
    let roundtrip_edge = roundtrip_rows
        .iter()
        .find(|row| row["edge"] == "MadeBy")
        .expect("made by edge row after roundtrip");
    assert_eq!(roundtrip_edge["from"].as_str(), Some("dec-build-mcp"));
    assert_eq!(roundtrip_edge["to"].as_str(), Some("act-andrew"));
    assert!(roundtrip_edge.get("id").is_none());
    assert!(roundtrip_edge.get("src").is_none());
    assert!(roundtrip_edge.get("dst").is_none());
}

#[tokio::test]
async fn export_preserves_user_property_named_id_for_nodes_and_edges() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let roundtrip_db_path = dir.path().join("roundtrip-db");
    let schema_path = dir.path().join("schema.pg");
    let export_path = dir.path().join("export.jsonl");
    let data_path = dir.path().join("data.jsonl");

    write_file(
        &schema_path,
        r#"node User {
    id: String @key
    name: String
}
edge Follows: User -> User"#,
    );
    write_file(
        &data_path,
        r#"{"type":"User","data":{"id":"usr_01","name":"Alice"}}
{"type":"User","data":{"id":"usr_02","name":"Bob"}}
{"edge":"Follows","from":"usr_01","to":"usr_02"}"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false, false)
        .await
        .unwrap();

    let rows = build_export_rows(&db_path, false, true).await.unwrap();
    assert!(
        rows.iter()
            .any(|row| row["type"] == "User" && row["data"]["id"] == "usr_01")
    );
    assert!(
        rows.iter()
            .any(|row| row["type"] == "User" && row["data"]["id"] == "usr_02")
    );
    assert!(rows.iter().any(|row| {
        row["edge"] == "Follows" && row["from"] == "usr_01" && row["to"] == "usr_02"
    }));

    let export_jsonl = rows
        .iter()
        .map(|row| serde_json::to_string(row).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    write_file(&export_path, &(export_jsonl + "\n"));

    cmd_init(&roundtrip_db_path, &schema_path, false, false)
        .await
        .unwrap();
    cmd_load(
        &roundtrip_db_path,
        &export_path,
        LoadModeArg::Overwrite,
        false,
        false,
    )
    .await
    .unwrap();

    let roundtrip_rows = build_export_rows(&roundtrip_db_path, false, true)
        .await
        .unwrap();
    assert!(
        roundtrip_rows
            .iter()
            .any(|row| row["type"] == "User" && row["data"]["id"] == "usr_01")
    );
    assert!(roundtrip_rows.iter().any(|row| {
        row["edge"] == "Follows" && row["from"] == "usr_01" && row["to"] == "usr_02"
    }));
}

#[tokio::test]
async fn storage_migrate_rejects_already_v4_database() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db.nano");
    let schema_path = dir.path().join("schema.pg");
    write_file(
        &schema_path,
        r#"node Person {
    slug: String @key
}"#,
    );

    cmd_init(&db_path, &schema_path, false, false)
        .await
        .unwrap();

    let err = cmd_storage_migrate(&db_path, StorageTargetArg::LanceV4, false, true)
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("already uses NamespaceLineage storage")
    );
}
