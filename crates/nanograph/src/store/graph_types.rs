use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct GraphVersion(pub(crate) u64);

impl GraphVersion {
    pub(crate) fn value(&self) -> u64 {
        self.0
    }
}

impl From<u64> for GraphVersion {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct GraphTxId(pub(crate) String);

impl GraphTxId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for GraphTxId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for GraphTxId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct GraphTableId(pub(crate) String);

impl GraphTableId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for GraphTableId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for GraphTableId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GraphTableVersion {
    pub(crate) table_id: GraphTableId,
    pub(crate) version: u64,
}

impl GraphTableVersion {
    pub(crate) fn new(table_id: impl Into<GraphTableId>, version: u64) -> Self {
        Self {
            table_id: table_id.into(),
            version,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GraphTouchedTableWindow {
    pub(crate) table_id: GraphTableId,
    pub(crate) entity_kind: String,
    pub(crate) type_name: String,
    pub(crate) before_version: u64,
    pub(crate) after_version: u64,
}

impl GraphTouchedTableWindow {
    pub(crate) fn new(
        table_id: impl Into<GraphTableId>,
        entity_kind: impl Into<String>,
        type_name: impl Into<String>,
        before_version: u64,
        after_version: u64,
    ) -> Self {
        Self {
            table_id: table_id.into(),
            entity_kind: entity_kind.into(),
            type_name: type_name.into(),
            before_version,
            after_version,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GraphSnapshot {
    pub(crate) graph_version: GraphVersion,
    pub(crate) table_versions: Vec<GraphTableVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GraphCommitRecord {
    pub(crate) tx_id: GraphTxId,
    pub(crate) graph_version: GraphVersion,
    pub(crate) table_versions: Vec<GraphTableVersion>,
    pub(crate) committed_at: String,
    pub(crate) op_summary: String,
    #[serde(default)]
    pub(crate) schema_identity_version: u32,
    #[serde(default)]
    pub(crate) touched_tables: Vec<GraphTouchedTableWindow>,
    #[serde(default)]
    pub(crate) tx_props: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct GraphChangeRecord {
    pub(crate) tx_id: GraphTxId,
    pub(crate) graph_version: GraphVersion,
    pub(crate) seq_in_tx: u32,
    pub(crate) op: String,
    pub(crate) entity_kind: String,
    pub(crate) type_name: String,
    pub(crate) entity_key: String,
    pub(crate) payload: serde_json::Value,
    #[serde(default)]
    pub(crate) rowid_if_known: Option<u64>,
    pub(crate) committed_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct GraphDeleteRecord {
    pub(crate) tx_id: GraphTxId,
    pub(crate) graph_version: GraphVersion,
    pub(crate) committed_at: String,
    pub(crate) entity_kind: String,
    pub(crate) type_name: String,
    pub(crate) table_id: GraphTableId,
    pub(crate) rowid: u64,
    pub(crate) entity_id: u64,
    pub(crate) logical_key: String,
    pub(crate) row: serde_json::Value,
    pub(crate) previous_graph_version: Option<u64>,
}
