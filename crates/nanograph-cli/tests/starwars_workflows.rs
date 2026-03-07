mod common;

use common::{ExampleProject, ExampleWorkspace, scalar_string};

#[test]
fn starwars_search_and_mutation_workflows_work() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();
    workspace.check();

    let keyword = workspace.json_rows(&[
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "keyword_search",
        "--format",
        "json",
        "--param",
        "q=chosen one",
    ]);
    assert!(keyword.iter().any(|row| row["slug"] == "anakin-skywalker"));

    let fuzzy = workspace.json_rows(&[
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "fuzzy_search",
        "--format",
        "json",
        "--param",
        "q=Skywaker",
    ]);
    assert!(fuzzy.iter().any(|row| row["slug"] == "anakin-skywalker"));
    assert!(fuzzy.iter().any(|row| row["slug"] == "luke-skywalker"));

    let hybrid = workspace.json_rows(&[
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "hybrid_search",
        "--format",
        "json",
        "--param",
        "q=father and son conflict",
    ]);
    assert!(!hybrid.is_empty());
    assert!(hybrid[0]["hybrid_score"].is_number());

    let same_debut = workspace.json_rows(&[
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "same_debut",
        "--format",
        "json",
        "--param",
        "film=a-new-hope",
    ]);
    assert!(same_debut.len() >= 4);
    assert!(same_debut.iter().any(|row| row["slug"] == "han-solo"));
    assert!(same_debut.iter().any(|row| row["slug"] == "leia-organa"));
    assert!(same_debut.iter().any(|row| row["slug"] == "luke-skywalker"));

    let duels_before = workspace
        .json_rows(&[
            "run",
            "--query",
            "starwars.gq",
            "--name",
            "all_duels",
            "--format",
            "json",
        ])
        .len();

    let insert_rows = workspace.jsonl_rows(&[
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "add_character",
        "--format",
        "jsonl",
    ]);
    assert_eq!(scalar_string(&insert_rows[0]["affected_nodes"]), "1");

    let ezra = workspace.json_rows(&[
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "character_profile",
        "--format",
        "json",
        "--param",
        "slug=ezra-bridger",
    ]);
    assert_eq!(ezra.len(), 1);
    assert_eq!(ezra[0]["name"], "Ezra Bridger");

    let add_parent_rows = workspace.jsonl_rows(&[
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "add_edge_parent",
        "--format",
        "jsonl",
        "--param",
        "from=luke-skywalker",
        "--param",
        "to=yoda",
    ]);
    assert_eq!(scalar_string(&add_parent_rows[0]["affected_edges"]), "1");

    let family_after = workspace.json_rows(&[
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "family_semantic",
        "--format",
        "json",
        "--param",
        "slug=luke-skywalker",
        "--param",
        "q=ancient jedi mentor",
    ]);
    assert!(family_after.iter().any(|row| row["slug"] == "yoda"));

    let delete_rows = workspace.jsonl_rows(&[
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "delete_character",
        "--format",
        "jsonl",
        "--param",
        "slug=darth-vader",
    ]);
    assert_eq!(scalar_string(&delete_rows[0]["affected_nodes"]), "1");

    let vader = workspace.json_rows(&[
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "character_profile",
        "--format",
        "json",
        "--param",
        "slug=darth-vader",
    ]);
    assert!(vader.is_empty());

    let duels_after = workspace
        .json_rows(&[
            "run",
            "--query",
            "starwars.gq",
            "--name",
            "all_duels",
            "--format",
            "json",
        ])
        .len();
    assert!(duels_after < duels_before);
}
