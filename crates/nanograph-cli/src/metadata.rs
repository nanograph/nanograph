use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr, eyre};
use tracing::instrument;

use crate::ui::{stdout_supports_color, style_label};
use nanograph::store::export::build_export_rows_at_path;
use nanograph::store::metadata::DatabaseMetadata;
use nanograph::store::snapshot::read_committed_graph_snapshot;

#[instrument(fields(db = ?db_path.as_ref().map(|p| p.display().to_string())))]
pub(crate) async fn cmd_version(db_path: Option<PathBuf>, json: bool, quiet: bool) -> Result<()> {
    let payload = build_version_payload(db_path.as_deref())?;

    if json {
        let out =
            serde_json::to_string_pretty(&payload).wrap_err("failed to serialize version JSON")?;
        println!("{}", out);
        return Ok(());
    }

    if !quiet {
        print_version_table(&payload);
    }
    Ok(())
}

pub(crate) fn build_version_payload(db_path: Option<&Path>) -> Result<serde_json::Value> {
    let mut payload = serde_json::json!({
        "binary_version": env!("CARGO_PKG_VERSION"),
    });

    if let Some(path) = db_path {
        let manifest = read_committed_graph_snapshot(path)?;
        let dataset_versions = manifest
            .datasets
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "kind": entry.kind,
                    "type_name": entry.type_name,
                    "type_id": entry.type_id,
                    "dataset_path": entry.dataset_path,
                    "dataset_version": entry.dataset_version,
                    "row_count": entry.row_count,
                })
            })
            .collect::<Vec<_>>();
        payload["db"] = serde_json::json!({
            "path": path.display().to_string(),
            "format_version": manifest.format_version,
            "db_version": manifest.db_version,
            "last_tx_id": manifest.last_tx_id,
            "committed_at": manifest.committed_at,
            "schema_ir_hash": manifest.schema_ir_hash,
            "schema_identity_version": manifest.schema_identity_version,
            "next_node_id": manifest.next_node_id,
            "next_edge_id": manifest.next_edge_id,
            "next_type_id": manifest.next_type_id,
            "next_prop_id": manifest.next_prop_id,
            "dataset_count": manifest.datasets.len(),
            "dataset_versions": dataset_versions,
        });
    }

    Ok(payload)
}

fn print_version_table(payload: &serde_json::Value) {
    let color = stdout_supports_color();
    println!(
        "{} {}",
        style_label("nanograph", color),
        payload["binary_version"].as_str().unwrap_or_default()
    );
    if let Some(db) = payload.get("db") {
        println!(
            "{} {}",
            style_label("Database:", color),
            db["path"].as_str().unwrap_or_default()
        );
        println!(
            "{} format v{}, db_version {}",
            style_label("Manifest:", color),
            db["format_version"].as_u64().unwrap_or(0),
            db["db_version"].as_u64().unwrap_or(0)
        );
        println!(
            "{} {} @ {}",
            style_label("Last TX:", color),
            db["last_tx_id"].as_str().unwrap_or_default(),
            db["committed_at"].as_str().unwrap_or_default()
        );
        println!(
            "{} {} (identity v{})",
            style_label("Schema hash:", color),
            db["schema_ir_hash"].as_str().unwrap_or_default(),
            db["schema_identity_version"].as_u64().unwrap_or(0)
        );
        println!(
            "{} node={} edge={} type={} prop={}",
            style_label("Next IDs:", color),
            db["next_node_id"].as_u64().unwrap_or(0),
            db["next_edge_id"].as_u64().unwrap_or(0),
            db["next_type_id"].as_u64().unwrap_or(0),
            db["next_prop_id"].as_u64().unwrap_or(0)
        );
        println!(
            "{} {}",
            style_label("Datasets:", color),
            db["dataset_count"].as_u64().unwrap_or(0)
        );
        if let Some(entries) = db["dataset_versions"].as_array() {
            for entry in entries {
                println!(
                    "  - {} {}: v{} (rows={})",
                    entry["kind"].as_str().unwrap_or_default(),
                    entry["type_name"].as_str().unwrap_or_default(),
                    entry["dataset_version"].as_u64().unwrap_or(0),
                    entry["row_count"].as_u64().unwrap_or(0),
                );
            }
        }
    }
}

#[instrument(fields(db_path = %db_path.display(), format = format))]
pub(crate) async fn cmd_describe(
    db_path: PathBuf,
    format: &str,
    json: bool,
    type_name: Option<&str>,
    verbose: bool,
    quiet: bool,
) -> Result<()> {
    let metadata = DatabaseMetadata::open(&db_path)?;
    let payload = build_describe_payload(&db_path, &metadata, type_name)?;
    let effective_format = if json { "json" } else { format };

    match effective_format {
        "json" => {
            let out = serde_json::to_string_pretty(&payload)
                .wrap_err("failed to serialize describe JSON")?;
            println!("{}", out);
        }
        "table" => {
            if !quiet {
                print_describe_table(&payload, verbose);
            }
        }
        other => return Err(eyre!("unknown format: {} (supported: table, json)", other)),
    }

    Ok(())
}

pub(crate) fn build_describe_payload(
    db_path: &Path,
    metadata: &DatabaseMetadata,
    type_name: Option<&str>,
) -> Result<serde_json::Value> {
    let manifest = metadata.manifest();
    let schema_ir = metadata.schema_ir();

    let mut nodes = Vec::new();
    for node in schema_ir.node_types() {
        if let Some(type_name) = type_name
            && node.name != type_name
        {
            continue;
        }
        let dataset = metadata.dataset_entry("node", &node.name);
        let properties = node
            .properties
            .iter()
            .map(|prop| {
                serde_json::json!({
                    "name": prop.name,
                    "prop_id": prop.prop_id,
                    "type": prop_type_string(prop),
                    "key": prop.key,
                    "unique": prop.unique,
                    "index": prop.index,
                    "embed_source": prop.embed_source,
                    "description": prop.description,
                })
            })
            .collect::<Vec<_>>();
        let outgoing_edges = schema_ir
            .edge_types()
            .filter(|edge| edge.src_type_name == node.name)
            .map(|edge| {
                serde_json::json!({
                    "name": edge.name,
                    "to_type": edge.dst_type_name,
                })
            })
            .collect::<Vec<_>>();
        let incoming_edges = schema_ir
            .edge_types()
            .filter(|edge| edge.dst_type_name == node.name)
            .map(|edge| {
                serde_json::json!({
                    "name": edge.name,
                    "from_type": edge.src_type_name,
                })
            })
            .collect::<Vec<_>>();
        nodes.push(serde_json::json!({
            "name": node.name,
            "type_id": node.type_id,
            "description": node.description,
            "instruction": node.instruction,
            "key_property": node.key_property_name(),
            "unique_properties": node.unique_properties().map(|prop| prop.name.clone()).collect::<Vec<_>>(),
            "outgoing_edges": outgoing_edges,
            "incoming_edges": incoming_edges,
            "rows": dataset.map(|d| d.row_count).unwrap_or(0),
            "dataset_path": dataset.map(|d| d.dataset_path.clone()),
            "dataset_version": dataset.map(|d| d.dataset_version),
            "properties": properties,
        }));
    }

    let mut edges = Vec::new();
    for edge in schema_ir.edge_types() {
        if let Some(type_name) = type_name
            && edge.name != type_name
        {
            continue;
        }
        let dataset = metadata.dataset_entry("edge", &edge.name);
        let properties = edge
            .properties
            .iter()
            .map(|prop| {
                serde_json::json!({
                    "name": prop.name,
                    "prop_id": prop.prop_id,
                    "type": prop_type_string(prop),
                    "description": prop.description,
                })
            })
            .collect::<Vec<_>>();
        edges.push(serde_json::json!({
            "name": edge.name,
            "type_id": edge.type_id,
            "src_type": edge.src_type_name,
            "dst_type": edge.dst_type_name,
            "description": edge.description,
            "instruction": edge.instruction,
            "endpoint_keys": {
                "src": schema_ir.node_key_property_name(&edge.src_type_name),
                "dst": schema_ir.node_key_property_name(&edge.dst_type_name),
            },
            "rows": dataset.map(|d| d.row_count).unwrap_or(0),
            "dataset_path": dataset.map(|d| d.dataset_path.clone()),
            "dataset_version": dataset.map(|d| d.dataset_version),
            "properties": properties,
        }));
    }

    if let Some(type_name) = type_name
        && nodes.is_empty()
        && edges.is_empty()
    {
        return Err(eyre!("type `{}` not found in schema", type_name));
    }

    Ok(serde_json::json!({
        "db_path": db_path.display().to_string(),
        "binary_version": env!("CARGO_PKG_VERSION"),
        "type_filter": type_name,
        "manifest": {
            "format_version": manifest.format_version,
            "db_version": manifest.db_version,
            "last_tx_id": manifest.last_tx_id,
            "committed_at": manifest.committed_at,
            "schema_ir_hash": manifest.schema_ir_hash,
            "schema_identity_version": manifest.schema_identity_version,
            "datasets": manifest.datasets.len(),
        },
        "schema_ir_version": schema_ir.ir_version,
        "nodes": nodes,
        "edges": edges,
    }))
}

fn print_describe_table(payload: &serde_json::Value, verbose: bool) {
    let color = stdout_supports_color();
    let node_count = payload["nodes"]
        .as_array()
        .map(|items| items.len())
        .unwrap_or(0);
    let edge_count = payload["edges"]
        .as_array()
        .map(|items| items.len())
        .unwrap_or(0);

    println!(
        "{} {}",
        style_label("Database:", color),
        payload["db_path"].as_str().unwrap_or_default()
    );
    println!(
        "{} db_version {}, last tx {}, {} node type(s), {} edge type(s)",
        style_label("Summary:", color),
        payload["manifest"]["db_version"].as_u64().unwrap_or(0),
        payload["manifest"]["last_tx_id"]
            .as_str()
            .unwrap_or_default(),
        node_count,
        edge_count
    );
    if verbose {
        println!(
            "{} format v{}, committed_at {}, schema ir v{}",
            style_label("Manifest:", color),
            payload["manifest"]["format_version"].as_u64().unwrap_or(0),
            payload["manifest"]["committed_at"]
                .as_str()
                .unwrap_or_default(),
            payload["schema_ir_version"].as_u64().unwrap_or(0)
        );
        println!(
            "{} {} (identity v{}, datasets={})",
            style_label("Schema hash:", color),
            payload["manifest"]["schema_ir_hash"]
                .as_str()
                .unwrap_or_default(),
            payload["manifest"]["schema_identity_version"]
                .as_u64()
                .unwrap_or(0),
            payload["manifest"]["datasets"].as_u64().unwrap_or(0)
        );
    }
    println!();

    println!("{}", style_label("Node Types", color));
    if let Some(nodes) = payload["nodes"].as_array() {
        for node in nodes {
            print!(
                "- {} (rows={}",
                node["name"].as_str().unwrap_or_default(),
                node["rows"].as_u64().unwrap_or(0),
            );
            if let Some(key_property) = node["key_property"].as_str() {
                print!(", key={}", key_property);
            }
            if verbose {
                let version = node["dataset_version"]
                    .as_u64()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string());
                print!(
                    ", type_id={}, dataset_version={}",
                    node["type_id"].as_u64().unwrap_or(0),
                    version
                );
            }
            println!(")");
            if let Some(description) = node["description"].as_str() {
                println!("  description: {}", description);
            }
            if let Some(instruction) = node["instruction"].as_str() {
                println!("  instruction: {}", instruction);
            }
            if let Some(unique_properties) = node["unique_properties"].as_array()
                && !unique_properties.is_empty()
            {
                let joined = unique_properties
                    .iter()
                    .filter_map(|value| value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("  unique: {}", joined);
            }
            if let Some(outgoing) = node["outgoing_edges"].as_array()
                && !outgoing.is_empty()
            {
                let joined = outgoing
                    .iter()
                    .map(|edge| {
                        format!(
                            "{} -> {}",
                            edge["name"].as_str().unwrap_or_default(),
                            edge["to_type"].as_str().unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("  outgoing: {}", joined);
            }
            if let Some(incoming) = node["incoming_edges"].as_array()
                && !incoming.is_empty()
            {
                let joined = incoming
                    .iter()
                    .map(|edge| {
                        format!(
                            "{} <- {}",
                            edge["name"].as_str().unwrap_or_default(),
                            edge["from_type"].as_str().unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("  incoming: {}", joined);
            }
            if verbose && let Some(dataset_path) = node["dataset_path"].as_str() {
                println!("  dataset: {}", dataset_path);
            }
            if let Some(props) = node["properties"].as_array() {
                for prop in props {
                    let mut anns: Vec<String> = Vec::new();
                    if prop["key"].as_bool().unwrap_or(false) {
                        anns.push("@key".to_string());
                    }
                    if prop["unique"].as_bool().unwrap_or(false) {
                        anns.push("@unique".to_string());
                    }
                    if prop["index"].as_bool().unwrap_or(false) {
                        anns.push("@index".to_string());
                    }
                    if let Some(source) = prop["embed_source"].as_str() {
                        anns.push(format!("@embed({})", source));
                    }
                    let ann_suffix = if anns.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", anns.join(" "))
                    };
                    println!(
                        "  - {}: {}{}",
                        prop["name"].as_str().unwrap_or_default(),
                        prop["type"].as_str().unwrap_or_default(),
                        ann_suffix
                    );
                    if let Some(description) = prop["description"].as_str() {
                        println!("    description: {}", description);
                    }
                }
            }
        }
    }
    println!();

    println!("{}", style_label("Edge Types", color));
    if let Some(edges) = payload["edges"].as_array() {
        for edge in edges {
            print!(
                "- {}: {} -> {} (rows={}",
                edge["name"].as_str().unwrap_or_default(),
                edge["src_type"].as_str().unwrap_or_default(),
                edge["dst_type"].as_str().unwrap_or_default(),
                edge["rows"].as_u64().unwrap_or(0),
            );
            if verbose {
                let version = edge["dataset_version"]
                    .as_u64()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string());
                print!(
                    ", type_id={}, dataset_version={}",
                    edge["type_id"].as_u64().unwrap_or(0),
                    version
                );
            }
            println!(")");
            if let Some(description) = edge["description"].as_str() {
                println!("  description: {}", description);
            }
            if let Some(instruction) = edge["instruction"].as_str() {
                println!("  instruction: {}", instruction);
            }
            if let Some(endpoint_keys) = edge["endpoint_keys"].as_object() {
                println!(
                    "  endpoint keys: {} -> {}",
                    endpoint_keys
                        .get("src")
                        .and_then(|value| value.as_str())
                        .unwrap_or("-"),
                    endpoint_keys
                        .get("dst")
                        .and_then(|value| value.as_str())
                        .unwrap_or("-")
                );
            }
            if verbose && let Some(dataset_path) = edge["dataset_path"].as_str() {
                println!("  dataset: {}", dataset_path);
            }
            if let Some(props) = edge["properties"].as_array() {
                for prop in props {
                    println!(
                        "  - {}: {}",
                        prop["name"].as_str().unwrap_or_default(),
                        prop["type"].as_str().unwrap_or_default()
                    );
                    if let Some(description) = prop["description"].as_str() {
                        println!("    description: {}", description);
                    }
                }
            }
        }
    }
}

#[instrument(fields(db_path = %db_path.display(), format = format, no_embeddings = no_embeddings))]
pub(crate) async fn cmd_export(
    db_path: PathBuf,
    format: &str,
    json: bool,
    no_embeddings: bool,
) -> Result<()> {
    let effective_format = if json { "json" } else { format };
    let include_internal_fields = effective_format == "json";
    let rows = build_export_rows(&db_path, include_internal_fields, !no_embeddings).await?;

    match effective_format {
        "jsonl" => {
            for row in rows {
                println!(
                    "{}",
                    serde_json::to_string(&row).wrap_err("failed to serialize export row")?
                );
            }
        }
        "json" => {
            let out =
                serde_json::to_string_pretty(&rows).wrap_err("failed to serialize export JSON")?;
            println!("{}", out);
        }
        other => return Err(eyre!("unknown format: {} (supported: jsonl, json)", other)),
    }

    Ok(())
}

pub(crate) async fn build_export_rows(
    db_path: &Path,
    include_internal_fields: bool,
    include_embeddings: bool,
) -> Result<Vec<serde_json::Value>> {
    build_export_rows_at_path(db_path, include_internal_fields, include_embeddings)
        .await
        .map_err(Into::into)
}

fn prop_type_string(prop: &nanograph::schema_ir::PropDef) -> String {
    let base = if prop.enum_values.is_empty() {
        prop.scalar_type.clone()
    } else {
        format!("enum({})", prop.enum_values.join(", "))
    };
    let wrapped = if prop.list {
        format!("[{}]", base)
    } else {
        base
    };
    if prop.nullable {
        format!("{}?", wrapped)
    } else {
        wrapped
    }
}
