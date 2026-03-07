mod common;

use common::{ExampleProject, ExampleWorkspace, scalar_string};

#[test]
fn revops_admin_cdc_and_describe_workflows_work() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    workspace.init();
    workspace.load();

    workspace.write_file(
        "admin_mutations.gq",
        r#"query signal_rows($slug: String) {
    match { $s: Signal { slug: $slug } }
    return { $s.slug, $s.urgency, $s.observedAt }
}

query opportunity_stage() {
    match { $o: Opportunity { slug: "opp-stripe-migration" } }
    return { $o.stage }
}

query task_rows() {
    match { $t: ActionItem { slug: "ai-draft-proposal" } }
    return { $t.slug, $t.status }
}

query insert_test_signal() {
    insert Signal {
        slug: "sig-test-renewal"
        observedAt: datetime("2026-02-15T15:00:00Z")
        summary: "Renewal risk identified during support exchange"
        urgency: "critical"
        sourceType: "message"
        assertion: "fact"
        createdAt: datetime("2026-02-15T15:00:00Z")
    }
}

query update_stage_to_hold() {
    update Opportunity set {
        stage: "hold"
        updatedAt: datetime("2026-02-15T15:10:00Z")
    } where slug = "opp-stripe-migration"
}

query set_task_in_progress() {
    update ActionItem set {
        status: "in_progress"
        updatedAt: datetime("2026-02-15T15:12:00Z")
    } where slug = "ai-draft-proposal"
}

query delete_task() {
    delete ActionItem where slug = "ai-draft-proposal"
}
"#,
    );
    workspace.write_file(
        "invalid_enum.gq",
        r#"query add_invalid_signal() {
    insert Signal {
        slug: "sig-invalid-enum"
        observedAt: datetime("2026-02-15T18:00:00Z")
        summary: "Invalid enum payload"
        urgency: "urgent"
        sourceType: "email"
        assertion: "fact"
        createdAt: datetime("2026-02-15T18:00:00Z")
    }
}
"#,
    );

    let check = workspace.json_value(&["--json", "check", "--query", "admin_mutations.gq"]);
    assert_eq!(check["status"], "ok");

    let bad = workspace.run_fail(&["check", "--query", "invalid_enum.gq"]);
    assert!(bad.stdout.contains("expects one of"));

    let insert = workspace.jsonl_rows(&[
        "run",
        "--query",
        "admin_mutations.gq",
        "--name",
        "insert_test_signal",
        "--format",
        "jsonl",
    ]);
    assert_eq!(scalar_string(&insert[0]["affected_nodes"]), "1");
    let insert_version = workspace.json_value(&["--json", "version"])["db"]["db_version"]
        .as_u64()
        .unwrap();

    let update_opp = workspace.jsonl_rows(&[
        "run",
        "--query",
        "admin_mutations.gq",
        "--name",
        "update_stage_to_hold",
        "--format",
        "jsonl",
    ]);
    assert_eq!(scalar_string(&update_opp[0]["affected_nodes"]), "1");
    let update_opp_version = workspace.json_value(&["--json", "version"])["db"]["db_version"]
        .as_u64()
        .unwrap();

    let update_task = workspace.jsonl_rows(&[
        "run",
        "--query",
        "admin_mutations.gq",
        "--name",
        "set_task_in_progress",
        "--format",
        "jsonl",
    ]);
    assert_eq!(scalar_string(&update_task[0]["affected_nodes"]), "1");
    let update_task_version = workspace.json_value(&["--json", "version"])["db"]["db_version"]
        .as_u64()
        .unwrap();

    let task_rows = workspace.json_rows(&[
        "run",
        "--query",
        "admin_mutations.gq",
        "--name",
        "task_rows",
        "--format",
        "json",
    ]);
    assert_eq!(task_rows.len(), 1);
    assert_eq!(task_rows[0]["status"], "in_progress");

    let delete_task = workspace.jsonl_rows(&[
        "run",
        "--query",
        "admin_mutations.gq",
        "--name",
        "delete_task",
        "--format",
        "jsonl",
    ]);
    assert_eq!(scalar_string(&delete_task[0]["affected_nodes"]), "1");
    let delete_task_version = workspace.json_value(&["--json", "version"])["db"]["db_version"]
        .as_u64()
        .unwrap();

    let inserted_signal = workspace.json_rows(&[
        "run",
        "--query",
        "admin_mutations.gq",
        "--name",
        "signal_rows",
        "--format",
        "json",
        "--param",
        "slug=sig-test-renewal",
    ]);
    assert_eq!(inserted_signal.len(), 1);
    assert_eq!(inserted_signal[0]["urgency"], "critical");

    let opportunity = workspace.json_rows(&[
        "run",
        "--query",
        "admin_mutations.gq",
        "--name",
        "opportunity_stage",
        "--format",
        "json",
    ]);
    assert_eq!(opportunity[0]["stage"], "hold");

    let tasks_after_delete = workspace.json_rows(&[
        "run",
        "--query",
        "admin_mutations.gq",
        "--name",
        "task_rows",
        "--format",
        "json",
    ]);
    assert!(tasks_after_delete.is_empty());

    let changes_range = workspace.run_ok(&[
        "changes",
        "--from",
        &insert_version.to_string(),
        "--to",
        &delete_task_version.to_string(),
        "--format",
        "json",
    ]);
    assert!(
        changes_range
            .stdout
            .contains(&format!("\"db_version\": {}", insert_version))
    );
    assert!(
        changes_range
            .stdout
            .contains(&format!("\"db_version\": {}", update_opp_version))
    );
    assert!(
        changes_range
            .stdout
            .contains(&format!("\"db_version\": {}", update_task_version))
    );
    assert!(
        changes_range
            .stdout
            .contains(&format!("\"db_version\": {}", delete_task_version))
    );
    assert!(changes_range.stdout.contains("\"type_name\": \"Signal\""));
    assert!(
        changes_range
            .stdout
            .contains("\"type_name\": \"Opportunity\"")
    );
    assert!(
        changes_range
            .stdout
            .contains("\"type_name\": \"ActionItem\"")
    );
    assert!(changes_range.stdout.contains("\"op\": \"insert\""));
    assert!(changes_range.stdout.contains("\"op\": \"update\""));
    assert!(changes_range.stdout.contains("\"op\": \"delete\""));
    assert!(changes_range.stdout.contains("\"urgency\": \"critical\""));

    let changes_since = workspace.run_ok(&[
        "changes",
        "--since",
        &update_task_version.to_string(),
        "--format",
        "jsonl",
    ]);
    assert!(
        changes_since
            .stdout
            .contains(&format!("\"db_version\":{}", delete_task_version))
    );
    assert!(changes_since.stdout.contains("\"op\":\"delete\""));

    let compact =
        workspace.json_value(&["--json", "compact", "--target-rows-per-fragment", "1024"]);
    assert_eq!(compact["status"], "ok");

    let doctor_before = workspace.json_value(&["--json", "doctor"]);
    assert_eq!(doctor_before["status"], "ok");
    assert_eq!(doctor_before["healthy"], true);

    let cleanup = workspace.json_value(&[
        "--json",
        "cleanup",
        "--retain-tx-versions",
        "2",
        "--retain-dataset-versions",
        "1",
    ]);
    assert_eq!(cleanup["status"], "ok");
    assert!(cleanup.get("tx_rows_kept").is_some());

    let cdc_mat = workspace.json_value(&["--json", "cdc-materialize", "--min-new-rows", "1"]);
    assert_eq!(cdc_mat["status"], "ok");
    assert_eq!(cdc_mat["dataset_written"], true);
    assert!(workspace.file("omni.nano/__cdc_analytics").is_dir());

    let cdc_skip = workspace.json_value(&["--json", "cdc-materialize", "--min-new-rows", "99999"]);
    assert_eq!(cdc_skip["status"], "ok");
    assert_eq!(cdc_skip["skipped_by_threshold"], true);

    let doctor_after = workspace.json_value(&["--json", "doctor"]);
    assert_eq!(doctor_after["status"], "ok");
    assert_eq!(doctor_after["healthy"], true);

    let version = workspace.json_value(&["--json", "version"]);
    assert!(version.get("binary_version").is_some());
    assert!(version["db"].get("db_version").is_some());

    let describe = workspace
        .run_ok(&["describe", "--type", "Signal", "--format", "json"])
        .stdout;
    assert!(describe.contains("\"name\": \"Signal\""));
    assert!(describe.contains("Observed customer, product, or market signals"));
    assert!(describe.contains("\"endpoint_keys\"") || describe.contains("\"outgoing_edges\""));

    let export = workspace.run_ok(&["export", "--format", "jsonl"]).stdout;
    assert!(export.contains("\"slug\":\"sig-test-renewal\""));
    assert!(!export.contains("\"slug\":\"ai-draft-proposal\""));
}
