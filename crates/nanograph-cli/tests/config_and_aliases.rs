mod common;

use common::{ExampleProject, ExampleWorkspace};

#[test]
fn starwars_config_aliases_and_metadata_work() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();
    workspace.check();

    let describe = workspace
        .run_ok(&["describe", "--type", "Character", "--format", "json"])
        .stdout;
    assert!(describe.contains("\"name\": \"Character\""));
    assert!(describe.contains("Named Star Wars characters used for search"));
    assert!(describe.contains("\"key_property\""));
    assert!(describe.contains("\"outgoing_edges\""));
    assert!(describe.contains("\"incoming_edges\""));
    assert!(describe.contains("Stable canonical character identifier"));

    let table = workspace
        .run_ok(&["run", "search", "father and son conflict"])
        .stdout;
    assert!(table.contains("Query: semantic_search"));
    assert!(table.contains("Description: Rank characters by semantic similarity"));
    assert!(table.contains("Instruction: Use for broad conceptual search"));
    assert!(table.contains("score"));

    let rows = workspace.json_rows(&[
        "run",
        "search",
        "father and son conflict",
        "--format",
        "json",
    ]);
    assert!(!rows.is_empty());
    assert!(rows[0].get("slug").is_some());

    let family = workspace.json_rows(&[
        "run",
        "family",
        "luke-skywalker",
        "chosen one prophecy",
        "--format",
        "json",
    ]);
    assert_eq!(family.len(), 2);
    assert!(family.iter().any(|row| row["slug"] == "anakin-skywalker"));
    assert!(family.iter().any(|row| row["slug"] == "padme-amidala"));
}

#[test]
fn revops_aliases_and_query_roots_work() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    workspace.init();
    workspace.load();
    workspace.check();

    let table = workspace
        .run_ok(&["run", "why", "opp-stripe-migration"])
        .stdout;
    assert!(table.contains("Query: decision_trace"));
    assert!(table.contains("Description: Trace an opportunity back"));
    assert!(table.contains("Instruction: Use as the default 'why did this happen?'"));
    assert!(table.contains("Make proposal for Stripe migration"));

    let rows = workspace.json_rows(&[
        "run",
        "signals",
        "cli-priya-shah",
        "vendor approval timing",
        "--format",
        "json",
    ]);
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .any(|row| row["slug"] == "sig-enterprise-procurement")
    );

    let pipeline = workspace.json_rows(&["run", "pipeline", "--format", "json"]);
    assert_eq!(pipeline.len(), 1);
    assert_eq!(pipeline[0]["stage"], "won");
    assert_eq!(pipeline[0]["deals"], 1);
}
