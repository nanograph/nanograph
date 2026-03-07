mod common;

use common::{ExampleProject, ExampleWorkspace};

#[test]
fn starwars_export_roundtrip_preserves_portable_seed_shape() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();

    let exported_rows = workspace.jsonl_rows(&["export", "--format", "jsonl"]);
    assert!(exported_rows.iter().any(|row| row["type"] == "Character"));
    assert!(exported_rows.iter().any(|row| {
        row["edge"] == "HasMentor"
            && row["from"].is_string()
            && row["to"].is_string()
            && row.get("id").is_none()
            && row.get("src").is_none()
            && row.get("dst").is_none()
    }));

    let exported = workspace.run_ok(&["export", "--format", "jsonl"]).stdout;
    workspace.write_file("roundtrip.jsonl", &exported);

    let db_b = workspace.file("starwars-roundtrip.nano");
    let db_b_owned = db_b.to_string_lossy().into_owned();
    let db_b_str = db_b_owned.as_str();
    let init = workspace.json_value(&["--json", "init", db_b_str, "--schema", "starwars.pg"]);
    assert_eq!(init["status"], "ok");
    let load = workspace.json_value(&[
        "--json",
        "load",
        db_b_str,
        "--data",
        "roundtrip.jsonl",
        "--mode",
        "overwrite",
    ]);
    assert_eq!(load["status"], "ok");

    let original_duels = workspace
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
    let roundtrip_duels = workspace
        .json_rows(&[
            "run",
            "--db",
            db_b_str,
            "--query",
            "starwars.gq",
            "--name",
            "all_duels",
            "--format",
            "json",
        ])
        .len();
    assert_eq!(original_duels, roundtrip_duels);

    let roundtrip_cast = workspace.json_rows(&[
        "run",
        "--db",
        db_b_str,
        "--query",
        "starwars.gq",
        "--name",
        "same_debut",
        "--format",
        "json",
        "--param",
        "film=a-new-hope",
    ]);
    assert!(roundtrip_cast.len() >= 4);
    assert!(roundtrip_cast.iter().any(|row| row["slug"] == "han-solo"));
    assert!(
        roundtrip_cast
            .iter()
            .any(|row| row["slug"] == "leia-organa")
    );
    assert!(
        roundtrip_cast
            .iter()
            .any(|row| row["slug"] == "luke-skywalker")
    );
}
