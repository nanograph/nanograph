#![recursion_limit = "256"]

mod convert;

use std::path::PathBuf;
use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi_derive::napi;
use tokio::sync::RwLock;

use nanograph::RunInputError;
use nanograph::error::NanoError;
use nanograph::find_named_query;
use nanograph::query::parser::parse_query;
use nanograph::query::typecheck::{CheckedQuery, typecheck_query_decl};
use nanograph::store::database::Database;

use convert::{
    js_object_to_param_map, parse_changes_options, parse_cleanup_options, parse_compact_options,
    parse_embed_options, parse_load_mode,
};

fn to_napi_err(e: NanoError) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

fn to_napi_input_err(e: RunInputError) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

#[napi(js_name = "Database")]
pub struct JsDatabase {
    inner: Arc<RwLock<Option<Database>>>,
}

impl JsDatabase {
    fn db_path(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    async fn db(&self) -> Result<Database> {
        let guard = self.inner.read().await;
        guard
            .as_ref()
            .cloned()
            .ok_or_else(|| napi::Error::from_reason("database is closed"))
    }
}

fn prop_def_to_json(prop: &nanograph::schema_ir::PropDef) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "name": prop.name,
        "propId": prop.prop_id,
        "type": prop.scalar_type,
        "nullable": prop.nullable,
    });
    if prop.list {
        obj["list"] = serde_json::Value::Bool(true);
    }
    if prop.key {
        obj["key"] = serde_json::Value::Bool(true);
    }
    if prop.unique {
        obj["unique"] = serde_json::Value::Bool(true);
    }
    if prop.index {
        obj["index"] = serde_json::Value::Bool(true);
    }
    if !prop.enum_values.is_empty() {
        obj["enumValues"] = serde_json::json!(prop.enum_values);
    }
    if let Some(ref src) = prop.embed_source {
        obj["embedSource"] = serde_json::Value::String(src.clone());
    }
    if let Some(ref mime_prop) = prop.media_mime_prop {
        obj["mediaMimeProp"] = serde_json::Value::String(mime_prop.clone());
    }
    if let Some(ref description) = prop.description {
        obj["description"] = serde_json::Value::String(description.clone());
    }
    obj
}

#[napi]
impl JsDatabase {
    /// Create a new database from a schema string.
    ///
    /// ```js
    /// const db = await Database.init("my.nano", schemaSource);
    /// ```
    #[napi(factory)]
    pub async fn init(db_path: String, schema_source: String) -> Result<Self> {
        let path = Self::db_path(&db_path);
        let db = Database::init(&path, &schema_source)
            .await
            .map_err(to_napi_err)?;
        Ok(JsDatabase {
            inner: Arc::new(RwLock::new(Some(db))),
        })
    }

    /// Open an existing database.
    ///
    /// ```js
    /// const db = await Database.open("my.nano");
    /// ```
    #[napi(factory)]
    pub async fn open(db_path: String) -> Result<Self> {
        let path = Self::db_path(&db_path);
        let db = Database::open(&path).await.map_err(to_napi_err)?;
        Ok(JsDatabase {
            inner: Arc::new(RwLock::new(Some(db))),
        })
    }

    /// Create a tempdir-backed database with automatic cleanup on last drop.
    ///
    /// ```js
    /// const db = await Database.openInMemory(schemaSource);
    /// ```
    #[napi(factory, js_name = "openInMemory")]
    pub async fn open_in_memory(schema_source: String) -> Result<Self> {
        let db = Database::open_in_memory(&schema_source)
            .await
            .map_err(to_napi_err)?;
        Ok(JsDatabase {
            inner: Arc::new(RwLock::new(Some(db))),
        })
    }

    /// Load JSONL data into the database.
    ///
    /// ```js
    /// await db.load(jsonlString, "overwrite");
    /// ```
    #[napi]
    pub async fn load(&self, data_source: String, mode: String) -> Result<()> {
        let load_mode = parse_load_mode(&mode)?;
        let db = self.db().await?;
        db.load_with_mode(&data_source, load_mode)
            .await
            .map_err(to_napi_err)
    }

    /// Load JSONL data from a file path.
    ///
    /// ```js
    /// await db.loadFile("/tmp/data.jsonl", "overwrite");
    /// ```
    #[napi(js_name = "loadFile")]
    pub async fn load_file(&self, data_path: String, mode: String) -> Result<()> {
        let load_mode = parse_load_mode(&mode)?;
        let db = self.db().await?;
        let data_path = PathBuf::from(data_path);
        db.load_file_with_mode(&data_path, load_mode)
            .await
            .map_err(to_napi_err)
    }

    /// Execute a named query from query source text.
    ///
    /// For read queries, returns an array of row objects.
    /// For mutation queries, returns `{ affectedNodes, affectedEdges }`.
    ///
    /// ```js
    /// const rows = await db.run(querySource, "findPeople", { minAge: 21 });
    /// ```
    #[napi]
    pub async fn run(
        &self,
        query_source: String,
        query_name: String,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let query = find_named_query(&query_source, &query_name).map_err(to_napi_input_err)?;
        let param_map = js_object_to_param_map(params.as_ref(), &query.params)?;
        let db = self.db().await?;

        if query.mutation.is_some() {
            let result = db
                .run_query(&query, &param_map)
                .await
                .map_err(to_napi_err)?;
            return Ok(result.to_sdk_json());
        }

        let prepared = db.prepare_read_query(&query).map_err(to_napi_err)?;
        let result = prepared.execute(&param_map).await.map_err(to_napi_err)?;
        Ok(result.to_sdk_json())
    }

    /// Execute a named read query and return an Arrow IPC stream as a Node Buffer.
    #[napi(js_name = "runArrow")]
    pub async fn run_arrow(
        &self,
        query_source: String,
        query_name: String,
        params: Option<serde_json::Value>,
    ) -> Result<Buffer> {
        let query = find_named_query(&query_source, &query_name).map_err(to_napi_input_err)?;
        if query.mutation.is_some() {
            return Err(napi::Error::from_reason(
                "runArrow only supports read queries; use run() for mutations",
            ));
        }

        let param_map = js_object_to_param_map(params.as_ref(), &query.params)?;
        let db = self.db().await?;
        let prepared = db.prepare_read_query(&query).map_err(to_napi_err)?;
        let result = prepared.execute(&param_map).await.map_err(to_napi_err)?;
        let encoded = result.to_arrow_ipc().map_err(to_napi_err)?;
        Ok(Buffer::from(encoded))
    }

    /// Typecheck all queries in the source text against the database schema.
    ///
    /// Returns an array of `{ name, kind, status, error? }` objects.
    ///
    /// ```js
    /// const checks = await db.check(querySource);
    /// ```
    #[napi]
    pub async fn check(&self, query_source: String) -> Result<serde_json::Value> {
        let queries = parse_query(&query_source).map_err(to_napi_err)?;
        let db = self.db().await?;
        let catalog = db.catalog().clone();

        let mut checks = Vec::with_capacity(queries.queries.len());
        for q in &queries.queries {
            match typecheck_query_decl(&catalog, q) {
                Ok(CheckedQuery::Read(_)) => {
                    checks.push(serde_json::json!({
                        "name": q.name,
                        "kind": "read",
                        "status": "ok",
                    }));
                }
                Ok(CheckedQuery::Mutation(_)) => {
                    checks.push(serde_json::json!({
                        "name": q.name,
                        "kind": "mutation",
                        "status": "ok",
                    }));
                }
                Err(e) => {
                    checks.push(serde_json::json!({
                        "name": q.name,
                        "kind": if q.mutation.is_some() { "mutation" } else { "read" },
                        "status": "error",
                        "error": e.to_string(),
                    }));
                }
            }
        }

        Ok(serde_json::Value::Array(checks))
    }

    /// Return schema introspection as a JSON object.
    ///
    /// ```js
    /// const info = await db.describe();
    /// ```
    #[napi]
    pub async fn describe(&self) -> Result<serde_json::Value> {
        let db = self.db().await?;
        let ir = db.schema_ir.clone();

        let mut node_types = Vec::new();
        for nt in ir.node_types() {
            let props: Vec<serde_json::Value> =
                nt.properties.iter().map(prop_def_to_json).collect();
            node_types.push(serde_json::json!({
                "name": nt.name,
                "typeId": nt.type_id,
                "description": nt.description,
                "instruction": nt.instruction,
                "keyProperty": nt.key_property_name(),
                "uniqueProperties": nt.unique_properties().map(|prop| prop.name.clone()).collect::<Vec<_>>(),
                "outgoingEdges": ir.edge_types().filter(|edge| edge.src_type_name == nt.name).map(|edge| serde_json::json!({"name": edge.name, "toType": edge.dst_type_name})).collect::<Vec<_>>(),
                "incomingEdges": ir.edge_types().filter(|edge| edge.dst_type_name == nt.name).map(|edge| serde_json::json!({"name": edge.name, "fromType": edge.src_type_name})).collect::<Vec<_>>(),
                "properties": props,
            }));
        }

        let mut edge_types = Vec::new();
        for et in ir.edge_types() {
            let props: Vec<serde_json::Value> =
                et.properties.iter().map(prop_def_to_json).collect();
            edge_types.push(serde_json::json!({
                "name": et.name,
                "srcType": et.src_type_name,
                "dstType": et.dst_type_name,
                "typeId": et.type_id,
                "description": et.description,
                "instruction": et.instruction,
                "endpointKeys": {
                    "src": ir.node_key_property_name(&et.src_type_name),
                    "dst": ir.node_key_property_name(&et.dst_type_name),
                },
                "properties": props,
            }));
        }

        Ok(serde_json::json!({
            "nodeTypes": node_types,
            "edgeTypes": edge_types,
        }))
    }

    /// Generate or backfill embeddings for @embed(...) properties.
    ///
    /// ```js
    /// const result = await db.embed({ onlyNull: true });
    /// ```
    #[napi]
    pub async fn embed(&self, options: Option<serde_json::Value>) -> Result<serde_json::Value> {
        let opts = parse_embed_options(options.as_ref())?;
        let db = self.db().await?;
        let result = db.embed(opts).await.map_err(to_napi_err)?;
        Ok(serde_json::json!({
            "nodeTypesConsidered": result.node_types_considered,
            "propertiesSelected": result.properties_selected,
            "rowsSelected": result.rows_selected,
            "embeddingsGenerated": result.embeddings_generated,
            "reindexedTypes": result.reindexed_types,
            "dryRun": result.dry_run,
        }))
    }

    /// Compact Lance datasets to reduce fragmentation.
    ///
    /// ```js
    /// const result = await db.compact({ targetRowsPerFragment: 1048576 });
    /// ```
    #[napi]
    pub async fn compact(&self, options: Option<serde_json::Value>) -> Result<serde_json::Value> {
        let opts = parse_compact_options(options.as_ref())?;
        let db = self.db().await?;
        let result = db.compact(opts).await.map_err(to_napi_err)?;
        Ok(serde_json::json!({
            "datasetsConsidered": result.datasets_considered,
            "datasetsCompacted": result.datasets_compacted,
            "fragmentsRemoved": result.fragments_removed,
            "fragmentsAdded": result.fragments_added,
            "filesRemoved": result.files_removed,
            "filesAdded": result.files_added,
            "manifestCommitted": result.manifest_committed,
        }))
    }

    /// Clean up old dataset versions and prune transaction/CDC logs.
    ///
    /// ```js
    /// const result = await db.cleanup({ retainTxVersions: 128 });
    /// ```
    #[napi]
    pub async fn cleanup(&self, options: Option<serde_json::Value>) -> Result<serde_json::Value> {
        let opts = parse_cleanup_options(options.as_ref())?;
        let db = self.db().await?;
        let result = db.cleanup(opts).await.map_err(to_napi_err)?;
        Ok(serde_json::json!({
            "txRowsRemoved": result.tx_rows_removed,
            "txRowsKept": result.tx_rows_kept,
            "cdcRowsRemoved": result.cdc_rows_removed,
            "cdcRowsKept": result.cdc_rows_kept,
            "datasetsCleaned": result.datasets_cleaned,
            "datasetOldVersionsRemoved": result.dataset_old_versions_removed,
            "datasetBytesRemoved": result.dataset_bytes_removed,
        }))
    }

    /// Run health checks on the database.
    ///
    /// ```js
    /// const report = await db.doctor();
    /// ```
    #[napi]
    pub async fn doctor(&self) -> Result<serde_json::Value> {
        let db = self.db().await?;
        let report = db.doctor().await.map_err(to_napi_err)?;
        let lineage_shadow = report.lineage_shadow.map(|shadow| {
            serde_json::json!({
                "windowsConsidered": shadow.windows_considered,
                "windowsVerified": shadow.windows_verified,
                "windowsSkipped": shadow.windows_skipped,
                "windowsMismatched": shadow.windows_mismatched,
                "missingRowidWindows": shadow.missing_rowid_windows,
                "windows": shadow.windows.into_iter().map(|window| {
                    serde_json::json!({
                        "kind": window.kind,
                        "typeName": window.type_name,
                        "graphVersion": window.graph_version,
                        "previousTableVersion": window.previous_table_version,
                        "currentTableVersion": window.current_table_version,
                        "expectedInserts": window.expected_inserts,
                        "expectedUpdates": window.expected_updates,
                        "actualInserts": window.actual_inserts,
                        "actualUpdates": window.actual_updates,
                        "status": window.status,
                        "detail": window.detail,
                    })
                }).collect::<Vec<_>>(),
            })
        });
        Ok(serde_json::json!({
            "healthy": report.healthy,
            "issues": report.issues,
            "warnings": report.warnings,
            "manifestDbVersion": report.manifest_db_version,
            "datasetsChecked": report.datasets_checked,
            "txRows": report.tx_rows,
            "cdcRows": report.cdc_rows,
            "lineageShadow": lineage_shadow,
        }))
    }

    /// Read committed lineage-native change rows.
    ///
    /// ```js
    /// const rows = await db.changes({ since: 0 });
    /// ```
    #[napi]
    pub async fn changes(&self, options: Option<serde_json::Value>) -> Result<serde_json::Value> {
        let opts = parse_changes_options(options.as_ref())?;
        let db = self.db().await?;
        let rows = db
            .changes(
                opts.from_graph_version_exclusive,
                opts.to_graph_version_inclusive,
            )
            .await
            .map_err(to_napi_err)?;
        serde_json::to_value(rows)
            .map_err(|err| napi::Error::from_reason(format!("serialize change rows: {}", err)))
    }

    /// Return whether this handle uses an internal tempdir-backed database.
    #[napi(js_name = "isInMemory")]
    pub async fn is_in_memory(&self) -> Result<bool> {
        let db = self.db().await?;
        Ok(db.is_in_memory())
    }

    /// Close the database, releasing resources.
    ///
    /// ```js
    /// await db.close();
    /// ```
    #[napi]
    pub async fn close(&self) -> Result<()> {
        let mut guard = self.inner.write().await;
        *guard = None;
        Ok(())
    }
}
