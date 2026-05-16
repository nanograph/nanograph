mod common;

use common::{ExampleProject, ExampleWorkspace, scalar_string};

fn toolbelt_schema() -> &'static str {
    r#"
node Issue {
    slug: String @key
    title: String
    status: enum(open, in_progress, closed)
    claimedBy: String?
    updatedAt: DateTime
}
"#
}

fn toolbelt_queries() -> &'static str {
    r#"
query ensure_issue($slug: String, $title: String) {
    put Issue {
        slug: $slug,
        title: $title,
        status: "open",
        updatedAt: now()
    }
}

query claim_issue($slug: String, $agent: String) {
    update Issue set {
        claimedBy: $agent,
        status: "in_progress",
        updatedAt: now()
    } where {
        slug = $slug
        claimedBy is null
    }
}

query inspect($slug: String) {
    match { $i: Issue { slug: $slug } }
    return { $i.slug, $i.status, $i.claimedBy, $i.title }
}
"#
}

fn init_toolbelt_db(workspace: &ExampleWorkspace) {
    workspace.write_file("toolbelt.pg", toolbelt_schema());
    workspace.write_file("toolbelt.gq", toolbelt_queries());

    let init =
        workspace.json_value(&["--json", "init", "toolbelt.nano", "--schema", "toolbelt.pg"]);
    assert_eq!(init["status"], "ok");
}

#[test]
fn cli_put_is_idempotent_and_envelope_carries_matched_nodes() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    init_toolbelt_db(&workspace);

    // First put — insert path. matched_nodes should be 1 (key gate).
    let first = workspace.json_rows(&[
        "run",
        "--db",
        "toolbelt.nano",
        "--query",
        "toolbelt.gq",
        "--name",
        "ensure_issue",
        "--param",
        "slug=ng-001",
        "--param",
        "title=Refresh tokens",
        "--format",
        "json",
    ]);
    assert_eq!(first.len(), 1);
    assert_eq!(scalar_string(&first[0]["affected_nodes"]), "1");
    assert_eq!(
        scalar_string(&first[0]["matched_nodes"]),
        "1",
        "put envelope must carry matched_nodes (insert path)"
    );

    // Second put — update path. Idempotent; same envelope shape.
    let second = workspace.json_rows(&[
        "run",
        "--db",
        "toolbelt.nano",
        "--query",
        "toolbelt.gq",
        "--name",
        "ensure_issue",
        "--param",
        "slug=ng-001",
        "--param",
        "title=Refresh tokens v2",
        "--format",
        "json",
    ]);
    assert_eq!(second.len(), 1);
    assert_eq!(scalar_string(&second[0]["affected_nodes"]), "1");
    assert_eq!(
        scalar_string(&second[0]["matched_nodes"]),
        "1",
        "put envelope must carry matched_nodes (update path)"
    );

    // Inspect confirms exactly one row, updated title.
    let rows = workspace.json_rows(&[
        "run",
        "--db",
        "toolbelt.nano",
        "--query",
        "toolbelt.gq",
        "--name",
        "inspect",
        "--param",
        "slug=ng-001",
        "--format",
        "json",
    ]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["slug"], "ng-001");
    assert_eq!(rows[0]["title"], "Refresh tokens v2");
    assert_eq!(rows[0]["status"], "open");
}

#[test]
fn cli_cas_claim_envelope_distinguishes_winner_from_loser() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Revops);
    init_toolbelt_db(&workspace);

    // Seed an open issue.
    workspace.json_rows(&[
        "run",
        "--db",
        "toolbelt.nano",
        "--query",
        "toolbelt.gq",
        "--name",
        "ensure_issue",
        "--param",
        "slug=ng-002",
        "--param",
        "title=The bug",
        "--format",
        "json",
    ]);

    // Alice claims — winner. matched=1, affected=1.
    let alice = workspace.json_rows(&[
        "run",
        "--db",
        "toolbelt.nano",
        "--query",
        "toolbelt.gq",
        "--name",
        "claim_issue",
        "--param",
        "slug=ng-002",
        "--param",
        "agent=alice",
        "--format",
        "json",
    ]);
    assert_eq!(alice.len(), 1);
    assert_eq!(scalar_string(&alice[0]["matched_nodes"]), "1");
    assert_eq!(scalar_string(&alice[0]["affected_nodes"]), "1");

    // Bob claims — loser. matched=0, affected=0. No silent overwrite.
    let bob = workspace.json_rows(&[
        "run",
        "--db",
        "toolbelt.nano",
        "--query",
        "toolbelt.gq",
        "--name",
        "claim_issue",
        "--param",
        "slug=ng-002",
        "--param",
        "agent=bob",
        "--format",
        "json",
    ]);
    assert_eq!(bob.len(), 1);
    assert_eq!(
        scalar_string(&bob[0]["matched_nodes"]),
        "0",
        "CAS-lost signal: matched_nodes must be 0"
    );
    assert_eq!(scalar_string(&bob[0]["affected_nodes"]), "0");

    // Inspect — Alice still owns the issue.
    let inspect = workspace.json_rows(&[
        "run",
        "--db",
        "toolbelt.nano",
        "--query",
        "toolbelt.gq",
        "--name",
        "inspect",
        "--param",
        "slug=ng-002",
        "--format",
        "json",
    ]);
    assert_eq!(inspect.len(), 1);
    assert_eq!(inspect[0]["claimedBy"], "alice");
    assert_eq!(inspect[0]["status"], "in_progress");
}
