mod common;

use common::{ExampleProject, ExampleWorkspace, scalar_string};

#[test]
fn revops_trace_search_and_mutation_workflows_work() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    workspace.init();
    workspace.load();
    workspace.check();

    let all_clients = workspace.json_rows(&[
        "run",
        "--query",
        "revops.gq",
        "--name",
        "all_clients",
        "--format",
        "json",
    ]);
    assert_eq!(all_clients.len(), 2);

    let why = workspace.json_rows(&["run", "why", "opp-stripe-migration", "--format", "json"]);
    assert_eq!(why.len(), 1);
    assert_eq!(why[0]["intent"], "Make proposal for Stripe migration");

    let search_rows = workspace.json_rows(&[
        "run",
        "--query",
        "revops.gq",
        "--name",
        "client_signal_search",
        "--format",
        "json",
        "--param",
        "client=cli-priya-shah",
        "--param",
        "q=vendor alternatives",
    ]);
    assert!(
        search_rows
            .iter()
            .any(|row| row["slug"] == "sig-hates-vendor")
    );

    let semantic_rows = workspace.json_rows(&[
        "run",
        "signals",
        "cli-priya-shah",
        "procurement approval timing",
        "--format",
        "json",
    ]);
    assert!(!semantic_rows.is_empty());
    assert!(
        semantic_rows
            .iter()
            .any(|row| row["slug"] == "sig-enterprise-procurement")
    );
    assert!(semantic_rows[0]["score"].is_number());

    let full_trace = workspace.json_rows(&[
        "run",
        "--query",
        "revops.gq",
        "--name",
        "full_trace",
        "--format",
        "json",
        "--param",
        "sig=sig-hates-vendor",
    ]);
    assert_eq!(full_trace.len(), 1);
    assert_eq!(full_trace[0]["title"], "Stripe Migration");

    let add_signal = workspace.jsonl_rows(&[
        "run",
        "--query",
        "revops.gq",
        "--name",
        "add_signal",
        "--format",
        "jsonl",
    ]);
    assert_eq!(scalar_string(&add_signal[0]["affected_nodes"]), "1");

    let inserted_signal = workspace.json_rows(&[
        "run",
        "--query",
        "revops.gq",
        "--name",
        "signal_lookup",
        "--format",
        "json",
        "--param",
        "slug=sig-vendor-renewal",
    ]);
    assert_eq!(inserted_signal.len(), 1);
    assert_eq!(inserted_signal[0]["urgency"], "medium");

    let remove_task = workspace.jsonl_rows(&[
        "run",
        "--query",
        "revops.gq",
        "--name",
        "remove_cancelled",
        "--format",
        "jsonl",
    ]);
    assert_eq!(scalar_string(&remove_task[0]["affected_nodes"]), "1");

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
