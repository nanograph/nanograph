//! In-process cache of typechecked + lowered queries.
//!
//! CL-508: the agent toolbelt's hot path is "agent calls the same named
//! query hundreds of times per workflow." Each call today re-does parse →
//! typecheck → lower. Parsing is the caller's responsibility (the CLI parses
//! once per process invocation); the engine's wasted work is the typecheck +
//! lower steps that run on every `Database::run_query` call.
//!
//! This cache keys compiled queries by a fingerprint of the `QueryDecl`
//! Debug format — stable within a binary run, cheap to compute. Cache hits
//! skip typecheck + lower entirely and go straight to execute.
//!
//! Invalidation: the cache is scoped to a `Database` instance. A schema
//! migration (`nanograph migrate`) opens a new `Database`, which gets a
//! fresh cache. Mutations don't invalidate (the catalog is stable across
//! data mutations).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use ahash::AHasher;
use arrow_schema::SchemaRef;

use crate::ir::{MutationIR, QueryIR};
use crate::query::ast::QueryDecl;

#[derive(Debug, Clone)]
pub(crate) enum CachedCompilation {
    Read {
        output_schema: SchemaRef,
        ir: QueryIR,
    },
    Mutation {
        ir: MutationIR,
    },
}

#[derive(Debug, Default)]
pub(crate) struct CompiledQueryCache {
    entries: Mutex<HashMap<u64, CachedCompilation>>,
}

impl CompiledQueryCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Compute the fingerprint key for a query. Hashes the Debug
    /// representation — stable within a binary run, collision-resistant for
    /// any realistic query corpus.
    pub(crate) fn fingerprint(query: &QueryDecl) -> u64 {
        let mut hasher = AHasher::default();
        format!("{:?}", query).hash(&mut hasher);
        hasher.finish()
    }

    pub(crate) fn get(&self, key: u64) -> Option<CachedCompilation> {
        let guard = self.entries.lock().ok()?;
        guard.get(&key).cloned()
    }

    pub(crate) fn insert(&self, key: u64, value: CachedCompilation) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.insert(key, value);
        }
    }

    /// Test helper — returns the number of cached compilations.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Test helper — clears the cache.
    #[cfg(test)]
    pub(crate) fn clear(&self) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.clear();
        }
    }
}
