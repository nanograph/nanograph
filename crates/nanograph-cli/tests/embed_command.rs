mod common;

use common::{ExampleProject, ExampleWorkspace};
use serde_json::json;

fn embed_schema() -> &'static str {
    r#"
node Note {
    slug: String @key
    kind: String
    title: String
    body: String
    title_embedding: Vector(3)? @embed(title)
    body_embedding: Vector(3)? @embed(body)
}
"#
}

fn plain_embed_schema() -> &'static str {
    r#"
node Note {
    slug: String @key
    kind: String
    title: String
    body: String
    title_embedding: Vector(3)?
    body_embedding: Vector(3)?
}
"#
}

fn embed_indexed_schema() -> &'static str {
    r#"
node Note {
    slug: String @key
    kind: String
    title: String
    body: String
    title_embedding: Vector(3)? @embed(title)
    body_embedding: Vector(3)? @embed(body) @index
}
"#
}

fn plain_indexed_schema() -> &'static str {
    r#"
node Note {
    slug: String @key
    kind: String
    title: String
    body: String
    title_embedding: Vector(3)?
    body_embedding: Vector(3)? @index
}
"#
}

fn embed_data() -> &'static str {
    r#"
{"type":"Note","data":{"slug":"a","kind":"memo","title":"Alpha","body":"First body","title_embedding":[0.0,0.0,0.0]}}
{"type":"Note","data":{"slug":"b","kind":"memo","title":"Beta","body":"Second body","title_embedding":[9.0,9.0,9.0]}}
"#
}

fn indexed_embed_data() -> &'static str {
    r#"
{"type":"Note","data":{"slug":"a","kind":"memo","title":"Alpha","body":"First body","title_embedding":[0.0,0.0,0.0],"body_embedding":[1.0,1.0,1.0]}}
{"type":"Note","data":{"slug":"b","kind":"memo","title":"Beta","body":"Second body","title_embedding":[9.0,9.0,9.0],"body_embedding":[2.0,2.0,2.0]}}
"#
}

fn embed_query() -> &'static str {
    r#"
query all_notes() {
    match { $n: Note }
    return { $n.slug, $n.title_embedding, $n.body_embedding }
    order { $n.slug asc }
}
"#
}

fn write_embed_fixture(workspace: &ExampleWorkspace) {
    workspace.write_file(".env.nano", "NANOGRAPH_EMBEDDINGS_MOCK=1\n");
    workspace.write_file("plain.pg", plain_embed_schema());
    workspace.write_file("embed.pg", embed_schema());
    workspace.write_file("embed.jsonl", embed_data());
    workspace.write_file("embed.gq", embed_query());
}

fn init_embed_db(workspace: &ExampleWorkspace) {
    write_embed_fixture(workspace);
    let init = workspace.json_value(&["--json", "init", "embed.nano", "--schema", "plain.pg"]);
    assert_eq!(init["status"], "ok");
    let load = workspace.json_value(&[
        "--json",
        "load",
        "embed.nano",
        "--data",
        "embed.jsonl",
        "--mode",
        "overwrite",
    ]);
    assert_eq!(load["status"], "ok");
    workspace.write_file("embed.nano/schema.pg", embed_schema());
    let migrate = workspace.run_ok(&["migrate", "embed.nano", "--auto-approve"]);
    assert!(migrate.stdout.contains("No migration steps."));
}

fn write_reindex_fixture(workspace: &ExampleWorkspace) {
    workspace.write_file(".env.nano", "NANOGRAPH_EMBEDDINGS_MOCK=1\n");
    workspace.write_file("plain-indexed.pg", plain_indexed_schema());
    workspace.write_file("embed-indexed.pg", embed_indexed_schema());
    workspace.write_file("embed-indexed.jsonl", indexed_embed_data());
}

fn init_reindex_db(workspace: &ExampleWorkspace) {
    write_reindex_fixture(workspace);
    let init = workspace.json_value(&[
        "--json",
        "init",
        "embed.nano",
        "--schema",
        "plain-indexed.pg",
    ]);
    assert_eq!(init["status"], "ok");
    let load = workspace.json_value(&[
        "--json",
        "load",
        "embed.nano",
        "--data",
        "embed-indexed.jsonl",
        "--mode",
        "overwrite",
    ]);
    assert_eq!(load["status"], "ok");
    workspace.write_file("embed.nano/schema.pg", embed_indexed_schema());
    let migrate = workspace.run_ok(&["migrate", "embed.nano", "--auto-approve"]);
    assert!(migrate.stdout.contains("No migration steps."));
}

#[test]
fn embed_dry_run_scoped_property_limit_does_not_write() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    init_embed_db(&workspace);

    let result = workspace.json_value(&[
        "--json",
        "embed",
        "--db",
        "embed.nano",
        "--type",
        "Note",
        "--property",
        "title_embedding",
        "--limit",
        "1",
        "--dry-run",
    ]);
    assert_eq!(result["status"], "ok");
    assert_eq!(result["dry_run"], true);
    assert_eq!(result["type_name"], "Note");
    assert_eq!(result["property"], "title_embedding");
    assert_eq!(result["rows_selected"], 1);
    assert_eq!(result["embeddings_generated"], 1);
    assert_eq!(result["reindexed_types"], 0);

    let rows = workspace.json_rows(&[
        "--json",
        "run",
        "--db",
        "embed.nano",
        "--query",
        "embed.gq",
        "--name",
        "all_notes",
    ]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["slug"], "a");
    assert_eq!(rows[0]["title_embedding"], json!([0.0, 0.0, 0.0]));
    assert_eq!(rows[0]["body_embedding"], serde_json::Value::Null);
    assert_eq!(rows[1]["slug"], "b");
    assert_eq!(rows[1]["title_embedding"], json!([9.0, 9.0, 9.0]));
    assert_eq!(rows[1]["body_embedding"], serde_json::Value::Null);
}

#[test]
fn embed_only_null_backfills_missing_vectors_and_preserves_existing_values() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    init_embed_db(&workspace);

    let result = workspace.json_value(&["--json", "embed", "--db", "embed.nano", "--only-null"]);
    assert_eq!(result["status"], "ok");
    assert_eq!(result["dry_run"], false);
    assert_eq!(result["rows_selected"], 2);
    assert_eq!(result["embeddings_generated"], 2);

    let rows = workspace.json_rows(&[
        "--json",
        "run",
        "--db",
        "embed.nano",
        "--query",
        "embed.gq",
        "--name",
        "all_notes",
    ]);
    assert_eq!(rows.len(), 2);

    let first = &rows[0];
    assert_eq!(first["slug"], "a");
    assert_eq!(first["title_embedding"], json!([0.0, 0.0, 0.0]));
    assert_eq!(first["body_embedding"].as_array().unwrap().len(), 3);

    let second = &rows[1];
    assert_eq!(second["slug"], "b");
    assert_eq!(second["title_embedding"], json!([9.0, 9.0, 9.0]));
    assert_eq!(second["body_embedding"].as_array().unwrap().len(), 3);
}

#[test]
fn embed_reindex_rebuilds_index_when_no_rows_need_updates() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    init_reindex_db(&workspace);

    let reindex = workspace.json_value(&[
        "--json",
        "embed",
        "--db",
        "embed.nano",
        "--type",
        "Note",
        "--only-null",
        "--reindex",
    ]);
    assert_eq!(reindex["status"], "ok");
    assert_eq!(reindex["rows_selected"], 0);
    assert_eq!(reindex["embeddings_generated"], 0);
    assert_eq!(reindex["reindexed_types"], 1);
}
