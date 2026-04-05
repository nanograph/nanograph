use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::catalog::Catalog;
use crate::catalog::schema_ir::{SchemaIR, build_catalog_from_ir};
use crate::error::{NanoError, Result};
use crate::store::manifest::{DatasetEntry, GraphManifest, hash_string};
use crate::store::migration::reconcile_migration_sidecars;
use crate::store::snapshot::read_committed_graph_snapshot;
use crate::store::storage_generation::{StorageGeneration, detect_storage_generation};
use crate::store::txlog::reconcile_logs_to_manifest;

pub const SCHEMA_IR_FILENAME: &str = "schema.ir.json";

#[derive(Debug, Clone)]
pub struct DatasetLocator {
    pub db_path: PathBuf,
    pub table_id: String,
    pub dataset_path: PathBuf,
    pub dataset_version: u64,
    pub row_count: u64,
    pub namespace_managed: bool,
}

#[derive(Debug, Clone)]
pub struct DatabaseMetadata {
    path: PathBuf,
    schema_ir: SchemaIR,
    catalog: Catalog,
    manifest: GraphManifest,
    dataset_map: HashMap<(String, String), DatasetEntry>,
}

impl DatabaseMetadata {
    pub fn open(db_path: &Path) -> Result<Self> {
        match detect_storage_generation(db_path)? {
            Some(StorageGeneration::V4Namespace | StorageGeneration::NamespaceLineage) => {
                Self::open_namespace(db_path)
            }
            None => Err(NanoError::Storage(format!(
                "database {} uses legacy v3 storage; run `nanograph storage migrate --db {} --target lineage-native`",
                db_path.display(),
                db_path.display()
            ))),
        }
    }

    pub(crate) fn open_v3_legacy(db_path: &Path) -> Result<Self> {
        reconcile_migration_sidecars(db_path)?;
        if !db_path.exists() {
            return Err(NanoError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("database not found: {}", db_path.display()),
            )));
        }

        let ir_json = std::fs::read_to_string(db_path.join(SCHEMA_IR_FILENAME))?;
        let schema_ir: SchemaIR = serde_json::from_str(&ir_json)
            .map_err(|e| NanoError::Manifest(format!("parse IR error: {}", e)))?;
        let catalog = build_catalog_from_ir(&schema_ir)?;
        let manifest = read_committed_graph_snapshot(db_path)?;

        let computed_hash = hash_string(&ir_json);
        if computed_hash != manifest.schema_ir_hash {
            return Err(NanoError::Manifest(format!(
                "schema mismatch: schema.ir.json has been modified since last load \
                 (expected hash {}, got {}). Re-run 'nanograph load' to update.",
                &manifest.schema_ir_hash[..8.min(manifest.schema_ir_hash.len())],
                &computed_hash[..8.min(computed_hash.len())]
            )));
        }
        reconcile_logs_to_manifest(db_path, manifest.db_version)?;

        let dataset_map = manifest
            .datasets
            .iter()
            .cloned()
            .map(|entry| ((entry.kind.clone(), entry.type_name.clone()), entry))
            .collect();

        Ok(Self {
            path: db_path.to_path_buf(),
            schema_ir,
            catalog,
            manifest,
            dataset_map,
        })
    }

    fn open_namespace(db_path: &Path) -> Result<Self> {
        if !db_path.exists() {
            return Err(NanoError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("database not found: {}", db_path.display()),
            )));
        }

        let ir_json = std::fs::read_to_string(db_path.join(SCHEMA_IR_FILENAME))?;
        let schema_ir: SchemaIR = serde_json::from_str(&ir_json)
            .map_err(|e| NanoError::Manifest(format!("parse IR error: {}", e)))?;
        let catalog = build_catalog_from_ir(&schema_ir)?;
        let mut manifest = read_committed_graph_snapshot(db_path)?;

        let computed_hash = hash_string(&ir_json);
        if computed_hash != manifest.schema_ir_hash {
            return Err(NanoError::Manifest(format!(
                "schema mismatch: schema.ir.json has been modified since last load \
                 (expected hash {}, got {}). Re-run 'nanograph load' to update.",
                &manifest.schema_ir_hash[..8.min(manifest.schema_ir_hash.len())],
                &computed_hash[..8.min(computed_hash.len())]
            )));
        }

        for entry in &mut manifest.datasets {
            if entry.dataset_path.is_empty() {
                entry.dataset_path = entry.effective_table_id().to_string();
            }
        }

        let dataset_map = manifest
            .datasets
            .iter()
            .cloned()
            .map(|entry| ((entry.kind.clone(), entry.type_name.clone()), entry))
            .collect();

        Ok(Self {
            path: db_path.to_path_buf(),
            schema_ir,
            catalog,
            manifest,
            dataset_map,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_ir(&self) -> &SchemaIR {
        &self.schema_ir
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn manifest(&self) -> &GraphManifest {
        &self.manifest
    }

    pub fn dataset_entry(&self, kind: &str, type_name: &str) -> Option<&DatasetEntry> {
        self.dataset_map
            .get(&(kind.to_string(), type_name.to_string()))
    }

    pub fn dataset_locator(&self, kind: &str, type_name: &str) -> Option<DatasetLocator> {
        self.dataset_entry(kind, type_name)
            .map(|entry| DatasetLocator {
                db_path: self.path.clone(),
                table_id: entry.effective_table_id().to_string(),
                dataset_path: self.path.join(&entry.dataset_path),
                dataset_version: entry.dataset_version,
                row_count: entry.row_count,
                namespace_managed: matches!(
                    detect_storage_generation(&self.path),
                    Ok(Some(generation)) if generation.is_namespace_managed()
                ),
            })
    }

    pub fn node_dataset_locator(&self, type_name: &str) -> Option<DatasetLocator> {
        self.dataset_locator("node", type_name)
    }

    pub fn edge_dataset_locator(&self, type_name: &str) -> Option<DatasetLocator> {
        self.dataset_locator("edge", type_name)
    }
}
