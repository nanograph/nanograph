mod common;

use common::{ExampleProject, ExampleWorkspace};

fn migrated_starwars_schema(original: &str) -> String {
    original
        .replace(
            "    tags: [String]?\n}",
            "    tags: [String]?\n    homeworld: String?\n}",
        )
        .replace(
            "    location_type: String\n",
            "    kind: String @rename_from(\"location_type\")\n",
        )
        .replace(
            "node Battle {\n",
            "node Conflict @rename_from(\"Battle\") {\n",
        )
        .replace(
            "edge DepictedIn: Battle -> Film\n",
            "edge DepictedIn: Conflict -> Film\n",
        )
        .replace(
            "edge Fought: Character -> Character @description(\"Combat encounter between two characters.\") {\n",
            "edge Dueled: Character -> Character @rename_from(\"Fought\") @description(\"Combat encounter between two characters.\") {\n",
        )
        .replace(
            "edge ParticipatedIn: Character -> Battle {\n",
            "edge ParticipatedIn: Character -> Conflict {\n",
        )
        .replace(
            "edge TookPlaceOn: Battle -> Planet\n",
            "edge TookPlaceOn: Conflict -> Planet\n",
        )
        .replace(
            "    color: String\n",
            "",
        )
        .replace(
            "// ── Film connections ────────────────────────────────────────────────────────\n",
            "node Species {\n    slug: String @key\n    name: String\n    classification: String?\n}\n\n// ── Film connections ────────────────────────────────────────────────────────\n",
        )
}

#[test]
fn starwars_schema_diff_and_migrate_work_from_example() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);
    workspace.init();
    workspace.load();

    let original_schema = workspace.read_file("starwars.pg");
    let migrated_schema = migrated_starwars_schema(&original_schema);
    workspace.write_file("migrated.pg", &migrated_schema);

    let diff = workspace
        .run_ok(&[
            "schema-diff",
            "--from",
            "starwars.pg",
            "--to",
            "migrated.pg",
            "--format",
            "json",
        ])
        .stdout;
    assert!(diff.contains("RenameType"));
    assert!(diff.contains("RenameProperty"));
    assert!(diff.contains("AddNodeType"));
    assert!(diff.contains("AddProperty"));
    assert!(diff.contains("DropProperty"));

    workspace.write_file("starwars.nano/schema.pg", &migrated_schema);

    let migrate = workspace.run_ok(&["migrate", "--auto-approve", "--format", "table"]);
    assert!(migrate.stdout.contains("RenameType") || migrate.stdout.contains("Applied"));

    workspace.write_file(
        "migrated_queries.gq",
        r#"query jedi() {
    match {
        $c: Character
        $c affiliatedWith $f
        $f.name = "Jedi Order"
    }
    return { $c.name, $c.rank, $c.era }
    order { $c.name asc }
}

query all_duels() {
    match {
        $a: Character
        $a dueled $b
    }
    return { $a.name, $b.name }
    order { $a.name asc }
}

query battle_veterans() {
    match {
        $c: Character
        $c participatedIn $b
    }
    return {
        $c.name
        count($b) as battles
    }
    order { battles desc }
    limit 5
}

query tatooine_locations() {
    match {
        $p: Planet { slug: "tatooine" }
        $loc locatedAt $p
    }
    return { $loc.name, $loc.kind }
    order { $loc.name asc }
}
"#,
    );

    let jedi = workspace.json_rows(&[
        "run",
        "--query",
        "migrated_queries.gq",
        "--name",
        "jedi",
        "--format",
        "json",
    ]);
    assert_eq!(jedi.len(), 7);

    let duels = workspace.json_rows(&[
        "run",
        "--query",
        "migrated_queries.gq",
        "--name",
        "all_duels",
        "--format",
        "json",
    ]);
    assert!(!duels.is_empty());

    let veterans = workspace.json_rows(&[
        "run",
        "--query",
        "migrated_queries.gq",
        "--name",
        "battle_veterans",
        "--format",
        "json",
    ]);
    assert!(!veterans.is_empty());

    let locations = workspace.json_rows(&[
        "run",
        "--query",
        "migrated_queries.gq",
        "--name",
        "tatooine_locations",
        "--format",
        "json",
    ]);
    assert!(!locations.is_empty());
    assert!(locations[0].get("kind").is_some());
}
