use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;

use ariadne::{Color, Label, Report, ReportKind, Source};
use arrow_array::{ArrayRef, RecordBatch};
use clap::{Parser, Subcommand};
use color_eyre::eyre::{Result, WrapErr, eyre};
use tracing::{debug, info, instrument, warn};
use tracing_subscriber::EnvFilter;

mod config;

use config::LoadedConfig;
use nanograph::ParamMap;
use nanograph::error::{NanoError, ParseDiagnostic};
use nanograph::query::ast::Literal;
use nanograph::query::parser::parse_query_diagnostic;
use nanograph::query::typecheck::{CheckedQuery, typecheck_query_decl};
use nanograph::schema::parser::parse_schema_diagnostic;
use nanograph::store::database::{
    CdcAnalyticsMaterializeOptions, CleanupOptions, CompactOptions, Database, DeleteOp,
    DeletePredicate, LoadMode,
};
use nanograph::store::manifest::GraphManifest;
use nanograph::store::migration::{
    MigrationExecution, MigrationPlan, MigrationStatus, MigrationStep, SchemaCompatibility,
    SchemaDiffReport, analyze_schema_diff, execute_schema_migration,
};
use nanograph::store::txlog::{CdcLogEntry, read_visible_cdc_entries};

#[derive(Parser)]
#[command(
    name = "nanograph",
    about = "nanograph — on-device typed property graph DB",
    version
)]
struct Cli {
    /// Emit machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,
    /// Load defaults from the given nanograph.toml file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show binary and optional database manifest version information
    Version {
        /// Optional database directory for manifest/db version details
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Describe database schema, manifest, and dataset summaries
    Describe {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Output format: table or json
        #[arg(long)]
        format: Option<String>,
        /// Show a single node or edge type
        #[arg(long = "type")]
        type_name: Option<String>,
    },
    /// Export full graph as JSONL or JSON (nodes first, then edges)
    Export {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Output format: jsonl or json
        #[arg(long)]
        format: Option<String>,
    },
    /// Compare two schema files without opening a database
    SchemaDiff {
        /// Existing schema file
        #[arg(long = "from")]
        from_schema: PathBuf,
        /// Desired schema file
        #[arg(long = "to")]
        to_schema: PathBuf,
        /// Output format: table or json
        #[arg(long)]
        format: Option<String>,
    },
    /// Initialize a new database
    Init {
        /// Path to the database directory
        db_path: Option<PathBuf>,
        #[arg(long)]
        schema: Option<PathBuf>,
    },
    /// Load data into an existing database
    Load {
        /// Path to the database directory
        db_path: Option<PathBuf>,
        #[arg(long)]
        data: PathBuf,
        /// Load mode: overwrite, append, or merge
        #[arg(long, value_enum)]
        mode: LoadModeArg,
    },
    /// Delete nodes by predicate, cascading incident edges
    Delete {
        /// Path to the database directory
        db_path: Option<PathBuf>,
        /// Node type name
        #[arg(long = "type")]
        type_name: String,
        /// Predicate expression, e.g. name=Alice or age>=30
        #[arg(long = "where")]
        predicate: String,
    },
    /// Stream CDC events from committed transactions
    Changes {
        /// Path to the database directory
        db_path: Option<PathBuf>,
        /// Return changes with db_version strictly greater than this value
        #[arg(long, conflicts_with_all = ["from_version", "to_version"])]
        since: Option<u64>,
        /// Inclusive lower bound for db_version (requires --to)
        #[arg(long = "from", requires = "to_version", conflicts_with = "since")]
        from_version: Option<u64>,
        /// Inclusive upper bound for db_version (requires --from)
        #[arg(long = "to", requires = "from_version", conflicts_with = "since")]
        to_version: Option<u64>,
        /// Output format: jsonl or json
        #[arg(long)]
        format: Option<String>,
    },
    /// Compact Lance datasets and commit updated pinned dataset versions
    Compact {
        /// Path to the database directory
        db_path: Option<PathBuf>,
        /// Target row count per compacted fragment
        #[arg(long, default_value_t = 1_048_576)]
        target_rows_per_fragment: usize,
        /// Whether to materialize deleted rows during compaction
        #[arg(long, default_value_t = true)]
        materialize_deletions: bool,
        /// Deletion fraction threshold for materialization
        #[arg(long, default_value_t = 0.1)]
        materialize_deletions_threshold: f32,
    },
    /// Prune old tx/CDC history and old Lance versions while keeping replay window
    Cleanup {
        /// Path to the database directory
        db_path: Option<PathBuf>,
        /// Keep this many latest tx versions for CDC replay
        #[arg(long, default_value_t = 128)]
        retain_tx_versions: u64,
        /// Keep at least this many latest versions per Lance dataset
        #[arg(long, default_value_t = 2)]
        retain_dataset_versions: usize,
    },
    /// Run consistency checks on manifest, datasets, logs, and graph integrity
    Doctor {
        /// Path to the database directory
        db_path: Option<PathBuf>,
    },
    /// Materialize visible CDC into a derived Lance analytics dataset
    CdcMaterialize {
        /// Path to the database directory
        db_path: Option<PathBuf>,
        /// Minimum number of new visible CDC rows required to run materialization
        #[arg(long, default_value_t = 0)]
        min_new_rows: usize,
        /// Force materialization regardless of threshold
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Diff and apply schema migration from <db>/schema.pg
    Migrate {
        /// Path to the database directory
        db_path: Option<PathBuf>,
        /// Show migration plan without applying writes
        #[arg(long)]
        dry_run: bool,
        /// Output format: table or json
        #[arg(long)]
        format: Option<String>,
        /// Apply confirm-level steps without interactive prompts
        #[arg(long)]
        auto_approve: bool,
    },
    /// Parse and typecheck query files
    Check {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long)]
        query: PathBuf,
    },
    /// Run a named query against data
    Run {
        /// Optional query alias defined under [query_aliases] in nanograph.toml
        alias: Option<String>,
        /// Positional values for alias-declared query params
        args: Vec<String>,
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long)]
        query: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        format: Option<String>,
        /// Query parameters (repeatable), e.g. --param name="Alice"
        #[arg(long = "param", value_parser = parse_param)]
        params: Vec<(String, String)>,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum LoadModeArg {
    Overwrite,
    Append,
    Merge,
}

impl From<LoadModeArg> for LoadMode {
    fn from(value: LoadModeArg) -> Self {
        match value {
            LoadModeArg::Overwrite => LoadMode::Overwrite,
            LoadModeArg::Append => LoadMode::Append,
            LoadModeArg::Merge => LoadMode::Merge,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();

    let cli = Cli::parse();
    let config = LoadedConfig::load(cli.config.as_deref())?;
    load_dotenv_for_process(&config.base_dir);
    config.apply_embedding_env_for_process()?;
    let json = config.effective_json(cli.json);

    match cli.command {
        Commands::Version { db } => cmd_version(config.resolve_optional_db_path(db), json).await,
        Commands::Describe {
            db,
            format,
            type_name,
        } => {
            let db = config.resolve_db_path(db)?;
            let format = config.resolve_format(format.as_deref(), "table", &["table", "json"])?;
            cmd_describe(db, &format, json, type_name.as_deref()).await
        }
        Commands::Export { db, format } => {
            let db = config.resolve_db_path(db)?;
            let format = config.resolve_format(format.as_deref(), "jsonl", &["jsonl", "json"])?;
            cmd_export(db, &format, json).await
        }
        Commands::SchemaDiff {
            from_schema,
            to_schema,
            format,
        } => {
            let format = config.resolve_format(format.as_deref(), "table", &["table", "json"])?;
            cmd_schema_diff(&from_schema, &to_schema, &format, json).await
        }
        Commands::Init { db_path, schema } => {
            let db_path = config.resolve_db_path(db_path)?;
            let schema = config.resolve_schema_path(schema)?;
            cmd_init(&db_path, &schema, json).await
        }
        Commands::Load {
            db_path,
            data,
            mode,
        } => {
            let db_path = config.resolve_db_path(db_path)?;
            cmd_load(&db_path, &data, mode, json).await
        }
        Commands::Delete {
            db_path,
            type_name,
            predicate,
        } => {
            let db_path = config.resolve_db_path(db_path)?;
            cmd_delete(&db_path, &type_name, &predicate, json).await
        }
        Commands::Changes {
            db_path,
            since,
            from_version,
            to_version,
            format,
        } => {
            let db_path = config.resolve_db_path(db_path)?;
            let format = config.resolve_format(format.as_deref(), "jsonl", &["jsonl", "json"])?;
            cmd_changes(&db_path, since, from_version, to_version, &format, json).await
        }
        Commands::Compact {
            db_path,
            target_rows_per_fragment,
            materialize_deletions,
            materialize_deletions_threshold,
        } => {
            let db_path = config.resolve_db_path(db_path)?;
            cmd_compact(
                &db_path,
                target_rows_per_fragment,
                materialize_deletions,
                materialize_deletions_threshold,
                json,
            )
            .await
        }
        Commands::Cleanup {
            db_path,
            retain_tx_versions,
            retain_dataset_versions,
        } => {
            let db_path = config.resolve_db_path(db_path)?;
            cmd_cleanup(&db_path, retain_tx_versions, retain_dataset_versions, json).await
        }
        Commands::Doctor { db_path } => {
            let db_path = config.resolve_db_path(db_path)?;
            cmd_doctor(&db_path, json).await
        }
        Commands::CdcMaterialize {
            db_path,
            min_new_rows,
            force,
        } => {
            let db_path = config.resolve_db_path(db_path)?;
            cmd_cdc_materialize(&db_path, min_new_rows, force, json).await
        }
        Commands::Migrate {
            db_path,
            dry_run,
            format,
            auto_approve,
        } => {
            let db_path = config.resolve_db_path(db_path)?;
            let format = config.resolve_format(format.as_deref(), "table", &["table", "json"])?;
            cmd_migrate(&db_path, dry_run, &format, auto_approve, json).await
        }
        Commands::Check { db, query } => {
            let db = config.resolve_db_path(db)?;
            let query = config.resolve_query_path(&query)?;
            cmd_check(db, &query, json).await
        }
        Commands::Run {
            alias,
            args,
            db,
            query,
            name,
            format,
            params,
        } => {
            let db = config.resolve_db_path(db)?;
            let run = config.resolve_run_config(
                alias.as_deref(),
                query,
                name.as_deref(),
                format.as_deref(),
            )?;
            let params =
                merge_run_params(alias.as_deref(), &run.positional_param_names, args, params)?;
            cmd_run(
                db,
                &run.query_path,
                &run.query_name,
                &run.format,
                params,
                json,
            )
            .await
        }
    }?;

    Ok(())
}

#[instrument(skip(format), fields(db_path = %db_path.display(), dry_run = dry_run, format = format))]
async fn cmd_migrate(
    db_path: &Path,
    dry_run: bool,
    format: &str,
    auto_approve: bool,
    json: bool,
) -> Result<()> {
    let schema_path = db_path.join("schema.pg");
    let schema_src = std::fs::read_to_string(&schema_path)
        .wrap_err_with(|| format!("failed to read schema: {}", schema_path.display()))?;
    let _ = parse_schema_or_report(&schema_path, &schema_src)?;

    let execution = execute_schema_migration(db_path, dry_run, auto_approve).await?;
    let effective_format = if json { "json" } else { format };
    render_migration_execution(&execution, effective_format)?;

    match execution.status {
        MigrationStatus::Applied => {
            info!("schema migration completed");
            Ok(())
        }
        MigrationStatus::NeedsConfirmation => {
            std::process::exit(2);
        }
        MigrationStatus::Blocked => {
            std::process::exit(3);
        }
    }
}

fn render_migration_execution(execution: &MigrationExecution, format: &str) -> Result<()> {
    match format {
        "json" => {
            let payload = serde_json::json!({
                "status": migration_status_label(execution.status),
                "plan": execution.plan,
            });
            let out = serde_json::to_string_pretty(&payload)
                .wrap_err("failed to serialize migration JSON")?;
            println!("{}", out);
        }
        "table" => print_migration_plan_table(&execution.plan),
        other => return Err(eyre!("unknown format: {}", other)),
    }

    Ok(())
}

fn migration_status_label(status: MigrationStatus) -> &'static str {
    match status {
        MigrationStatus::Applied => "applied",
        MigrationStatus::NeedsConfirmation => "needs_confirmation",
        MigrationStatus::Blocked => "blocked",
    }
}

fn print_migration_plan_table(plan: &MigrationPlan) {
    println!("Migration Plan");
    println!("  DB: {}", plan.db_path);
    println!("  Old schema hash: {}", plan.old_schema_hash);
    println!("  New schema hash: {}", plan.new_schema_hash);
    println!();

    if !plan.steps.is_empty() {
        println!("{:<10} {:<24} DETAIL", "SAFETY", "STEP");
        for planned in &plan.steps {
            let safety = match planned.safety {
                nanograph::store::migration::MigrationSafety::Safe => "safe",
                nanograph::store::migration::MigrationSafety::Confirm => "confirm",
                nanograph::store::migration::MigrationSafety::Blocked => "blocked",
            };
            let step = migration_step_kind(&planned.step);
            println!("{:<10} {:<24} {}", safety, step, planned.reason);
        }
    } else {
        println!("No migration steps.");
    }

    if !plan.warnings.is_empty() {
        println!();
        println!("Warnings:");
        for w in &plan.warnings {
            println!("- {}", w);
        }
    }
    if !plan.blocked.is_empty() {
        println!();
        println!("Blocked:");
        for b in &plan.blocked {
            println!("- {}", b);
        }
    }
}

fn migration_step_kind(step: &MigrationStep) -> &'static str {
    match step {
        MigrationStep::AddNodeType { .. } => "AddNodeType",
        MigrationStep::AddEdgeType { .. } => "AddEdgeType",
        MigrationStep::DropNodeType { .. } => "DropNodeType",
        MigrationStep::DropEdgeType { .. } => "DropEdgeType",
        MigrationStep::RenameType { .. } => "RenameType",
        MigrationStep::AddProperty { .. } => "AddProperty",
        MigrationStep::DropProperty { .. } => "DropProperty",
        MigrationStep::RenameProperty { .. } => "RenameProperty",
        MigrationStep::AlterPropertyType { .. } => "AlterPropertyType",
        MigrationStep::AlterPropertyNullability { .. } => "AlterPropertyNullability",
        MigrationStep::AlterPropertyKey { .. } => "AlterPropertyKey",
        MigrationStep::AlterPropertyUnique { .. } => "AlterPropertyUnique",
        MigrationStep::AlterPropertyIndex { .. } => "AlterPropertyIndex",
        MigrationStep::AlterPropertyEnumValues { .. } => "AlterPropertyEnumValues",
        MigrationStep::AlterMetadata { .. } => "AlterMetadata",
        MigrationStep::RebindEdgeEndpoints { .. } => "RebindEdgeEndpoints",
    }
}

#[instrument(skip(format), fields(from_schema = %from_schema.display(), to_schema = %to_schema.display(), format = format))]
async fn cmd_schema_diff(
    from_schema: &PathBuf,
    to_schema: &PathBuf,
    format: &str,
    json: bool,
) -> Result<()> {
    let old_source = std::fs::read_to_string(from_schema)
        .wrap_err_with(|| format!("failed to read schema: {}", from_schema.display()))?;
    let new_source = std::fs::read_to_string(to_schema)
        .wrap_err_with(|| format!("failed to read schema: {}", to_schema.display()))?;
    let old_schema = parse_schema_or_report(from_schema, &old_source)?;
    let new_schema = parse_schema_or_report(to_schema, &new_source)?;
    let report = analyze_schema_diff(&old_schema, &new_schema)?;
    let effective_format = if json { "json" } else { format };
    render_schema_diff_report(&report, effective_format)
}

fn render_schema_diff_report(report: &SchemaDiffReport, format: &str) -> Result<()> {
    match format {
        "json" => {
            let out = serde_json::to_string_pretty(report)
                .wrap_err("failed to serialize schema diff JSON")?;
            println!("{}", out);
        }
        "table" => {
            println!("Schema Diff");
            println!("  Old schema hash: {}", report.old_schema_hash);
            println!("  New schema hash: {}", report.new_schema_hash);
            println!(
                "  Compatibility: {}",
                schema_compatibility_label(report.compatibility)
            );
            println!("  Has breaking: {}", report.has_breaking);
            println!();

            if report.steps.is_empty() {
                println!("No schema changes.");
            } else {
                println!("{:<28} {:<30} DETAIL", "CLASSIFICATION", "STEP");
                for step in &report.steps {
                    println!(
                        "{:<28} {:<30} {}",
                        schema_compatibility_label(step.classification),
                        migration_step_kind(&step.step),
                        step.reason
                    );
                    if let Some(remediation) = &step.remediation {
                        println!("  remediation: {}", remediation);
                    }
                }
            }

            if !report.warnings.is_empty() {
                println!();
                println!("Warnings:");
                for warning in &report.warnings {
                    println!("- {}", warning);
                }
            }
            if !report.blocked.is_empty() {
                println!();
                println!("Blocked:");
                for item in &report.blocked {
                    println!("- {}", item);
                }
            }
        }
        other => return Err(eyre!("unknown format: {} (supported: table, json)", other)),
    }
    Ok(())
}

fn schema_compatibility_label(value: SchemaCompatibility) -> &'static str {
    match value {
        SchemaCompatibility::Additive => "additive",
        SchemaCompatibility::CompatibleWithConfirmation => "compatible_with_confirmation",
        SchemaCompatibility::Breaking => "breaking",
        SchemaCompatibility::Blocked => "blocked",
    }
}

#[instrument(fields(db = ?db_path.as_ref().map(|p| p.display().to_string())))]
async fn cmd_version(db_path: Option<PathBuf>, json: bool) -> Result<()> {
    let payload = build_version_payload(db_path.as_deref())?;

    if json {
        let out =
            serde_json::to_string_pretty(&payload).wrap_err("failed to serialize version JSON")?;
        println!("{}", out);
        return Ok(());
    }

    print_version_table(&payload);
    Ok(())
}

fn build_version_payload(db_path: Option<&Path>) -> Result<serde_json::Value> {
    let mut payload = serde_json::json!({
        "binary_version": env!("CARGO_PKG_VERSION"),
    });

    if let Some(path) = db_path {
        let manifest = GraphManifest::read(path)?;
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
    println!(
        "nanograph {}",
        payload["binary_version"].as_str().unwrap_or_default()
    );
    if let Some(db) = payload.get("db") {
        println!("Database: {}", db["path"].as_str().unwrap_or_default());
        println!(
            "Manifest: format v{}, db_version {}",
            db["format_version"].as_u64().unwrap_or(0),
            db["db_version"].as_u64().unwrap_or(0)
        );
        println!(
            "Last TX: {} @ {}",
            db["last_tx_id"].as_str().unwrap_or_default(),
            db["committed_at"].as_str().unwrap_or_default()
        );
        println!(
            "Schema hash: {} (identity v{})",
            db["schema_ir_hash"].as_str().unwrap_or_default(),
            db["schema_identity_version"].as_u64().unwrap_or(0)
        );
        println!(
            "Next IDs: node={} edge={} type={} prop={}",
            db["next_node_id"].as_u64().unwrap_or(0),
            db["next_edge_id"].as_u64().unwrap_or(0),
            db["next_type_id"].as_u64().unwrap_or(0),
            db["next_prop_id"].as_u64().unwrap_or(0)
        );
        println!("Datasets: {}", db["dataset_count"].as_u64().unwrap_or(0));
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
async fn cmd_describe(
    db_path: PathBuf,
    format: &str,
    json: bool,
    type_name: Option<&str>,
) -> Result<()> {
    let db = Database::open(&db_path).await?;
    let manifest = GraphManifest::read(&db_path)?;
    let payload = build_describe_payload(&db_path, &db, &manifest, type_name)?;
    let effective_format = if json { "json" } else { format };

    match effective_format {
        "json" => {
            let out = serde_json::to_string_pretty(&payload)
                .wrap_err("failed to serialize describe JSON")?;
            println!("{}", out);
        }
        "table" => print_describe_table(&payload),
        other => return Err(eyre!("unknown format: {} (supported: table, json)", other)),
    }

    Ok(())
}

fn build_describe_payload(
    db_path: &Path,
    db: &Database,
    manifest: &GraphManifest,
    type_name: Option<&str>,
) -> Result<serde_json::Value> {
    let storage = db.snapshot();
    let dataset_map = manifest
        .datasets
        .iter()
        .map(|d| ((d.kind.clone(), d.type_name.clone()), d))
        .collect::<HashMap<_, _>>();

    let mut nodes = Vec::new();
    for node in db.schema_ir.node_types() {
        if let Some(type_name) = type_name
            && node.name != type_name
        {
            continue;
        }
        let rows = storage
            .get_all_nodes(&node.name)?
            .map(|b| b.num_rows() as u64)
            .unwrap_or(0);
        let dataset = dataset_map.get(&("node".to_string(), node.name.clone()));
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
        let outgoing_edges = db
            .schema_ir
            .edge_types()
            .filter(|edge| edge.src_type_name == node.name)
            .map(|edge| {
                serde_json::json!({
                    "name": edge.name,
                    "to_type": edge.dst_type_name,
                })
            })
            .collect::<Vec<_>>();
        let incoming_edges = db
            .schema_ir
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
            "rows": rows,
            "dataset_path": dataset.map(|d| d.dataset_path.clone()),
            "dataset_version": dataset.map(|d| d.dataset_version),
            "properties": properties,
        }));
    }

    let mut edges = Vec::new();
    for edge in db.schema_ir.edge_types() {
        if let Some(type_name) = type_name
            && edge.name != type_name
        {
            continue;
        }
        let rows = storage
            .edge_batch_for_save(&edge.name)?
            .map(|b| b.num_rows() as u64)
            .unwrap_or(0);
        let dataset = dataset_map.get(&("edge".to_string(), edge.name.clone()));
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
                "src": db.schema_ir.node_key_property_name(&edge.src_type_name),
                "dst": db.schema_ir.node_key_property_name(&edge.dst_type_name),
            },
            "rows": rows,
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
        "schema_ir_version": db.schema_ir.ir_version,
        "nodes": nodes,
        "edges": edges,
    }))
}

fn print_describe_table(payload: &serde_json::Value) {
    println!(
        "Database: {}",
        payload["db_path"].as_str().unwrap_or_default()
    );
    println!(
        "Manifest: format v{}, db_version {}",
        payload["manifest"]["format_version"].as_u64().unwrap_or(0),
        payload["manifest"]["db_version"].as_u64().unwrap_or(0)
    );
    println!(
        "Last TX: {} @ {}",
        payload["manifest"]["last_tx_id"]
            .as_str()
            .unwrap_or_default(),
        payload["manifest"]["committed_at"]
            .as_str()
            .unwrap_or_default()
    );
    println!(
        "Schema: ir_version {}, hash {}",
        payload["schema_ir_version"].as_u64().unwrap_or(0),
        payload["manifest"]["schema_ir_hash"]
            .as_str()
            .unwrap_or_default()
    );
    println!();

    println!("Node Types");
    if let Some(nodes) = payload["nodes"].as_array() {
        for node in nodes {
            let version = node["dataset_version"]
                .as_u64()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "- {} (type_id={}, rows={}, dataset_version={})",
                node["name"].as_str().unwrap_or_default(),
                node["type_id"].as_u64().unwrap_or(0),
                node["rows"].as_u64().unwrap_or(0),
                version,
            );
            if let Some(description) = node["description"].as_str() {
                println!("  description: {}", description);
            }
            if let Some(instruction) = node["instruction"].as_str() {
                println!("  instruction: {}", instruction);
            }
            if let Some(key_property) = node["key_property"].as_str() {
                println!("  key: {}", key_property);
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

    println!("Edge Types");
    if let Some(edges) = payload["edges"].as_array() {
        for edge in edges {
            let version = edge["dataset_version"]
                .as_u64()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "- {}: {} -> {} (type_id={}, rows={}, dataset_version={})",
                edge["name"].as_str().unwrap_or_default(),
                edge["src_type"].as_str().unwrap_or_default(),
                edge["dst_type"].as_str().unwrap_or_default(),
                edge["type_id"].as_u64().unwrap_or(0),
                edge["rows"].as_u64().unwrap_or(0),
                version,
            );
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

#[instrument(fields(db_path = %db_path.display(), format = format))]
async fn cmd_export(db_path: PathBuf, format: &str, json: bool) -> Result<()> {
    let db = Database::open(&db_path).await?;
    let effective_format = if json { "json" } else { format };
    let include_internal_fields = effective_format == "json";
    let rows = build_export_rows(&db, include_internal_fields)?;

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

fn build_export_rows(
    db: &Database,
    include_internal_fields: bool,
) -> Result<Vec<serde_json::Value>> {
    use arrow_array::{Array, UInt64Array};

    let storage = db.snapshot();
    let mut rows = Vec::new();
    let mut node_key_tokens: HashMap<String, HashMap<u64, String>> = HashMap::new();

    for node in db.schema_ir.node_types() {
        let Some(batch) = storage.get_all_nodes(&node.name)? else {
            continue;
        };
        let id_arr = batch
            .column_by_name("id")
            .and_then(|col| col.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| eyre!("node batch '{}' missing UInt64 id column", node.name))?;
        let key_prop = node
            .properties
            .iter()
            .find(|prop| prop.key)
            .map(|prop| prop.name.as_str());
        let key_col = match key_prop {
            Some(prop_name) => {
                let key_idx =
                    node_property_index(batch.schema().as_ref(), prop_name).ok_or_else(|| {
                        eyre!(
                            "node batch '{}' missing @key property '{}'",
                            node.name,
                            prop_name
                        )
                    })?;
                Some((prop_name.to_string(), batch.column(key_idx).clone()))
            }
            None => None,
        };

        let mut key_tokens = HashMap::new();
        for row_idx in 0..batch.num_rows() {
            let id = id_arr.value(row_idx);
            if let Some((prop_name, key_array)) = key_col.as_ref() {
                let key_token = export_key_token(key_array, row_idx, prop_name)?;
                key_tokens.insert(id, key_token);
            }

            let data = export_data_map(&batch, row_idx, &[0]);
            let mut row = serde_json::json!({
                "type": node.name,
                "data": data,
            });
            if include_internal_fields {
                row["id"] = serde_json::Value::Number(id.into());
            }
            rows.push(row);
        }
        if !key_tokens.is_empty() {
            node_key_tokens.insert(node.name.clone(), key_tokens);
        }
    }

    for edge in db.schema_ir.edge_types() {
        let Some(batch) = storage.edge_batch_for_save(&edge.name)? else {
            continue;
        };
        let id_arr = batch
            .column_by_name("id")
            .and_then(|col| col.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| eyre!("edge batch '{}' missing UInt64 id column", edge.name))?;
        let src_arr = batch
            .column_by_name("src")
            .and_then(|col| col.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| eyre!("edge batch '{}' missing UInt64 src column", edge.name))?;
        let dst_arr = batch
            .column_by_name("dst")
            .and_then(|col| col.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| eyre!("edge batch '{}' missing UInt64 dst column", edge.name))?;

        for row_idx in 0..batch.num_rows() {
            let id = id_arr.value(row_idx);
            let src = src_arr.value(row_idx);
            let dst = dst_arr.value(row_idx);
            let from = node_key_tokens
                .get(&edge.src_type_name)
                .and_then(|m| m.get(&src))
                .cloned()
                .ok_or_else(|| {
                    eyre!(
                        "cannot export portable edge '{}': source {} node {} is missing an @key token",
                        edge.name,
                        edge.src_type_name,
                        src
                    )
                })?;
            let to = node_key_tokens
                .get(&edge.dst_type_name)
                .and_then(|m| m.get(&dst))
                .cloned()
                .ok_or_else(|| {
                    eyre!(
                        "cannot export portable edge '{}': destination {} node {} is missing an @key token",
                        edge.name,
                        edge.dst_type_name,
                        dst
                    )
                })?;
            let data = export_data_map(&batch, row_idx, &[0, 1, 2]);

            let mut row = serde_json::json!({
                "edge": edge.name,
                "from": from,
                "to": to,
                "data": data,
            });
            if include_internal_fields {
                row["id"] = serde_json::Value::Number(id.into());
                row["src"] = serde_json::Value::Number(src.into());
                row["dst"] = serde_json::Value::Number(dst.into());
            }
            rows.push(row);
        }
    }

    Ok(rows)
}

fn node_property_index(schema: &arrow_schema::Schema, prop_name: &str) -> Option<usize> {
    schema
        .fields()
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(idx, field)| (field.name() == prop_name).then_some(idx))
}

fn export_key_token(array: &ArrayRef, row_idx: usize, prop_name: &str) -> Result<String> {
    match nanograph::json_output::array_value_to_json(array, row_idx) {
        serde_json::Value::Null => Err(eyre!("@key property {} cannot be null", prop_name)),
        serde_json::Value::String(value) => Ok(value),
        serde_json::Value::Bool(value) => Ok(value.to_string()),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        other => Err(eyre!(
            "unsupported @key export value for {}: {}",
            prop_name,
            other
        )),
    }
}

fn export_data_map(
    batch: &RecordBatch,
    row_idx: usize,
    excluded_indices: &[usize],
) -> serde_json::Value {
    let excluded = excluded_indices
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    let mut data = serde_json::Map::new();
    for (col_idx, field) in batch.schema().fields().iter().enumerate() {
        if excluded.contains(&col_idx) {
            continue;
        }
        data.insert(
            field.name().clone(),
            nanograph::json_output::array_value_to_json(batch.column(col_idx), row_idx),
        );
    }
    serde_json::Value::Object(data)
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DotenvLoadStats {
    loaded: usize,
    skipped_existing: usize,
}

fn load_dotenv_for_process(base_dir: &Path) {
    let results = load_project_dotenv_from_dir_with(
        base_dir,
        |key| std::env::var_os(key).is_some(),
        |key, value| {
            // SAFETY: this runs once during CLI process bootstrap before command execution.
            unsafe { std::env::set_var(key, value) };
        },
    );
    let mut loaded_any = false;
    for (file_name, result) in results {
        match result {
            Ok(Some(stats)) => {
                loaded_any = true;
                debug!(
                    loaded = stats.loaded,
                    skipped_existing = stats.skipped_existing,
                    dotenv_path = %base_dir.join(file_name).display(),
                    "loaded env file entries"
                );
            }
            Ok(None) => {}
            Err(err) => {
                warn!(
                    dotenv_path = %base_dir.join(file_name).display(),
                    "failed to load {}: {}",
                    file_name,
                    err
                );
            }
        }
    }
    if !loaded_any {
        debug!(cwd = %base_dir.display(), "no .env.nano or .env file found");
    }
}

fn load_project_dotenv_from_dir_with<FExists, FSet>(
    dir: &Path,
    mut exists: FExists,
    mut set: FSet,
) -> Vec<(
    &'static str,
    std::result::Result<Option<DotenvLoadStats>, String>,
)>
where
    FExists: FnMut(&str) -> bool,
    FSet: FnMut(&str, &str),
{
    let mut results = Vec::with_capacity(2);
    for file_name in [".env.nano", ".env"] {
        results.push((
            file_name,
            load_named_dotenv_from_dir_with(dir, file_name, &mut exists, &mut set),
        ));
    }
    results
}

#[cfg(test)]
fn load_dotenv_from_dir_with<FExists, FSet>(
    dir: &Path,
    exists: FExists,
    set: FSet,
) -> std::result::Result<Option<DotenvLoadStats>, String>
where
    FExists: FnMut(&str) -> bool,
    FSet: FnMut(&str, &str),
{
    load_named_dotenv_from_dir_with(dir, ".env", exists, set)
}

fn load_named_dotenv_from_dir_with<FExists, FSet>(
    dir: &Path,
    file_name: &str,
    exists: FExists,
    set: FSet,
) -> std::result::Result<Option<DotenvLoadStats>, String>
where
    FExists: FnMut(&str) -> bool,
    FSet: FnMut(&str, &str),
{
    let path = dir.join(file_name);
    if !path.exists() {
        return Ok(None);
    }
    load_dotenv_from_path_with(&path, exists, set).map(Some)
}

fn load_dotenv_from_path_with<FExists, FSet>(
    path: &Path,
    mut exists: FExists,
    mut set: FSet,
) -> std::result::Result<DotenvLoadStats, String>
where
    FExists: FnMut(&str) -> bool,
    FSet: FnMut(&str, &str),
{
    let mut stats = DotenvLoadStats::default();
    for (key, value) in parse_dotenv_entries(path)? {
        if exists(&key) {
            stats.skipped_existing += 1;
            continue;
        }
        set(&key, &value);
        stats.loaded += 1;
    }
    Ok(stats)
}

fn parse_dotenv_entries(path: &Path) -> std::result::Result<Vec<(String, String)>, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
    let mut out = Vec::new();

    for (line_no, raw_line) in source.lines().enumerate() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim_start();
        }

        let Some(eq_pos) = line.find('=') else {
            return Err(format!(
                "invalid .env line {} in {}: expected KEY=VALUE",
                line_no + 1,
                path.display()
            ));
        };
        let key = line[..eq_pos].trim();
        if !is_valid_env_key(key) {
            return Err(format!(
                "invalid .env key '{}' on line {} in {}",
                key,
                line_no + 1,
                path.display()
            ));
        }

        let value_part = line[eq_pos + 1..].trim();
        let value = parse_dotenv_value(value_part).map_err(|msg| {
            format!(
                "invalid .env value for '{}' on line {} in {}: {}",
                key,
                line_no + 1,
                path.display(),
                msg
            )
        })?;
        out.push((key.to_string(), value));
    }

    Ok(out)
}

fn parse_dotenv_value(value: &str) -> std::result::Result<String, &'static str> {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        let inner = &value[1..value.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                out.push(ch);
                continue;
            }
            let Some(next) = chars.next() else {
                return Err("unterminated escape sequence");
            };
            match next {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                other => out.push(other),
            }
        }
        return Ok(out);
    }

    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return Ok(value[1..value.len() - 1].to_string());
    }

    let unquoted = value
        .split_once(" #")
        .map(|(left, _)| left)
        .unwrap_or(value)
        .trim_end();
    Ok(unquoted.to_string())
}

fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_log_filter()));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn default_log_filter() -> &'static str {
    "error"
}

fn normalize_span(span: Option<nanograph::error::SourceSpan>, source: &str) -> Range<usize> {
    if source.is_empty() {
        return 0..0;
    }
    let len = source.len();
    match span {
        Some(s) => {
            let start = s.start.min(len.saturating_sub(1));
            let end = s.end.max(start.saturating_add(1)).min(len);
            start..end
        }
        None => 0..1.min(len),
    }
}

fn render_parse_diagnostic(path: &Path, source: &str, diag: &ParseDiagnostic) {
    let file_id = path.display().to_string();
    let span = normalize_span(diag.span, source);
    let mut report = Report::build(ReportKind::Error, file_id.clone(), span.start)
        .with_message("parse error")
        .with_label(
            Label::new((file_id.clone(), span.clone()))
                .with_color(Color::Red)
                .with_message(diag.message.clone()),
        );
    if diag.span.is_none() {
        report = report.with_note(diag.message.clone());
    }
    let _ = report
        .finish()
        .eprint((file_id.clone(), Source::from(source)));
}

fn parse_schema_or_report(path: &Path, source: &str) -> Result<nanograph::schema::ast::SchemaFile> {
    parse_schema_diagnostic(source).map_err(|diag| {
        render_parse_diagnostic(path, source, &diag);
        eyre!("schema parse failed")
    })
}

fn parse_query_or_report(path: &Path, source: &str) -> Result<nanograph::query::ast::QueryFile> {
    parse_query_diagnostic(source).map_err(|diag| {
        render_parse_diagnostic(path, source, &diag);
        eyre!("query parse failed")
    })
}

#[instrument(skip(schema_path), fields(db_path = %db_path.display()))]
async fn cmd_init(db_path: &Path, schema_path: &Path, json: bool) -> Result<()> {
    let schema_src = std::fs::read_to_string(schema_path)
        .wrap_err_with(|| format!("failed to read schema: {}", schema_path.display()))?;
    let _ = parse_schema_or_report(schema_path, &schema_src)?;

    Database::init(db_path, &schema_src).await?;
    let current_dir = std::env::current_dir().wrap_err("failed to resolve current directory")?;
    let project_dir = infer_init_project_dir(&current_dir, db_path, schema_path);
    let generated_files = scaffold_project_files(&project_dir, db_path, schema_path)?;

    info!("database initialized");
    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "ok",
                "db_path": db_path.display().to_string(),
                "schema_path": schema_path.display().to_string(),
                "generated_files": generated_files
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>(),
            })
        );
    } else {
        println!("Initialized database at {}", db_path.display());
        for path in &generated_files {
            println!("Generated {}", path.display());
        }
    }
    Ok(())
}

fn scaffold_project_files(
    project_dir: &Path,
    db_path: &Path,
    schema_path: &Path,
) -> Result<Vec<PathBuf>> {
    let mut generated = Vec::new();
    let config_path = project_dir.join("nanograph.toml");
    if write_file_if_missing(
        &config_path,
        &default_nanograph_toml(project_dir, db_path, schema_path),
    )? {
        generated.push(config_path);
    }

    let dotenv_path = project_dir.join(".env.nano");
    if write_file_if_missing(&dotenv_path, DEFAULT_DOTENV_NANO)? {
        generated.push(dotenv_path);
    }

    Ok(generated)
}

fn infer_init_project_dir(current_dir: &Path, db_path: &Path, schema_path: &Path) -> PathBuf {
    let resolved_db = resolve_against_dir(current_dir, db_path);
    let resolved_schema = resolve_against_dir(current_dir, schema_path);
    common_ancestor(&resolved_db, &resolved_schema)
        .filter(|path| path.parent().is_some())
        .unwrap_or_else(|| current_dir.to_path_buf())
}

fn resolve_against_dir(base_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn common_ancestor(left: &Path, right: &Path) -> Option<PathBuf> {
    let left_components: Vec<_> = left.components().collect();
    let right_components: Vec<_> = right.components().collect();
    let mut shared = PathBuf::new();
    let mut matched_any = false;

    for (left_component, right_component) in left_components.iter().zip(right_components.iter()) {
        if left_component != right_component {
            break;
        }
        shared.push(left_component.as_os_str());
        matched_any = true;
    }

    matched_any.then_some(shared)
}

fn write_file_if_missing(path: &Path, contents: &str) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    std::fs::write(path, contents)
        .wrap_err_with(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

fn default_nanograph_toml(project_dir: &Path, db_path: &Path, schema_path: &Path) -> String {
    let schema_value = toml_basic_string(&render_project_relative_path(project_dir, schema_path));
    let db_value = toml_basic_string(&render_project_relative_path(project_dir, db_path));
    format!(
        "# Shared nanograph project defaults.\n\
         # Keep secrets in .env.nano, not in this file.\n\n\
         [db]\n\
         default_path = {db_value}\n\n\
         [schema]\n\
         default_path = {schema_value}\n\n\
         [query]\n\
         roots = [\"queries\"]\n\n\
         [embedding]\n\
         provider = \"openai\"\n\
         model = \"text-embedding-3-small\"\n\
         batch_size = 64\n\
         chunk_size = 0\n\
         chunk_overlap_chars = 128\n\n\
         # Example:\n\
         # [query_aliases.search]\n\
         # query = \"queries/search.gq\"\n\
         # name = \"semantic_search\"\n\
         # args = [\"q\"]\n\
         # format = \"table\"\n"
    )
}

fn render_project_relative_path(project_dir: &Path, path: &Path) -> String {
    path.strip_prefix(project_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn toml_basic_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

const DEFAULT_DOTENV_NANO: &str = "\
# Local-only nanograph secrets and overrides.\n\
# Do not commit this file.\n\
# OPENAI_API_KEY=sk-...\n\
# NANOGRAPH_EMBEDDINGS_MOCK=1\n";

#[instrument(skip(data_path), fields(db_path = %db_path.display(), mode = ?mode))]
async fn cmd_load(db_path: &Path, data_path: &Path, mode: LoadModeArg, json: bool) -> Result<()> {
    let db = Database::open(db_path).await?;

    if let Err(err) = db.load_file_with_mode(data_path, mode.into()).await {
        render_load_error(db_path, &err, json);
        return Err(err.into());
    }

    info!("data load complete");
    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "ok",
                "db_path": db_path.display().to_string(),
                "data_path": data_path.display().to_string(),
                "mode": format!("{:?}", mode).to_lowercase(),
            })
        );
    } else {
        println!("Loaded data into {}", db_path.display());
    }
    Ok(())
}

fn render_load_error(db_path: &Path, err: &NanoError, json: bool) {
    if let NanoError::UniqueConstraint {
        type_name,
        property,
        value,
        first_row,
        second_row,
    } = err
    {
        if json {
            eprintln!(
                "{}",
                serde_json::json!({
                    "status": "error",
                    "error_kind": "unique_constraint",
                    "db_path": db_path.display().to_string(),
                    "type_name": type_name,
                    "property": property,
                    "value": value,
                    "first_row": first_row,
                    "second_row": second_row,
                })
            );
        } else {
            eprintln!("Load failed for {}.", db_path.display());
            eprintln!(
                "Unique constraint violation: {}.{} has duplicate value '{}'.",
                type_name, property, value
            );
            eprintln!(
                "Conflicting rows in loaded dataset: {} and {}.",
                first_row, second_row
            );
        }
    }
}

#[instrument(skip(type_name, predicate), fields(db_path = %db_path.display(), type_name = type_name))]
async fn cmd_delete(db_path: &Path, type_name: &str, predicate: &str, json: bool) -> Result<()> {
    let pred = parse_delete_predicate(predicate)?;
    let db = Database::open(db_path).await?;
    let result = db.delete_nodes(type_name, &pred).await?;

    info!(
        deleted_nodes = result.deleted_nodes,
        deleted_edges = result.deleted_edges,
        "delete complete"
    );
    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "ok",
                "db_path": db_path.display().to_string(),
                "type_name": type_name,
                "deleted_nodes": result.deleted_nodes,
                "deleted_edges": result.deleted_edges,
            })
        );
    } else {
        println!(
            "Deleted {} node(s) and {} edge(s) in {}",
            result.deleted_nodes,
            result.deleted_edges,
            db_path.display()
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChangesWindow {
    from_db_version_exclusive: u64,
    to_db_version_inclusive: Option<u64>,
}

fn resolve_changes_window(
    since: Option<u64>,
    from_version: Option<u64>,
    to_version: Option<u64>,
) -> Result<ChangesWindow> {
    if since.is_some() && (from_version.is_some() || to_version.is_some()) {
        return Err(eyre!("use either --since or --from/--to, not both"));
    }

    if let Some(since) = since {
        return Ok(ChangesWindow {
            from_db_version_exclusive: since,
            to_db_version_inclusive: None,
        });
    }

    match (from_version, to_version) {
        (Some(from), Some(to)) => {
            if from > to {
                return Err(eyre!("--from must be <= --to"));
            }
            Ok(ChangesWindow {
                from_db_version_exclusive: from.saturating_sub(1),
                to_db_version_inclusive: Some(to),
            })
        }
        (None, None) => Ok(ChangesWindow {
            from_db_version_exclusive: 0,
            to_db_version_inclusive: None,
        }),
        _ => Err(eyre!("--from and --to must be provided together")),
    }
}

#[instrument(
    skip(format),
    fields(
        db_path = %db_path.display(),
        since = since,
        from = from_version,
        to = to_version,
        format = format
    )
)]
async fn cmd_changes(
    db_path: &Path,
    since: Option<u64>,
    from_version: Option<u64>,
    to_version: Option<u64>,
    format: &str,
    json: bool,
) -> Result<()> {
    let window = resolve_changes_window(since, from_version, to_version)?;
    let rows = read_visible_cdc_entries(
        db_path,
        window.from_db_version_exclusive,
        window.to_db_version_inclusive,
    )?;

    let effective_format = if json { "json" } else { format };
    render_changes(effective_format, &rows)
}

fn render_changes(format: &str, rows: &[CdcLogEntry]) -> Result<()> {
    match format {
        "jsonl" => {
            for row in rows {
                let line = serde_json::to_string(row).wrap_err("failed to serialize CDC row")?;
                println!("{}", line);
            }
        }
        "json" => {
            let out =
                serde_json::to_string_pretty(rows).wrap_err("failed to serialize CDC rows")?;
            println!("{}", out);
        }
        other => {
            return Err(eyre!("unknown format: {} (supported: jsonl, json)", other));
        }
    }
    Ok(())
}

#[instrument(
    fields(
        db_path = %db_path.display(),
        target_rows_per_fragment = target_rows_per_fragment,
        materialize_deletions = materialize_deletions,
        materialize_deletions_threshold = materialize_deletions_threshold
    )
)]
async fn cmd_compact(
    db_path: &Path,
    target_rows_per_fragment: usize,
    materialize_deletions: bool,
    materialize_deletions_threshold: f32,
    json: bool,
) -> Result<()> {
    let db = Database::open(db_path).await?;
    let result = db
        .compact(CompactOptions {
            target_rows_per_fragment,
            materialize_deletions,
            materialize_deletions_threshold,
        })
        .await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "ok",
                "db_path": db_path.display().to_string(),
                "datasets_considered": result.datasets_considered,
                "datasets_compacted": result.datasets_compacted,
                "fragments_removed": result.fragments_removed,
                "fragments_added": result.fragments_added,
                "files_removed": result.files_removed,
                "files_added": result.files_added,
                "manifest_committed": result.manifest_committed,
            })
        );
    } else {
        println!(
            "Compaction complete for {} (datasets compacted: {}, fragments -{} +{}, files -{} +{}, manifest committed: {})",
            db_path.display(),
            result.datasets_compacted,
            result.fragments_removed,
            result.fragments_added,
            result.files_removed,
            result.files_added,
            result.manifest_committed
        );
    }
    Ok(())
}

#[instrument(
    fields(
        db_path = %db_path.display(),
        retain_tx_versions = retain_tx_versions,
        retain_dataset_versions = retain_dataset_versions
    )
)]
async fn cmd_cleanup(
    db_path: &Path,
    retain_tx_versions: u64,
    retain_dataset_versions: usize,
    json: bool,
) -> Result<()> {
    let db = Database::open(db_path).await?;
    let result = db
        .cleanup(CleanupOptions {
            retain_tx_versions,
            retain_dataset_versions,
        })
        .await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "ok",
                "db_path": db_path.display().to_string(),
                "tx_rows_removed": result.tx_rows_removed,
                "tx_rows_kept": result.tx_rows_kept,
                "cdc_rows_removed": result.cdc_rows_removed,
                "cdc_rows_kept": result.cdc_rows_kept,
                "datasets_cleaned": result.datasets_cleaned,
                "dataset_old_versions_removed": result.dataset_old_versions_removed,
                "dataset_bytes_removed": result.dataset_bytes_removed,
            })
        );
    } else {
        println!(
            "Cleanup complete for {} (tx removed {}, cdc removed {}, datasets cleaned {}, old versions removed {}, bytes removed {})",
            db_path.display(),
            result.tx_rows_removed,
            result.cdc_rows_removed,
            result.datasets_cleaned,
            result.dataset_old_versions_removed,
            result.dataset_bytes_removed
        );
    }

    Ok(())
}

#[instrument(
    fields(
        db_path = %db_path.display(),
        min_new_rows = min_new_rows,
        force = force
    )
)]
async fn cmd_cdc_materialize(
    db_path: &Path,
    min_new_rows: usize,
    force: bool,
    json: bool,
) -> Result<()> {
    let db = Database::open(db_path).await?;
    let result = db
        .materialize_cdc_analytics(CdcAnalyticsMaterializeOptions {
            min_new_rows,
            force,
        })
        .await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "ok",
                "db_path": db_path.display().to_string(),
                "source_rows": result.source_rows,
                "previously_materialized_rows": result.previously_materialized_rows,
                "new_rows_since_last_run": result.new_rows_since_last_run,
                "materialized_rows": result.materialized_rows,
                "dataset_written": result.dataset_written,
                "skipped_by_threshold": result.skipped_by_threshold,
                "dataset_version": result.dataset_version,
            })
        );
    } else if result.skipped_by_threshold {
        println!(
            "CDC analytics materialization skipped for {} (new rows {}, threshold {})",
            db_path.display(),
            result.new_rows_since_last_run,
            min_new_rows
        );
    } else {
        println!(
            "CDC analytics materialized for {} (rows {}, dataset written {}, version {:?})",
            db_path.display(),
            result.materialized_rows,
            result.dataset_written,
            result.dataset_version
        );
    }

    Ok(())
}

#[instrument(fields(db_path = %db_path.display()))]
async fn cmd_doctor(db_path: &Path, json: bool) -> Result<()> {
    let db = Database::open(db_path).await?;
    let report = db.doctor().await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": if report.healthy { "ok" } else { "error" },
                "db_path": db_path.display().to_string(),
                "healthy": report.healthy,
                "manifest_db_version": report.manifest_db_version,
                "datasets_checked": report.datasets_checked,
                "tx_rows": report.tx_rows,
                "cdc_rows": report.cdc_rows,
                "issues": report.issues,
                "warnings": report.warnings,
            })
        );
    } else {
        if report.healthy {
            println!(
                "Doctor OK for {} (db_version {}, datasets checked {}, tx rows {}, cdc rows {})",
                db_path.display(),
                report.manifest_db_version,
                report.datasets_checked,
                report.tx_rows,
                report.cdc_rows
            );
        } else {
            println!(
                "Doctor found issues for {} (db_version {}, datasets checked {}, tx rows {}, cdc rows {})",
                db_path.display(),
                report.manifest_db_version,
                report.datasets_checked,
                report.tx_rows,
                report.cdc_rows
            );
            for issue in &report.issues {
                println!("ISSUE: {}", issue);
            }
        }
        for warning in &report.warnings {
            println!("WARN: {}", warning);
        }
    }

    if report.healthy {
        Ok(())
    } else {
        Err(eyre!("doctor detected {} issue(s)", report.issues.len()))
    }
}

#[instrument(skip(query_path), fields(db_path = %db_path.display(), query_path = %query_path.display()))]
async fn cmd_check(db_path: PathBuf, query_path: &PathBuf, json: bool) -> Result<()> {
    let query_src = std::fs::read_to_string(query_path)
        .wrap_err_with(|| format!("failed to read query: {}", query_path.display()))?;
    let db = Database::open(&db_path).await?;
    let catalog = db.catalog().clone();

    let queries = parse_query_or_report(query_path, &query_src)?;

    let mut error_count = 0;
    let mut checks = Vec::with_capacity(queries.queries.len());
    for q in &queries.queries {
        match typecheck_query_decl(&catalog, q) {
            Ok(CheckedQuery::Read(_)) => {
                if !json {
                    println!("OK: query `{}` (read)", q.name);
                }
                checks.push(serde_json::json!({
                    "name": q.name,
                    "kind": "read",
                    "status": "ok",
                }));
            }
            Ok(CheckedQuery::Mutation(_)) => {
                if !json {
                    println!("OK: query `{}` (mutation)", q.name);
                }
                checks.push(serde_json::json!({
                    "name": q.name,
                    "kind": "mutation",
                    "status": "ok",
                }));
            }
            Err(e) => {
                if !json {
                    println!("ERROR: query `{}`: {}", q.name, e);
                }
                checks.push(serde_json::json!({
                    "name": q.name,
                    "kind": if q.mutation.is_some() { "mutation" } else { "read" },
                    "status": "error",
                    "error": e.to_string(),
                }));
                error_count += 1;
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": if error_count == 0 { "ok" } else { "error" },
                "query_path": query_path.display().to_string(),
                "queries_processed": queries.queries.len(),
                "errors": error_count,
                "results": checks,
            })
        );
    } else {
        println!(
            "Check complete: {} queries processed",
            queries.queries.len()
        );
    }
    if error_count > 0 {
        return Err(eyre!("{} query(s) failed typecheck", error_count));
    }
    Ok(())
}

#[instrument(
    skip(query_path, format, raw_params),
    fields(db_path = %db_path.display(), query_name = query_name, query_path = %query_path.display(), format = format)
)]
async fn cmd_run(
    db_path: PathBuf,
    query_path: &PathBuf,
    query_name: &str,
    format: &str,
    raw_params: Vec<(String, String)>,
    json: bool,
) -> Result<()> {
    let query_src = std::fs::read_to_string(query_path)
        .wrap_err_with(|| format!("failed to read query: {}", query_path.display()))?;

    // Parse queries and find the named one
    let queries = parse_query_or_report(query_path, &query_src)?;
    let query = queries
        .queries
        .iter()
        .find(|q| q.name == query_name)
        .ok_or_else(|| eyre!("query `{}` not found", query_name))?;
    info!("executing query");

    // Build param map from CLI args, using query param type info for inference
    let param_map = build_param_map(&query.params, &raw_params)?;

    let effective_format = if json { "json" } else { format };
    if let Some(preamble) = query_execution_preamble(query, effective_format, json) {
        print!("{}", preamble);
    }
    let db = Database::open(&db_path).await?;
    let run_result = db.run_query(query, &param_map).await?;
    let results = run_result.into_record_batches()?;
    render_results(effective_format, &results)
}

fn query_execution_preamble(
    query: &nanograph::query::ast::QueryDecl,
    format: &str,
    json: bool,
) -> Option<String> {
    if json || format != "table" {
        return None;
    }
    let has_metadata = query.description.is_some() || query.instruction.is_some();
    if !has_metadata {
        return None;
    }

    let mut lines = vec![format!("Query: {}", query.name)];
    if let Some(description) = &query.description {
        lines.push(format!("Description: {}", description));
    }
    if let Some(instruction) = &query.instruction {
        lines.push(format!("Instruction: {}", instruction));
    }
    Some(format!("{}\n\n", lines.join("\n")))
}

fn render_results(format: &str, results: &[RecordBatch]) -> Result<()> {
    match format {
        "table" => {
            if results.is_empty() {
                println!("(empty result)");
            } else {
                let formatted = arrow_cast::pretty::pretty_format_batches(results)
                    .wrap_err("failed to render table output")?;
                println!("{}", formatted);
            }
        }
        "csv" => {
            for batch in results {
                print_csv(batch);
            }
        }
        "jsonl" => {
            for batch in results {
                print_jsonl(batch);
            }
        }
        "json" => {
            print_json(results)?;
        }
        _ => return Err(eyre!("unknown format: {}", format)),
    }
    Ok(())
}

fn print_csv(batch: &RecordBatch) {
    let schema = batch.schema();
    let header: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    println!("{}", header.join(","));

    for row in 0..batch.num_rows() {
        let mut values = Vec::new();
        for col in 0..batch.num_columns() {
            let col_arr = batch.column(col);
            values
                .push(arrow_cast::display::array_value_to_string(col_arr, row).unwrap_or_default());
        }
        println!("{}", values.join(","));
    }
}

fn print_jsonl(batch: &RecordBatch) {
    let schema = batch.schema();
    for row in 0..batch.num_rows() {
        let mut map = serde_json::Map::new();
        for (col_idx, field) in schema.fields().iter().enumerate() {
            let col_arr = batch.column(col_idx);
            let val = arrow_cast::display::array_value_to_string(col_arr, row).unwrap_or_default();
            map.insert(field.name().clone(), serde_json::Value::String(val));
        }
        println!("{}", serde_json::Value::Object(map));
    }
}

fn print_json(results: &[RecordBatch]) -> Result<()> {
    let rows = nanograph::json_output::record_batches_to_json_rows(results);
    let out = serde_json::to_string_pretty(&rows).wrap_err("failed to serialize JSON output")?;
    println!("{}", out);
    Ok(())
}

/// Parse a `key=value` CLI parameter.
fn parse_param(s: &str) -> std::result::Result<(String, String), String> {
    let pos = s
        .find('=')
        .ok_or_else(|| format!("invalid param '{}': expected key=value", s))?;
    let key = s[..pos].to_string();
    let value = s[pos + 1..].to_string();
    // Strip surrounding quotes from value if present
    let value = strip_matching_quotes(&value).to_string();
    Ok((key, value))
}

fn strip_matching_quotes(input: &str) -> &str {
    if input.len() >= 2 {
        let bytes = input.as_bytes();
        let first = bytes[0];
        let last = bytes[input.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &input[1..input.len() - 1];
        }
    }
    input
}

fn parse_delete_predicate(input: &str) -> Result<DeletePredicate> {
    let ops = [
        (">=", DeleteOp::Ge),
        ("<=", DeleteOp::Le),
        ("!=", DeleteOp::Ne),
        ("=", DeleteOp::Eq),
        (">", DeleteOp::Gt),
        ("<", DeleteOp::Lt),
    ];

    for (token, op) in ops {
        if let Some(pos) = input.find(token) {
            let property = input[..pos].trim();
            let raw_value = input[pos + token.len()..].trim();
            if property.is_empty() || raw_value.is_empty() {
                return Err(eyre!(
                    "invalid --where predicate '{}': expected <property><op><value>",
                    input
                ));
            }

            let value = strip_matching_quotes(raw_value).to_string();

            return Ok(DeletePredicate {
                property: property.to_string(),
                op,
                value,
            });
        }
    }

    Err(eyre!(
        "invalid --where predicate '{}': supported operators are =, !=, >, >=, <, <=",
        input
    ))
}

/// Build a ParamMap from raw CLI strings using query param type declarations.
fn build_param_map(
    query_params: &[nanograph::query::ast::Param],
    raw: &[(String, String)],
) -> Result<ParamMap> {
    let mut map = ParamMap::new();
    for (key, value) in raw {
        // Find the declared type for this param
        let decl = query_params.iter().find(|p| p.name == *key);
        let lit = if let Some(decl) = decl {
            match decl.type_name.as_str() {
                "String" => Literal::String(value.clone()),
                "I32" | "I64" => {
                    let n: i64 = value
                        .parse()
                        .map_err(|_| eyre!("param '{}': expected integer, got '{}'", key, value))?;
                    Literal::Integer(n)
                }
                "U32" => {
                    let n: u32 = value.parse().map_err(|_| {
                        eyre!(
                            "param '{}': expected unsigned integer, got '{}'",
                            key,
                            value
                        )
                    })?;
                    Literal::Integer(i64::from(n))
                }
                "U64" => {
                    let n: u64 = value.parse().map_err(|_| {
                        eyre!(
                            "param '{}': expected unsigned integer, got '{}'",
                            key,
                            value
                        )
                    })?;
                    let n = i64::try_from(n).map_err(|_| {
                        eyre!(
                            "param '{}': value '{}' exceeds supported range for numeric literals (max {})",
                            key,
                            value,
                            i64::MAX
                        )
                    })?;
                    Literal::Integer(n)
                }
                "F32" | "F64" => {
                    let f: f64 = value
                        .parse()
                        .map_err(|_| eyre!("param '{}': expected float, got '{}'", key, value))?;
                    Literal::Float(f)
                }
                "Bool" => {
                    let b: bool = value
                        .parse()
                        .map_err(|_| eyre!("param '{}': expected bool, got '{}'", key, value))?;
                    Literal::Bool(b)
                }
                "Date" => Literal::Date(value.clone()),
                "DateTime" => Literal::DateTime(value.clone()),
                other if other.starts_with("Vector(") => {
                    let expected_dim = parse_vector_dim_type(other).ok_or_else(|| {
                        eyre!(
                            "param '{}': invalid vector type '{}' (expected Vector(N))",
                            key,
                            other
                        )
                    })?;
                    let parsed: serde_json::Value = serde_json::from_str(value).map_err(|e| {
                        eyre!(
                            "param '{}': expected JSON array for {}, got '{}': {}",
                            key,
                            other,
                            value,
                            e
                        )
                    })?;
                    let items = parsed.as_array().ok_or_else(|| {
                        eyre!(
                            "param '{}': expected JSON array for {}, got '{}'",
                            key,
                            other,
                            value
                        )
                    })?;
                    if items.len() != expected_dim {
                        return Err(eyre!(
                            "param '{}': expected {} values for {}, got {}",
                            key,
                            expected_dim,
                            other,
                            items.len()
                        ));
                    }
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        let num = item.as_f64().ok_or_else(|| {
                            eyre!("param '{}': vector element '{}' is not numeric", key, item)
                        })?;
                        out.push(Literal::Float(num));
                    }
                    Literal::List(out)
                }
                _ => Literal::String(value.clone()),
            }
        } else {
            // No type declaration found — default to string
            Literal::String(value.clone())
        };
        map.insert(key.clone(), lit);
    }
    Ok(map)
}

fn parse_vector_dim_type(type_name: &str) -> Option<usize> {
    let dim = type_name
        .strip_prefix("Vector(")?
        .strip_suffix(')')?
        .parse::<usize>()
        .ok()?;
    if dim == 0 { None } else { Some(dim) }
}

fn merge_run_params(
    alias: Option<&str>,
    positional_param_names: &[String],
    positional_args: Vec<String>,
    explicit_params: Vec<(String, String)>,
) -> Result<Vec<(String, String)>> {
    if positional_args.is_empty() {
        return Ok(explicit_params);
    }

    let alias_name = alias
        .ok_or_else(|| eyre!("positional query arguments require a configured query alias"))?;
    if positional_param_names.is_empty() {
        return Err(eyre!(
            "query alias `{}` does not declare args = [...] in nanograph.toml; use --param or add args",
            alias_name
        ));
    }
    if positional_args.len() > positional_param_names.len() {
        return Err(eyre!(
            "query alias `{}` accepts {} positional argument(s) ({}) but received {}",
            alias_name,
            positional_param_names.len(),
            positional_param_names.join(", "),
            positional_args.len()
        ));
    }

    let mut merged = Vec::with_capacity(positional_args.len() + explicit_params.len());
    for (name, value) in positional_param_names
        .iter()
        .zip(positional_args.into_iter())
    {
        merged.push((name.clone(), value));
    }
    merged.extend(explicit_params);
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{ArrayRef, Date32Array, Date64Array, Int32Array, StringArray};
    use nanograph::store::txlog::read_visible_cdc_entries;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn write_file(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn dotenv_loader_sets_missing_and_skips_existing_keys() {
        let dir = TempDir::new().unwrap();
        let dotenv_path = dir.path().join(".env");
        write_file(
            &dotenv_path,
            "OPENAI_API_KEY=from_file\nNANOGRAPH_EMBEDDINGS_MOCK=1\n",
        );

        let env = RefCell::new(HashMap::from([(
            "OPENAI_API_KEY".to_string(),
            "preset".to_string(),
        )]));
        let stats = load_dotenv_from_path_with(
            &dotenv_path,
            |key| env.borrow().contains_key(key),
            |key, value| {
                env.borrow_mut().insert(key.to_string(), value.to_string());
            },
        )
        .unwrap();

        assert_eq!(stats.loaded, 1);
        assert_eq!(stats.skipped_existing, 1);
        assert_eq!(
            env.borrow().get("OPENAI_API_KEY").map(String::as_str),
            Some("preset")
        );
        assert_eq!(
            env.borrow()
                .get("NANOGRAPH_EMBEDDINGS_MOCK")
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn dotenv_loader_is_noop_when_file_missing() {
        let dir = TempDir::new().unwrap();
        let env = RefCell::new(HashMap::<String, String>::new());
        let stats = load_dotenv_from_dir_with(
            dir.path(),
            |key| env.borrow().contains_key(key),
            |key, value| {
                env.borrow_mut().insert(key.to_string(), value.to_string());
            },
        )
        .unwrap();
        assert!(stats.is_none());
        assert!(env.borrow().is_empty());
    }

    #[test]
    fn project_dotenv_loader_prefers_env_nano_before_env() {
        let dir = TempDir::new().unwrap();
        write_file(&dir.path().join(".env.nano"), "OPENAI_API_KEY=from_nano\n");
        write_file(&dir.path().join(".env"), "OPENAI_API_KEY=from_env\n");

        let env = RefCell::new(HashMap::<String, String>::new());
        let results = load_project_dotenv_from_dir_with(
            dir.path(),
            |key| env.borrow().contains_key(key),
            |key, value| {
                env.borrow_mut().insert(key.to_string(), value.to_string());
            },
        );

        assert_eq!(results.len(), 2);
        assert_eq!(
            env.borrow().get("OPENAI_API_KEY").map(String::as_str),
            Some("from_nano")
        );
    }

    #[test]
    fn scaffold_project_files_creates_shared_config_and_env_template() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("demo.nano");
        let schema_path = dir.path().join("schema.pg");
        write_file(
            &schema_path,
            r#"node Person {
    name: String @key
}"#,
        );

        let generated = scaffold_project_files(dir.path(), &db_path, &schema_path).unwrap();
        assert_eq!(generated.len(), 2);

        let config = std::fs::read_to_string(dir.path().join("nanograph.toml")).unwrap();
        assert!(config.contains("[db]"));
        assert!(config.contains("default_path = \"demo.nano\""));
        assert!(config.contains("[schema]"));
        assert!(config.contains("default_path = \"schema.pg\""));
        assert!(config.contains("[embedding]"));
        assert!(config.contains("provider = \"openai\""));

        let dotenv = std::fs::read_to_string(dir.path().join(".env.nano")).unwrap();
        assert!(dotenv.contains("OPENAI_API_KEY=sk-..."));
        assert!(dotenv.contains("Do not commit this file."));

        let generated_again = scaffold_project_files(dir.path(), &db_path, &schema_path).unwrap();
        assert!(generated_again.is_empty());
    }

    #[test]
    fn infer_init_project_dir_prefers_shared_parent_of_db_and_schema() {
        let cwd = Path::new("/workspace");
        let project_dir = infer_init_project_dir(
            cwd,
            Path::new("/tmp/demo/db"),
            Path::new("/tmp/demo/schema.pg"),
        );
        assert_eq!(project_dir, PathBuf::from("/tmp/demo"));
    }

    #[test]
    fn query_execution_preamble_renders_description_and_instruction() {
        let query = nanograph::query::ast::QueryDecl {
            name: "semantic_search".to_string(),
            description: Some("Find semantically similar documents.".to_string()),
            instruction: Some(
                "Use for conceptual search. Prefer keyword_search for exact terms.".to_string(),
            ),
            params: Vec::new(),
            match_clause: Vec::new(),
            return_clause: Vec::new(),
            order_clause: Vec::new(),
            limit: None,
            mutation: None,
        };

        assert_eq!(
            query_execution_preamble(&query, "table", false).as_deref(),
            Some(
                "Query: semantic_search\nDescription: Find semantically similar documents.\nInstruction: Use for conceptual search. Prefer keyword_search for exact terms.\n\n"
            )
        );
    }

    #[test]
    fn query_execution_preamble_skips_machine_formats() {
        let query = nanograph::query::ast::QueryDecl {
            name: "semantic_search".to_string(),
            description: Some("Find semantically similar documents.".to_string()),
            instruction: None,
            params: Vec::new(),
            match_clause: Vec::new(),
            return_clause: Vec::new(),
            order_clause: Vec::new(),
            limit: None,
            mutation: None,
        };

        assert!(query_execution_preamble(&query, "json", false).is_none());
        assert!(query_execution_preamble(&query, "table", true).is_none());
    }

    #[test]
    fn default_log_filter_matches_build_mode() {
        assert_eq!(default_log_filter(), "error");
    }

    #[test]
    fn parse_load_mode_from_cli() {
        let cli = Cli::parse_from([
            "nanograph",
            "load",
            "/tmp/db",
            "--data",
            "/tmp/data.jsonl",
            "--mode",
            "append",
        ]);
        match cli.command {
            Commands::Load { mode, .. } => assert_eq!(mode, LoadModeArg::Append),
            _ => panic!("expected load command"),
        }
    }

    #[test]
    fn parse_changes_range_from_cli() {
        let cli = Cli::parse_from([
            "nanograph",
            "changes",
            "/tmp/db",
            "--from",
            "2",
            "--to",
            "4",
            "--format",
            "json",
        ]);
        match cli.command {
            Commands::Changes {
                from_version,
                to_version,
                format,
                ..
            } => {
                assert_eq!(from_version, Some(2));
                assert_eq!(to_version, Some(4));
                assert_eq!(format.as_deref(), Some("json"));
            }
            _ => panic!("expected changes command"),
        }
    }

    #[test]
    fn parse_maintenance_commands_from_cli() {
        let compact = Cli::parse_from([
            "nanograph",
            "compact",
            "/tmp/db",
            "--target-rows-per-fragment",
            "1000",
        ]);
        match compact.command {
            Commands::Compact {
                target_rows_per_fragment,
                ..
            } => assert_eq!(target_rows_per_fragment, 1000),
            _ => panic!("expected compact command"),
        }

        let cleanup = Cli::parse_from([
            "nanograph",
            "cleanup",
            "/tmp/db",
            "--retain-tx-versions",
            "4",
            "--retain-dataset-versions",
            "3",
        ]);
        match cleanup.command {
            Commands::Cleanup {
                retain_tx_versions,
                retain_dataset_versions,
                ..
            } => {
                assert_eq!(retain_tx_versions, 4);
                assert_eq!(retain_dataset_versions, 3);
            }
            _ => panic!("expected cleanup command"),
        }

        let doctor = Cli::parse_from(["nanograph", "doctor", "/tmp/db"]);
        match doctor.command {
            Commands::Doctor { .. } => {}
            _ => panic!("expected doctor command"),
        }

        let materialize = Cli::parse_from([
            "nanograph",
            "cdc-materialize",
            "/tmp/db",
            "--min-new-rows",
            "50",
            "--force",
        ]);
        match materialize.command {
            Commands::CdcMaterialize {
                min_new_rows,
                force,
                ..
            } => {
                assert_eq!(min_new_rows, 50);
                assert!(force);
            }
            _ => panic!("expected cdc-materialize command"),
        }
    }

    #[test]
    fn parse_metadata_commands_from_cli() {
        let version = Cli::parse_from(["nanograph", "version", "--db", "/tmp/db"]);
        match version.command {
            Commands::Version { db } => assert_eq!(db, Some(PathBuf::from("/tmp/db"))),
            _ => panic!("expected version command"),
        }

        let describe = Cli::parse_from([
            "nanograph",
            "describe",
            "--db",
            "/tmp/db",
            "--format",
            "json",
        ]);
        match describe.command {
            Commands::Describe {
                db,
                format,
                type_name,
            } => {
                assert_eq!(db, Some(PathBuf::from("/tmp/db")));
                assert_eq!(format.as_deref(), Some("json"));
                assert!(type_name.is_none());
            }
            _ => panic!("expected describe command"),
        }

        let export = Cli::parse_from([
            "nanograph",
            "export",
            "--db",
            "/tmp/db",
            "--format",
            "jsonl",
        ]);
        match export.command {
            Commands::Export { db, format } => {
                assert_eq!(db, Some(PathBuf::from("/tmp/db")));
                assert_eq!(format.as_deref(), Some("jsonl"));
            }
            _ => panic!("expected export command"),
        }

        let schema_diff = Cli::parse_from([
            "nanograph",
            "schema-diff",
            "--from",
            "/tmp/old.pg",
            "--to",
            "/tmp/new.pg",
            "--format",
            "json",
        ]);
        match schema_diff.command {
            Commands::SchemaDiff {
                from_schema,
                to_schema,
                format,
            } => {
                assert_eq!(from_schema, PathBuf::from("/tmp/old.pg"));
                assert_eq!(to_schema, PathBuf::from("/tmp/new.pg"));
                assert_eq!(format.as_deref(), Some("json"));
            }
            _ => panic!("expected schema-diff command"),
        }
    }

    #[test]
    fn resolve_changes_window_supports_since_and_range_modes() {
        let since = resolve_changes_window(Some(5), None, None).unwrap();
        assert_eq!(
            since,
            ChangesWindow {
                from_db_version_exclusive: 5,
                to_db_version_inclusive: None
            }
        );

        let range = resolve_changes_window(None, Some(2), Some(4)).unwrap();
        assert_eq!(
            range,
            ChangesWindow {
                from_db_version_exclusive: 1,
                to_db_version_inclusive: Some(4)
            }
        );
    }

    #[test]
    fn resolve_changes_window_rejects_invalid_ranges() {
        assert!(resolve_changes_window(Some(1), Some(1), Some(2)).is_err());
        assert!(resolve_changes_window(None, Some(4), Some(3)).is_err());
        assert!(resolve_changes_window(None, Some(2), None).is_err());
        assert!(resolve_changes_window(None, None, Some(2)).is_err());
    }

    #[tokio::test]
    async fn load_mode_merge_requires_keyed_schema() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let schema_path = dir.path().join("schema.pg");
        let data_path = dir.path().join("data.jsonl");

        write_file(
            &schema_path,
            r#"node Person {
    name: String
}"#,
        );
        write_file(&data_path, r#"{"type":"Person","data":{"name":"Alice"}}"#);

        cmd_init(&db_path, &schema_path, false).await.unwrap();
        let err = cmd_load(&db_path, &data_path, LoadModeArg::Merge, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires at least one node @key"));
    }

    #[tokio::test]
    async fn load_mode_append_and_merge_behave_as_expected() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let schema_path = dir.path().join("schema.pg");
        let data_initial = dir.path().join("initial.jsonl");
        let data_append = dir.path().join("append.jsonl");
        let data_merge = dir.path().join("merge.jsonl");

        write_file(
            &schema_path,
            r#"node Person {
    name: String @key
    age: I32?
}"#,
        );
        write_file(
            &data_initial,
            r#"{"type":"Person","data":{"name":"Alice","age":30}}"#,
        );
        write_file(
            &data_append,
            r#"{"type":"Person","data":{"name":"Bob","age":22}}"#,
        );
        write_file(
            &data_merge,
            r#"{"type":"Person","data":{"name":"Alice","age":31}}"#,
        );

        cmd_init(&db_path, &schema_path, false).await.unwrap();
        cmd_load(&db_path, &data_initial, LoadModeArg::Overwrite, false)
            .await
            .unwrap();
        cmd_load(&db_path, &data_append, LoadModeArg::Append, false)
            .await
            .unwrap();
        cmd_load(&db_path, &data_merge, LoadModeArg::Merge, false)
            .await
            .unwrap();

        let db = Database::open(&db_path).await.unwrap();
        let storage = db.snapshot();
        let batch = storage.get_all_nodes("Person").unwrap().unwrap();
        assert_eq!(batch.num_rows(), 2);
        let names = batch
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let ages = batch
            .column_by_name("age")
            .unwrap()
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        let mut alice_age = None;
        let mut has_bob = false;
        for row in 0..batch.num_rows() {
            if names.value(row) == "Alice" {
                alice_age = Some(ages.value(row));
            }
            if names.value(row) == "Bob" {
                has_bob = true;
            }
        }
        assert_eq!(alice_age, Some(31));
        assert!(has_bob);
    }

    #[test]
    fn check_and_run_allow_db_to_be_resolved_later() {
        let check = Cli::try_parse_from(["nanograph", "check", "--query", "/tmp/q.gq"]).unwrap();
        match check.command {
            Commands::Check { db, query } => {
                assert!(db.is_none());
                assert_eq!(query, PathBuf::from("/tmp/q.gq"));
            }
            _ => panic!("expected check command"),
        }

        let run =
            Cli::try_parse_from(["nanograph", "run", "search", "--param", "q=hello"]).unwrap();
        match run.command {
            Commands::Run {
                alias,
                args,
                db,
                query,
                name,
                ..
            } => {
                assert_eq!(alias.as_deref(), Some("search"));
                assert!(args.is_empty());
                assert!(db.is_none());
                assert!(query.is_none());
                assert!(name.is_none());
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn parse_run_alias_with_positional_args() {
        let run = Cli::try_parse_from([
            "nanograph",
            "run",
            "search",
            "vector databases",
            "--param",
            "limit=5",
        ])
        .unwrap();
        match run.command {
            Commands::Run {
                alias,
                args,
                params,
                ..
            } => {
                assert_eq!(alias.as_deref(), Some("search"));
                assert_eq!(args, vec!["vector databases".to_string()]);
                assert_eq!(params, vec![("limit".to_string(), "5".to_string())]);
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn merge_run_params_maps_alias_positionals_and_preserves_explicit_overrides() {
        let merged = merge_run_params(
            Some("search"),
            &[String::from("q"), String::from("limit")],
            vec!["vector databases".to_string()],
            vec![
                ("q".to_string(), "override".to_string()),
                ("format".to_string(), "json".to_string()),
            ],
        )
        .unwrap();
        assert_eq!(
            merged,
            vec![
                ("q".to_string(), "vector databases".to_string()),
                ("q".to_string(), "override".to_string()),
                ("format".to_string(), "json".to_string()),
            ]
        );
    }

    #[test]
    fn merge_run_params_rejects_positional_args_without_alias_mapping() {
        let err = merge_run_params(
            Some("search"),
            &[],
            vec!["vector databases".to_string()],
            Vec::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not declare args"));
    }

    #[test]
    fn build_param_map_parses_date_and_datetime_types() {
        let query_params = vec![
            nanograph::query::ast::Param {
                name: "d".to_string(),
                type_name: "Date".to_string(),
                nullable: false,
            },
            nanograph::query::ast::Param {
                name: "dt".to_string(),
                type_name: "DateTime".to_string(),
                nullable: false,
            },
        ];
        let raw = vec![
            ("d".to_string(), "2026-02-14".to_string()),
            ("dt".to_string(), "2026-02-14T10:00:00Z".to_string()),
        ];

        let params = build_param_map(&query_params, &raw).unwrap();
        assert!(matches!(
            params.get("d"),
            Some(Literal::Date(v)) if v == "2026-02-14"
        ));
        assert!(matches!(
            params.get("dt"),
            Some(Literal::DateTime(v)) if v == "2026-02-14T10:00:00Z"
        ));
    }

    #[test]
    fn build_param_map_parses_vector_type() {
        let query_params = vec![nanograph::query::ast::Param {
            name: "q".to_string(),
            type_name: "Vector(3)".to_string(),
            nullable: false,
        }];
        let raw = vec![("q".to_string(), "[0.1, 0.2, 0.3]".to_string())];

        let params = build_param_map(&query_params, &raw).unwrap();
        match params.get("q") {
            Some(Literal::List(items)) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(items[0], Literal::Float(_)));
                assert!(matches!(items[1], Literal::Float(_)));
                assert!(matches!(items[2], Literal::Float(_)));
            }
            other => panic!("expected vector list literal, got {:?}", other),
        }
    }

    #[test]
    fn build_param_map_parses_u32_and_u64_types() {
        let query_params = vec![
            nanograph::query::ast::Param {
                name: "u32v".to_string(),
                type_name: "U32".to_string(),
                nullable: false,
            },
            nanograph::query::ast::Param {
                name: "u64v".to_string(),
                type_name: "U64".to_string(),
                nullable: false,
            },
        ];
        let raw = vec![
            ("u32v".to_string(), "42".to_string()),
            ("u64v".to_string(), "9001".to_string()),
        ];

        let params = build_param_map(&query_params, &raw).unwrap();
        assert!(matches!(params.get("u32v"), Some(Literal::Integer(42))));
        assert!(matches!(params.get("u64v"), Some(Literal::Integer(9001))));
    }

    #[test]
    fn build_param_map_rejects_u64_values_outside_literal_range() {
        let query_params = vec![nanograph::query::ast::Param {
            name: "u64v".to_string(),
            type_name: "U64".to_string(),
            nullable: false,
        }];
        let too_large = format!("{}", (i64::MAX as u128) + 1);
        let raw = vec![("u64v".to_string(), too_large)];

        let err = build_param_map(&query_params, &raw).unwrap_err();
        assert!(err.to_string().contains("exceeds supported range"));
    }

    #[test]
    fn parse_param_single_quote_value_does_not_panic() {
        let (key, value) = parse_param("x='").unwrap();
        assert_eq!(key, "x");
        assert_eq!(value, "'");
    }

    #[test]
    fn parse_delete_predicate_single_quote_value_does_not_panic() {
        let pred = parse_delete_predicate("slug='").unwrap();
        assert_eq!(pred.property, "slug");
        assert_eq!(pred.op, DeleteOp::Eq);
        assert_eq!(pred.value, "'");
    }

    #[test]
    fn array_value_to_json_formats_temporal_types_as_iso_strings() {
        use nanograph::json_output::array_value_to_json;
        let date: ArrayRef = Arc::new(Date32Array::from(vec![Some(20498)]));
        let dt: ArrayRef = Arc::new(Date64Array::from(vec![Some(1771063200000)]));

        assert_eq!(
            array_value_to_json(&date, 0),
            serde_json::Value::String("2026-02-14".to_string())
        );
        assert_eq!(
            array_value_to_json(&dt, 0),
            serde_json::Value::String("2026-02-14T10:00:00.000Z".to_string())
        );
    }

    #[tokio::test]
    async fn run_mutation_insert_in_db_mode() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let schema_path = dir.path().join("schema.pg");
        let query_path = dir.path().join("mut.gq");

        write_file(
            &schema_path,
            r#"node Person {
    name: String
    age: I32?
}"#,
        );
        write_file(
            &query_path,
            r#"
query add_person($name: String, $age: I32) {
    insert Person {
        name: $name
        age: $age
    }
}
"#,
        );

        cmd_init(&db_path, &schema_path, false).await.unwrap();
        cmd_run(
            db_path.clone(),
            &query_path,
            "add_person",
            "table",
            vec![
                ("name".to_string(), "Eve".to_string()),
                ("age".to_string(), "29".to_string()),
            ],
            false,
        )
        .await
        .unwrap();

        let db = Database::open(&db_path).await.unwrap();
        let storage = db.snapshot();
        let people = storage.get_all_nodes("Person").unwrap().unwrap();
        let names = people
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!((0..people.num_rows()).any(|row| names.value(row) == "Eve"));

        let cdc_rows = read_visible_cdc_entries(&db_path, 0, None).unwrap();
        assert_eq!(cdc_rows.len(), 1);
        assert_eq!(cdc_rows[0].op, "insert");
        assert_eq!(cdc_rows[0].type_name, "Person");
    }

    #[tokio::test]
    async fn maintenance_commands_work_on_real_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let schema_path = dir.path().join("schema.pg");
        let data_path = dir.path().join("data.jsonl");

        write_file(
            &schema_path,
            r#"node Person {
    name: String @key
}"#,
        );
        write_file(
            &data_path,
            r#"{"type":"Person","data":{"name":"Alice"}}
{"type":"Person","data":{"name":"Bob"}}"#,
        );

        cmd_init(&db_path, &schema_path, false).await.unwrap();
        cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false)
            .await
            .unwrap();

        cmd_compact(&db_path, 1_024, true, 0.1, false)
            .await
            .unwrap();
        cmd_cleanup(&db_path, 1, 1, false).await.unwrap();
        cmd_cdc_materialize(&db_path, 0, true, false).await.unwrap();
        cmd_doctor(&db_path, false).await.unwrap();

        assert!(db_path.join("__cdc_analytics").exists());

        let db = Database::open(&db_path).await.unwrap();
        let report = db.doctor().await.unwrap();
        assert!(report.tx_rows <= 1);
    }

    #[tokio::test]
    async fn version_describe_export_helpers_work_on_real_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let schema_path = dir.path().join("schema.pg");
        let data_path = dir.path().join("data.jsonl");

        write_file(
            &schema_path,
            r#"node Person {
    name: String @key
    summary: String
    embedding: Vector(3) @embed(summary)
}
edge Knows: Person -> Person"#,
        );
        write_file(
            &data_path,
            r#"{"type":"Person","data":{"name":"Alice","summary":"Alpha","embedding":[1.0,0.0,0.0]}}
{"type":"Person","data":{"name":"Bob","summary":"Beta","embedding":[0.0,1.0,0.0]}}
{"edge":"Knows","from":"Alice","to":"Bob"}"#,
        );

        cmd_init(&db_path, &schema_path, false).await.unwrap();
        cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false)
            .await
            .unwrap();

        let version = build_version_payload(Some(&db_path)).unwrap();
        assert_eq!(
            version["db"]["db_version"].as_u64(),
            Some(1),
            "expected one committed load version"
        );
        assert_eq!(version["db"]["dataset_count"].as_u64(), Some(2));
        assert_eq!(
            version["db"]["dataset_versions"].as_array().unwrap().len(),
            2
        );

        let db = Database::open(&db_path).await.unwrap();
        let manifest = GraphManifest::read(&db_path).unwrap();
        let describe = build_describe_payload(&db_path, &db, &manifest, None).unwrap();
        assert_eq!(describe["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(describe["edges"].as_array().unwrap().len(), 1);
        assert_eq!(describe["nodes"][0]["rows"].as_u64(), Some(2));
        assert_eq!(describe["edges"][0]["rows"].as_u64(), Some(1));
        assert_eq!(describe["nodes"][0]["key_property"].as_str(), Some("name"));
        assert_eq!(
            describe["edges"][0]["endpoint_keys"]["src"].as_str(),
            Some("name")
        );
        let embedding_prop = describe["nodes"][0]["properties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "embedding")
            .expect("embedding property present in describe payload");
        assert_eq!(embedding_prop["embed_source"].as_str(), Some("summary"));

        let rows = build_export_rows(&db, false).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .any(|row| row["type"] == "Person" && row["data"]["name"] == "Alice")
        );
        assert!(
            rows.iter()
                .any(|row| row["edge"] == "Knows" && row["from"] == "Alice" && row["to"] == "Bob")
        );
    }

    #[tokio::test]
    async fn describe_type_filter_and_metadata_fields_are_present() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let schema_path = dir.path().join("schema.pg");

        write_file(
            &schema_path,
            r#"node Task @description("Tracked work item") @instruction("Query by slug") {
    slug: String @key @description("Stable external identifier")
    title: String
}
edge DependsOn: Task -> Task @description("Hard dependency") @instruction("Use only for blockers")
"#,
        );

        cmd_init(&db_path, &schema_path, false).await.unwrap();

        let db = Database::open(&db_path).await.unwrap();
        let manifest = GraphManifest::read(&db_path).unwrap();
        let task = build_describe_payload(&db_path, &db, &manifest, Some("Task")).unwrap();
        assert_eq!(task["nodes"].as_array().unwrap().len(), 1);
        assert!(task["edges"].as_array().unwrap().is_empty());
        assert_eq!(
            task["nodes"][0]["description"].as_str(),
            Some("Tracked work item")
        );
        assert_eq!(
            task["nodes"][0]["instruction"].as_str(),
            Some("Query by slug")
        );
        assert_eq!(
            task["nodes"][0]["properties"][0]["description"].as_str(),
            Some("Stable external identifier")
        );
        assert_eq!(
            task["nodes"][0]["outgoing_edges"][0]["name"].as_str(),
            Some("DependsOn")
        );

        let edge = build_describe_payload(&db_path, &db, &manifest, Some("DependsOn")).unwrap();
        assert!(edge["nodes"].as_array().unwrap().is_empty());
        assert_eq!(edge["edges"].as_array().unwrap().len(), 1);
        assert_eq!(
            edge["edges"][0]["endpoint_keys"]["src"].as_str(),
            Some("slug")
        );
    }

    #[tokio::test]
    async fn export_uses_key_properties_for_edge_endpoints_and_round_trips() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let roundtrip_db_path = dir.path().join("roundtrip-db");
        let schema_path = dir.path().join("schema.pg");
        let export_path = dir.path().join("export.jsonl");
        let data_path = dir.path().join("data.jsonl");

        write_file(
            &schema_path,
            r#"node ActionItem {
    slug: String @key
    title: String
}
node Person {
    slug: String @key
    name: String
}
edge MadeBy: ActionItem -> Person"#,
        );
        write_file(
            &data_path,
            r#"{"type":"ActionItem","data":{"slug":"dec-build-mcp","title":"Build MCP"}}
{"type":"Person","data":{"slug":"act-andrew","name":"Andrew"}}
{"edge":"MadeBy","from":"dec-build-mcp","to":"act-andrew"}"#,
        );

        cmd_init(&db_path, &schema_path, false).await.unwrap();
        cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false)
            .await
            .unwrap();

        let db = Database::open(&db_path).await.unwrap();
        let rows = build_export_rows(&db, false).unwrap();
        let edge = rows
            .iter()
            .find(|row| row["edge"] == "MadeBy")
            .expect("made by edge row");
        assert_eq!(edge["from"].as_str(), Some("dec-build-mcp"));
        assert_eq!(edge["to"].as_str(), Some("act-andrew"));
        assert!(edge.get("id").is_none());
        assert!(edge.get("src").is_none());
        assert!(edge.get("dst").is_none());

        let export_jsonl = rows
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        write_file(&export_path, &(export_jsonl + "\n"));

        cmd_init(&roundtrip_db_path, &schema_path, false)
            .await
            .unwrap();
        cmd_load(
            &roundtrip_db_path,
            &export_path,
            LoadModeArg::Overwrite,
            false,
        )
        .await
        .unwrap();

        let roundtrip_db = Database::open(&roundtrip_db_path).await.unwrap();
        let roundtrip_rows = build_export_rows(&roundtrip_db, false).unwrap();
        let roundtrip_edge = roundtrip_rows
            .iter()
            .find(|row| row["edge"] == "MadeBy")
            .expect("made by edge row after roundtrip");
        assert_eq!(roundtrip_edge["from"].as_str(), Some("dec-build-mcp"));
        assert_eq!(roundtrip_edge["to"].as_str(), Some("act-andrew"));
        assert!(roundtrip_edge.get("id").is_none());
        assert!(roundtrip_edge.get("src").is_none());
        assert!(roundtrip_edge.get("dst").is_none());
    }

    #[tokio::test]
    async fn export_preserves_user_property_named_id_for_nodes_and_edges() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let roundtrip_db_path = dir.path().join("roundtrip-db");
        let schema_path = dir.path().join("schema.pg");
        let export_path = dir.path().join("export.jsonl");
        let data_path = dir.path().join("data.jsonl");

        write_file(
            &schema_path,
            r#"node User {
    id: String @key
    name: String
}
edge Follows: User -> User"#,
        );
        write_file(
            &data_path,
            r#"{"type":"User","data":{"id":"usr_01","name":"Alice"}}
{"type":"User","data":{"id":"usr_02","name":"Bob"}}
{"edge":"Follows","from":"usr_01","to":"usr_02"}"#,
        );

        cmd_init(&db_path, &schema_path, false).await.unwrap();
        cmd_load(&db_path, &data_path, LoadModeArg::Overwrite, false)
            .await
            .unwrap();

        let db = Database::open(&db_path).await.unwrap();
        let rows = build_export_rows(&db, false).unwrap();
        assert!(
            rows.iter()
                .any(|row| row["type"] == "User" && row["data"]["id"] == "usr_01")
        );
        assert!(
            rows.iter()
                .any(|row| row["type"] == "User" && row["data"]["id"] == "usr_02")
        );
        assert!(rows.iter().any(|row| {
            row["edge"] == "Follows" && row["from"] == "usr_01" && row["to"] == "usr_02"
        }));

        let export_jsonl = rows
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        write_file(&export_path, &(export_jsonl + "\n"));

        cmd_init(&roundtrip_db_path, &schema_path, false)
            .await
            .unwrap();
        cmd_load(
            &roundtrip_db_path,
            &export_path,
            LoadModeArg::Overwrite,
            false,
        )
        .await
        .unwrap();

        let roundtrip_db = Database::open(&roundtrip_db_path).await.unwrap();
        let roundtrip_rows = build_export_rows(&roundtrip_db, false).unwrap();
        assert!(
            roundtrip_rows
                .iter()
                .any(|row| { row["type"] == "User" && row["data"]["id"] == "usr_01" })
        );
        assert!(roundtrip_rows.iter().any(|row| {
            row["edge"] == "Follows" && row["from"] == "usr_01" && row["to"] == "usr_02"
        }));
    }
}
