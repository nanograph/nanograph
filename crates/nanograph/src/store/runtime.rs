use std::collections::HashMap;
use std::sync::Arc;

use ahash::AHashMap;
use arrow_array::{Array, Float32Array, RecordBatch, UInt64Array};
use futures::StreamExt;
use lance_index::scalar::FullTextSearchQuery;
use lance_index::scalar::inverted::SCORE_COL;
use lance_index::scalar::inverted::query::{
    BooleanQuery, FtsQuery, MatchQuery, Occur, Operator, PhraseQuery,
};
use tokio::sync::Mutex;

use crate::catalog::Catalog;
use crate::error::{NanoError, Result};

use super::csr::CsrIndex;
use super::lance_io::{
    LANCE_INTERNAL_ID_FIELD, logical_node_field_to_lance, open_dataset_for_locator,
    read_lance_batches_for_locator, read_lance_projected_batches_for_locator,
};
use super::metadata::{DatabaseMetadata, DatasetLocator};

#[derive(Debug, Clone)]
pub(crate) struct NodeLookup {
    pub(crate) batch: RecordBatch,
    pub(crate) id_to_row: AHashMap<u64, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum TextSearchKind {
    Search,
    Fuzzy { max_edits: Option<u32> },
    MatchText,
    Bm25,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TextSearchKey {
    type_name: String,
    property: String,
    dataset_version: u64,
    query: String,
    kind: TextSearchKind,
}

#[derive(Debug, Default)]
pub(crate) struct TextSearchCache {
    inner: Mutex<AHashMap<TextSearchKey, Arc<AHashMap<u64, f64>>>>,
}

impl TextSearchCache {
    async fn get_or_load(
        &self,
        type_name: &str,
        locator: &DatasetLocator,
        property: &str,
        query: &str,
        kind: TextSearchKind,
    ) -> Result<Arc<AHashMap<u64, f64>>> {
        let key = TextSearchKey {
            type_name: type_name.to_string(),
            property: property.to_string(),
            dataset_version: locator.dataset_version,
            query: query.to_string(),
            kind: kind.clone(),
        };

        let mut guard = self.inner.lock().await;
        if let Some(scores) = guard.get(&key) {
            return Ok(scores.clone());
        }

        let scores: Arc<AHashMap<u64, f64>> = Arc::new(
            load_native_text_scores(locator, property, query, kind)
                .await?
                .into_iter()
                .collect(),
        );
        guard.insert(key, scores.clone());
        Ok(scores)
    }
}

/// CL-510: in-process cache of query-text embeddings, scoped to the
/// `DatabaseRuntime` lifetime. Agents that issue the same `nearest($x, $q)`
/// query repeatedly (e.g. `similar_issues("memory leak")` called 5×) hit
/// the cache instead of round-tripping to LM Studio / OpenAI / Gemini.
///
/// Key: `(model_name, text, dim)` — model is included so switching providers
/// (OPENAI_API_KEY vs LMSTUDIO_BASE_URL) yields fresh entries without manual
/// invalidation. Dim is included so a schema change that bumps `Vector(N)`
/// re-embeds rather than returning a stale vector.
///
/// Unbounded for v1: a typical query-text embedding is ~4 KB at 1024 dims,
/// so 1000 unique queries = ~4 MB. Add LRU eviction if real workloads grow
/// beyond that.
#[derive(Debug, Default)]
pub(crate) struct QueryEmbeddingCache {
    entries: std::sync::Mutex<std::collections::HashMap<(String, String, usize), Vec<f32>>>,
}

impl QueryEmbeddingCache {
    pub(crate) fn get(&self, model: &str, text: &str, dim: usize) -> Option<Vec<f32>> {
        let guard = self.entries.lock().ok()?;
        guard
            .get(&(model.to_string(), text.to_string(), dim))
            .cloned()
    }

    pub(crate) fn insert(&self, model: &str, text: String, dim: usize, vector: Vec<f32>) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.insert((model.to_string(), text, dim), vector);
        }
    }

    /// Test-only — counts entries. Always-available so cross-crate tests
    /// (engine_integration) can call it without cfg(test) gymnastics.
    #[doc(hidden)]
    pub fn __test_len(&self) -> usize {
        self.entries.lock().map(|g| g.len()).unwrap_or(0)
    }
}

async fn load_native_text_scores(
    locator: &DatasetLocator,
    property: &str,
    query: &str,
    kind: TextSearchKind,
) -> Result<Vec<(u64, f64)>> {
    let dataset = open_dataset_for_locator(locator)
        .await
        .map_err(|e| NanoError::Lance(format!("fts open error: {}", e)))?;

    let fts_query = build_native_text_query(property, query, &kind).ok_or_else(|| {
        NanoError::Execution(format!(
            "could not build native text query for property `{}`",
            property
        ))
    })?;

    let mut scanner = dataset.scan();
    scanner
        .project(&[LANCE_INTERNAL_ID_FIELD.to_string()])
        .map_err(|e| NanoError::Lance(format!("fts project error: {}", e)))?;
    scanner
        .full_text_search(fts_query)
        .map_err(|e| NanoError::Lance(format!("fts search error: {}", e)))?;

    let mut stream = scanner
        .try_into_stream()
        .await
        .map_err(|e| NanoError::Lance(format!("fts stream error: {}", e)))?;
    let mut scores = Vec::new();
    while let Some(batch) = stream.next().await {
        let batch = batch.map_err(|e| NanoError::Lance(format!("fts batch error: {}", e)))?;
        if batch.num_rows() == 0 {
            continue;
        }
        let ids = batch
            .column_by_name(LANCE_INTERNAL_ID_FIELD)
            .ok_or_else(|| NanoError::Storage("fts batch missing internal id column".to_string()))?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| NanoError::Storage("fts id column is not UInt64".to_string()))?;
        let score_arr = batch
            .column_by_name(SCORE_COL)
            .ok_or_else(|| NanoError::Storage("fts batch missing _score column".to_string()))?
            .as_any()
            .downcast_ref::<Float32Array>()
            .ok_or_else(|| NanoError::Storage("fts _score column is not Float32".to_string()))?;
        for row in 0..batch.num_rows() {
            if ids.is_null(row) {
                continue;
            }
            let score = if score_arr.is_null(row) {
                0.0
            } else {
                score_arr.value(row) as f64
            };
            scores.push((ids.value(row), score));
        }
    }

    Ok(scores)
}

pub(crate) fn tokenize_native_text_terms(input: &str) -> Vec<String> {
    input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

pub(crate) fn build_native_text_query(
    property: &str,
    query: &str,
    kind: &TextSearchKind,
) -> Option<FullTextSearchQuery> {
    let column = logical_node_field_to_lance(property).to_string();
    let query = match kind {
        TextSearchKind::Search => {
            let terms = tokenize_native_text_terms(query);
            if terms.is_empty() {
                return None;
            }
            let clauses = terms.into_iter().map(|term| {
                (
                    Occur::Must,
                    FtsQuery::Match(MatchQuery::new(term).with_column(Some(column.clone()))),
                )
            });
            FullTextSearchQuery::new_query(FtsQuery::Boolean(BooleanQuery::new(clauses)))
        }
        TextSearchKind::Fuzzy { max_edits } => {
            let terms = tokenize_native_text_terms(query);
            if terms.is_empty() {
                return None;
            }
            let clauses = terms.into_iter().map(|term| {
                (
                    Occur::Must,
                    FtsQuery::Match(
                        MatchQuery::new(term)
                            .with_column(Some(column.clone()))
                            .with_operator(Operator::And)
                            .with_fuzziness(*max_edits),
                    ),
                )
            });
            FullTextSearchQuery::new_query(FtsQuery::Boolean(BooleanQuery::new(clauses)))
        }
        TextSearchKind::MatchText => FullTextSearchQuery::new_query(FtsQuery::Phrase(
            PhraseQuery::new(query.to_string()).with_column(Some(column)),
        )),
        TextSearchKind::Bm25 => FullTextSearchQuery::new_query(FtsQuery::Match(
            MatchQuery::new(query.to_string()).with_column(Some(column)),
        )),
    };
    Some(query)
}

#[derive(Debug, Default)]
pub(crate) struct NodeBatchCache {
    inner: Mutex<AHashMap<(String, u64), Option<RecordBatch>>>,
}

impl NodeBatchCache {
    pub(crate) async fn get_or_load(
        &self,
        type_name: &str,
        locator: &DatasetLocator,
    ) -> Result<Option<RecordBatch>> {
        let key = (type_name.to_string(), locator.dataset_version);
        let mut guard = self.inner.lock().await;
        if let Some(batch) = guard.get(&key) {
            return Ok(batch.clone());
        }

        let batches = read_lance_batches_for_locator(locator)
            .await
            .map_err(|err| {
                NanoError::Storage(format!(
                    "load node batches for {} at {} failed: {}",
                    type_name, locator.table_id, err
                ))
            })?;
        let batch = if batches.is_empty() {
            None
        } else if batches.len() == 1 {
            Some(batches[0].clone())
        } else {
            let schema = batches[0].schema();
            Some(
                arrow_select::concat::concat_batches(&schema, &batches)
                    .map_err(|e| NanoError::Storage(format!("concat error: {}", e)))?,
            )
        };
        guard.insert(key, batch.clone());
        Ok(batch)
    }
}

#[derive(Debug, Default)]
pub(crate) struct NodeLookupCache {
    inner: Mutex<AHashMap<(String, u64), Option<Arc<NodeLookup>>>>,
}

impl NodeLookupCache {
    pub(crate) async fn get_or_build(
        &self,
        type_name: &str,
        locator: &DatasetLocator,
        batch_cache: &NodeBatchCache,
    ) -> Result<Option<Arc<NodeLookup>>> {
        let key = (type_name.to_string(), locator.dataset_version);
        let mut guard = self.inner.lock().await;
        if let Some(entry) = guard.get(&key) {
            return Ok(entry.clone());
        }

        let Some(batch) = batch_cache.get_or_load(type_name, locator).await? else {
            guard.insert(key, None);
            return Ok(None);
        };
        let id_array = batch
            .column_by_name("id")
            .ok_or_else(|| {
                NanoError::Storage(format!("node dataset {} missing id column", type_name))
            })?
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| {
                NanoError::Storage(format!(
                    "node dataset {} id column is not UInt64",
                    type_name
                ))
            })?;
        let mut id_to_row = AHashMap::with_capacity(batch.num_rows());
        for row in 0..batch.num_rows() {
            id_to_row.insert(id_array.value(row), row);
        }
        let lookup = Arc::new(NodeLookup { batch, id_to_row });
        guard.insert(key, Some(lookup.clone()));
        Ok(Some(lookup))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EdgeIndexPair {
    pub(crate) csr: Arc<CsrIndex>,
    pub(crate) csc: Arc<CsrIndex>,
}

#[derive(Debug, Default)]
pub(crate) struct EdgeIndexCache {
    inner: Mutex<AHashMap<(String, u64), Arc<EdgeIndexPair>>>,
}

impl EdgeIndexCache {
    pub(crate) async fn get_or_build(
        &self,
        edge_type: &str,
        locator: &DatasetLocator,
        max_node_id: u64,
    ) -> Result<Arc<EdgeIndexPair>> {
        let key = (edge_type.to_string(), locator.dataset_version);
        let mut guard = self.inner.lock().await;
        if let Some(pair) = guard.get(&key) {
            return Ok(pair.clone());
        }

        let batches = read_lance_projected_batches_for_locator(locator, &["id", "src", "dst"])
            .await
            .map_err(|err| {
                NanoError::Storage(format!(
                    "load edge index batches for {} at {} failed: {}",
                    edge_type, locator.table_id, err
                ))
            })?;
        let mut out_edges = Vec::new();
        let mut in_edges = Vec::new();
        for batch in batches {
            let id_arr = batch
                .column_by_name("id")
                .ok_or_else(|| NanoError::Storage("edge batch missing id column".to_string()))?
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| NanoError::Storage("edge id column is not UInt64".to_string()))?;
            let src_arr = batch
                .column_by_name("src")
                .ok_or_else(|| NanoError::Storage("edge batch missing src column".to_string()))?
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| NanoError::Storage("edge src column is not UInt64".to_string()))?;
            let dst_arr = batch
                .column_by_name("dst")
                .ok_or_else(|| NanoError::Storage("edge batch missing dst column".to_string()))?
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| NanoError::Storage("edge dst column is not UInt64".to_string()))?;

            for row in 0..batch.num_rows() {
                let edge_id = id_arr.value(row);
                let src = src_arr.value(row);
                let dst = dst_arr.value(row);
                out_edges.push((src, dst, edge_id));
                in_edges.push((dst, src, edge_id));
            }
        }

        let pair = Arc::new(EdgeIndexPair {
            csr: Arc::new(CsrIndex::build(max_node_id as usize, &mut out_edges)),
            csc: Arc::new(CsrIndex::build(max_node_id as usize, &mut in_edges)),
        });
        guard.insert(key, pair.clone());
        Ok(pair)
    }
}

#[derive(Debug)]
pub(crate) struct DatabaseRuntime {
    catalog: Catalog,
    node_locators: HashMap<String, DatasetLocator>,
    edge_locators: HashMap<String, DatasetLocator>,
    next_node_id: u64,
    next_edge_id: u64,
    edge_index_cache: Arc<EdgeIndexCache>,
    node_batch_cache: Arc<NodeBatchCache>,
    node_lookup_cache: Arc<NodeLookupCache>,
    text_search_cache: Arc<TextSearchCache>,
    query_embedding_cache: Arc<QueryEmbeddingCache>,
}

impl DatabaseRuntime {
    pub(crate) fn empty(catalog: Catalog) -> Self {
        Self {
            catalog,
            node_locators: HashMap::new(),
            edge_locators: HashMap::new(),
            next_node_id: 0,
            next_edge_id: 0,
            edge_index_cache: Arc::new(EdgeIndexCache::default()),
            node_batch_cache: Arc::new(NodeBatchCache::default()),
            node_lookup_cache: Arc::new(NodeLookupCache::default()),
            text_search_cache: Arc::new(TextSearchCache::default()),
            query_embedding_cache: Arc::new(QueryEmbeddingCache::default()),
        }
    }

    pub(crate) fn from_metadata(metadata: &DatabaseMetadata) -> Self {
        let mut runtime = Self::empty(metadata.catalog().clone());
        runtime.next_node_id = metadata.manifest().next_node_id;
        runtime.next_edge_id = metadata.manifest().next_edge_id;
        for entry in &metadata.manifest().datasets {
            let locator = DatasetLocator {
                db_path: metadata.path().to_path_buf(),
                table_id: entry.effective_table_id().to_string(),
                dataset_path: metadata.path().join(&entry.dataset_path),
                dataset_version: entry.dataset_version,
                row_count: entry.row_count,
                namespace_managed: true,
            };
            match entry.kind.as_str() {
                "node" => {
                    runtime
                        .node_locators
                        .insert(entry.type_name.clone(), locator);
                }
                "edge" => {
                    runtime
                        .edge_locators
                        .insert(entry.type_name.clone(), locator);
                }
                _ => {}
            }
        }
        runtime
    }

    pub(crate) fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub(crate) fn next_node_id(&self) -> u64 {
        self.next_node_id
    }

    pub(crate) fn node_dataset_locator(&self, type_name: &str) -> Option<&DatasetLocator> {
        self.node_locators.get(type_name)
    }

    pub(crate) fn edge_dataset_locator(&self, type_name: &str) -> Option<&DatasetLocator> {
        self.edge_locators.get(type_name)
    }

    #[allow(dead_code)]
    pub(crate) fn node_dataset_version(&self, type_name: &str) -> Option<u64> {
        self.node_dataset_locator(type_name)
            .map(|locator| locator.dataset_version)
    }

    #[allow(dead_code)]
    pub(crate) fn node_dataset_path(&self, type_name: &str) -> Option<&std::path::Path> {
        self.node_dataset_locator(type_name)
            .map(|locator| locator.dataset_path.as_path())
    }

    #[allow(dead_code)]
    pub(crate) fn edge_dataset_version(&self, type_name: &str) -> Option<u64> {
        self.edge_dataset_locator(type_name)
            .map(|locator| locator.dataset_version)
    }

    #[allow(dead_code)]
    pub(crate) fn edge_dataset_path(&self, type_name: &str) -> Option<&std::path::Path> {
        self.edge_dataset_locator(type_name)
            .map(|locator| locator.dataset_path.as_path())
    }

    pub(crate) fn node_dataset_count(&self) -> usize {
        self.node_locators.len()
    }

    pub(crate) fn edge_dataset_count(&self) -> usize {
        self.edge_locators.len()
    }

    pub(crate) async fn load_node_lookup(
        &self,
        type_name: &str,
    ) -> Result<Option<Arc<NodeLookup>>> {
        let Some(locator) = self.node_dataset_locator(type_name) else {
            return Ok(None);
        };
        self.node_lookup_cache
            .get_or_build(type_name, locator, &self.node_batch_cache)
            .await
    }

    pub(crate) fn edge_index_cache(&self) -> Arc<EdgeIndexCache> {
        self.edge_index_cache.clone()
    }

    pub(crate) fn query_embedding_cache(&self) -> Arc<QueryEmbeddingCache> {
        self.query_embedding_cache.clone()
    }

    pub(crate) async fn native_text_scores(
        &self,
        type_name: &str,
        property: &str,
        query: &str,
        kind: TextSearchKind,
    ) -> Result<Arc<AHashMap<u64, f64>>> {
        let locator = self.node_dataset_locator(type_name).ok_or_else(|| {
            NanoError::Storage(format!("node dataset {} is not available", type_name))
        })?;
        self.text_search_cache
            .get_or_load(type_name, locator, property, query, kind)
            .await
    }
}
