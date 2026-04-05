mod common;

use common::{ExampleProject, ExampleWorkspace};

#[test]
fn revops_load_modes_and_standalone_delete_work() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    workspace.init();
    workspace.load();

    workspace.write_file(
        "append_patch.jsonl",
        "{\"type\":\"ActionItem\",\"data\":{\"slug\":\"ai-follow-up-renewal\",\"title\":\"Follow up on renewal risk\",\"description\":\"Reach out before the renewal window closes.\",\"status\":\"open\",\"priority\":\"high\",\"dueDate\":\"2026-02-20T10:00:00Z\",\"createdAt\":\"2026-02-16T09:00:00Z\",\"updatedAt\":\"2026-02-16T09:00:00Z\"}}\n",
    );
    let append = workspace.json_value(&[
        "--json",
        "load",
        "--data",
        "append_patch.jsonl",
        "--mode",
        "append",
    ]);
    assert_eq!(append["status"], "ok");

    let appended_task = workspace.json_rows(&[
        "run",
        "--query",
        "revops.gq",
        "--name",
        "task_lookup",
        "--format",
        "json",
        "--param",
        "slug=ai-follow-up-renewal",
    ]);
    assert_eq!(appended_task.len(), 1);
    assert_eq!(appended_task[0]["priority"], "high");

    workspace.write_file(
        "merge_patch.jsonl",
        "{\"type\":\"Opportunity\",\"data\":{\"slug\":\"opp-stripe-migration\",\"title\":\"Stripe Migration\",\"description\":\"Migration opportunity tied to vendor dissatisfaction.\",\"dealType\":\"net_new\",\"stage\":\"hold\",\"priority\":\"high\",\"risk\":\"medium\",\"amount\":25000.0,\"currency\":\"USD\",\"amountPaid\":0.0,\"expectedClose\":\"2026-03-01T00:00:00Z\",\"createdAt\":\"2026-02-01T09:00:00Z\",\"updatedAt\":\"2026-02-16T10:30:00Z\",\"notes\":[\"stage changed via merge mode\"]}}\n",
    );
    let merge = workspace.json_value(&[
        "--json",
        "load",
        "--data",
        "merge_patch.jsonl",
        "--mode",
        "merge",
    ]);
    assert_eq!(merge["status"], "ok");

    let opportunity = workspace.json_rows(&[
        "run",
        "--query",
        "revops.gq",
        "--name",
        "opportunity_lookup",
        "--format",
        "json",
        "--param",
        "slug=opp-stripe-migration",
    ]);
    assert_eq!(opportunity.len(), 1);
    assert_eq!(opportunity[0]["stage"], "hold");

    let tasks_before = workspace
        .json_rows(&[
            "run",
            "--query",
            "revops.gq",
            "--name",
            "task_lookup",
            "--format",
            "json",
            "--param",
            "slug=ai-draft-proposal",
        ])
        .len();
    assert_eq!(tasks_before, 1);

    let delete = workspace.json_value(&[
        "--json",
        "delete",
        "--type",
        "ActionItem",
        "--where",
        "slug=ai-draft-proposal",
    ]);
    assert_eq!(delete["status"], "ok");
    assert_eq!(delete["deleted_nodes"], 1);
    assert!(delete["deleted_edges"].as_u64().unwrap_or(0) > 0);

    let removed_task = workspace.json_rows(&[
        "run",
        "--query",
        "revops.gq",
        "--name",
        "task_lookup",
        "--format",
        "json",
        "--param",
        "slug=ai-draft-proposal",
    ]);
    assert!(removed_task.is_empty());
}

#[test]
fn load_accepts_db_flag_end_to_end() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();

    let load = workspace.json_value(&[
        "--json",
        "load",
        "--db",
        "starwars.nano",
        "--data",
        "starwars.jsonl",
        "--mode",
        "overwrite",
    ]);
    assert_eq!(load["status"], "ok");

    let changes = workspace.run_ok(&[
        "changes",
        "--db",
        "starwars.nano",
        "--since",
        "0",
        "--format",
        "jsonl",
    ]);
    assert!(changes.stdout.contains("\"graph_version\""));
    assert!(changes.stdout.contains("\"change_kind\""));
    assert!(!changes.stdout.contains("\"seq_in_tx\""));
}

#[test]
fn export_json_keeps_debug_internal_fields() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();

    let rows = workspace.json_rows(&["export", "--format", "json"]);
    assert!(rows.iter().any(|row| {
        row["type"] == "Character"
            && row["data"]["slug"] == "luke-skywalker"
            && row.get("id").is_some()
    }));
    assert!(rows.iter().any(|row| {
        row["edge"] == "HasMentor"
            && row.get("id").is_some()
            && row.get("src").is_some()
            && row.get("dst").is_some()
            && row["from"].is_string()
            && row["to"].is_string()
    }));
}
