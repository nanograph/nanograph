use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{NanoError, Result};
use crate::store::manifest::GraphManifest;

const TX_CATALOG_FILENAME: &str = "_tx_catalog.jsonl";
const CDC_LOG_FILENAME: &str = "_cdc_log.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TxCatalogEntry {
    pub tx_id: String,
    pub db_version: u64,
    pub dataset_versions: BTreeMap<String, u64>,
    pub committed_at: String,
    pub op_summary: String,
    pub cdc_start_offset: Option<u64>,
    pub cdc_end_offset: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CdcLogEntry {
    pub tx_id: String,
    pub db_version: u64,
    pub seq_in_tx: u32,
    pub op: String,
    pub entity_kind: String,
    pub type_name: String,
    pub entity_key: String,
    pub payload: serde_json::Value,
    pub committed_at: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LogPruneStats {
    pub(crate) tx_rows_removed: usize,
    pub(crate) tx_rows_kept: usize,
    pub(crate) cdc_rows_removed: usize,
    pub(crate) cdc_rows_kept: usize,
}

fn tx_catalog_path(db_dir: &Path) -> PathBuf {
    db_dir.join(TX_CATALOG_FILENAME)
}

fn cdc_log_path(db_dir: &Path) -> PathBuf {
    db_dir.join(CDC_LOG_FILENAME)
}

fn repair_tx_and_cdc_logs(db_dir: &Path) -> Result<()> {
    truncate_trailing_partial_jsonl(&tx_catalog_path(db_dir))?;
    truncate_trailing_partial_jsonl(&cdc_log_path(db_dir))?;
    Ok(())
}

pub(crate) fn reconcile_logs_to_manifest(db_dir: &Path, manifest_db_version: u64) -> Result<()> {
    repair_tx_and_cdc_logs(db_dir)?;

    let tx_path = tx_catalog_path(db_dir);
    let (tx_keep_len, visible_cdc_end_offset) =
        compute_tx_visible_prefix(&tx_path, manifest_db_version)?;
    truncate_file_to_len(&tx_path, tx_keep_len)?;

    let cdc_path = cdc_log_path(db_dir);
    if !cdc_path.exists() {
        return Ok(());
    }
    let cdc_len = std::fs::metadata(&cdc_path)?.len();

    let cdc_keep_len = match visible_cdc_end_offset {
        Some(end) => {
            if end > cdc_len {
                return Err(NanoError::Manifest(format!(
                    "tx catalog references CDC offset {} beyond {}",
                    end,
                    cdc_path.display()
                )));
            }
            Some(end)
        }
        None => {
            if tx_keep_len == 0 {
                Some(0)
            } else {
                None
            }
        }
    };

    if let Some(keep_len) = cdc_keep_len {
        truncate_file_to_len(&cdc_path, keep_len)?;
    }
    Ok(())
}

pub(crate) fn append_tx_catalog_entry(db_dir: &Path, entry: &TxCatalogEntry) -> Result<(u64, u64)> {
    append_jsonl_row(&tx_catalog_path(db_dir), entry)
}

fn append_cdc_log_entries(db_dir: &Path, entries: &[CdcLogEntry]) -> Result<Option<(u64, u64)>> {
    if entries.is_empty() {
        return Ok(None);
    }

    let path = cdc_log_path(db_dir);
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let start_offset = file.metadata()?.len();

    for entry in entries {
        let json = serde_json::to_vec(entry)
            .map_err(|e| NanoError::Manifest(format!("serialize CDC row: {}", e)))?;
        file.write_all(&json)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;

    let end_offset = file.metadata()?.len();
    Ok(Some((start_offset, end_offset)))
}

pub fn read_tx_catalog_entries(db_dir: &Path) -> Result<Vec<TxCatalogEntry>> {
    read_jsonl_rows(&tx_catalog_path(db_dir))
}

pub(crate) fn read_cdc_log_entries(db_dir: &Path) -> Result<Vec<CdcLogEntry>> {
    read_jsonl_rows(&cdc_log_path(db_dir))
}

/// Read CDC rows that are visible through the committed tx catalog/manifest window.
///
/// Visibility rules:
/// - only tx rows with `db_version <= manifest.db_version` are considered
/// - CDC rows are included only when their `tx_id` is in the visible tx range
/// - output is ordered by `(db_version, seq_in_tx, tx_id)`
pub fn read_visible_cdc_entries(
    db_dir: &Path,
    from_db_version_exclusive: u64,
    to_db_version_inclusive: Option<u64>,
) -> Result<Vec<CdcLogEntry>> {
    let manifest = GraphManifest::read(db_dir)?;
    reconcile_logs_to_manifest(db_dir, manifest.db_version)?;

    let upper = to_db_version_inclusive
        .unwrap_or(manifest.db_version)
        .min(manifest.db_version);
    if upper <= from_db_version_exclusive {
        return Ok(Vec::new());
    }

    let visible_tx_ids: HashSet<String> = read_tx_catalog_entries(db_dir)?
        .into_iter()
        .filter(|tx| {
            tx.db_version <= manifest.db_version
                && tx.db_version > from_db_version_exclusive
                && tx.db_version <= upper
        })
        .map(|tx| tx.tx_id)
        .collect();
    if visible_tx_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut rows: Vec<CdcLogEntry> = read_cdc_log_entries(db_dir)?
        .into_iter()
        .filter(|row| {
            row.db_version > from_db_version_exclusive
                && row.db_version <= upper
                && visible_tx_ids.contains(&row.tx_id)
        })
        .collect();

    rows.sort_by(|a, b| {
        a.db_version
            .cmp(&b.db_version)
            .then(a.seq_in_tx.cmp(&b.seq_in_tx))
            .then(a.tx_id.cmp(&b.tx_id))
    });

    Ok(rows)
}

pub(crate) fn commit_manifest_and_logs(
    db_dir: &Path,
    manifest: &GraphManifest,
    cdc_entries: &[CdcLogEntry],
    op_summary: &str,
) -> Result<()> {
    let cdc_offsets = append_cdc_log_entries(db_dir, cdc_entries)?;
    let (cdc_start_offset, cdc_end_offset) = match cdc_offsets {
        Some((start, end)) => (Some(start), Some(end)),
        None => (None, None),
    };

    let dataset_versions: BTreeMap<String, u64> = manifest
        .datasets
        .iter()
        .map(|entry| (entry.dataset_path.clone(), entry.dataset_version))
        .collect();
    let tx_entry = TxCatalogEntry {
        tx_id: manifest.last_tx_id.clone(),
        db_version: manifest.db_version,
        dataset_versions,
        committed_at: manifest.committed_at.clone(),
        op_summary: op_summary.to_string(),
        cdc_start_offset,
        cdc_end_offset,
    };
    append_tx_catalog_entry(db_dir, &tx_entry)?;

    manifest.write_atomic(db_dir)?;

    Ok(())
}

/// Prune tx/CDC history to the last N visible db versions.
///
/// Always keeps rows at or below `manifest.db_version` and rewrites both logs together.
pub(crate) fn prune_logs_for_replay_window(
    db_dir: &Path,
    retain_tx_versions: u64,
) -> Result<LogPruneStats> {
    if retain_tx_versions == 0 {
        return Err(NanoError::Manifest(
            "retain_tx_versions must be >= 1".to_string(),
        ));
    }

    let manifest = GraphManifest::read(db_dir)?;
    reconcile_logs_to_manifest(db_dir, manifest.db_version)?;

    let tx_rows = read_tx_catalog_entries(db_dir)?;
    let tx_rows_before = tx_rows.len();
    if tx_rows.is_empty() {
        let cdc_rows = read_cdc_log_entries(db_dir)?;
        let cdc_rows_before = cdc_rows.len();
        rewrite_jsonl_rows(&cdc_log_path(db_dir), &[] as &[CdcLogEntry])?;
        return Ok(LogPruneStats {
            tx_rows_removed: 0,
            tx_rows_kept: 0,
            cdc_rows_removed: cdc_rows_before,
            cdc_rows_kept: 0,
        });
    }

    let min_db_version = manifest
        .db_version
        .saturating_sub(retain_tx_versions.saturating_sub(1));
    let mut kept_tx = Vec::new();
    for tx in tx_rows {
        if tx.db_version >= min_db_version && tx.db_version <= manifest.db_version {
            kept_tx.push(tx);
        }
    }
    let kept_tx_ids: HashSet<String> = kept_tx.iter().map(|row| row.tx_id.clone()).collect();

    let cdc_rows = read_cdc_log_entries(db_dir)?;
    let cdc_rows_before = cdc_rows.len();
    let kept_cdc: Vec<CdcLogEntry> = cdc_rows
        .into_iter()
        .filter(|row| kept_tx_ids.contains(&row.tx_id))
        .collect();

    let mut next_offset = 0u64;
    let mut cdc_offsets_by_tx = HashMap::new();
    for row in &kept_cdc {
        let start_offset = next_offset;
        let json = serde_json::to_vec(row)
            .map_err(|e| NanoError::Manifest(format!("serialize CDC row: {}", e)))?;
        next_offset = next_offset.saturating_add(json.len() as u64 + 1);
        cdc_offsets_by_tx
            .entry(row.tx_id.clone())
            .and_modify(|(_, end_offset)| *end_offset = next_offset)
            .or_insert((start_offset, next_offset));
    }

    for tx in &mut kept_tx {
        match cdc_offsets_by_tx.get(&tx.tx_id) {
            Some((start_offset, end_offset)) => {
                tx.cdc_start_offset = Some(*start_offset);
                tx.cdc_end_offset = Some(*end_offset);
            }
            None => {
                tx.cdc_start_offset = None;
                tx.cdc_end_offset = None;
            }
        }
    }

    let tx_rows_kept = kept_tx.len();
    let cdc_rows_kept = kept_cdc.len();
    rewrite_jsonl_rows(&tx_catalog_path(db_dir), &kept_tx)?;
    rewrite_jsonl_rows(&cdc_log_path(db_dir), &kept_cdc)?;

    Ok(LogPruneStats {
        tx_rows_removed: tx_rows_before.saturating_sub(tx_rows_kept),
        tx_rows_kept,
        cdc_rows_removed: cdc_rows_before.saturating_sub(cdc_rows_kept),
        cdc_rows_kept,
    })
}

fn append_jsonl_row<T: Serialize>(path: &Path, row: &T) -> Result<(u64, u64)> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let start_offset = file.metadata()?.len();
    let json = serde_json::to_vec(row)
        .map_err(|e| NanoError::Manifest(format!("serialize JSONL row: {}", e)))?;
    file.write_all(&json)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    let end_offset = file.metadata()?.len();
    Ok((start_offset, end_offset))
}

fn rewrite_jsonl_rows<T: Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;

        for row in rows {
            let json = serde_json::to_vec(row)
                .map_err(|e| NanoError::Manifest(format!("serialize JSONL row: {}", e)))?;
            file.write_all(&json)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn read_jsonl_rows<T>(path: &Path) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();

    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: T = serde_json::from_str(&line).map_err(|e| {
            NanoError::Manifest(format!(
                "parse JSONL row {} in {}: {}",
                line_no + 1,
                path.display(),
                e
            ))
        })?;
        out.push(parsed);
    }

    Ok(out)
}

fn truncate_trailing_partial_jsonl(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let bytes = std::fs::read(path)?;
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        return Ok(());
    }

    let keep_len = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(keep_len as u64)?;
    file.sync_all()?;
    Ok(())
}

fn compute_tx_visible_prefix(path: &Path, manifest_db_version: u64) -> Result<(u64, Option<u64>)> {
    if !path.exists() {
        return Ok((0, None));
    }

    let bytes = std::fs::read(path)?;
    if bytes.is_empty() {
        return Ok((0, None));
    }

    let mut keep_len = 0u64;
    let mut max_cdc_end = None;
    let mut prev_db_version = None;
    let mut offset = 0u64;

    for (line_no, chunk) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
        let next_offset = offset + chunk.len() as u64;
        let line = if chunk.last() == Some(&b'\n') {
            &chunk[..chunk.len().saturating_sub(1)]
        } else {
            chunk
        };

        let line_no = line_no + 1;
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            keep_len = next_offset;
            offset = next_offset;
            continue;
        }

        let line_str = std::str::from_utf8(line).map_err(|e| {
            NanoError::Manifest(format!(
                "invalid UTF-8 in tx catalog line {} ({}): {}",
                line_no,
                path.display(),
                e
            ))
        })?;
        let entry: TxCatalogEntry = serde_json::from_str(line_str).map_err(|e| {
            NanoError::Manifest(format!(
                "parse tx catalog line {} ({}): {}",
                line_no,
                path.display(),
                e
            ))
        })?;

        if let Some(prev) = prev_db_version
            && entry.db_version <= prev
        {
            return Err(NanoError::Manifest(format!(
                "non-monotonic db_version in tx catalog at line {} (prev {}, got {})",
                line_no, prev, entry.db_version
            )));
        }
        prev_db_version = Some(entry.db_version);

        if entry.db_version > manifest_db_version {
            break;
        }

        keep_len = next_offset;
        if let Some(end) = entry.cdc_end_offset {
            max_cdc_end = Some(max_cdc_end.unwrap_or(0).max(end));
        }
        offset = next_offset;
    }

    Ok((keep_len, max_cdc_end))
}

fn truncate_file_to_len(path: &Path, keep_len: u64) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let current_len = std::fs::metadata(path)?.len();
    if keep_len >= current_len {
        return Ok(());
    }

    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(keep_len)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CommitFailpoint {
        BeforeCdcAppend,
        AfterCdcAppend,
        AfterTxAppend,
        AfterManifestWrite,
    }

    impl CommitFailpoint {
        fn label(self) -> &'static str {
            match self {
                CommitFailpoint::BeforeCdcAppend => "before_cdc_append",
                CommitFailpoint::AfterCdcAppend => "after_cdc_append",
                CommitFailpoint::AfterTxAppend => "after_tx_append",
                CommitFailpoint::AfterManifestWrite => "after_manifest_write",
            }
        }
    }

    fn maybe_inject_commit_failpoint(
        failpoint: Option<CommitFailpoint>,
        stage: CommitFailpoint,
    ) -> Result<()> {
        if failpoint == Some(stage) {
            return Err(NanoError::Manifest(format!(
                "injected commit crash at {}",
                stage.label()
            )));
        }
        Ok(())
    }

    fn commit_manifest_and_logs_with_failpoint(
        db_dir: &Path,
        manifest: &GraphManifest,
        cdc_entries: &[CdcLogEntry],
        op_summary: &str,
        failpoint: Option<CommitFailpoint>,
    ) -> Result<()> {
        maybe_inject_commit_failpoint(failpoint, CommitFailpoint::BeforeCdcAppend)?;

        let cdc_offsets = append_cdc_log_entries(db_dir, cdc_entries)?;
        maybe_inject_commit_failpoint(failpoint, CommitFailpoint::AfterCdcAppend)?;

        let (cdc_start_offset, cdc_end_offset) = match cdc_offsets {
            Some((start, end)) => (Some(start), Some(end)),
            None => (None, None),
        };

        let dataset_versions: BTreeMap<String, u64> = manifest
            .datasets
            .iter()
            .map(|entry| (entry.dataset_path.clone(), entry.dataset_version))
            .collect();
        let tx_entry = TxCatalogEntry {
            tx_id: manifest.last_tx_id.clone(),
            db_version: manifest.db_version,
            dataset_versions,
            committed_at: manifest.committed_at.clone(),
            op_summary: op_summary.to_string(),
            cdc_start_offset,
            cdc_end_offset,
        };
        append_tx_catalog_entry(db_dir, &tx_entry)?;
        maybe_inject_commit_failpoint(failpoint, CommitFailpoint::AfterTxAppend)?;

        manifest.write_atomic(db_dir)?;
        maybe_inject_commit_failpoint(failpoint, CommitFailpoint::AfterManifestWrite)?;

        Ok(())
    }

    fn sample_manifest(db_version: u64) -> GraphManifest {
        let mut manifest = GraphManifest::new("abc".to_string());
        manifest.db_version = db_version;
        manifest.last_tx_id = format!("manifest-{}", db_version);
        manifest.committed_at = format!("170000000{}", db_version);
        manifest
    }

    fn sample_cdc(tx_id: &str, db_version: u64, seq_in_tx: u32, key: &str) -> CdcLogEntry {
        CdcLogEntry {
            tx_id: tx_id.to_string(),
            db_version,
            seq_in_tx,
            op: "insert".to_string(),
            entity_kind: "node".to_string(),
            type_name: "Person".to_string(),
            entity_key: key.to_string(),
            payload: serde_json::json!({ "key": key }),
            committed_at: format!("170000000{}", db_version),
        }
    }

    fn sample_tx_entry() -> TxCatalogEntry {
        let mut dataset_versions = BTreeMap::new();
        dataset_versions.insert("nodes/99c1bf00".to_string(), 3);
        dataset_versions.insert("edges/f7012952".to_string(), 1);
        TxCatalogEntry {
            tx_id: "tx-1".to_string(),
            db_version: 1,
            dataset_versions,
            committed_at: "1700000000".to_string(),
            op_summary: "test".to_string(),
            cdc_start_offset: None,
            cdc_end_offset: None,
        }
    }

    #[test]
    fn tx_catalog_roundtrip() {
        let dir = TempDir::new().unwrap();
        let entry = sample_tx_entry();

        let offsets = append_tx_catalog_entry(dir.path(), &entry).unwrap();
        assert!(offsets.1 > offsets.0);

        let loaded = read_tx_catalog_entries(dir.path()).unwrap();
        assert_eq!(loaded, vec![entry]);
    }

    #[test]
    fn cdc_log_roundtrip_and_offsets() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            CdcLogEntry {
                tx_id: "tx-2".to_string(),
                db_version: 2,
                seq_in_tx: 0,
                op: "insert".to_string(),
                entity_kind: "node".to_string(),
                type_name: "Person".to_string(),
                entity_key: "name=Alice".to_string(),
                payload: serde_json::json!({"name":"Alice"}),
                committed_at: "1700000001".to_string(),
            },
            CdcLogEntry {
                tx_id: "tx-2".to_string(),
                db_version: 2,
                seq_in_tx: 1,
                op: "insert".to_string(),
                entity_kind: "edge".to_string(),
                type_name: "Knows".to_string(),
                entity_key: "src=1,dst=2".to_string(),
                payload: serde_json::json!({"src":1,"dst":2}),
                committed_at: "1700000001".to_string(),
            },
        ];

        let offsets = append_cdc_log_entries(dir.path(), &entries)
            .unwrap()
            .unwrap();
        assert!(offsets.1 > offsets.0);

        let loaded = read_cdc_log_entries(dir.path()).unwrap();
        assert_eq!(loaded, entries);
    }

    #[test]
    fn repair_truncates_partial_tail_record() {
        let dir = TempDir::new().unwrap();
        let entry = sample_tx_entry();
        append_tx_catalog_entry(dir.path(), &entry).unwrap();

        let path = tx_catalog_path(dir.path());
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"tx_id":"partial""#).unwrap();
        file.sync_all().unwrap();

        repair_tx_and_cdc_logs(dir.path()).unwrap();
        let loaded = read_tx_catalog_entries(dir.path()).unwrap();
        assert_eq!(loaded, vec![entry]);
    }

    #[test]
    fn reconcile_truncates_tx_rows_beyond_manifest_version() {
        let dir = TempDir::new().unwrap();

        let mut tx1 = sample_tx_entry();
        tx1.tx_id = "tx-1".to_string();
        tx1.db_version = 1;
        append_tx_catalog_entry(dir.path(), &tx1).unwrap();

        let mut tx2 = tx1.clone();
        tx2.tx_id = "tx-2".to_string();
        tx2.db_version = 2;
        append_tx_catalog_entry(dir.path(), &tx2).unwrap();

        reconcile_logs_to_manifest(dir.path(), 1).unwrap();
        let loaded = read_tx_catalog_entries(dir.path()).unwrap();
        assert_eq!(loaded, vec![tx1]);
    }

    #[test]
    fn reconcile_truncates_cdc_tail_using_visible_tx_offsets() {
        let dir = TempDir::new().unwrap();
        let cdc1 = vec![CdcLogEntry {
            tx_id: "tx-1".to_string(),
            db_version: 1,
            seq_in_tx: 0,
            op: "insert".to_string(),
            entity_kind: "node".to_string(),
            type_name: "Person".to_string(),
            entity_key: "name=Alice".to_string(),
            payload: serde_json::json!({"name":"Alice"}),
            committed_at: "1700000001".to_string(),
        }];
        let cdc2 = vec![CdcLogEntry {
            tx_id: "tx-2".to_string(),
            db_version: 2,
            seq_in_tx: 0,
            op: "insert".to_string(),
            entity_kind: "node".to_string(),
            type_name: "Person".to_string(),
            entity_key: "name=Bob".to_string(),
            payload: serde_json::json!({"name":"Bob"}),
            committed_at: "1700000002".to_string(),
        }];

        let cdc1_offsets = append_cdc_log_entries(dir.path(), &cdc1).unwrap().unwrap();
        let cdc2_offsets = append_cdc_log_entries(dir.path(), &cdc2).unwrap().unwrap();

        let mut tx1 = sample_tx_entry();
        tx1.tx_id = "tx-1".to_string();
        tx1.db_version = 1;
        tx1.cdc_start_offset = Some(cdc1_offsets.0);
        tx1.cdc_end_offset = Some(cdc1_offsets.1);
        append_tx_catalog_entry(dir.path(), &tx1).unwrap();

        let mut tx2 = tx1.clone();
        tx2.tx_id = "tx-2".to_string();
        tx2.db_version = 2;
        tx2.cdc_start_offset = Some(cdc2_offsets.0);
        tx2.cdc_end_offset = Some(cdc2_offsets.1);
        append_tx_catalog_entry(dir.path(), &tx2).unwrap();

        reconcile_logs_to_manifest(dir.path(), 1).unwrap();

        let tx_rows = read_tx_catalog_entries(dir.path()).unwrap();
        assert_eq!(tx_rows, vec![tx1]);
        let cdc_rows = read_cdc_log_entries(dir.path()).unwrap();
        assert_eq!(cdc_rows, cdc1);
    }

    #[test]
    fn commit_manifest_and_logs_writes_cdc_then_tx_then_manifest() {
        let dir = TempDir::new().unwrap();
        let mut manifest = GraphManifest::new("abc".to_string());
        manifest.db_version = 1;
        manifest.last_tx_id = "manifest-1".to_string();
        manifest.committed_at = "1700000001".to_string();
        manifest
            .datasets
            .push(crate::store::manifest::DatasetEntry {
                type_id: 1,
                type_name: "Person".to_string(),
                kind: "node".to_string(),
                dataset_path: "nodes/00000001".to_string(),
                dataset_version: 7,
                row_count: 1,
            });

        let cdc = vec![CdcLogEntry {
            tx_id: manifest.last_tx_id.clone(),
            db_version: manifest.db_version,
            seq_in_tx: 0,
            op: "insert".to_string(),
            entity_kind: "node".to_string(),
            type_name: "Person".to_string(),
            entity_key: "name=Alice".to_string(),
            payload: serde_json::json!({"name":"Alice"}),
            committed_at: manifest.committed_at.clone(),
        }];

        commit_manifest_and_logs(dir.path(), &manifest, &cdc, "test_commit").unwrap();

        let tx_rows = read_tx_catalog_entries(dir.path()).unwrap();
        assert_eq!(tx_rows.len(), 1);
        assert_eq!(tx_rows[0].tx_id, "manifest-1");
        assert_eq!(tx_rows[0].op_summary, "test_commit");
        assert!(tx_rows[0].cdc_start_offset.is_some());
        assert!(tx_rows[0].cdc_end_offset.is_some());

        let cdc_rows = read_cdc_log_entries(dir.path()).unwrap();
        assert_eq!(cdc_rows, cdc);

        let loaded_manifest = GraphManifest::read(dir.path()).unwrap();
        assert_eq!(loaded_manifest.db_version, 1);
    }

    #[test]
    fn commit_failpoint_matrix_preserves_visible_state() {
        let cases = [
            (
                CommitFailpoint::BeforeCdcAppend,
                1u64,
                vec![1u64],
                vec!["v1"],
            ),
            (
                CommitFailpoint::AfterCdcAppend,
                1u64,
                vec![1u64],
                vec!["v1"],
            ),
            (CommitFailpoint::AfterTxAppend, 1u64, vec![1u64], vec!["v1"]),
            (
                CommitFailpoint::AfterManifestWrite,
                2u64,
                vec![1u64, 2u64],
                vec!["v1", "v2"],
            ),
        ];

        for (failpoint, expected_manifest_db_version, expected_tx_versions, expected_keys) in cases
        {
            let dir = TempDir::new().unwrap();

            let manifest1 = sample_manifest(1);
            let cdc1 = vec![sample_cdc(&manifest1.last_tx_id, 1, 0, "v1")];
            commit_manifest_and_logs(dir.path(), &manifest1, &cdc1, "tx1").unwrap();

            let manifest2 = sample_manifest(2);
            let cdc2 = vec![sample_cdc(&manifest2.last_tx_id, 2, 0, "v2")];
            let err = commit_manifest_and_logs_with_failpoint(
                dir.path(),
                &manifest2,
                &cdc2,
                "tx2",
                Some(failpoint),
            )
            .unwrap_err();
            let err_msg = format!("{}", err);
            assert!(err_msg.contains(failpoint.label()));

            let loaded_manifest = GraphManifest::read(dir.path()).unwrap();
            assert_eq!(loaded_manifest.db_version, expected_manifest_db_version);

            reconcile_logs_to_manifest(dir.path(), loaded_manifest.db_version).unwrap();
            let tx_rows = read_tx_catalog_entries(dir.path()).unwrap();
            let tx_versions = tx_rows.iter().map(|row| row.db_version).collect::<Vec<_>>();
            assert_eq!(tx_versions, expected_tx_versions);

            let visible = read_visible_cdc_entries(dir.path(), 0, None).unwrap();
            let keys = visible
                .iter()
                .map(|row| row.entity_key.as_str())
                .collect::<Vec<_>>();
            assert_eq!(keys, expected_keys);
        }
    }

    #[test]
    fn read_visible_cdc_entries_obeys_range_and_ordering() {
        let dir = TempDir::new().unwrap();

        let manifest1 = sample_manifest(1);
        let cdc1 = vec![sample_cdc(&manifest1.last_tx_id, 1, 0, "v1")];
        commit_manifest_and_logs(dir.path(), &manifest1, &cdc1, "tx1").unwrap();

        let manifest2 = sample_manifest(2);
        let cdc2 = vec![
            sample_cdc(&manifest2.last_tx_id, 2, 1, "v2-seq1"),
            sample_cdc(&manifest2.last_tx_id, 2, 0, "v2-seq0"),
        ];
        commit_manifest_and_logs(dir.path(), &manifest2, &cdc2, "tx2").unwrap();

        let manifest3 = sample_manifest(3);
        let cdc3 = vec![sample_cdc(&manifest3.last_tx_id, 3, 0, "v3")];
        commit_manifest_and_logs(dir.path(), &manifest3, &cdc3, "tx3").unwrap();

        let rows_since_1 = read_visible_cdc_entries(dir.path(), 1, None).unwrap();
        assert_eq!(rows_since_1.len(), 3);
        assert_eq!(rows_since_1[0].db_version, 2);
        assert_eq!(rows_since_1[0].seq_in_tx, 0);
        assert_eq!(rows_since_1[1].db_version, 2);
        assert_eq!(rows_since_1[1].seq_in_tx, 1);
        assert_eq!(rows_since_1[2].db_version, 3);
        assert_eq!(rows_since_1[2].seq_in_tx, 0);

        let rows_range_2_only = read_visible_cdc_entries(dir.path(), 1, Some(2)).unwrap();
        assert_eq!(rows_range_2_only.len(), 2);
        assert!(rows_range_2_only.iter().all(|r| r.db_version == 2));
    }

    #[test]
    fn read_visible_cdc_entries_manifest_gates_uncommitted_tail() {
        let dir = TempDir::new().unwrap();

        let manifest1 = sample_manifest(1);
        let cdc1 = vec![sample_cdc(&manifest1.last_tx_id, 1, 0, "v1")];
        commit_manifest_and_logs(dir.path(), &manifest1, &cdc1, "tx1").unwrap();

        let future_cdc = vec![sample_cdc("manifest-2", 2, 0, "v2")];
        let offsets = append_cdc_log_entries(dir.path(), &future_cdc)
            .unwrap()
            .unwrap();
        let mut tx2 = sample_tx_entry();
        tx2.tx_id = "manifest-2".to_string();
        tx2.db_version = 2;
        tx2.cdc_start_offset = Some(offsets.0);
        tx2.cdc_end_offset = Some(offsets.1);
        append_tx_catalog_entry(dir.path(), &tx2).unwrap();

        let visible = read_visible_cdc_entries(dir.path(), 0, None).unwrap();
        assert_eq!(visible, cdc1);

        let tx_rows = read_tx_catalog_entries(dir.path()).unwrap();
        assert_eq!(tx_rows.len(), 1);
        assert_eq!(tx_rows[0].db_version, 1);
    }

    #[test]
    fn prune_logs_for_replay_window_keeps_recent_versions() {
        let dir = TempDir::new().unwrap();

        let manifest1 = sample_manifest(1);
        let cdc1 = vec![sample_cdc(&manifest1.last_tx_id, 1, 0, "v1")];
        commit_manifest_and_logs(dir.path(), &manifest1, &cdc1, "tx1").unwrap();

        let manifest2 = sample_manifest(2);
        let cdc2 = vec![sample_cdc(&manifest2.last_tx_id, 2, 0, "v2")];
        commit_manifest_and_logs(dir.path(), &manifest2, &cdc2, "tx2").unwrap();

        let manifest3 = sample_manifest(3);
        let cdc3 = vec![sample_cdc(&manifest3.last_tx_id, 3, 0, "v3")];
        commit_manifest_and_logs(dir.path(), &manifest3, &cdc3, "tx3").unwrap();

        let stats = prune_logs_for_replay_window(dir.path(), 2).unwrap();
        assert_eq!(stats.tx_rows_removed, 1);
        assert_eq!(stats.tx_rows_kept, 2);
        assert_eq!(stats.cdc_rows_removed, 1);
        assert_eq!(stats.cdc_rows_kept, 2);

        let tx_rows = read_tx_catalog_entries(dir.path()).unwrap();
        assert_eq!(tx_rows.len(), 2);
        assert_eq!(tx_rows[0].db_version, 2);
        assert_eq!(tx_rows[1].db_version, 3);
        assert_eq!(tx_rows[0].cdc_start_offset, Some(0));
        assert!(
            tx_rows[0]
                .cdc_end_offset
                .is_some_and(|end| end > tx_rows[0].cdc_start_offset.unwrap())
        );
        assert_eq!(tx_rows[1].cdc_start_offset, tx_rows[0].cdc_end_offset);
        assert!(
            tx_rows[1]
                .cdc_end_offset
                .is_some_and(|end| end > tx_rows[1].cdc_start_offset.unwrap())
        );

        let cdc_rows = read_cdc_log_entries(dir.path()).unwrap();
        assert_eq!(cdc_rows.len(), 2);
        assert!(cdc_rows.iter().all(|r| r.db_version >= 2));
        let visible = read_visible_cdc_entries(dir.path(), 0, None).unwrap();
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().all(|r| r.db_version >= 2));
    }
}
