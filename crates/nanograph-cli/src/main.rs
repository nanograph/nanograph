#![recursion_limit = "256"]

use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal};
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{error::Error as StdError, fmt};

use ariadne::{Color, Label, Report, ReportKind, Source};
use arrow_array::RecordBatch;
use arrow_schema::DataType;
use clap::{Parser, Subcommand};
use color_eyre::eyre::{Result, WrapErr, eyre};
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::{ASCII_BORDERS_ONLY_CONDENSED, UTF8_FULL_CONDENSED};
use comfy_table::{
    Attribute, Cell, CellAlignment, Color as TableColor, ColumnConstraint, ContentArrangement,
    Table, Width,
};
use tracing::{debug, info, instrument, warn};
use tracing_subscriber::EnvFilter;

mod config;
mod metadata;
mod schema_ops;
mod ui;

#[cfg(test)]
mod tests;

use config::{LoadedConfig, ResolvedRunConfig, TableCellLayout};
use metadata::{cmd_describe, cmd_export, cmd_version};
use nanograph::error::{NanoError, ParseDiagnostic};
use nanograph::query::ast::{Clause, CompOp, Expr, Literal, MatchValue, Mutation, QueryDecl};
use nanograph::query::parser::parse_query_diagnostic;
use nanograph::query::typecheck::{BindingKind, CheckedQuery, TypeContext, typecheck_query_decl};
use nanograph::schema::ast::{SchemaDecl, annotation_value, has_annotation};
use nanograph::schema::parser::parse_schema_diagnostic;
use nanograph::store::database::{
    CdcAnalyticsMaterializeOptions, CleanupOptions, CompactOptions, Database, DeleteOp,
    DeletePredicate, EmbedOptions, LoadMode, cleanup_database, compact_database,
    load_database_file_sparse, run_mutation_query_sparse,
};
use nanograph::store::metadata::DatabaseMetadata;
use nanograph::store::migration::{SchemaDiffReport, analyze_schema_diff};
use nanograph::store::storage_generation::{StorageGeneration, detect_storage_generation};
use nanograph::store::storage_migrate::{
    migrate_storage_to_lance_v4, migrate_storage_to_lineage_native,
};
use nanograph::store::txlog::{
    CdcLogEntry, VisibleCdcSource, VisibleChangeRow, read_visible_cdc_entries_with_source,
    read_visible_change_rows,
};
use nanograph::{ParamMap, build_catalog, lower_query};
use schema_ops::{cmd_migrate, cmd_schema_diff, schema_compatibility_label};
use ui::{
    StatusTone, format_status_line, stderr_supports_color, stdout_supports_color, style_key,
    style_label, style_scalar,
};

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
    /// Suppress human-readable stdout output.
    #[arg(long, short = 'q', global = true)]
    quiet: bool,
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
        /// Show manifest and dataset internals in table mode
        #[arg(long)]
        verbose: bool,
    },
    /// Export full graph as JSONL or JSON (nodes first, then edges)
    Export {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Output format: jsonl or json
        #[arg(long)]
        format: Option<String>,
        /// Omit vector embedding properties from exported rows
        #[arg(long, default_value_t = false)]
        no_embeddings: bool,
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
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long)]
        schema: Option<PathBuf>,
        /// Deprecated positional database directory
        #[arg(hide = true)]
        legacy_db_path: Option<PathBuf>,
    },
    /// Load data into an existing database
    Load {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long)]
        data: PathBuf,
        /// Load mode: overwrite, append, or merge
        #[arg(long, value_enum)]
        mode: LoadModeArg,
        /// Deprecated positional database directory
        #[arg(hide = true)]
        legacy_db_path: Option<PathBuf>,
    },
    /// Backfill or recompute @embed(...) vector properties on existing rows
    Embed {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Restrict to one node type
        #[arg(long = "type")]
        type_name: Option<String>,
        /// Restrict to one @embed target property (requires --type)
        #[arg(long)]
        property: Option<String>,
        /// Only embed rows where the target property is currently null
        #[arg(long, default_value_t = false)]
        only_null: bool,
        /// Maximum number of rows to process
        #[arg(long)]
        limit: Option<usize>,
        /// Rebuild vector indexes even if no rows are updated
        #[arg(long, default_value_t = false)]
        reindex: bool,
        /// Report what would be embedded without writing
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Delete nodes by predicate, cascading incident edges
    Delete {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Node type name
        #[arg(long = "type")]
        type_name: String,
        /// Predicate expression, e.g. name=Alice or age>=30
        #[arg(long = "where")]
        predicate: String,
        /// Deprecated positional database directory
        #[arg(hide = true)]
        legacy_db_path: Option<PathBuf>,
    },
    /// Stream CDC events from committed transactions
    Changes {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Return changes with graph_version strictly greater than this value
        #[arg(long, conflicts_with_all = ["from_version", "to_version"])]
        since: Option<u64>,
        /// Inclusive lower bound for graph_version (requires --to)
        #[arg(long = "from", requires = "to_version", conflicts_with = "since")]
        from_version: Option<u64>,
        /// Inclusive upper bound for graph_version (requires --from)
        #[arg(long = "to", requires = "from_version", conflicts_with = "since")]
        to_version: Option<u64>,
        /// Output format: jsonl or json
        #[arg(long)]
        format: Option<String>,
        /// Omit vector embedding properties from CDC payloads
        #[arg(long, default_value_t = false)]
        no_embeddings: bool,
        /// Hidden debug source selector for CDC reads
        #[arg(long, hide = true, value_enum, default_value_t = ChangesSourceArg::Authoritative)]
        cdc_source: ChangesSourceArg,
        /// Deprecated positional database directory
        #[arg(hide = true)]
        legacy_db_path: Option<PathBuf>,
    },
    /// Compact Lance datasets and commit updated pinned dataset versions
    Compact {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Target row count per compacted fragment
        #[arg(long, default_value_t = 1_048_576)]
        target_rows_per_fragment: usize,
        /// Whether to materialize deleted rows during compaction
        #[arg(long, default_value_t = true)]
        materialize_deletions: bool,
        /// Deletion fraction threshold for materialization
        #[arg(long, default_value_t = 0.1)]
        materialize_deletions_threshold: f32,
        /// Deprecated positional database directory
        #[arg(hide = true)]
        legacy_db_path: Option<PathBuf>,
    },
    /// Prune old tx/CDC history and old Lance versions while keeping replay window
    Cleanup {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Keep this many latest tx versions for CDC replay
        #[arg(long, default_value_t = 128)]
        retain_tx_versions: u64,
        /// Keep at least this many latest versions per Lance dataset
        #[arg(long, default_value_t = 2)]
        retain_dataset_versions: usize,
        /// Deprecated positional database directory
        #[arg(hide = true)]
        legacy_db_path: Option<PathBuf>,
    },
    /// Run consistency checks on manifest, datasets, logs, and graph integrity
    Doctor {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Desired schema file to compare against the current DB schema
        #[arg(long)]
        schema: Option<PathBuf>,
        /// Show per-dataset Lance storage formats
        #[arg(long)]
        verbose: bool,
        /// Deprecated positional database directory
        #[arg(hide = true)]
        legacy_db_path: Option<PathBuf>,
    },
    /// Materialize visible CDC into a derived Lance analytics dataset
    CdcMaterialize {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Minimum number of new visible CDC rows required to run materialization
        #[arg(long, default_value_t = 0)]
        min_new_rows: usize,
        /// Force materialization regardless of threshold
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Deprecated positional database directory
        #[arg(hide = true)]
        legacy_db_path: Option<PathBuf>,
    },
    /// Diff and apply schema migration from <db>/schema.pg
    Migrate {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Desired schema file to migrate toward
        #[arg(long)]
        schema: Option<PathBuf>,
        /// Show migration plan without applying writes
        #[arg(long)]
        dry_run: bool,
        /// Output format: table or json
        #[arg(long)]
        format: Option<String>,
        /// Apply confirm-level steps without interactive prompts
        #[arg(long)]
        auto_approve: bool,
        /// Deprecated positional database directory
        #[arg(hide = true)]
        legacy_db_path: Option<PathBuf>,
    },
    /// One-shot storage generation transitions
    Storage {
        #[command(subcommand)]
        command: StorageCommands,
    },
    /// Lint query files against a schema or database
    Lint {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long)]
        query: PathBuf,
        /// Schema file to lint against; defaults to <db>/schema.pg when --db is provided
        #[arg(long)]
        schema: Option<PathBuf>,
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
        /// Output format: table, kv, csv, jsonl, or json
        #[arg(long)]
        format: Option<String>,
        /// Query parameters (repeatable), e.g. --param name="Alice"
        #[arg(long = "param", value_parser = parse_param)]
        params: Vec<(String, String)>,
    },
}

#[derive(Subcommand)]
enum StorageCommands {
    /// Migrate a legacy v3 database to lance-v4 namespace storage
    Migrate {
        /// Database directory
        #[arg(long)]
        db: Option<PathBuf>,
        /// Target storage generation
        #[arg(long, value_enum)]
        target: StorageTargetArg,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum LoadModeArg {
    Overwrite,
    Append,
    Merge,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum StorageTargetArg {
    LanceV4,
    LineageNative,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum ChangesSourceArg {
    Authoritative,
    LineageShadow,
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

impl From<ChangesSourceArg> for VisibleCdcSource {
    fn from(value: ChangesSourceArg) -> Self {
        match value {
            ChangesSourceArg::Authoritative => VisibleCdcSource::Authoritative,
            ChangesSourceArg::LineageShadow => VisibleCdcSource::LineageShadow,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();

    let cli = Cli::parse();
    let config = match LoadedConfig::load(cli.config.as_deref()) {
        Ok(config) => config,
        Err(error) => return finish_cli_failure(CliFailure::new(cli.json, false, error)),
    };
    load_dotenv_for_process(&config.base_dir);
    if let Err(error) = config.apply_embedding_env_for_process() {
        return finish_cli_failure(CliFailure::new(cli.json, false, error));
    }
    let json = config.effective_json(cli.json);
    let quiet = cli.quiet;

    match dispatch_cli(cli.command, &config, json, quiet).await {
        Ok(()) => Ok(()),
        Err(failure) => {
            let rendered = failure.downcast_ref::<RenderedJsonError>().is_some();
            finish_cli_failure(CliFailure::new(json, rendered, failure))
        }
    }
}

async fn dispatch_cli(
    command: Commands,
    config: &LoadedConfig,
    json: bool,
    quiet: bool,
) -> Result<()> {
    match command {
        Commands::Version { db } => {
            cmd_version(config.resolve_optional_db_path(db), json, quiet).await
        }
        Commands::Describe {
            db,
            format,
            type_name,
            verbose,
        } => {
            let db = config.resolve_db_path(db)?;
            let format =
                config.resolve_command_format(format.as_deref(), "table", &["table", "json"])?;
            cmd_describe(db, &format, json, type_name.as_deref(), verbose, quiet).await
        }
        Commands::Export {
            db,
            format,
            no_embeddings,
        } => {
            let db = config.resolve_db_path(db)?;
            let format =
                config.resolve_command_format(format.as_deref(), "jsonl", &["jsonl", "json"])?;
            cmd_export(db, &format, json, no_embeddings).await
        }
        Commands::SchemaDiff {
            from_schema,
            to_schema,
            format,
        } => {
            let format =
                config.resolve_command_format(format.as_deref(), "table", &["table", "json"])?;
            cmd_schema_diff(&from_schema, &to_schema, &format, json, quiet).await
        }
        Commands::Init {
            db,
            schema,
            legacy_db_path,
        } => {
            let db_path = config.resolve_db_path(db.or(legacy_db_path))?;
            let schema = config.resolve_schema_path(schema)?;
            cmd_init(&db_path, &schema, json, quiet).await
        }
        Commands::Load {
            db,
            data,
            mode,
            legacy_db_path,
        } => {
            let db_path = config.resolve_db_path(db.or(legacy_db_path))?;
            cmd_load(&db_path, &data, mode, json, quiet).await
        }
        Commands::Embed {
            db,
            type_name,
            property,
            only_null,
            limit,
            reindex,
            dry_run,
        } => {
            let db_path = config.resolve_db_path(db)?;
            cmd_embed(
                &db_path,
                EmbedOptions {
                    type_name,
                    property,
                    only_null,
                    limit,
                    reindex,
                    dry_run,
                },
                json,
                quiet,
            )
            .await
        }
        Commands::Delete {
            db,
            type_name,
            predicate,
            legacy_db_path,
        } => {
            let db_path = config.resolve_db_path(db.or(legacy_db_path))?;
            cmd_delete(&db_path, &type_name, &predicate, json, quiet).await
        }
        Commands::Changes {
            db,
            since,
            from_version,
            to_version,
            format,
            no_embeddings,
            cdc_source,
            legacy_db_path,
        } => {
            let db_path = config.resolve_db_path(db.or(legacy_db_path))?;
            let format =
                config.resolve_command_format(format.as_deref(), "jsonl", &["jsonl", "json"])?;
            cmd_changes(
                &db_path,
                since,
                from_version,
                to_version,
                &format,
                json,
                no_embeddings,
                cdc_source.into(),
            )
            .await
        }
        Commands::Compact {
            db,
            target_rows_per_fragment,
            materialize_deletions,
            materialize_deletions_threshold,
            legacy_db_path,
        } => {
            let db_path = config.resolve_db_path(db.or(legacy_db_path))?;
            cmd_compact(
                &db_path,
                target_rows_per_fragment,
                materialize_deletions,
                materialize_deletions_threshold,
                json,
                quiet,
            )
            .await
        }
        Commands::Cleanup {
            db,
            retain_tx_versions,
            retain_dataset_versions,
            legacy_db_path,
        } => {
            let db_path = config.resolve_db_path(db.or(legacy_db_path))?;
            cmd_cleanup(
                &db_path,
                retain_tx_versions,
                retain_dataset_versions,
                json,
                quiet,
            )
            .await
        }
        Commands::Doctor {
            db,
            schema,
            verbose,
            legacy_db_path,
        } => {
            let db_path = config.resolve_db_path(db.or(legacy_db_path))?;
            let schema = schema
                .map(|path| config.resolve_schema_path(Some(path)))
                .transpose()?;
            cmd_doctor(&db_path, schema.as_deref(), verbose, json, quiet).await
        }
        Commands::CdcMaterialize {
            db,
            min_new_rows,
            force,
            legacy_db_path,
        } => {
            let db_path = config.resolve_db_path(db.or(legacy_db_path))?;
            cmd_cdc_materialize(&db_path, min_new_rows, force, json, quiet).await
        }
        Commands::Migrate {
            db,
            schema,
            dry_run,
            format,
            auto_approve,
            legacy_db_path,
        } => {
            let db_path = config.resolve_db_path(db.or(legacy_db_path))?;
            let schema = config.resolve_optional_schema_path(schema);
            let format =
                config.resolve_command_format(format.as_deref(), "table", &["table", "json"])?;
            cmd_migrate(
                &db_path,
                schema.as_deref(),
                dry_run,
                &format,
                auto_approve,
                json,
                quiet,
            )
            .await
        }
        Commands::Storage { command } => match command {
            StorageCommands::Migrate { db, target } => {
                let db_path = config.resolve_db_path(db)?;
                cmd_storage_migrate(&db_path, target, json, quiet).await
            }
        },
        Commands::Lint { db, query, schema } => {
            let db = if db.is_some() {
                config.resolve_optional_db_path(db)
            } else {
                None
            };
            let query = config.resolve_query_path(&query)?;
            let schema = if schema.is_some() || db.is_none() {
                config.resolve_optional_schema_path(schema)
            } else {
                None
            };
            cmd_lint(db, &query, schema.as_deref(), json, quiet).await
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
                &run,
                config.effective_table_max_column_width(),
                config.effective_table_cell_layout(),
                params,
                json,
                quiet,
            )
            .await
        }
    }
}

fn finish_cli_failure(failure: CliFailure) -> Result<()> {
    if failure.json {
        if !failure.rendered {
            println!(
                "{}",
                serde_json::json!({
                    "status": "error",
                    "message": failure.error.to_string(),
                })
            );
        }
        std::process::exit(1);
    }
    Err(failure.error)
}

struct CliFailure {
    json: bool,
    rendered: bool,
    error: color_eyre::eyre::Report,
}

impl CliFailure {
    fn new(json: bool, rendered: bool, error: color_eyre::eyre::Report) -> Self {
        Self {
            json,
            rendered,
            error,
        }
    }
}

#[derive(Debug)]
struct RenderedJsonError(String);

impl fmt::Display for RenderedJsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl StdError for RenderedJsonError {}

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
async fn cmd_init(db_path: &Path, schema_path: &Path, json: bool, quiet: bool) -> Result<()> {
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
    } else if !quiet {
        let color = stdout_supports_color();
        println!(
            "{}",
            format_status_line(
                StatusTone::Ok,
                &format!("Initialized database at {}", db_path.display()),
                color
            )
        );
        for path in &generated_files {
            println!(
                "{}",
                format_status_line(
                    StatusTone::Info,
                    &format!("Generated {}", path.display()),
                    color
                )
            );
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
         [cli]\n\
         table_max_column_width = 80\n\n\
         table_cell_layout = \"truncate\"\n\n\
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
# GEMINI_API_KEY=...\n\
# NANOGRAPH_EMBEDDINGS_MOCK=1\n";

#[instrument(skip(data_path), fields(db_path = %db_path.display(), mode = ?mode))]
async fn cmd_load(
    db_path: &Path,
    data_path: &Path,
    mode: LoadModeArg,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let load_result = match mode {
        LoadModeArg::Append | LoadModeArg::Merge => {
            load_database_file_sparse(db_path, data_path, mode.into()).await
        }
        LoadModeArg::Overwrite => {
            let db = Database::open(db_path).await?;
            db.load_file_with_mode(data_path, mode.into()).await
        }
    };

    if let Err(err) = load_result {
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
    } else if !quiet {
        println!(
            "{}",
            format_status_line(
                StatusTone::Ok,
                &format!("Loaded data into {}", db_path.display()),
                stdout_supports_color()
            )
        );
    }
    Ok(())
}

#[instrument(fields(db_path = %db_path.display(), target = ?target))]
async fn cmd_storage_migrate(
    db_path: &Path,
    target: StorageTargetArg,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let result = match target {
        StorageTargetArg::LanceV4 => migrate_storage_to_lance_v4(db_path).await?,
        StorageTargetArg::LineageNative => migrate_storage_to_lineage_native(db_path).await?,
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "ok",
                "db_path": result.db_path,
                "backup_path": result.backup_path,
                "graph_version": result.graph_version,
                "node_tables": result.node_tables,
                "edge_tables": result.edge_tables,
                "media_uris_rewritten": result.media_uris_rewritten,
                "target": match target {
                    StorageTargetArg::LanceV4 => "lance-v4",
                    StorageTargetArg::LineageNative => "lineage-native",
                },
            })
        );
    } else if !quiet {
        let label = match target {
            StorageTargetArg::LanceV4 => "lance-v4",
            StorageTargetArg::LineageNative => "lineage-native",
        };
        println!(
            "{}",
            format_status_line(
                StatusTone::Ok,
                &format!(
                    "Migrated {} to {} storage (backup: {})",
                    db_path.display(),
                    label,
                    result.backup_path
                ),
                stdout_supports_color()
            )
        );
    }

    Ok(())
}

#[instrument(
    fields(
        db_path = %db_path.display(),
        type_name = options.type_name.as_deref(),
        property = options.property.as_deref(),
        only_null = options.only_null,
        limit = options.limit,
        reindex = options.reindex,
        dry_run = options.dry_run
    )
)]
async fn cmd_embed(db_path: &Path, options: EmbedOptions, json: bool, quiet: bool) -> Result<()> {
    let db = Database::open(db_path).await?;
    let result = db.embed(options.clone()).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "ok",
                "db_path": db_path.display().to_string(),
                "type_name": options.type_name,
                "property": options.property,
                "only_null": options.only_null,
                "limit": options.limit,
                "reindex": options.reindex,
                "dry_run": options.dry_run,
                "node_types_considered": result.node_types_considered,
                "properties_selected": result.properties_selected,
                "rows_selected": result.rows_selected,
                "embeddings_generated": result.embeddings_generated,
                "reindexed_types": result.reindexed_types,
            })
        );
    } else if !quiet {
        let mut message = if result.dry_run {
            format!(
                "Would embed {} value(s) across {} row(s) in {}",
                result.embeddings_generated,
                result.rows_selected,
                db_path.display()
            )
        } else {
            format!(
                "Embedded {} value(s) across {} row(s) in {}",
                result.embeddings_generated,
                result.rows_selected,
                db_path.display()
            )
        };
        if result.reindexed_types > 0 {
            message.push_str(&format!(" (reindexed {} type(s))", result.reindexed_types));
        }
        let tone = if result.dry_run {
            StatusTone::Skip
        } else {
            StatusTone::Ok
        };
        println!(
            "{}",
            format_status_line(tone, &message, stdout_supports_color())
        );
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
        && !json
    {
        let color = stderr_supports_color();
        eprintln!(
            "{}",
            format_status_line(
                StatusTone::Error,
                &format!("Load failed for {}.", db_path.display()),
                color
            )
        );
        eprintln!(
            "  Unique constraint violation: {}.{} has duplicate value '{}'.",
            type_name, property, value
        );
        eprintln!(
            "  Conflicting rows in loaded dataset: {} and {}.",
            first_row, second_row
        );
    }
}

#[instrument(skip(type_name, predicate), fields(db_path = %db_path.display(), type_name = type_name))]
async fn cmd_delete(
    db_path: &Path,
    type_name: &str,
    predicate: &str,
    json: bool,
    quiet: bool,
) -> Result<()> {
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
    } else if !quiet {
        println!(
            "{}",
            format_status_line(
                StatusTone::Ok,
                &format!(
                    "Deleted {} node(s) and {} edge(s) in {}",
                    result.deleted_nodes,
                    result.deleted_edges,
                    db_path.display()
                ),
                stdout_supports_color()
            )
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
    no_embeddings: bool,
    source: VisibleCdcSource,
) -> Result<()> {
    let window = resolve_changes_window(since, from_version, to_version)?;
    let effective_format = if json { "json" } else { format };
    if matches!(
        detect_storage_generation(db_path)?,
        Some(StorageGeneration::NamespaceLineage)
    ) {
        let mut rows = read_visible_change_rows(
            db_path,
            window.from_db_version_exclusive,
            window.to_db_version_inclusive,
        )?;
        if no_embeddings {
            let metadata = DatabaseMetadata::open(db_path)?;
            let embedding_props = build_embedding_property_map(metadata.schema_ir());
            strip_embedding_fields_from_visible_rows(&mut rows, &embedding_props);
        }
        render_visible_change_rows(effective_format, &rows)
    } else {
        let mut rows = read_visible_cdc_entries_with_source(
            db_path,
            window.from_db_version_exclusive,
            window.to_db_version_inclusive,
            source,
        )?;
        if no_embeddings {
            let metadata = DatabaseMetadata::open(db_path)?;
            let embedding_props = build_embedding_property_map(metadata.schema_ir());
            strip_embedding_payloads(&mut rows, &embedding_props);
        }
        render_changes(effective_format, &rows)
    }
}

fn build_embedding_property_map(
    schema_ir: &nanograph::schema_ir::SchemaIR,
) -> HashMap<(String, String), HashSet<String>> {
    let mut out = HashMap::new();
    for node in schema_ir.node_types() {
        let props = node
            .properties
            .iter()
            .filter(|prop| is_embedding_property(prop))
            .map(|prop| prop.name.clone())
            .collect::<HashSet<_>>();
        if !props.is_empty() {
            out.insert(("node".to_string(), node.name.clone()), props);
        }
    }
    for edge in schema_ir.edge_types() {
        let props = edge
            .properties
            .iter()
            .filter(|prop| is_embedding_property(prop))
            .map(|prop| prop.name.clone())
            .collect::<HashSet<_>>();
        if !props.is_empty() {
            out.insert(("edge".to_string(), edge.name.clone()), props);
        }
    }
    out
}

fn is_embedding_property(prop: &nanograph::schema_ir::PropDef) -> bool {
    prop.embed_source.is_some() || prop.scalar_type.starts_with("Vector(")
}

fn strip_embedding_payloads(
    rows: &mut [CdcLogEntry],
    embedding_props: &HashMap<(String, String), HashSet<String>>,
) {
    for row in rows {
        let Some(props) = embedding_props.get(&(row.entity_kind.clone(), row.type_name.clone()))
        else {
            continue;
        };
        strip_embedding_fields_from_payload(&mut row.payload, props);
    }
}

fn strip_embedding_fields_from_visible_rows(
    rows: &mut [VisibleChangeRow],
    embedding_props: &HashMap<(String, String), HashSet<String>>,
) {
    for row in rows {
        let Some(props) = embedding_props.get(&(row.entity_kind.clone(), row.type_name.clone()))
        else {
            continue;
        };
        strip_embedding_fields_from_row(&mut row.row, props);
    }
}

fn strip_embedding_fields_from_payload(
    payload: &mut serde_json::Value,
    embedding_props: &HashSet<String>,
) {
    if let Some(object) = payload.as_object_mut() {
        if let Some(before) = object.get_mut("before") {
            strip_embedding_fields_from_row(before, embedding_props);
        }
        if let Some(after) = object.get_mut("after") {
            strip_embedding_fields_from_row(after, embedding_props);
        }
        if object.contains_key("before") || object.contains_key("after") {
            return;
        }
    }
    strip_embedding_fields_from_row(payload, embedding_props);
}

fn strip_embedding_fields_from_row(row: &mut serde_json::Value, embedding_props: &HashSet<String>) {
    let Some(object) = row.as_object_mut() else {
        return;
    };
    for prop in embedding_props {
        object.remove(prop);
    }
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

fn render_visible_change_rows(format: &str, rows: &[VisibleChangeRow]) -> Result<()> {
    match format {
        "jsonl" => {
            for row in rows {
                let line = serde_json::to_string(row)
                    .wrap_err("failed to serialize visible change row")?;
                println!("{}", line);
            }
        }
        "json" => {
            let out = serde_json::to_string_pretty(rows)
                .wrap_err("failed to serialize visible change rows")?;
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
    quiet: bool,
) -> Result<()> {
    let result = compact_database(
        db_path,
        CompactOptions {
            target_rows_per_fragment,
            materialize_deletions,
            materialize_deletions_threshold,
        },
    )
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
    } else if !quiet {
        println!(
            "{}",
            format_status_line(
                StatusTone::Ok,
                &format!(
                    "Compaction complete for {} (datasets compacted: {}, fragments -{} +{}, files -{} +{}, manifest committed: {})",
                    db_path.display(),
                    result.datasets_compacted,
                    result.fragments_removed,
                    result.fragments_added,
                    result.files_removed,
                    result.files_added,
                    result.manifest_committed
                ),
                stdout_supports_color()
            )
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
    quiet: bool,
) -> Result<()> {
    let result = cleanup_database(
        db_path,
        CleanupOptions {
            retain_tx_versions,
            retain_dataset_versions,
        },
    )
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
    } else if !quiet {
        println!(
            "{}",
            format_status_line(
                StatusTone::Ok,
                &format!(
                    "Cleanup complete for {} (tx removed {}, cdc removed {}, datasets cleaned {}, old versions removed {}, bytes removed {})",
                    db_path.display(),
                    result.tx_rows_removed,
                    result.cdc_rows_removed,
                    result.datasets_cleaned,
                    result.dataset_old_versions_removed,
                    result.dataset_bytes_removed
                ),
                stdout_supports_color()
            )
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
    quiet: bool,
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
    } else if !quiet {
        let color = stdout_supports_color();
        let (tone, message) = if result.skipped_by_threshold {
            (
                StatusTone::Skip,
                format!(
                    "CDC analytics materialization skipped for {} (new rows {}, threshold {})",
                    db_path.display(),
                    result.new_rows_since_last_run,
                    min_new_rows
                ),
            )
        } else {
            (
                StatusTone::Ok,
                format!(
                    "CDC analytics materialized for {} (rows {}, dataset written {}, version {:?})",
                    db_path.display(),
                    result.materialized_rows,
                    result.dataset_written,
                    result.dataset_version
                ),
            )
        };
        println!("{}", format_status_line(tone, &message, color));
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct SchemaDriftSummary {
    current_schema_path: PathBuf,
    desired_schema_path: PathBuf,
    report: SchemaDiffReport,
}

impl SchemaDriftSummary {
    fn matches(&self) -> bool {
        self.report.steps.is_empty()
    }
}

fn load_schema_drift_summary(
    db_path: &Path,
    desired_schema_path: &Path,
) -> Result<SchemaDriftSummary> {
    let current_schema_path = db_path.join("schema.pg");
    let current_schema_src = std::fs::read_to_string(&current_schema_path).wrap_err_with(|| {
        format!(
            "failed to read current DB schema: {}",
            current_schema_path.display()
        )
    })?;
    let desired_schema_src = std::fs::read_to_string(desired_schema_path)
        .wrap_err_with(|| format!("failed to read schema: {}", desired_schema_path.display()))?;
    let current_schema = parse_schema_or_report(&current_schema_path, &current_schema_src)?;
    let desired_schema = parse_schema_or_report(desired_schema_path, &desired_schema_src)?;
    let report = analyze_schema_diff(&current_schema, &desired_schema)?;
    Ok(SchemaDriftSummary {
        current_schema_path,
        desired_schema_path: desired_schema_path.to_path_buf(),
        report,
    })
}

fn schema_drift_hint_message(db_path: &Path, drift: Option<&SchemaDriftSummary>) -> String {
    match drift {
        Some(drift) if !drift.matches() => format!(
            "query is checked against the current DB schema at {}; desired schema {} differs ({}, {} step(s)); run `nanograph migrate {}` to bring the DB schema up to date",
            drift.current_schema_path.display(),
            drift.desired_schema_path.display(),
            schema_compatibility_label(drift.report.compatibility),
            drift.report.steps.len(),
            db_path.display()
        ),
        _ => format!(
            "queries are checked against the current DB schema at {}; if your source schema changed, migrate the DB or pass --schema <path> for drift diagnostics",
            db_path.join("schema.pg").display()
        ),
    }
}

fn typecheck_error_looks_like_schema_drift(error_message: &str) -> bool {
    error_message.contains(" has no property `")
        || error_message.contains("unknown node type `")
        || error_message.contains("unknown node/edge type `")
        || error_message.contains(" not found in catalog")
}

fn render_check_error(
    error: &dyn StdError,
    db_path: &Path,
    drift: Option<&SchemaDriftSummary>,
) -> String {
    let rendered = error.to_string();
    if typecheck_error_looks_like_schema_drift(&rendered) {
        format!(
            "{}\n  hint: {}",
            rendered,
            schema_drift_hint_message(db_path, drift)
        )
    } else {
        rendered
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct LintFinding {
    severity: String,
    code: String,
    message: String,
    type_name: Option<String>,
    property: Option<String>,
    query_names: Vec<String>,
}

#[derive(Debug, Default)]
struct TypeMutationCoverage {
    update_queries: Vec<String>,
    update_props: HashMap<String, Vec<String>>,
}

fn sorted_unique(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn push_unique(target: &mut Vec<String>, value: &str) {
    if !target.iter().any(|existing| existing == value) {
        target.push(value.to_string());
    }
}

fn record_property_query(
    by_property: &mut HashMap<String, Vec<String>>,
    property: &str,
    query_name: &str,
) {
    let entries = by_property.entry(property.to_string()).or_default();
    push_unique(entries, query_name);
}

fn load_lint_schema(
    db_path: Option<&Path>,
    schema_path: Option<&Path>,
) -> Result<(
    PathBuf,
    nanograph::schema::ast::SchemaFile,
    Option<SchemaDriftSummary>,
)> {
    let (schema_path, schema_source, drift) = match (db_path, schema_path) {
        (Some(db_path), Some(schema_path)) => (
            schema_path.to_path_buf(),
            std::fs::read_to_string(schema_path)
                .wrap_err_with(|| format!("failed to read schema: {}", schema_path.display()))?,
            Some(load_schema_drift_summary(db_path, schema_path)?),
        ),
        (_, Some(schema_path)) => (
            schema_path.to_path_buf(),
            std::fs::read_to_string(schema_path)
                .wrap_err_with(|| format!("failed to read schema: {}", schema_path.display()))?,
            None,
        ),
        (Some(db_path), None) => {
            let schema_path = db_path.join("schema.pg");
            (
                schema_path.clone(),
                std::fs::read_to_string(&schema_path).wrap_err_with(|| {
                    format!("failed to read schema: {}", schema_path.display())
                })?,
                None,
            )
        }
        (None, None) => {
            return Err(eyre!(
                "lint requires --schema <schema.pg> or --db <db_path>"
            ));
        }
    };

    let schema = parse_schema_or_report(&schema_path, &schema_source)?;
    Ok((schema_path, schema, drift))
}

fn collect_type_mutation_coverage(
    valid_mutations: &[(String, Mutation)],
) -> HashMap<String, TypeMutationCoverage> {
    let mut coverage: HashMap<String, TypeMutationCoverage> = HashMap::new();

    for (query_name, mutation) in valid_mutations {
        match mutation {
            Mutation::Update(update) => {
                let entry = coverage.entry(update.type_name.clone()).or_default();
                push_unique(&mut entry.update_queries, query_name);
                for assignment in &update.assignments {
                    record_property_query(
                        &mut entry.update_props,
                        &assignment.property,
                        query_name,
                    );
                }
            }
            Mutation::Insert(_) | Mutation::Delete(_) => {}
        }
    }

    coverage
}

fn build_lint_findings(
    schema: &nanograph::schema::ast::SchemaFile,
    valid_mutations: &[(String, Mutation)],
) -> Vec<LintFinding> {
    let coverage = collect_type_mutation_coverage(valid_mutations);
    let mut findings = Vec::new();

    for declaration in &schema.declarations {
        let SchemaDecl::Node(node) = declaration else {
            continue;
        };
        let Some(type_coverage) = coverage.get(&node.name) else {
            continue;
        };
        if type_coverage.update_queries.is_empty() {
            continue;
        }

        let derived_media_props = node
            .properties
            .iter()
            .filter_map(|prop| annotation_value(&prop.annotations, "media_uri"))
            .map(ToOwned::to_owned)
            .collect::<HashSet<_>>();
        let query_names = sorted_unique(type_coverage.update_queries.clone());

        for prop in &node.properties {
            if !prop.prop_type.nullable {
                continue;
            }
            if has_annotation(&prop.annotations, "key") {
                continue;
            }
            if annotation_value(&prop.annotations, "embed").is_some() {
                continue;
            }
            if derived_media_props.contains(&prop.name) {
                continue;
            }
            if type_coverage.update_props.contains_key(&prop.name) {
                continue;
            }

            findings.push(LintFinding {
                severity: "warning".to_string(),
                code: "L201".to_string(),
                message: format!(
                    "{}.{} exists in schema but no update query sets it",
                    node.name, prop.name
                ),
                type_name: Some(node.name.clone()),
                property: Some(prop.name.clone()),
                query_names: query_names.clone(),
            });
        }
    }

    findings.sort_by(|left, right| {
        left.severity
            .cmp(&right.severity)
            .then_with(|| left.type_name.cmp(&right.type_name))
            .then_with(|| left.property.cmp(&right.property))
            .then_with(|| left.message.cmp(&right.message))
    });
    findings
}

#[derive(Debug, Clone)]
enum RunExecutionClass {
    LanceFirst(LanceFirstRunPlan),
    SparseMutation,
    GraphAware,
}

#[derive(Debug, Clone)]
struct LanceFirstRunPlan {
    node_type: String,
    type_ctx: TypeContext,
}

fn classify_run_execution(
    query: &QueryDecl,
    checked: &CheckedQuery,
    metadata: &DatabaseMetadata,
) -> RunExecutionClass {
    match checked {
        CheckedQuery::Read(type_ctx) => {
            if !query_is_lance_first_eligible(query, type_ctx, metadata) {
                return RunExecutionClass::GraphAware;
            }
            let Some(binding) = type_ctx.bindings.values().next() else {
                return RunExecutionClass::GraphAware;
            };
            RunExecutionClass::LanceFirst(LanceFirstRunPlan {
                node_type: binding.type_name.clone(),
                type_ctx: type_ctx.clone(),
            })
        }
        CheckedQuery::Mutation(_) => {
            if query_is_sparse_mutation_eligible(query, metadata) {
                RunExecutionClass::SparseMutation
            } else {
                RunExecutionClass::GraphAware
            }
        }
    }
}

fn query_is_sparse_mutation_eligible(query: &QueryDecl, metadata: &DatabaseMetadata) -> bool {
    match query.mutation.as_ref() {
        Some(nanograph::query::ast::Mutation::Insert(insert)) => {
            metadata
                .catalog()
                .node_types
                .contains_key(&insert.type_name)
                || metadata
                    .catalog()
                    .edge_types
                    .contains_key(&insert.type_name)
        }
        Some(nanograph::query::ast::Mutation::Update(update)) => {
            metadata
                .catalog()
                .node_types
                .contains_key(&update.type_name)
                && !metadata
                    .catalog()
                    .edge_types
                    .contains_key(&update.type_name)
                && metadata
                    .schema_ir()
                    .node_types()
                    .find(|node| node.name == update.type_name)
                    .map(|node| node.properties.iter().any(|prop| prop.key))
                    .unwrap_or(false)
        }
        Some(nanograph::query::ast::Mutation::Delete(delete)) => {
            if metadata
                .catalog()
                .edge_types
                .contains_key(&delete.type_name)
            {
                return true;
            }
            metadata
                .catalog()
                .node_types
                .contains_key(&delete.type_name)
                && !metadata.catalog().edge_types.values().any(|edge| {
                    edge.from_type == delete.type_name || edge.to_type == delete.type_name
                })
        }
        _ => false,
    }
}

fn query_is_lance_first_eligible(
    query: &QueryDecl,
    type_ctx: &TypeContext,
    metadata: &DatabaseMetadata,
) -> bool {
    if query.mutation.is_some() || !type_ctx.traversals.is_empty() || type_ctx.bindings.len() != 1 {
        return false;
    }
    let Some(binding) = type_ctx.bindings.values().next() else {
        return false;
    };
    if binding.kind != BindingKind::Node {
        return false;
    }

    let bound_var = binding.var_name.as_str();
    query.match_clause.iter().all(|clause| {
        clause_is_lance_first_eligible(clause, bound_var, &binding.type_name, metadata)
    }) && query
        .return_clause
        .iter()
        .all(|projection| expr_is_lance_first_eligible(&projection.expr, bound_var, true))
        && query
            .order_clause
            .iter()
            .all(|ordering| expr_is_lance_first_eligible(&ordering.expr, bound_var, false))
}

fn clause_is_lance_first_eligible(
    clause: &Clause,
    bound_var: &str,
    bound_type: &str,
    metadata: &DatabaseMetadata,
) -> bool {
    match clause {
        Clause::Binding(binding) => {
            binding.variable == bound_var
                && binding.prop_matches.iter().all(|prop| {
                    match_value_is_lance_first_eligible(&prop.value)
                        && binding_prop_is_lance_first_eligible(
                            bound_type,
                            &prop.prop_name,
                            metadata,
                        )
                })
        }
        Clause::Filter(filter) => filter_is_lance_first_eligible(filter, bound_var),
        Clause::Traversal(_) | Clause::Negation(_) => false,
    }
}

fn binding_prop_is_lance_first_eligible(
    bound_type: &str,
    prop_name: &str,
    metadata: &DatabaseMetadata,
) -> bool {
    metadata
        .catalog()
        .node_types
        .get(bound_type)
        .and_then(|node_type| node_type.properties.get(prop_name))
        .map(|prop| !prop.list)
        .unwrap_or(false)
}

fn match_value_is_lance_first_eligible(value: &MatchValue) -> bool {
    matches!(
        value,
        MatchValue::Literal(_) | MatchValue::Variable(_) | MatchValue::Now
    )
}

fn filter_is_lance_first_eligible(filter: &nanograph::query::ast::Filter, bound_var: &str) -> bool {
    if filter.op == CompOp::Contains {
        return false;
    }
    (is_bound_prop_access(&filter.left, bound_var) && is_simple_filter_operand(&filter.right))
        || (is_bound_prop_access(&filter.right, bound_var)
            && is_simple_filter_operand(&filter.left))
}

fn is_simple_filter_operand(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(_) | Expr::Variable(_) | Expr::Now)
}

fn expr_is_lance_first_eligible(expr: &Expr, bound_var: &str, allow_literal: bool) -> bool {
    match expr {
        Expr::Variable(variable) => variable == bound_var,
        Expr::PropAccess { variable, .. } => variable == bound_var,
        Expr::Nearest {
            variable,
            query,
            property: _,
        } => variable == bound_var && nearest_query_is_lance_first_eligible(query),
        Expr::Literal(_) => allow_literal,
        Expr::Now
        | Expr::Search { .. }
        | Expr::Fuzzy { .. }
        | Expr::MatchText { .. }
        | Expr::Bm25 { .. }
        | Expr::Rrf { .. }
        | Expr::Aggregate { .. }
        | Expr::AliasRef(_) => false,
    }
}

fn nearest_query_is_lance_first_eligible(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(_) | Expr::Variable(_))
}

fn is_bound_prop_access(expr: &Expr, bound_var: &str) -> bool {
    matches!(
        expr,
        Expr::PropAccess {
            variable,
            property: _
        } if variable == bound_var
    )
}

fn params_with_runtime_now(params: &ParamMap) -> Result<ParamMap> {
    let mut runtime_params = params.clone();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| eyre!("failed to read system time: {}", e))?;
    let millis =
        i64::try_from(now.as_millis()).map_err(|_| eyre!("system time exceeds supported range"))?;
    let dt = arrow_array::temporal_conversions::date64_to_datetime(millis)
        .ok_or_else(|| eyre!("failed to convert system time to DateTime literal"))?;
    runtime_params.insert(
        "__nanograph_now".to_string(),
        Literal::DateTime(dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
    );
    Ok(runtime_params)
}

async fn execute_lance_first_query(
    metadata: &DatabaseMetadata,
    plan: &LanceFirstRunPlan,
    query: &QueryDecl,
    params: &ParamMap,
) -> Result<Vec<RecordBatch>> {
    let _ = lower_query(metadata.catalog(), query, &plan.type_ctx)?;
    let db = Database::open_from_metadata(metadata.clone())?;
    let runtime_params = params_with_runtime_now(params)?;
    match db.run_query(query, &runtime_params).await? {
        nanograph::RunResult::Query(result) => Ok(result.into_batches()),
        nanograph::RunResult::Mutation(_) => Err(eyre!("expected query result")),
    }
}

async fn execute_run_query_batches(
    db_path: &Path,
    query: &QueryDecl,
    params: &ParamMap,
) -> Result<Vec<RecordBatch>> {
    let metadata = DatabaseMetadata::open(db_path)?;
    let checked = typecheck_query_decl(metadata.catalog(), query)?;
    match classify_run_execution(query, &checked, &metadata) {
        RunExecutionClass::LanceFirst(plan) => {
            debug!(
                query = %query.name,
                node_type = %plan.node_type,
                "executing run query via Lance-first path"
            );
            execute_lance_first_query(&metadata, &plan, query, params).await
        }
        RunExecutionClass::SparseMutation => {
            debug!(
                query = %query.name,
                "executing run mutation via sparse mutation path"
            );
            let result = run_mutation_query_sparse(db_path, query, params).await?;
            Ok(vec![result.to_record_batch()?])
        }
        RunExecutionClass::GraphAware => {
            debug!(query = %query.name, "executing run query via full graph path");
            let db = Database::open(db_path).await?;
            let run_result = db.run_query(query, params).await?;
            Ok(run_result.into_record_batches()?)
        }
    }
}

#[instrument(fields(db_path = %db_path.display()))]
async fn cmd_doctor(
    db_path: &Path,
    desired_schema_path: Option<&Path>,
    verbose: bool,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let db = Database::open(db_path).await?;
    let mut report = db.doctor().await?;
    let schema_drift = desired_schema_path
        .map(|path| load_schema_drift_summary(db_path, path))
        .transpose()?;

    if let Some(drift) = &schema_drift
        && !drift.matches()
    {
        report.issues.push(format!(
            "database schema differs from desired schema {} ({}, {} step(s)); run `nanograph migrate {}`",
            drift.desired_schema_path.display(),
            schema_compatibility_label(drift.report.compatibility),
            drift.report.steps.len(),
            db_path.display()
        ));
        report.warnings.extend(
            drift
                .report
                .warnings
                .iter()
                .map(|warning| format!("schema diff: {}", warning)),
        );
        report.issues.extend(
            drift
                .report
                .blocked
                .iter()
                .map(|item| format!("schema diff blocked: {}", item)),
        );
        report.healthy = false;
    }

    let dataset_storage_formats: Vec<serde_json::Value> = report
        .datasets
        .iter()
        .map(|dataset| {
            serde_json::json!({
                "kind": dataset.kind,
                "type_name": dataset.type_name,
                "dataset_path": dataset.dataset_path,
                "dataset_version": dataset.dataset_version,
                "storage_version": dataset.storage_version,
            })
        })
        .collect();

    if json {
        let schema_status = schema_drift.as_ref().map(|drift| {
            serde_json::json!({
                "current_schema_path": drift.current_schema_path.display().to_string(),
                "desired_schema_path": drift.desired_schema_path.display().to_string(),
                "matches": drift.matches(),
                "old_schema_hash": drift.report.old_schema_hash,
                "new_schema_hash": drift.report.new_schema_hash,
                "compatibility": schema_compatibility_label(drift.report.compatibility),
                "has_breaking": drift.report.has_breaking,
                "step_count": drift.report.steps.len(),
                "steps": drift.report.steps.clone(),
                "warnings": drift.report.warnings.clone(),
                "blocked": drift.report.blocked.clone(),
            })
        });
        println!(
            "{}",
            serde_json::json!({
                "status": if report.healthy { "ok" } else { "error" },
                "db_path": db_path.display().to_string(),
                "healthy": report.healthy,
                "manifest_db_version": report.manifest_db_version,
                "datasets_checked": report.datasets_checked,
                "dataset_storage_formats": dataset_storage_formats,
                "tx_rows": report.tx_rows,
                "cdc_rows": report.cdc_rows,
                "lineage_shadow": report.lineage_shadow,
                "issues": report.issues,
                "warnings": report.warnings,
                "schema_status": schema_status,
            })
        );
    } else {
        let use_stderr = quiet && !report.healthy;
        let color = if use_stderr {
            stderr_supports_color()
        } else {
            stdout_supports_color()
        };
        if report.healthy {
            if !quiet {
                println!(
                    "{}",
                    format_status_line(
                        StatusTone::Ok,
                        &format!(
                            "Doctor OK for {} (db_version {}, datasets checked {}, tx rows {}, cdc rows {})",
                            db_path.display(),
                            report.manifest_db_version,
                            report.datasets_checked,
                            report.tx_rows,
                            report.cdc_rows
                        ),
                        color
                    )
                );
                if verbose && !report.datasets.is_empty() {
                    println!("{}", style_label("Dataset storage formats:", color));
                    for dataset in &report.datasets {
                        println!(
                            "  {} {}: {} ({}, v{})",
                            dataset.kind,
                            dataset.type_name,
                            style_scalar(
                                &format!("Lance {}", dataset.storage_version),
                                "1;34",
                                color
                            ),
                            dataset.dataset_path,
                            dataset.dataset_version
                        );
                    }
                }
                for warning in &report.warnings {
                    println!("{}", format_status_line(StatusTone::Warn, warning, color));
                }
            }
        } else {
            let summary = format_status_line(
                StatusTone::Error,
                &format!(
                    "Doctor found issues for {} (db_version {}, datasets checked {}, tx rows {}, cdc rows {})",
                    db_path.display(),
                    report.manifest_db_version,
                    report.datasets_checked,
                    report.tx_rows,
                    report.cdc_rows
                ),
                color,
            );
            if use_stderr {
                eprintln!("{}", summary);
                if verbose && !report.datasets.is_empty() {
                    eprintln!("{}", style_label("Dataset storage formats:", color));
                    for dataset in &report.datasets {
                        eprintln!(
                            "  {} {}: {} ({}, v{})",
                            dataset.kind,
                            dataset.type_name,
                            style_scalar(
                                &format!("Lance {}", dataset.storage_version),
                                "1;34",
                                color
                            ),
                            dataset.dataset_path,
                            dataset.dataset_version
                        );
                    }
                }
                for issue in &report.issues {
                    eprintln!("{}", format_status_line(StatusTone::Error, issue, color));
                }
                for warning in &report.warnings {
                    eprintln!("{}", format_status_line(StatusTone::Warn, warning, color));
                }
            } else {
                println!("{}", summary);
                if verbose && !report.datasets.is_empty() {
                    println!("{}", style_label("Dataset storage formats:", color));
                    for dataset in &report.datasets {
                        println!(
                            "  {} {}: {} ({}, v{})",
                            dataset.kind,
                            dataset.type_name,
                            style_scalar(
                                &format!("Lance {}", dataset.storage_version),
                                "1;34",
                                color
                            ),
                            dataset.dataset_path,
                            dataset.dataset_version
                        );
                    }
                }
                for issue in &report.issues {
                    println!("{}", format_status_line(StatusTone::Error, issue, color));
                }
                for warning in &report.warnings {
                    println!("{}", format_status_line(StatusTone::Warn, warning, color));
                }
            }
        }
    }

    if report.healthy {
        Ok(())
    } else {
        if json {
            return Err(RenderedJsonError(format!(
                "doctor detected {} issue(s)",
                report.issues.len()
            ))
            .into());
        }
        Err(eyre!("doctor detected {} issue(s)", report.issues.len()))
    }
}

#[instrument(skip(query_path), fields(query_path = %query_path.display()))]
async fn cmd_lint(
    db_path: Option<PathBuf>,
    query_path: &PathBuf,
    schema_path: Option<&Path>,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let query_src = std::fs::read_to_string(query_path)
        .wrap_err_with(|| format!("failed to read query: {}", query_path.display()))?;
    let (effective_schema_path, schema, schema_drift) =
        load_lint_schema(db_path.as_deref(), schema_path)?;
    let catalog = build_catalog(&schema)?;

    let queries = parse_query_or_report(query_path, &query_src)?;

    let mut error_count = 0;
    let mut warning_count = 0;
    let mut info_count = 0;
    let mut checks = Vec::with_capacity(queries.queries.len());
    let mut valid_mutations = Vec::new();
    for q in &queries.queries {
        let mutation_warning = (q.mutation.is_some() && q.params.is_empty()).then(|| {
            "mutation declares no params; hardcoded mutations are easy to miss".to_string()
        });
        match typecheck_query_decl(&catalog, q) {
            Ok(CheckedQuery::Read(_)) => {
                if !json && !quiet {
                    println!(
                        "{}",
                        format_status_line(
                            StatusTone::Ok,
                            &format!("query `{}` (read)", q.name),
                            stdout_supports_color()
                        )
                    );
                }
                checks.push(serde_json::json!({
                    "name": q.name,
                    "kind": "read",
                    "status": "ok",
                }));
            }
            Ok(CheckedQuery::Mutation(_)) => {
                if let Some(mutation) = &q.mutation {
                    valid_mutations.push((q.name.clone(), mutation.clone()));
                }
                if mutation_warning.is_some() {
                    warning_count += 1;
                }
                if !json && !quiet {
                    println!(
                        "{}",
                        format_status_line(
                            StatusTone::Ok,
                            &format!("query `{}` (mutation)", q.name),
                            stdout_supports_color()
                        )
                    );
                    if let Some(warning) = &mutation_warning {
                        println!(
                            "{}",
                            format_status_line(
                                StatusTone::Warn,
                                &format!("query `{}`: {}", q.name, warning),
                                stdout_supports_color()
                            )
                        );
                    }
                }
                let mut check = serde_json::json!({
                    "name": q.name,
                    "kind": "mutation",
                    "status": "ok",
                });
                if let Some(warning) = mutation_warning {
                    check["warnings"] = serde_json::json!([warning]);
                }
                checks.push(check);
            }
            Err(e) => {
                let rendered_error = if let Some(db_path) = db_path.as_deref() {
                    render_check_error(&e, db_path, schema_drift.as_ref())
                } else {
                    e.to_string()
                };
                if !json {
                    let message = format_status_line(
                        StatusTone::Error,
                        &format!("query `{}`: {}", q.name, rendered_error),
                        if quiet {
                            stderr_supports_color()
                        } else {
                            stdout_supports_color()
                        },
                    );
                    if quiet {
                        eprintln!("{}", message);
                    } else {
                        println!("{}", message);
                    }
                }
                checks.push(serde_json::json!({
                    "name": q.name,
                    "kind": if q.mutation.is_some() { "mutation" } else { "read" },
                    "status": "error",
                    "error": rendered_error,
                }));
                error_count += 1;
            }
        }
    }

    let findings = build_lint_findings(&schema, &valid_mutations);
    for finding in &findings {
        match finding.severity.as_str() {
            "error" => error_count += 1,
            "warning" => warning_count += 1,
            "info" => info_count += 1,
            _ => {}
        }

        if !json && !quiet {
            let tone = match finding.severity.as_str() {
                "error" => StatusTone::Error,
                "warning" => StatusTone::Warn,
                _ => StatusTone::Info,
            };
            println!(
                "{}",
                format_status_line(tone, &finding.message, stdout_supports_color())
            );
        }
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": if error_count == 0 { "ok" } else { "error" },
                "schema_path": effective_schema_path.display().to_string(),
                "query_path": query_path.display().to_string(),
                "queries_processed": queries.queries.len(),
                "errors": error_count,
                "warnings": warning_count,
                "infos": info_count,
                "results": checks,
                "findings": findings,
            })
        );
    } else if !quiet {
        println!(
            "{}",
            format_status_line(
                StatusTone::Info,
                &format!(
                    "Lint complete: {} queries processed ({} error(s), {} warning(s), {} info item(s))",
                    queries.queries.len(),
                    error_count,
                    warning_count,
                    info_count
                ),
                stdout_supports_color()
            )
        );
    }
    if error_count > 0 {
        if json {
            return Err(
                RenderedJsonError(format!("lint detected {} error(s)", error_count)).into(),
            );
        }
        return Err(eyre!("lint detected {} error(s)", error_count));
    }
    Ok(())
}

#[instrument(
    skip(run, raw_params),
    fields(
        db_path = %db_path.display(),
        query_name = %run.query_name,
        query_path = %run.query_path.display(),
        format = %run.format
    )
)]
async fn cmd_run(
    db_path: PathBuf,
    run: &ResolvedRunConfig,
    table_max_column_width: usize,
    table_cell_layout: TableCellLayout,
    raw_params: Vec<(String, String)>,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let query_src = std::fs::read_to_string(&run.query_path)
        .wrap_err_with(|| format!("failed to read query: {}", run.query_path.display()))?;

    // Parse queries and find the named one
    let queries = parse_query_or_report(&run.query_path, &query_src)?;
    let query = queries
        .queries
        .iter()
        .find(|q| q.name == run.query_name)
        .ok_or_else(|| eyre!("query `{}` not found", run.query_name))?;
    info!("executing query");

    // Build param map from CLI args, using query param type info for inference
    let param_map = build_param_map(&query.params, &raw_params)?;

    let effective_format = if json { "json" } else { run.format.as_str() };
    let human_output = !quiet || !is_human_run_format(effective_format);
    if human_output && let Some(preamble) = query_execution_preamble(query, effective_format, json)
    {
        print!("{}", preamble);
    }
    let started = Instant::now();
    let results = execute_run_query_batches(&db_path, query, &param_map).await?;
    let elapsed = started.elapsed();
    if human_output {
        render_results(
            query,
            effective_format,
            &results,
            table_max_column_width,
            table_cell_layout,
        )?;
    }
    if human_output && effective_format == "table" {
        print_table_footer(&results, elapsed);
    }
    Ok(())
}

fn is_human_run_format(format: &str) -> bool {
    matches!(format, "table" | "kv")
}

fn query_execution_preamble(
    query: &nanograph::query::ast::QueryDecl,
    format: &str,
    json: bool,
) -> Option<String> {
    if json || !matches!(format, "table" | "kv") {
        return None;
    }
    let has_metadata = query.description.is_some() || query.instruction.is_some();
    if !has_metadata {
        return None;
    }

    let color = stdout_supports_color();
    let mut lines = vec![format!("{} {}", style_label("Query:", color), query.name)];
    if let Some(description) = &query.description {
        lines.push(format!(
            "{} {}",
            style_label("Description:", color),
            description
        ));
    }
    if let Some(instruction) = &query.instruction {
        lines.push(format!(
            "{} {}",
            style_label("Instruction:", color),
            instruction
        ));
    }
    Some(format!("{}\n\n", lines.join("\n")))
}

fn render_results(
    query: &nanograph::query::ast::QueryDecl,
    format: &str,
    results: &[RecordBatch],
    table_max_column_width: usize,
    table_cell_layout: TableCellLayout,
) -> Result<()> {
    match format {
        "table" => {
            print!(
                "{}",
                format_table(results, table_max_column_width, table_cell_layout)
            );
        }
        "kv" => {
            print_kv(results);
        }
        "csv" => {
            print!("{}", format_csv(results));
        }
        "jsonl" => {
            print!("{}", format_jsonl_with_query(query, results)?);
        }
        "json" => {
            print_json_with_query(query, results)?;
        }
        _ => return Err(eyre!("unknown format: {}", format)),
    }
    Ok(())
}

fn print_table_footer(results: &[RecordBatch], elapsed: Duration) {
    let rows: usize = results.iter().map(RecordBatch::num_rows).sum();
    println!("({} rows, {})", rows, format_elapsed(elapsed));
}

fn print_kv(results: &[RecordBatch]) {
    let rows = nanograph::json_output::record_batches_to_json_rows(results);
    print!("{}", format_kv_rows(&rows, stdout_supports_color()));
}

fn format_table(
    results: &[RecordBatch],
    table_max_column_width: usize,
    table_cell_layout: TableCellLayout,
) -> String {
    if results.is_empty() {
        return "(empty result)\n".to_string();
    }

    let mut table = Table::new();
    table
        .load_preset(if io::stdout().is_terminal() {
            UTF8_FULL_CONDENSED
        } else {
            ASCII_BORDERS_ONLY_CONDENSED
        })
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic);
    if matches!(table_cell_layout, TableCellLayout::Wrap) {
        let column_count = results[0].num_columns() as u16;
        if column_count > 0 {
            let content_width = table_max_column_width.max(1) as u16;
            table.set_constraints(std::iter::repeat_n(
                ColumnConstraint::UpperBoundary(Width::Fixed(content_width)),
                column_count.into(),
            ));
            let estimated_total_width = column_count
                .saturating_mul(content_width.saturating_add(3))
                .saturating_add(1);
            table.set_width(estimated_total_width);
        }
    }

    let schema = results[0].schema();
    let header = schema
        .fields()
        .iter()
        .map(|field| {
            let mut cell = Cell::new(field.name().as_str()).add_attribute(Attribute::Bold);
            if stdout_supports_color() {
                cell = cell.fg(TableColor::Cyan);
            }
            if is_numeric_type(field.data_type()) {
                cell = cell.set_alignment(CellAlignment::Right);
            }
            cell
        })
        .collect::<Vec<_>>();
    table.set_header(header);

    for batch in results {
        for row_idx in 0..batch.num_rows() {
            let row = batch
                .schema()
                .fields()
                .iter()
                .enumerate()
                .map(|(col_idx, field)| {
                    let value =
                        arrow_cast::display::array_value_to_string(batch.column(col_idx), row_idx)
                            .unwrap_or_default();
                    let rendered = match table_cell_layout {
                        TableCellLayout::Truncate => {
                            format_table_cell_value(&value, table_max_column_width)
                        }
                        TableCellLayout::Wrap => compact_whitespace(&value),
                    };
                    let mut cell = Cell::new(rendered);
                    if is_numeric_type(field.data_type()) {
                        cell = cell.set_alignment(CellAlignment::Right);
                    }
                    cell
                })
                .collect::<Vec<_>>();
            table.add_row(row);
        }
    }

    format!("{}\n", table)
}

fn format_table_cell_value(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        return compact_whitespace(value);
    }

    let compacted = compact_whitespace(value);
    let char_count = compacted.chars().count();
    if char_count <= max_width {
        return compacted;
    }

    if max_width <= 3 {
        return ".".repeat(max_width);
    }

    let truncated: String = compacted.chars().take(max_width - 3).collect();
    format!("{truncated}...")
}

fn compact_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_numeric_type(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float16
            | DataType::Float32
            | DataType::Float64
            | DataType::Decimal128(_, _)
            | DataType::Decimal256(_, _)
    )
}

fn format_elapsed(elapsed: Duration) -> String {
    if elapsed.as_millis() < 1_000 {
        format!("{}ms", elapsed.as_millis())
    } else {
        format!("{:.2}s", elapsed.as_secs_f64())
    }
}

fn format_csv(results: &[RecordBatch]) -> String {
    let Some(first_batch) = results.first() else {
        return String::new();
    };

    let schema = first_batch.schema();
    let header = schema
        .fields()
        .iter()
        .map(|field| escape_csv_field(field.name()))
        .collect::<Vec<_>>()
        .join(",");

    let mut out = String::new();
    out.push_str(&header);
    out.push('\n');

    for batch in results {
        for row in 0..batch.num_rows() {
            let mut values = Vec::with_capacity(batch.num_columns());
            for col in 0..batch.num_columns() {
                let col_arr = batch.column(col);
                let value =
                    arrow_cast::display::array_value_to_string(col_arr, row).unwrap_or_default();
                values.push(escape_csv_field(&value));
            }
            out.push_str(&values.join(","));
            out.push('\n');
        }
    }

    out
}

fn escape_csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
fn format_jsonl(results: &[RecordBatch]) -> Result<String> {
    let rows = nanograph::json_output::record_batches_to_json_rows(results);
    let mut out = String::new();
    for row in rows {
        let line = serde_json::to_string(&row).wrap_err("failed to serialize JSONL output")?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

fn format_jsonl_with_query(
    query: &nanograph::query::ast::QueryDecl,
    results: &[RecordBatch],
) -> Result<String> {
    let rows = nanograph::json_output::record_batches_to_json_rows(results);
    let mut out = String::new();
    let metadata = serde_json::json!({
        "$nanograph": {
            "query": query_metadata_json(query),
        }
    });
    let metadata_line =
        serde_json::to_string(&metadata).wrap_err("failed to serialize JSONL metadata")?;
    out.push_str(&metadata_line);
    out.push('\n');
    for row in rows {
        let line = serde_json::to_string(&row).wrap_err("failed to serialize JSONL output")?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

fn print_json_with_query(
    query: &nanograph::query::ast::QueryDecl,
    results: &[RecordBatch],
) -> Result<()> {
    let rows = nanograph::json_output::record_batches_to_json_rows(results);
    let out = serde_json::to_string_pretty(&serde_json::json!({
        "$nanograph": {
            "query": query_metadata_json(query),
        },
        "rows": rows,
    }))
    .wrap_err("failed to serialize JSON output")?;
    println!("{}", out);
    Ok(())
}

fn query_metadata_json(query: &nanograph::query::ast::QueryDecl) -> serde_json::Value {
    let mut query_json = serde_json::Map::new();
    query_json.insert(
        "name".to_string(),
        serde_json::Value::String(query.name.clone()),
    );
    if let Some(description) = &query.description {
        query_json.insert(
            "description".to_string(),
            serde_json::Value::String(description.clone()),
        );
    }
    if let Some(instruction) = &query.instruction {
        query_json.insert(
            "instruction".to_string(),
            serde_json::Value::String(instruction.clone()),
        );
    }
    serde_json::Value::Object(query_json)
}

fn format_kv_rows(rows: &[serde_json::Value], color: bool) -> String {
    if rows.is_empty() {
        return "(empty result)\n".to_string();
    }

    let mut out = String::new();
    for (idx, row) in rows.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
            out.push_str(&render_kv_row_separator(color));
            out.push('\n');
            out.push('\n');
        }
        match row {
            serde_json::Value::Object(map) => {
                out.push_str(&render_kv_row_header(idx + 1, map, color));
                out.push('\n');
                format_kv_object(&mut out, map, 0, color);
            }
            other => {
                out.push_str(&render_kv_row_header(
                    idx + 1,
                    &serde_json::Map::new(),
                    color,
                ));
                out.push('\n');
                out.push_str(&render_kv_scalar(other, color));
                out.push('\n');
            }
        }
    }
    out
}

fn render_kv_row_header(
    row_number: usize,
    map: &serde_json::Map<String, serde_json::Value>,
    color: bool,
) -> String {
    let type_name = map.get("type").and_then(kv_header_scalar);
    let identity = ["slug", "id", "name"]
        .into_iter()
        .find_map(|key| map.get(key).and_then(kv_header_scalar));

    let header = match (type_name, identity) {
        (Some(type_name), Some(identity)) => format!("{type_name}: {identity}"),
        (Some(type_name), None) => format!("Row {row_number}: {type_name}"),
        (None, Some(identity)) => format!("Row {row_number}: {identity}"),
        (None, None) => format!("Row {row_number}"),
    };

    if color {
        format!("\x1b[1;36m{header}\x1b[0m")
    } else {
        header
    }
}

fn kv_header_scalar(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) if !text.is_empty() => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(boolean) => Some(boolean.to_string()),
        _ => None,
    }
}

fn render_kv_row_separator(color: bool) -> String {
    let separator = "────────────────────────";
    if color {
        format!("\x1b[2m{separator}\x1b[0m")
    } else {
        separator.to_string()
    }
}

fn format_kv_object(
    out: &mut String,
    map: &serde_json::Map<String, serde_json::Value>,
    indent: usize,
    color: bool,
) {
    let key_width = map.keys().map(|key| key.chars().count()).max().unwrap_or(0);
    for (key, value) in map {
        format_kv_entry(out, key, value, indent, key_width, color);
    }
}

fn format_kv_entry(
    out: &mut String,
    key: &str,
    value: &serde_json::Value,
    indent: usize,
    key_width: usize,
    color: bool,
) {
    out.push_str(&" ".repeat(indent));
    let padded_key = format!("{key:width$}", width = key_width);
    out.push_str(&style_key(&padded_key, color));
    out.push(':');

    match value {
        serde_json::Value::Array(items) if !items.is_empty() => {
            out.push('\n');
            format_kv_array(out, items, indent + 2, color);
        }
        serde_json::Value::Object(map) if !map.is_empty() => {
            out.push('\n');
            format_kv_object(out, map, indent + 2, color);
        }
        _ => {
            out.push(' ');
            out.push_str(&render_kv_scalar(value, color));
            out.push('\n');
        }
    }
}

fn format_kv_array(out: &mut String, items: &[serde_json::Value], indent: usize, color: bool) {
    for item in items {
        out.push_str(&" ".repeat(indent));
        out.push_str("- ");
        match item {
            serde_json::Value::Array(values) if !values.is_empty() => {
                out.push('\n');
                format_kv_array(out, values, indent + 2, color);
            }
            serde_json::Value::Object(map) if !map.is_empty() => {
                out.push('\n');
                format_kv_object(out, map, indent + 2, color);
            }
            _ => {
                out.push_str(&render_kv_scalar(item, color));
                out.push('\n');
            }
        }
    }
}

fn render_kv_scalar(value: &serde_json::Value, color: bool) -> String {
    match value {
        serde_json::Value::Null => style_scalar("null", "2", color),
        serde_json::Value::Bool(true) => style_scalar("true", "32", color),
        serde_json::Value::Bool(false) => style_scalar("false", "31", color),
        serde_json::Value::Number(number) => style_scalar(&number.to_string(), "33", color),
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(items) if items.is_empty() => "[]".to_string(),
        serde_json::Value::Object(map) if map.is_empty() => "{}".to_string(),
        other => other.to_string(),
    }
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
