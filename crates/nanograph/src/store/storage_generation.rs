use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{NanoError, Result};

const STORAGE_METADATA_FILENAME: &str = "storage.generation.json";
const STORAGE_METADATA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageGeneration {
    V4Namespace,
    #[serde(alias = "v5-lineage-native")]
    NamespaceLineage,
}

impl StorageGeneration {
    pub(crate) fn is_namespace_managed(self) -> bool {
        matches!(self, Self::V4Namespace | Self::NamespaceLineage)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StorageMetadata {
    version: u32,
    generation: StorageGeneration,
}

pub(crate) fn storage_metadata_path(db_path: &Path) -> std::path::PathBuf {
    db_path.join(STORAGE_METADATA_FILENAME)
}

pub(crate) fn write_storage_generation(
    db_path: &Path,
    generation: StorageGeneration,
) -> Result<()> {
    let path = storage_metadata_path(db_path);
    let metadata = StorageMetadata {
        version: STORAGE_METADATA_VERSION,
        generation,
    };
    let json = serde_json::to_string_pretty(&metadata)
        .map_err(|err| NanoError::Storage(format!("serialize storage metadata error: {}", err)))?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn read_storage_generation(db_path: &Path) -> Result<Option<StorageGeneration>> {
    let path = storage_metadata_path(db_path);
    if !path.exists() {
        return Ok(None);
    }
    let json = std::fs::read_to_string(&path)?;
    let metadata: StorageMetadata = serde_json::from_str(&json).map_err(|err| {
        NanoError::Storage(format!(
            "parse storage metadata {} error: {}",
            path.display(),
            err
        ))
    })?;
    if metadata.version != STORAGE_METADATA_VERSION {
        return Err(NanoError::Storage(format!(
            "unsupported storage metadata version {} in {}",
            metadata.version,
            path.display()
        )));
    }
    Ok(Some(metadata.generation))
}

pub fn detect_storage_generation(db_path: &Path) -> Result<Option<StorageGeneration>> {
    read_storage_generation(db_path)
}
