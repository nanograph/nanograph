mod common;

use common::{ExampleProject, ExampleWorkspace};

#[test]
fn generated_project_scaffolding_is_usable_end_to_end() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.delete_file("nanograph.toml");

    let init = workspace.json_value(&[
        "--json",
        "init",
        "generated.nano",
        "--schema",
        "starwars.pg",
    ]);
    assert_eq!(init["status"], "ok");
    assert!(workspace.file("nanograph.toml").is_file());
    assert!(workspace.file(".env.nano").is_file());

    let generated = workspace.read_file("nanograph.toml");
    assert!(generated.contains("default_path = \"generated.nano\""));
    assert!(generated.contains("default_path = \"starwars.pg\""));
    assert!(generated.contains("roots = [\"queries\"]"));

    workspace.write_file(".env.nano", "NANOGRAPH_EMBEDDINGS_MOCK=1\n");
    workspace.append_file(
        "nanograph.toml",
        r#"

[query_aliases.search]
query = "starwars.gq"
name = "semantic_search"
args = ["q"]
format = "table"
"#,
    );

    let load = workspace.json_value(&[
        "--json",
        "load",
        "--data",
        "starwars.jsonl",
        "--mode",
        "overwrite",
    ]);
    assert_eq!(load["status"], "ok");

    let check = workspace.json_value(&["--json", "check", "--query", "starwars.gq"]);
    assert_eq!(check["status"], "ok");

    let table = workspace
        .run_ok(&["run", "search", "father and son conflict"])
        .stdout;
    assert!(table.contains("Query: semantic_search"));
    assert!(table.contains("score"));

    let describe = workspace
        .run_ok(&["describe", "--type", "Character"])
        .stdout;
    assert!(describe.contains("Database:"));
    assert!(describe.contains("Node Types"));
    assert!(describe.contains("Character"));
}

#[test]
fn dotenv_nano_enables_embeddings_and_wins_over_dotenv() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.write_file(
        "nanograph.toml",
        r#"[db]
default_path = "starwars.nano"

[schema]
default_path = "starwars.pg"

[query]
roots = ["."]
"#,
    );
    workspace.write_file(".env", "NANOGRAPH_EMBEDDINGS_MOCK=0\n");

    workspace.init();

    let missing_env =
        workspace.run_fail(&["load", "--data", "starwars.jsonl", "--mode", "overwrite"]);
    let missing_env_output = format!("{}\n{}", missing_env.stdout, missing_env.stderr);
    assert!(missing_env_output.contains("OPENAI_API_KEY is required"));

    workspace.write_file(".env.nano", "NANOGRAPH_EMBEDDINGS_MOCK=1\n");

    let load = workspace.json_value(&[
        "--json",
        "load",
        "--data",
        "starwars.jsonl",
        "--mode",
        "overwrite",
    ]);
    assert_eq!(load["status"], "ok");

    let rows = workspace.json_rows(&[
        "run",
        "--query",
        "starwars.gq",
        "--name",
        "semantic_search",
        "--format",
        "json",
        "--param",
        "q=father and son conflict",
    ]);
    assert!(!rows.is_empty());
}
