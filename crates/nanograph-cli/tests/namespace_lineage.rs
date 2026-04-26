mod common;

use std::cmp::Ordering;

use common::{ExampleProject, ExampleWorkspace};
use nanograph::store::database::Database;
use nanograph::store::storage_generation::StorageGeneration;

fn db_version(workspace: &ExampleWorkspace) -> u64 {
    workspace.json_value(&["--json", "version"])["db"]["db_version"]
        .as_u64()
        .unwrap()
}

fn sort_key_compare(left: &serde_json::Value, right: &serde_json::Value) -> Ordering {
    left["graph_version"]
        .as_u64()
        .cmp(&right["graph_version"].as_u64())
        .then(
            left["entity_kind"]
                .as_str()
                .cmp(&right["entity_kind"].as_str()),
        )
        .then(left["type_name"].as_str().cmp(&right["type_name"].as_str()))
        .then(left["rowid"].as_u64().cmp(&right["rowid"].as_u64()))
        .then(
            left["logical_key"]
                .as_str()
                .cmp(&right["logical_key"].as_str()),
        )
        .then(
            left["change_kind"]
                .as_str()
                .cmp(&right["change_kind"].as_str()),
        )
}

#[test]
fn namespace_lineage_changes_have_full_shape_and_deterministic_ordering() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    workspace.init();
    workspace.load();

    let before_version = db_version(&workspace);
    workspace.write_file(
        "mixed_patch.jsonl",
        concat!(
            "{\"type\":\"Client\",\"data\":{\"slug\":\"cli-lineage-order\",\"name\":\"Lineage Order Client\",\"status\":\"focus\",\"tags\":[\"lineage\"],\"createdAt\":\"2026-03-01T09:00:00Z\",\"updatedAt\":\"2026-03-01T09:00:00Z\"}}\n",
            "{\"type\":\"Signal\",\"data\":{\"slug\":\"sig-lineage-order\",\"observedAt\":\"2026-03-01T09:05:00Z\",\"summary\":\"Lineage ordering smoke signal\",\"urgency\":\"high\",\"sourceType\":\"message\",\"assertion\":\"fact\",\"createdAt\":\"2026-03-01T09:05:00Z\"}}\n",
            "{\"edge\":\"SignalAffects\",\"from\":\"sig-lineage-order\",\"to\":\"cli-lineage-order\"}\n"
        ),
    );
    let load = workspace.json_value(&[
        "--json",
        "load",
        "--data",
        "mixed_patch.jsonl",
        "--mode",
        "append",
    ]);
    assert_eq!(load["status"], "ok");

    let graph_version = db_version(&workspace);
    let rows = workspace.json_rows(&[
        "changes",
        "--from",
        &graph_version.to_string(),
        "--to",
        &graph_version.to_string(),
        "--format",
        "json",
    ]);

    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.get("db_version").is_none()));
    assert!(rows.iter().all(|row| row.get("seq_in_tx").is_none()));
    assert!(
        rows.iter()
            .all(|row| row["graph_version"].as_u64() == Some(graph_version))
    );
    assert!(
        rows.iter()
            .all(|row| row["previous_graph_version"].as_u64() == Some(before_version))
    );
    assert!(rows.iter().all(|row| row["tx_id"].is_string()));
    assert!(rows.iter().all(|row| row["table_id"].is_string()));
    assert!(rows.iter().all(|row| row["rowid"].is_u64()));
    assert!(
        rows.iter()
            .all(|row| row["entity_id"].as_u64().unwrap_or(0) > 0)
    );
    assert!(rows.iter().all(|row| row["logical_key"].is_string()));
    assert!(rows.iter().all(|row| row["row"].is_object()));

    assert!(rows.iter().any(|row| {
        row["entity_kind"] == "node"
            && row["type_name"] == "Client"
            && row["change_kind"] == "insert"
            && row["row"]["slug"] == "cli-lineage-order"
    }));
    assert!(rows.iter().any(|row| {
        row["entity_kind"] == "node"
            && row["type_name"] == "Signal"
            && row["change_kind"] == "insert"
            && row["row"]["slug"] == "sig-lineage-order"
    }));
    assert!(rows.iter().any(|row| {
        row["entity_kind"] == "edge"
            && row["type_name"] == "SignalAffects"
            && row["change_kind"] == "insert"
            && row["row"]["src"].as_u64().unwrap_or(0) > 0
            && row["row"]["dst"].as_u64().unwrap_or(0) > 0
    }));

    let mut sorted = rows.clone();
    sorted.sort_by(sort_key_compare);
    assert_eq!(rows, sorted);
}

#[test]
fn namespace_lineage_delete_rows_keep_tombstone_payloads() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    workspace.init();
    workspace.load();

    workspace.write_file(
        "delete_flow.gq",
        r#"query set_task_in_progress() {
    update ActionItem set {
        status: "in_progress"
        updatedAt: datetime("2026-03-02T12:00:00Z")
    } where slug = "ai-draft-proposal"
}

query delete_task() {
    delete ActionItem where slug = "ai-draft-proposal"
}
"#,
    );

    let update = workspace.jsonl_rows(&[
        "run",
        "--query",
        "delete_flow.gq",
        "--name",
        "set_task_in_progress",
        "--format",
        "jsonl",
    ]);
    assert_eq!(update.len(), 1);
    let update_version = db_version(&workspace);

    let delete = workspace.jsonl_rows(&[
        "run",
        "--query",
        "delete_flow.gq",
        "--name",
        "delete_task",
        "--format",
        "jsonl",
    ]);
    assert_eq!(delete.len(), 1);
    let delete_version = db_version(&workspace);

    let rows = workspace.json_rows(&[
        "changes",
        "--from",
        &delete_version.to_string(),
        "--to",
        &delete_version.to_string(),
        "--format",
        "json",
    ]);

    let node_delete = rows
        .iter()
        .find(|row| {
            row["change_kind"] == "delete"
                && row["entity_kind"] == "node"
                && row["type_name"] == "ActionItem"
        })
        .unwrap();
    assert_eq!(node_delete["graph_version"].as_u64(), Some(delete_version));
    assert_eq!(
        node_delete["previous_graph_version"].as_u64(),
        Some(update_version)
    );
    assert!(node_delete["rowid"].is_u64());
    assert!(node_delete["entity_id"].as_u64().unwrap_or(0) > 0);
    assert!(node_delete["table_id"].is_string());
    assert_eq!(node_delete["row"]["slug"], "ai-draft-proposal");
    assert_eq!(node_delete["row"]["status"], "in_progress");
    assert_eq!(node_delete["row"]["title"], "Draft Proposal");

    assert!(rows.iter().any(|row| {
        row["change_kind"] == "delete"
            && row["entity_kind"] == "edge"
            && row["rowid"].is_u64()
            && row["row"].is_object()
    }));
}

#[test]
fn namespace_lineage_cleanup_preserves_retained_changes_window() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    workspace.init();
    workspace.load();

    workspace.write_file(
        "append_signal.jsonl",
        "{\"type\":\"Signal\",\"data\":{\"slug\":\"sig-cleanup-retained\",\"observedAt\":\"2026-03-03T09:00:00Z\",\"summary\":\"Cleanup retained signal\",\"urgency\":\"medium\",\"sourceType\":\"message\",\"assertion\":\"fact\",\"createdAt\":\"2026-03-03T09:00:00Z\"}}\n",
    );
    let load_append = workspace.json_value(&[
        "--json",
        "load",
        "--data",
        "append_signal.jsonl",
        "--mode",
        "append",
    ]);
    assert_eq!(load_append["status"], "ok");
    let append_version = db_version(&workspace);

    workspace.write_file(
        "merge_opportunity.jsonl",
        "{\"type\":\"Opportunity\",\"data\":{\"slug\":\"opp-stripe-migration\",\"title\":\"Stripe Migration\",\"description\":\"Migration opportunity tied to vendor dissatisfaction.\",\"dealType\":\"net_new\",\"stage\":\"hold\",\"priority\":\"high\",\"risk\":\"medium\",\"amount\":25000.0,\"currency\":\"USD\",\"amountPaid\":0.0,\"expectedClose\":\"2026-03-01T00:00:00Z\",\"createdAt\":\"2026-02-01T09:00:00Z\",\"updatedAt\":\"2026-03-03T10:00:00Z\",\"notes\":[\"cleanup retained window\"]}}\n",
    );
    let load_merge = workspace.json_value(&[
        "--json",
        "load",
        "--data",
        "merge_opportunity.jsonl",
        "--mode",
        "merge",
    ]);
    assert_eq!(load_merge["status"], "ok");
    let merge_version = db_version(&workspace);

    workspace.write_file(
        "delete_cleanup.gq",
        r#"query delete_task() {
    delete ActionItem where slug = "ai-draft-proposal"
}
"#,
    );
    let delete = workspace.jsonl_rows(&[
        "run",
        "--query",
        "delete_cleanup.gq",
        "--name",
        "delete_task",
        "--format",
        "jsonl",
    ]);
    assert_eq!(delete.len(), 1);
    let delete_version = db_version(&workspace);

    let cleanup = workspace.json_value(&[
        "--json",
        "cleanup",
        "--retain-tx-versions",
        "2",
        "--retain-dataset-versions",
        "1",
    ]);
    assert_eq!(cleanup["status"], "ok");

    let rows = workspace.json_rows(&[
        "changes",
        "--from",
        &append_version.to_string(),
        "--to",
        &delete_version.to_string(),
        "--format",
        "json",
    ]);
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .all(|row| row["graph_version"].as_u64() != Some(append_version))
    );
    assert!(
        rows.iter()
            .any(|row| row["graph_version"].as_u64() == Some(merge_version))
    );
    assert!(
        rows.iter()
            .any(|row| row["graph_version"].as_u64() == Some(delete_version))
    );
}

#[test]
fn storage_migrate_to_lineage_native_works_end_to_end() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    let schema_source = workspace.read_file("revops.pg");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime
        .block_on(Database::init_with_generation(
            &workspace.db_path(),
            &schema_source,
            StorageGeneration::V4Namespace,
        ))
        .unwrap();

    let load = workspace.json_value(&[
        "--json",
        "load",
        "--db",
        "omni.nano",
        "--data",
        "revops.jsonl",
        "--mode",
        "overwrite",
    ]);
    assert_eq!(load["status"], "ok");

    let migrate = workspace.json_value(&[
        "--json",
        "storage",
        "migrate",
        "--db",
        "omni.nano",
        "--target",
        "lineage-native",
    ]);
    assert_eq!(migrate["status"], "ok");
    assert_eq!(migrate["target"], "lineage-native");
    assert_eq!(migrate["graph_version"].as_u64(), Some(1));
    assert!(workspace.file("omni.nano.v3-backup").exists());
    assert_eq!(
        workspace
            .read_file("omni.nano/storage.generation.json")
            .contains("\"namespace-lineage\""),
        true
    );

    let version = workspace.json_value(&["--json", "version"]);
    assert_eq!(version["db"]["db_version"].as_u64(), Some(1));

    let doctor = workspace.json_value(&["--json", "doctor"]);
    assert_eq!(doctor["status"], "ok");
    assert_eq!(doctor["healthy"], true);

    let changes = workspace.json_rows(&[
        "changes",
        "--db",
        "omni.nano",
        "--since",
        "0",
        "--format",
        "json",
    ]);
    assert!(!changes.is_empty());
    assert!(
        changes
            .iter()
            .all(|row| row["graph_version"].as_u64() == Some(1))
    );
    assert!(changes.iter().all(|row| row.get("db_version").is_none()));
    assert!(changes.iter().all(|row| row["change_kind"] == "insert"));
}
