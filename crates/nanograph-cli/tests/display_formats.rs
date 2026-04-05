mod common;

use common::{ExampleProject, ExampleWorkspace, parse_json_value, parse_jsonl_rows};
use serde_json::Value;

fn override_cli_layout(workspace: &ExampleWorkspace, width: usize, layout: &str) {
    let mut updated = Vec::new();
    let mut in_cli = false;
    let mut saw_width = false;
    let mut saw_layout = false;

    for line in workspace.read_file("nanograph.toml").lines() {
        if line.starts_with('[') {
            if in_cli {
                if !saw_width {
                    updated.push(format!("table_max_column_width = {}", width));
                }
                if !saw_layout {
                    updated.push(format!("table_cell_layout = \"{}\"", layout));
                }
            }
            in_cli = line.trim() == "[cli]";
            updated.push(line.to_string());
            continue;
        }

        if in_cli && line.starts_with("table_max_column_width") {
            updated.push(format!("table_max_column_width = {}", width));
            saw_width = true;
            continue;
        }

        if in_cli && line.starts_with("table_cell_layout") {
            updated.push(format!("table_cell_layout = \"{}\"", layout));
            saw_layout = true;
            continue;
        }

        updated.push(line.to_string());
    }

    if in_cli {
        if !saw_width {
            updated.push(format!("table_max_column_width = {}", width));
        }
        if !saw_layout {
            updated.push(format!("table_cell_layout = \"{}\"", layout));
        }
    }

    let updated = updated.join("\n") + "\n";
    workspace.write_file("nanograph.toml", &updated);
}

fn override_output_format(workspace: &ExampleWorkspace, format: &str) {
    let mut updated = Vec::new();
    let mut in_cli = false;
    let mut saw_output_format = false;

    for line in workspace.read_file("nanograph.toml").lines() {
        if line.starts_with('[') {
            if in_cli && !saw_output_format {
                updated.push(format!("output_format = \"{}\"", format));
            }
            in_cli = line.trim() == "[cli]";
            updated.push(line.to_string());
            continue;
        }

        if in_cli && line.starts_with("output_format") {
            updated.push(format!("output_format = \"{}\"", format));
            saw_output_format = true;
            continue;
        }

        updated.push(line.to_string());
    }

    if in_cli && !saw_output_format {
        updated.push(format!("output_format = \"{}\"", format));
    }

    let updated = updated.join("\n") + "\n";
    workspace.write_file("nanograph.toml", &updated);
}

fn first_nonempty_lines(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect()
}

#[test]
fn kv_output_groups_rows_with_headers_and_dividers() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();

    let output = workspace
        .run_ok(&["run", "search", "father and son conflict", "--format", "kv"])
        .stdout;

    assert!(output.contains("Query: semantic_search"));
    assert!(output.contains("Description: Rank characters by semantic similarity"));
    assert!(output.contains("Instruction: Use for broad conceptual search"));
    assert!(output.contains("Row 1: "));
    assert!(output.contains("slug : "));
    assert!(output.contains("name : "));
    assert!(output.contains("score: "));
    assert!(output.contains("────────────────────────"));
    assert!(output.matches("Row ").count() >= 2);
}

#[test]
fn table_output_respects_truncate_and_wrap_layout_config() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();

    override_cli_layout(&workspace, 20, "truncate");
    let truncated = workspace
        .run_ok(&[
            "run",
            "--query",
            "starwars.gq",
            "--name",
            "character_profile",
            "--param",
            "slug=luke-skywalker",
            "--format",
            "table",
        ])
        .stdout;

    assert!(truncated.contains("Luke Skywalker"));
    assert!(truncated.contains("..."));
    assert!(truncated.contains("(1 rows, "));
    assert!(
        !truncated.contains("Son of Anakin, destroyed the Death Star and redeemed his father.")
    );

    override_cli_layout(&workspace, 20, "wrap");
    let wrapped = workspace
        .run_ok(&[
            "run",
            "--query",
            "starwars.gq",
            "--name",
            "character_profile",
            "--param",
            "slug=luke-skywalker",
            "--format",
            "table",
        ])
        .stdout;

    assert!(wrapped.contains("Luke Skywalker"));
    assert!(wrapped.contains("(1 rows, "));
    assert!(!wrapped.contains("..."));
}

#[test]
fn json_run_output_wraps_rows_with_query_metadata() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    workspace.init();
    workspace.load();

    let value = workspace.json_value(&["run", "why", "opp-stripe-migration", "--format", "json"]);

    assert_eq!(value["$nanograph"]["query"]["name"], "decision_trace");
    assert_eq!(
        value["$nanograph"]["query"]["description"],
        "Trace an opportunity back to the decision, actor, and signal that created motion."
    );
    assert_eq!(
        value["$nanograph"]["query"]["instruction"],
        "Use as the default 'why did this happen?' query for an opportunity slug."
    );
    assert_eq!(value["rows"].as_array().unwrap().len(), 1);
}

#[test]
fn jsonl_run_output_emits_metadata_header_then_rows() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    workspace.init();
    workspace.load();

    let output = workspace
        .run_ok(&["run", "why", "opp-stripe-migration", "--format", "jsonl"])
        .stdout;
    let lines = first_nonempty_lines(&output);
    assert!(lines.len() >= 2);

    let header = parse_json_value(lines[0]);
    assert_eq!(header["$nanograph"]["query"]["name"], "decision_trace");
    assert_eq!(
        header["$nanograph"]["query"]["description"],
        "Trace an opportunity back to the decision, actor, and signal that created motion."
    );

    let row: Value = serde_json::from_str(lines[1]).unwrap();
    assert!(row.get("title").is_some());
    assert!(row.get("intent").is_some());
}

#[test]
fn csv_run_output_escapes_fields_and_stays_row_only() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();

    let output = workspace
        .run_ok(&[
            "run",
            "--query",
            "starwars.gq",
            "--name",
            "character_profile",
            "--param",
            "slug=luke-skywalker",
            "--format",
            "csv",
        ])
        .stdout;

    assert!(output.starts_with("slug,name,note,alignment,tags\n"));
    assert_eq!(output.matches("slug,name,note,alignment,tags").count(), 1);
    assert!(
        output.contains("\"Son of Anakin, destroyed the Death Star and redeemed his father.\"")
    );
    assert!(!output.contains("Query:"));
    assert!(!output.contains("Description:"));
    assert!(!output.contains("Instruction:"));
}

#[test]
fn export_and_changes_fall_back_when_global_output_format_is_unsupported() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();
    override_output_format(&workspace, "table");

    let exported = workspace.run_ok(&["export"]).stdout;
    let export_rows = parse_jsonl_rows(&exported);
    assert!(!export_rows.is_empty());
    assert!(export_rows.iter().any(|row| row["type"] == "Character"));

    let changes = workspace.run_ok(&["changes"]).stdout;
    let change_rows = parse_jsonl_rows(&changes);
    assert!(!change_rows.is_empty());
    assert!(
        change_rows
            .iter()
            .all(|row| row.get("graph_version").is_some() && row.get("change_kind").is_some())
    );
}

#[test]
fn export_and_changes_can_omit_embedding_vectors() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();

    let exported = workspace.run_ok(&["export", "--format", "jsonl"]).stdout;
    assert!(exported.contains("\"embedding\""));

    let exported_without_embeddings = workspace
        .run_ok(&["export", "--format", "jsonl", "--no-embeddings"])
        .stdout;
    assert!(!exported_without_embeddings.contains("\"embedding\""));

    let changes = workspace.run_ok(&["changes", "--format", "json"]).stdout;
    assert!(changes.contains("\"embedding\""));

    let changes_without_embeddings = workspace
        .run_ok(&["changes", "--format", "json", "--no-embeddings"])
        .stdout;
    assert!(!changes_without_embeddings.contains("\"embedding\""));
}

#[test]
fn run_json_failure_emits_json_to_stdout_only() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();

    let result = workspace.run_fail(&[
        "--json",
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "nonexistent",
    ]);

    let value = parse_json_value(&result.stdout);
    assert_eq!(value["status"], "error");
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("query `nonexistent` not found")
    );
    assert!(result.stderr.trim().is_empty());
}

#[test]
fn load_json_failure_emits_json_to_stdout_only() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();
    workspace.write_file("broken.jsonl", "{\"not valid json\n");

    let result = workspace.run_fail(&[
        "--json",
        "load",
        "--data",
        "broken.jsonl",
        "--mode",
        "append",
    ]);

    let value = parse_json_value(&result.stdout);
    assert_eq!(value["status"], "error");
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("JSON parse error")
    );
    assert!(result.stderr.trim().is_empty());
}

#[test]
fn check_json_failure_emits_summary_without_stderr_leak() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    workspace.init();
    workspace.load();
    workspace.write_file(
        "invalid_enum.gq",
        r#"query add_invalid_signal() {
    insert Signal {
        slug: "sig-invalid-enum"
        summary: "Invalid enum payload"
        observedAt: datetime("2026-02-15T18:00:00Z")
        urgency: "urgent"
        sourceType: "email"
        assertion: "fact"
        createdAt: datetime("2026-02-15T18:00:00Z")
    }
}"#,
    );

    let result = workspace.run_fail(&["--json", "check", "--query", "invalid_enum.gq"]);
    let value = parse_json_value(&result.stdout);
    assert_eq!(value["status"], "error");
    assert_eq!(value["errors"], 1);
    assert!(
        value["results"][0]["error"]
            .as_str()
            .unwrap()
            .contains("expects one of")
    );
    assert!(result.stderr.trim().is_empty());
}
