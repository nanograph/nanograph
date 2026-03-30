#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr, eyre};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NanographConfig {
    #[serde(default)]
    pub project: ProjectConfig,
    #[serde(default)]
    pub db: DbConfig,
    #[serde(default)]
    pub schema: SchemaConfig,
    #[serde(default)]
    pub query: QueryConfig,
    #[serde(default)]
    pub query_aliases: HashMap<String, QueryAliasConfig>,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
    #[serde(default)]
    pub cli: CliDefaults,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectConfig {
    pub name: Option<String>,
    pub description: Option<String>,
    pub instruction: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DbConfig {
    pub default_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SchemaConfig {
    pub default_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct QueryConfig {
    #[serde(default)]
    pub roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct QueryAliasConfig {
    pub query: Option<PathBuf>,
    pub name: Option<String>,
    pub format: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EmbeddingConfig {
    pub api_key_env: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub mock: Option<bool>,
    pub batch_size: Option<usize>,
    #[serde(alias = "chunk_chars")]
    pub chunk_size: Option<usize>,
    pub chunk_overlap_chars: Option<usize>,
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CliDefaults {
    pub output_format: Option<String>,
    pub json: Option<bool>,
    pub table_max_column_width: Option<usize>,
    pub table_cell_layout: Option<TableCellLayout>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TableCellLayout {
    #[default]
    Truncate,
    Wrap,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub path: Option<PathBuf>,
    pub base_dir: PathBuf,
    pub settings: NanographConfig,
}

#[derive(Debug, Clone)]
pub struct ResolvedRunConfig {
    pub query_path: PathBuf,
    pub query_name: String,
    pub format: String,
    pub positional_param_names: Vec<String>,
}

impl LoadedConfig {
    pub fn effective_table_max_column_width(&self) -> usize {
        self.settings.cli.table_max_column_width.unwrap_or(80)
    }

    pub fn effective_table_cell_layout(&self) -> TableCellLayout {
        self.settings.cli.table_cell_layout.unwrap_or_default()
    }

    pub fn load(explicit_path: Option<&Path>) -> Result<Self> {
        let current_dir =
            std::env::current_dir().wrap_err("failed to resolve current directory")?;
        Self::load_from_dir(explicit_path, &current_dir)
    }

    fn load_from_dir(explicit_path: Option<&Path>, current_dir: &Path) -> Result<Self> {
        let shared_path = match explicit_path {
            Some(path) => Some(path.to_path_buf()),
            None => {
                let candidate = current_dir.join("nanograph.toml");
                candidate.exists().then_some(candidate)
            }
        };

        let Some(base_dir) = shared_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(Path::to_path_buf)
        else {
            return Ok(Self {
                path: None,
                base_dir: current_dir.to_path_buf(),
                settings: NanographConfig::default(),
            });
        };

        let mut settings = NanographConfig::default();
        if let Some(path) = &shared_path {
            settings.merge(parse_config_file(path)?);
        }

        Ok(Self {
            path: shared_path,
            base_dir,
            settings,
        })
    }

    pub fn effective_json(&self, cli_json: bool) -> bool {
        cli_json || self.settings.cli.json.unwrap_or(false)
    }

    pub fn resolve_format(
        &self,
        cli_format: Option<&str>,
        fallback: &'static str,
        allowed: &[&str],
    ) -> Result<String> {
        let value = cli_format
            .map(str::to_string)
            .or_else(|| self.settings.cli.output_format.clone())
            .unwrap_or_else(|| fallback.to_string());
        self.validate_format(value, allowed)
    }

    pub fn resolve_command_format(
        &self,
        cli_format: Option<&str>,
        fallback: &'static str,
        allowed: &[&str],
    ) -> Result<String> {
        let value = if let Some(value) = cli_format {
            value.to_string()
        } else if let Some(configured) = self.settings.cli.output_format.clone() {
            if allowed
                .iter()
                .any(|allowed_value| *allowed_value == configured)
            {
                configured
            } else {
                fallback.to_string()
            }
        } else {
            fallback.to_string()
        };
        self.validate_format(value, allowed)
    }

    pub fn resolve_db_path(&self, cli_path: Option<PathBuf>) -> Result<PathBuf> {
        match self.resolve_optional_db_path(cli_path) {
            Some(path) => Ok(path),
            None => Err(eyre!(
                "database path is required (<db_path>, --db, or db.default_path in nanograph.toml)"
            )),
        }
    }

    pub fn resolve_optional_db_path(&self, cli_path: Option<PathBuf>) -> Option<PathBuf> {
        cli_path
            .or_else(|| self.settings.db.default_path.clone())
            .map(|path| self.resolve_relative(path))
    }

    pub fn resolve_run_config(
        &self,
        alias: Option<&str>,
        cli_query: Option<PathBuf>,
        cli_name: Option<&str>,
        cli_format: Option<&str>,
    ) -> Result<ResolvedRunConfig> {
        let alias_config = match alias {
            Some(alias_name) => Some(
                self.settings
                    .query_aliases
                    .get(alias_name)
                    .ok_or_else(|| eyre!(
                        "query alias `{}` not found in nanograph.toml (use --query for explicit query files)",
                        alias_name
                    ))?,
            ),
            None => None,
        };

        let query = cli_query
            .or_else(|| alias_config.and_then(|alias| alias.query.clone()))
            .ok_or_else(|| {
                eyre!(
                    "query path is required (--query or query_aliases.<alias>.query in nanograph.toml)"
                )
            })?;
        let query_path = self.resolve_query_path(&query)?;

        let query_name = cli_name
            .map(str::to_string)
            .or_else(|| alias_config.and_then(|alias| alias.name.clone()))
            .ok_or_else(|| {
                eyre!(
                    "query name is required (--name or query_aliases.<alias>.name in nanograph.toml)"
                )
            })?;

        let format = cli_format
            .map(str::to_string)
            .or_else(|| alias_config.and_then(|alias| alias.format.clone()))
            .or_else(|| self.settings.cli.output_format.clone())
            .unwrap_or_else(|| "table".to_string());
        let format = self.validate_format(format, &["table", "kv", "csv", "jsonl", "json"])?;
        let positional_param_names = alias_config
            .map(|alias| alias.args.clone())
            .unwrap_or_default();

        Ok(ResolvedRunConfig {
            query_path,
            query_name,
            format,
            positional_param_names,
        })
    }

    fn validate_format(&self, value: String, allowed: &[&str]) -> Result<String> {
        if allowed.iter().any(|allowed_value| *allowed_value == value) {
            Ok(value)
        } else {
            Err(eyre!(
                "unsupported configured output format `{}` for this command (allowed: {})",
                value,
                allowed.join(", ")
            ))
        }
    }

    pub fn resolve_schema_path(&self, cli_path: Option<PathBuf>) -> Result<PathBuf> {
        match cli_path.or_else(|| self.settings.schema.default_path.clone()) {
            Some(path) => Ok(self.resolve_relative(path)),
            None => Err(eyre!(
                "schema path is required (--schema or schema.default_path in nanograph.toml)"
            )),
        }
    }

    pub fn resolve_optional_schema_path(&self, cli_path: Option<PathBuf>) -> Option<PathBuf> {
        cli_path
            .or_else(|| self.settings.schema.default_path.clone())
            .map(|path| self.resolve_relative(path))
    }

    pub fn resolve_query_path(&self, query_path: &Path) -> Result<PathBuf> {
        if query_path.is_absolute() || query_path.exists() {
            return Ok(query_path.to_path_buf());
        }

        let direct = self.base_dir.join(query_path);
        if direct.exists() {
            return Ok(direct);
        }

        for root in &self.settings.query.roots {
            let candidate = self.resolve_relative(root.clone()).join(query_path);
            if candidate.exists() {
                return Ok(candidate);
            }
        }

        Err(eyre!(
            "failed to resolve query path: {}",
            query_path.display()
        ))
    }

    fn resolve_relative(&self, path: PathBuf) -> PathBuf {
        if path.is_absolute() {
            path
        } else {
            self.base_dir.join(path)
        }
    }

    pub fn apply_embedding_env_for_process(&self) -> Result<()> {
        self.apply_embedding_env_with(
            |key| std::env::var_os(key).is_some(),
            |key| std::env::var(key).ok(),
            |key, value| {
                // SAFETY: this runs during CLI bootstrap before command execution.
                unsafe { std::env::set_var(key, value) };
            },
        )
    }

    fn apply_embedding_env_with<FExists, FGet, FSet>(
        &self,
        mut exists: FExists,
        mut get: FGet,
        mut set: FSet,
    ) -> Result<()>
    where
        FExists: FnMut(&str) -> bool,
        FGet: FnMut(&str) -> Option<String>,
        FSet: FnMut(&str, &str),
    {
        let embedding = &self.settings.embedding;
        let provider = trim_nonempty(embedding.provider.as_deref()).unwrap_or("openai");
        match provider {
            "openai" | "gemini" | "local" => {
                set_if_missing(
                    &mut exists,
                    &mut set,
                    "NANOGRAPH_EMBED_PROVIDER",
                    Some(provider),
                );
            }
            "mock" => {
                if !exists("NANOGRAPH_EMBEDDINGS_MOCK") {
                    set("NANOGRAPH_EMBEDDINGS_MOCK", "1");
                }
            }
            other => {
                return Err(eyre!(
                    "unsupported embedding.provider `{}` (supported: openai, gemini, local, mock)",
                    other
                ));
            }
        }

        if embedding.mock == Some(true) && !exists("NANOGRAPH_EMBEDDINGS_MOCK") {
            set("NANOGRAPH_EMBEDDINGS_MOCK", "1");
        }
        set_if_missing(
            &mut exists,
            &mut set,
            "NANOGRAPH_EMBED_MODEL",
            embedding.model.as_deref(),
        );
        set_if_missing(
            &mut exists,
            &mut set,
            match provider {
                "gemini" => "GEMINI_BASE_URL",
                _ => "OPENAI_BASE_URL",
            },
            embedding.base_url.as_deref(),
        );
        set_if_missing_usize(
            &mut exists,
            &mut set,
            "NANOGRAPH_EMBED_BATCH_SIZE",
            embedding.batch_size,
        );
        set_if_missing_usize(
            &mut exists,
            &mut set,
            "NANOGRAPH_EMBED_CHUNK_CHARS",
            embedding.chunk_size,
        );
        set_if_missing_usize(
            &mut exists,
            &mut set,
            "NANOGRAPH_EMBED_CHUNK_OVERLAP_CHARS",
            embedding.chunk_overlap_chars,
        );

        let api_key_target = match provider {
            "gemini" => "GEMINI_API_KEY",
            _ => "OPENAI_API_KEY",
        };

        if !exists(api_key_target)
            && let Some(api_key_env) = trim_nonempty(embedding.api_key_env.as_deref())
            && let Some(value) = get(api_key_env)
            && let Some(value) = trim_nonempty(Some(value.as_str()))
        {
            set(api_key_target, value);
        }

        Ok(())
    }
}

impl NanographConfig {
    fn merge(&mut self, other: Self) {
        self.project.merge(other.project);
        self.db.merge(other.db);
        self.schema.merge(other.schema);
        self.query.merge(other.query);
        self.query_aliases.extend(other.query_aliases);
        self.embedding.merge(other.embedding);
        self.cli.merge(other.cli);
    }
}

impl ProjectConfig {
    fn merge(&mut self, other: Self) {
        if other.name.is_some() {
            self.name = other.name;
        }
        if other.description.is_some() {
            self.description = other.description;
        }
        if other.instruction.is_some() {
            self.instruction = other.instruction;
        }
    }
}

impl DbConfig {
    fn merge(&mut self, other: Self) {
        if other.default_path.is_some() {
            self.default_path = other.default_path;
        }
    }
}

impl SchemaConfig {
    fn merge(&mut self, other: Self) {
        if other.default_path.is_some() {
            self.default_path = other.default_path;
        }
    }
}

impl QueryConfig {
    fn merge(&mut self, other: Self) {
        if !other.roots.is_empty() {
            self.roots = other.roots;
        }
    }
}

impl EmbeddingConfig {
    fn merge(&mut self, other: Self) {
        if other.api_key_env.is_some() {
            self.api_key_env = other.api_key_env;
        }
        if other.model.is_some() {
            self.model = other.model;
        }
        if other.base_url.is_some() {
            self.base_url = other.base_url;
        }
        if other.mock.is_some() {
            self.mock = other.mock;
        }
        if other.batch_size.is_some() {
            self.batch_size = other.batch_size;
        }
        if other.chunk_size.is_some() {
            self.chunk_size = other.chunk_size;
        }
        if other.chunk_overlap_chars.is_some() {
            self.chunk_overlap_chars = other.chunk_overlap_chars;
        }
        if other.provider.is_some() {
            self.provider = other.provider;
        }
    }
}

impl CliDefaults {
    fn merge(&mut self, other: Self) {
        if other.output_format.is_some() {
            self.output_format = other.output_format;
        }
        if other.json.is_some() {
            self.json = other.json;
        }
        if other.table_max_column_width.is_some() {
            self.table_max_column_width = other.table_max_column_width;
        }
        if other.table_cell_layout.is_some() {
            self.table_cell_layout = other.table_cell_layout;
        }
    }
}

fn parse_config_file(path: &Path) -> Result<NanographConfig> {
    let raw = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("failed to read config: {}", path.display()))?;
    toml::from_str(&raw).wrap_err_with(|| format!("failed to parse config: {}", path.display()))
}

fn trim_nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn set_if_missing<FExists, FSet>(
    exists: &mut FExists,
    set: &mut FSet,
    key: &str,
    value: Option<&str>,
) where
    FExists: FnMut(&str) -> bool,
    FSet: FnMut(&str, &str),
{
    if exists(key) {
        return;
    }
    if let Some(value) = trim_nonempty(value) {
        set(key, value);
    }
}

fn set_if_missing_usize<FExists, FSet>(
    exists: &mut FExists,
    set: &mut FSet,
    key: &str,
    value: Option<usize>,
) where
    FExists: FnMut(&str) -> bool,
    FSet: FnMut(&str, &str),
{
    if exists(key) {
        return;
    }
    if let Some(value) = value {
        let rendered = value.to_string();
        set(key, &rendered);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn resolve_format_uses_config_default() {
        let cfg = LoadedConfig {
            path: None,
            base_dir: PathBuf::from("/tmp"),
            settings: NanographConfig {
                cli: CliDefaults {
                    output_format: Some("json".to_string()),
                    json: Some(true),
                    table_max_column_width: None,
                    table_cell_layout: None,
                },
                ..NanographConfig::default()
            },
        };

        assert_eq!(
            cfg.resolve_format(None, "table", &["table", "json"])
                .unwrap(),
            "json"
        );
        assert!(cfg.effective_json(false));
    }

    #[test]
    fn resolve_command_format_falls_back_when_config_default_is_unsupported() {
        let cfg = LoadedConfig {
            path: None,
            base_dir: PathBuf::from("/tmp"),
            settings: NanographConfig {
                cli: CliDefaults {
                    output_format: Some("table".to_string()),
                    ..CliDefaults::default()
                },
                ..NanographConfig::default()
            },
        };

        assert_eq!(
            cfg.resolve_command_format(None, "jsonl", &["jsonl", "json"])
                .unwrap(),
            "jsonl"
        );
        assert!(
            cfg.resolve_command_format(Some("table"), "jsonl", &["jsonl", "json"])
                .is_err()
        );
    }

    #[test]
    fn resolve_db_path_uses_default_path() {
        let cfg = LoadedConfig {
            path: Some(PathBuf::from("/tmp/project/nanograph.toml")),
            base_dir: PathBuf::from("/tmp/project"),
            settings: NanographConfig {
                db: DbConfig {
                    default_path: Some(PathBuf::from("app.nano")),
                },
                ..NanographConfig::default()
            },
        };

        assert_eq!(
            cfg.resolve_db_path(None).unwrap(),
            PathBuf::from("/tmp/project/app.nano")
        );
    }

    #[test]
    fn resolve_schema_path_uses_default_path() {
        let cfg = LoadedConfig {
            path: Some(PathBuf::from("/tmp/project/nanograph.toml")),
            base_dir: PathBuf::from("/tmp/project"),
            settings: NanographConfig {
                schema: SchemaConfig {
                    default_path: Some(PathBuf::from("schema.pg")),
                },
                ..NanographConfig::default()
            },
        };

        assert_eq!(
            cfg.resolve_schema_path(None).unwrap(),
            PathBuf::from("/tmp/project/schema.pg")
        );
    }

    #[test]
    fn load_explicit_config_path_and_resolve_query_roots() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("nanograph.toml");
        let query_root = dir.path().join("queries");
        let query_path = query_root.join("people.gq");
        write_file(
            &config_path,
            r#"
[schema]
default_path = "schema.pg"

[db]
default_path = "app.nano"

[query]
roots = ["queries"]

[query_aliases.search]
query = "queries/people.gq"
name = "all_people"
format = "json"
args = ["q"]

[cli]
output_format = "json"
json = true
table_max_column_width = 72
table_cell_layout = "wrap"
"#,
        );
        write_file(&query_path, "query all_people { Person { name } }");

        let cfg = LoadedConfig::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.path.as_deref(), Some(config_path.as_path()));
        assert_eq!(
            cfg.resolve_db_path(None).unwrap(),
            dir.path().join("app.nano")
        );
        assert_eq!(
            cfg.resolve_query_path(Path::new("people.gq")).unwrap(),
            query_path
        );
        let run = cfg
            .resolve_run_config(Some("search"), None, None, None)
            .unwrap();
        assert_eq!(run.query_path, query_path);
        assert_eq!(run.query_name, "all_people");
        assert_eq!(run.format, "json");
        assert_eq!(run.positional_param_names, vec!["q".to_string()]);
        assert_eq!(cfg.effective_table_max_column_width(), 72);
        assert_eq!(cfg.effective_table_cell_layout(), TableCellLayout::Wrap);
        assert_eq!(
            cfg.resolve_format(None, "table", &["table", "json"])
                .unwrap(),
            "json"
        );
        assert!(cfg.effective_json(false));
    }

    #[test]
    fn run_alias_can_be_overridden_by_explicit_flags() {
        let dir = TempDir::new().unwrap();
        let query_root = dir.path().join("queries");
        let aliased_query = query_root.join("people.gq");
        let direct_query = dir.path().join("other.gq");
        write_file(
            &aliased_query,
            "query all_people() { match { $p: Person } return { $p.name } }",
        );
        write_file(
            &direct_query,
            "query custom() { match { $p: Person } return { $p.name } }",
        );

        let cfg = LoadedConfig {
            path: Some(dir.path().join("nanograph.toml")),
            base_dir: dir.path().to_path_buf(),
            settings: NanographConfig {
                query: QueryConfig {
                    roots: vec![PathBuf::from("queries")],
                },
                query_aliases: HashMap::from([(
                    "search".to_string(),
                    QueryAliasConfig {
                        query: Some(PathBuf::from("queries/people.gq")),
                        name: Some("all_people".to_string()),
                        format: Some("json".to_string()),
                        args: vec!["q".to_string()],
                    },
                )]),
                ..NanographConfig::default()
            },
        };

        let run = cfg
            .resolve_run_config(
                Some("search"),
                Some(PathBuf::from("other.gq")),
                Some("custom"),
                Some("csv"),
            )
            .unwrap();
        assert_eq!(run.query_path, direct_query);
        assert_eq!(run.query_name, "custom");
        assert_eq!(run.format, "csv");
        assert_eq!(run.positional_param_names, vec!["q".to_string()]);
    }

    #[test]
    fn resolve_run_config_accepts_kv_format() {
        let dir = TempDir::new().unwrap();
        let query_root = dir.path().join("queries");
        let aliased_query = query_root.join("people.gq");
        write_file(
            &aliased_query,
            "query all_people() { match { $p: Person } return { $p.name } }",
        );

        let cfg = LoadedConfig {
            path: Some(dir.path().join("nanograph.toml")),
            base_dir: dir.path().to_path_buf(),
            settings: NanographConfig {
                query_aliases: HashMap::from([(
                    "detail".to_string(),
                    QueryAliasConfig {
                        query: Some(PathBuf::from("queries/people.gq")),
                        name: Some("all_people".to_string()),
                        format: Some("kv".to_string()),
                        args: Vec::new(),
                    },
                )]),
                ..NanographConfig::default()
            },
        };

        let run = cfg
            .resolve_run_config(Some("detail"), None, None, None)
            .unwrap();
        assert_eq!(run.query_path, aliased_query);
        assert_eq!(run.format, "kv");
    }

    #[test]
    fn effective_table_max_column_width_uses_config_or_default() {
        let configured = LoadedConfig {
            path: None,
            base_dir: PathBuf::from("/tmp"),
            settings: NanographConfig {
                cli: CliDefaults {
                    table_max_column_width: Some(42),
                    table_cell_layout: Some(TableCellLayout::Wrap),
                    ..CliDefaults::default()
                },
                ..NanographConfig::default()
            },
        };
        assert_eq!(configured.effective_table_max_column_width(), 42);
        assert_eq!(
            configured.effective_table_cell_layout(),
            TableCellLayout::Wrap
        );

        let defaulted = LoadedConfig {
            path: None,
            base_dir: PathBuf::from("/tmp"),
            settings: NanographConfig::default(),
        };
        assert_eq!(defaulted.effective_table_max_column_width(), 80);
        assert_eq!(
            defaulted.effective_table_cell_layout(),
            TableCellLayout::Truncate
        );
    }

    #[test]
    fn resolve_run_config_requires_alias_entry_to_exist() {
        let cfg = LoadedConfig {
            path: None,
            base_dir: PathBuf::from("/tmp"),
            settings: NanographConfig::default(),
        };

        let err = cfg
            .resolve_run_config(Some("missing"), None, None, None)
            .unwrap_err();
        assert!(err.to_string().contains("query alias `missing` not found"));
        assert!(err.to_string().contains("use --query"));
    }

    #[test]
    fn shared_config_loads_embedding_defaults() {
        let dir = TempDir::new().unwrap();
        let shared_path = dir.path().join("nanograph.toml");
        write_file(
            &shared_path,
            r#"
[embedding]
provider = "openai"
model = "text-embedding-3-small"
api_key_env = "OPENAI_API_KEY"
chunk_size = 512
"#,
        );

        let cfg = LoadedConfig::load(Some(&shared_path)).unwrap();
        assert_eq!(
            cfg.settings.embedding.model.as_deref(),
            Some("text-embedding-3-small")
        );
        assert_eq!(cfg.settings.embedding.chunk_size, Some(512));
        assert_eq!(
            cfg.settings.embedding.api_key_env.as_deref(),
            Some("OPENAI_API_KEY")
        );
    }

    #[test]
    fn embedding_env_resolution_respects_existing_env_then_config_defaults() {
        let cfg = LoadedConfig {
            path: Some(PathBuf::from("/tmp/project/nanograph.toml")),
            base_dir: PathBuf::from("/tmp/project"),
            settings: NanographConfig {
                embedding: EmbeddingConfig {
                    provider: Some("openai".to_string()),
                    model: Some("text-embedding-3-large".to_string()),
                    api_key_env: Some("ALT_OPENAI_KEY".to_string()),
                    batch_size: Some(32),
                    chunk_size: Some(1024),
                    chunk_overlap_chars: Some(64),
                    base_url: Some("https://example.invalid/v1".to_string()),
                    mock: None,
                },
                ..NanographConfig::default()
            },
        };
        let env = std::cell::RefCell::new(std::collections::HashMap::from([
            ("OPENAI_API_KEY".to_string(), "existing-secret".to_string()),
            ("ALT_OPENAI_KEY".to_string(), "alt-secret".to_string()),
        ]));

        cfg.apply_embedding_env_with(
            |key| env.borrow().contains_key(key),
            |key| env.borrow().get(key).cloned(),
            |key, value| {
                env.borrow_mut().insert(key.to_string(), value.to_string());
            },
        )
        .unwrap();

        assert_eq!(
            env.borrow().get("OPENAI_API_KEY").map(String::as_str),
            Some("existing-secret")
        );
        assert_eq!(
            env.borrow()
                .get("NANOGRAPH_EMBED_MODEL")
                .map(String::as_str),
            Some("text-embedding-3-large")
        );
        assert_eq!(
            env.borrow()
                .get("NANOGRAPH_EMBED_BATCH_SIZE")
                .map(String::as_str),
            Some("32")
        );
        assert_eq!(
            env.borrow()
                .get("NANOGRAPH_EMBED_CHUNK_CHARS")
                .map(String::as_str),
            Some("1024")
        );
        assert_eq!(
            env.borrow()
                .get("NANOGRAPH_EMBED_CHUNK_OVERLAP_CHARS")
                .map(String::as_str),
            Some("64")
        );
        assert_eq!(
            env.borrow().get("OPENAI_BASE_URL").map(String::as_str),
            Some("https://example.invalid/v1")
        );
    }

    #[test]
    fn embedding_env_resolution_uses_api_key_env_when_openai_key_missing() {
        let cfg = LoadedConfig {
            path: Some(PathBuf::from("/tmp/project/nanograph.toml")),
            base_dir: PathBuf::from("/tmp/project"),
            settings: NanographConfig {
                embedding: EmbeddingConfig {
                    provider: Some("openai".to_string()),
                    api_key_env: Some("ALT_OPENAI_KEY".to_string()),
                    mock: Some(false),
                    ..EmbeddingConfig::default()
                },
                ..NanographConfig::default()
            },
        };
        let env = std::cell::RefCell::new(std::collections::HashMap::from([(
            "ALT_OPENAI_KEY".to_string(),
            "env-secret".to_string(),
        )]));

        cfg.apply_embedding_env_with(
            |key| env.borrow().contains_key(key),
            |key| env.borrow().get(key).cloned(),
            |key, value| {
                env.borrow_mut().insert(key.to_string(), value.to_string());
            },
        )
        .unwrap();
        assert_eq!(
            env.borrow().get("OPENAI_API_KEY").map(String::as_str),
            Some("env-secret")
        );
    }

    #[test]
    fn embedding_env_resolution_supports_gemini_provider() {
        let cfg = LoadedConfig {
            path: Some(PathBuf::from("/tmp/project/nanograph.toml")),
            base_dir: PathBuf::from("/tmp/project"),
            settings: NanographConfig {
                embedding: EmbeddingConfig {
                    provider: Some("gemini".to_string()),
                    model: Some("gemini-embedding-2-preview".to_string()),
                    api_key_env: Some("ALT_GEMINI_KEY".to_string()),
                    base_url: Some("https://example.invalid/gemini".to_string()),
                    ..EmbeddingConfig::default()
                },
                ..NanographConfig::default()
            },
        };
        let env = std::cell::RefCell::new(std::collections::HashMap::from([(
            "ALT_GEMINI_KEY".to_string(),
            "gem-secret".to_string(),
        )]));

        cfg.apply_embedding_env_with(
            |key| env.borrow().contains_key(key),
            |key| env.borrow().get(key).cloned(),
            |key, value| {
                env.borrow_mut().insert(key.to_string(), value.to_string());
            },
        )
        .unwrap();

        assert_eq!(
            env.borrow()
                .get("NANOGRAPH_EMBED_PROVIDER")
                .map(String::as_str),
            Some("gemini")
        );
        assert_eq!(
            env.borrow().get("GEMINI_API_KEY").map(String::as_str),
            Some("gem-secret")
        );
        assert_eq!(
            env.borrow().get("GEMINI_BASE_URL").map(String::as_str),
            Some("https://example.invalid/gemini")
        );
    }

    #[test]
    fn load_without_config_is_noop() {
        let dir = TempDir::new().unwrap();
        let loaded = LoadedConfig::load_from_dir(None, dir.path()).unwrap();

        assert!(loaded.path.is_none());
        assert!(loaded.settings.db.default_path.is_none());
        assert_eq!(loaded.settings.cli.output_format, None);
        assert!(loaded.settings.query.roots.is_empty());
    }
}
