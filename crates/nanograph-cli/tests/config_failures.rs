mod common;

use common::{ExampleProject, ExampleWorkspace};

#[test]
fn config_failure_modes_surface_clear_errors() {
    let malformed = ExampleWorkspace::copy(ExampleProject::Starwars);
    malformed.write_file("nanograph.toml", "[db\n");
    let malformed_err = malformed.run_fail(&["describe"]);
    let malformed_output = format!("{}\n{}", malformed_err.stdout, malformed_err.stderr);
    assert!(malformed_output.contains("failed to parse config"));

    let missing_config = ExampleWorkspace::copy(ExampleProject::Starwars);
    let missing_config_err = missing_config.run_fail(&["--config", "missing.toml", "describe"]);
    let missing_config_output = format!(
        "{}\n{}",
        missing_config_err.stdout, missing_config_err.stderr
    );
    assert!(missing_config_output.contains("failed to read config"));
    assert!(missing_config_output.contains("missing.toml"));
}

#[test]
fn alias_and_path_resolution_failures_surface_clear_errors() {
    let missing_alias = ExampleWorkspace::copy(ExampleProject::Starwars);
    missing_alias.init();
    missing_alias.load();
    let missing_alias_err = missing_alias.run_fail(&["run", "missing", "father and son conflict"]);
    let missing_alias_output =
        format!("{}\n{}", missing_alias_err.stdout, missing_alias_err.stderr);
    assert!(missing_alias_output.contains("query alias `missing` not found"));

    let broken_alias = ExampleWorkspace::copy(ExampleProject::Starwars);
    broken_alias.init();
    broken_alias.load();
    broken_alias.write_file(
        "nanograph.toml",
        r#"[db]
default_path = "starwars.nano"

[schema]
default_path = "starwars.pg"

[query]
roots = ["queries"]

[embedding]
provider = "mock"

[query_aliases.search]
query = "queries/missing.gq"
name = "semantic_search"
args = ["q"]
format = "table"
"#,
    );
    let broken_alias_err = broken_alias.run_fail(&["run", "search", "father and son conflict"]);
    let broken_alias_output = format!("{}\n{}", broken_alias_err.stdout, broken_alias_err.stderr);
    assert!(broken_alias_output.contains("failed to resolve query path"));

    let missing_db = ExampleWorkspace::copy(ExampleProject::Starwars);
    missing_db.write_file(
        "nanograph.toml",
        r#"[db]
default_path = "missing.nano"

[schema]
default_path = "starwars.pg"

[query]
roots = ["."]

[embedding]
provider = "mock"
"#,
    );
    let missing_db_err = missing_db.run_fail(&["describe"]);
    let missing_db_output = format!("{}\n{}", missing_db_err.stdout, missing_db_err.stderr);
    assert!(missing_db_output.contains("missing.nano"));
}
