#![cfg(feature = "local-embed")]

mod common;

use common::{ExampleProject, ExampleWorkspace};

fn schema() -> &'static str {
    r#"
node Document {
    slug: String @key
    title: String
    body: String
    embedding: Vector(384)? @embed(body) @index
}

node Tag {
    slug: String @key
    name: String
}

edge TaggedWith: Document -> Tag
"#
}

fn data() -> &'static str {
    r#"
{"type":"Document","data":{"slug":"rust","title":"Rust Programming","body":"Rust is a systems programming language focused on safety, speed, and concurrency. It achieves memory safety without garbage collection."}}
{"type":"Document","data":{"slug":"cake","title":"Chocolate Cake","body":"A rich chocolate cake recipe with layers of ganache frosting. Preheat the oven to 350 degrees and mix flour with cocoa powder."}}
{"type":"Document","data":{"slug":"quantum","title":"Quantum Physics","body":"Quantum mechanics describes the behavior of particles at atomic and subatomic scales. Wave-particle duality and superposition are fundamental concepts."}}
{"type":"Document","data":{"slug":"guitar","title":"Guitar Chords","body":"Learn basic guitar chord progressions including major, minor, and seventh chords. Practice strumming patterns for rhythm guitar."}}
{"type":"Document","data":{"slug":"ml","title":"Machine Learning","body":"Machine learning algorithms learn patterns from data. Neural networks, decision trees, and support vector machines are common supervised learning methods."}}
{"type":"Tag","data":{"slug":"tech","name":"Technology"}}
{"type":"Tag","data":{"slug":"food","name":"Food & Cooking"}}
{"type":"Tag","data":{"slug":"science","name":"Science"}}
{"edge":"TaggedWith","from":"rust","to":"tech"}
{"edge":"TaggedWith","from":"cake","to":"food"}
{"edge":"TaggedWith","from":"quantum","to":"science"}
{"edge":"TaggedWith","from":"ml","to":"tech"}
"#
}

fn queries() -> &'static str {
    r#"
query all_docs() {
    match { $d: Document }
    return { $d.slug, $d.title }
    order { $d.slug asc }
}

query search($q: String) {
    match { $d: Document }
    return {
        $d.slug,
        $d.title,
        nearest($d.embedding, $q) as score
    }
    order { nearest($d.embedding, $q) }
    limit 5
}

query docs_by_tag($tag: String) {
    match {
        $d: Document
        $t: Tag
        $d taggedWith $t
        $t.slug = $tag
    }
    return { $d.slug, $d.title, $t.name }
    order { $d.slug asc }
}
"#
}

fn config() -> &'static str {
    r#"
[db]
default_path = "test.nano"

[schema]
default_path = "test.pg"

[embedding]
provider = "local"
model = "hf:sentence-transformers/all-MiniLM-L6-v2"
"#
}

/// Full local-embed CLI workflow: init, load with auto-embedding, match query,
/// semantic nearest query, and edge traversal.
///
/// Downloads `sentence-transformers/all-MiniLM-L6-v2` (~80 MB) on first run;
/// subsequent runs use the HuggingFace Hub disk cache.
#[test]
fn local_embed_init_load_search_and_traversal() {
    let workspace = ExampleWorkspace::copy(ExampleProject::Starwars);

    workspace.write_file("test.pg", schema());
    workspace.write_file("test.jsonl", data());
    workspace.write_file("test.gq", queries());
    workspace.write_file("nanograph.toml", config());

    // ── init ────────────────────────────────────────────────────────────
    let init = workspace.json_value(&["--json", "init"]);
    assert_eq!(init["status"], "ok");

    // ── load (triggers auto-embedding via local ONNX model) ─────────────
    let load = workspace.json_value(&[
        "--json",
        "load",
        "--data",
        "test.jsonl",
        "--mode",
        "overwrite",
    ]);
    assert_eq!(load["status"], "ok");

    // ── basic match: all 5 documents present ────────────────────────────
    let all = workspace.json_rows(&[
        "--json",
        "run",
        "--query",
        "test.gq",
        "--name",
        "all_docs",
    ]);
    assert_eq!(all.len(), 5);
    let slugs: Vec<&str> = all.iter().map(|r| r["slug"].as_str().unwrap()).collect();
    assert_eq!(slugs, vec!["cake", "guitar", "ml", "quantum", "rust"]);

    // ── semantic search: "programming" should rank rust first ───────────
    let search = workspace.json_rows(&[
        "--json",
        "run",
        "--query",
        "test.gq",
        "--name",
        "search",
        "--param",
        "q=systems programming language",
    ]);
    assert_eq!(search.len(), 5);
    assert_eq!(
        search[0]["slug"].as_str().unwrap(),
        "rust",
        "expected 'rust' to rank first for 'systems programming language', got: {:?}",
        search.iter().map(|r| r["slug"].as_str().unwrap()).collect::<Vec<_>>()
    );
    // score must be a number (actual float from vector distance)
    assert!(search[0]["score"].is_number());

    // ── semantic search: "baking recipe" should rank cake first ─────────
    let baking = workspace.json_rows(&[
        "--json",
        "run",
        "--query",
        "test.gq",
        "--name",
        "search",
        "--param",
        "q=baking dessert recipe",
    ]);
    assert_eq!(
        baking[0]["slug"].as_str().unwrap(),
        "cake",
        "expected 'cake' to rank first for 'baking dessert recipe', got: {:?}",
        baking.iter().map(|r| r["slug"].as_str().unwrap()).collect::<Vec<_>>()
    );

    // ── traversal: documents tagged with "tech" ─────────────────────────
    let tech = workspace.json_rows(&[
        "--json",
        "run",
        "--query",
        "test.gq",
        "--name",
        "docs_by_tag",
        "--param",
        "tag=tech",
    ]);
    assert_eq!(tech.len(), 2);
    let tech_slugs: Vec<&str> = tech.iter().map(|r| r["slug"].as_str().unwrap()).collect();
    assert_eq!(tech_slugs, vec!["ml", "rust"]);
    assert_eq!(tech[0]["name"].as_str().unwrap(), "Technology");

    // ── traversal: documents tagged with "food" ─────────────────────────
    let food = workspace.json_rows(&[
        "--json",
        "run",
        "--query",
        "test.gq",
        "--name",
        "docs_by_tag",
        "--param",
        "tag=food",
    ]);
    assert_eq!(food.len(), 1);
    assert_eq!(food[0]["slug"].as_str().unwrap(), "cake");
    assert_eq!(food[0]["name"].as_str().unwrap(), "Food & Cooking");
}
