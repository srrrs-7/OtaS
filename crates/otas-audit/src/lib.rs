//! OtaS の監査ログ。
//!
//! 各記録は、1つ前の記録のハッシュを含めた SHA-256 で封をする（ハッシュチェーン）。
//! 途中の記録を書き換えたり消したりすると、それ以降のハッシュが合わなくなるので検知できる。
//!
//! - [`MemoryAuditLog`]：メモリ上の実装（テストや一時的な用途）
//! - [`FileAuditLog`]：JSON Lines で追記するファイル実装。開くときにチェーンを検証する

use otas_ports::{AuditError, AuditSink};
use otas_types::{AuditEntry, AuditRecord};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// 最初の記録の `prev_hash`。
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// 記録のハッシュを計算する。
pub fn compute_hash(seq: u64, timestamp_ms: u64, prev_hash: &str, entry: &AuditEntry) -> String {
    #[derive(Serialize)]
    struct Payload<'a> {
        seq: u64,
        timestamp_ms: u64,
        prev_hash: &'a str,
        entry: &'a AuditEntry,
    }
    let bytes = serde_json::to_vec(&Payload {
        seq,
        timestamp_ms,
        prev_hash,
        entry,
    })
    .expect("監査エントリは常にシリアライズできる");
    Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 記録の並びがハッシュチェーンとして正しいか検証する。
pub fn verify_chain(records: &[AuditRecord]) -> Result<(), AuditError> {
    let mut prev = GENESIS_HASH.to_string();
    for (i, r) in records.iter().enumerate() {
        let expected = compute_hash(r.seq, r.timestamp_ms, &r.prev_hash, &r.entry);
        if r.seq != i as u64 || r.prev_hash != prev || r.hash != expected {
            return Err(AuditError::Broken { seq: r.seq });
        }
        prev.clone_from(&r.hash);
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// チェーンの末尾の状態。
#[derive(Debug, Clone)]
struct Chain {
    next_seq: u64,
    last_hash: String,
}

impl Chain {
    fn genesis() -> Self {
        Self {
            next_seq: 0,
            last_hash: GENESIS_HASH.to_string(),
        }
    }

    fn after(records: &[AuditRecord]) -> Self {
        match records.last() {
            Some(last) => Self {
                next_seq: last.seq + 1,
                last_hash: last.hash.clone(),
            },
            None => Self::genesis(),
        }
    }

    /// 次の記録を作る。書き込みに成功するまで状態は進めない。
    fn prepare(&self, entry: AuditEntry) -> AuditRecord {
        let timestamp_ms = now_ms();
        let hash = compute_hash(self.next_seq, timestamp_ms, &self.last_hash, &entry);
        AuditRecord {
            seq: self.next_seq,
            timestamp_ms,
            prev_hash: self.last_hash.clone(),
            hash,
            entry,
        }
    }

    fn commit(&mut self, record: &AuditRecord) {
        self.next_seq = record.seq + 1;
        self.last_hash.clone_from(&record.hash);
    }
}

/// メモリ上の監査ログ。
#[derive(Debug)]
pub struct MemoryAuditLog {
    inner: Mutex<(Chain, Vec<AuditRecord>)>,
}

impl MemoryAuditLog {
    /// 空のログを作る。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new((Chain::genesis(), Vec::new())),
        }
    }

    /// これまでの記録の複製を返す。
    pub fn records(&self) -> Vec<AuditRecord> {
        lock(&self.inner).1.clone()
    }
}

impl Default for MemoryAuditLog {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditSink for MemoryAuditLog {
    fn append(&self, entry: AuditEntry) -> Result<AuditRecord, AuditError> {
        let mut guard = lock(&self.inner);
        let (chain, records) = &mut *guard;
        let record = chain.prepare(entry);
        chain.commit(&record);
        records.push(record.clone());
        Ok(record)
    }
}

/// JSON Lines で追記するファイルの監査ログ。
#[derive(Debug)]
pub struct FileAuditLog {
    path: PathBuf,
    inner: Mutex<(Chain, File)>,
}

impl FileAuditLog {
    /// ファイルを開く（なければ作る）。既存の記録のチェーンを検証し、続きから追記する。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AuditError> {
        let path = path.as_ref().to_path_buf();
        let records = if path.exists() {
            Self::read_all(&path)?
        } else {
            Vec::new()
        };
        verify_chain(&records)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            inner: Mutex::new((Chain::after(&records), file)),
        })
    }

    /// ファイルの記録をすべて読む（検証はしない）。
    pub fn read_all(path: impl AsRef<Path>) -> Result<Vec<AuditRecord>, AuditError> {
        let reader = BufReader::new(File::open(path)?);
        let mut records = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let record =
                serde_json::from_str(&line).map_err(|e| AuditError::Format(e.to_string()))?;
            records.push(record);
        }
        Ok(records)
    }

    /// ログファイルのパス。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AuditSink for FileAuditLog {
    fn append(&self, entry: AuditEntry) -> Result<AuditRecord, AuditError> {
        let mut guard = lock(&self.inner);
        let (chain, file) = &mut *guard;
        let record = chain.prepare(entry);
        let mut line =
            serde_json::to_string(&record).map_err(|e| AuditError::Format(e.to_string()))?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.sync_data()?;
        chain.commit(&record);
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otas_types::{Action, Decision, Effect, LabelSet, Outcome, TaskId};

    fn entry(n: u32) -> AuditEntry {
        AuditEntry {
            task: TaskId::new(),
            action: Action::CallTool {
                tool: format!("tool.{n}"),
            },
            effect: Effect::Internal,
            labels: LabelSet::empty(),
            decision: Decision::Allow,
            approved_by_human: false,
            outcome: Outcome::Executed,
        }
    }

    #[test]
    fn chain_verifies() {
        let log = MemoryAuditLog::new();
        for n in 0..5 {
            log.append(entry(n)).unwrap();
        }
        let records = log.records();
        assert_eq!(records.len(), 5);
        assert_eq!(records[0].prev_hash, GENESIS_HASH);
        verify_chain(&records).unwrap();
    }

    #[test]
    fn tampering_is_detected() {
        let log = MemoryAuditLog::new();
        for n in 0..3 {
            log.append(entry(n)).unwrap();
        }
        let mut records = log.records();
        records[1].entry.decision = Decision::Deny {
            reason: "書き換え".into(),
        };
        assert!(matches!(
            verify_chain(&records),
            Err(AuditError::Broken { seq: 1 })
        ));

        let mut records = log.records();
        records.remove(1);
        assert!(verify_chain(&records).is_err());
    }

    #[test]
    fn file_log_continues_after_reopen() {
        let path = std::env::temp_dir().join(format!("otas-audit-{}.jsonl", TaskId::new()));
        {
            let log = FileAuditLog::open(&path).unwrap();
            log.append(entry(0)).unwrap();
            log.append(entry(1)).unwrap();
        }
        {
            let log = FileAuditLog::open(&path).unwrap();
            let r = log.append(entry(2)).unwrap();
            assert_eq!(r.seq, 2);
        }
        let records = FileAuditLog::read_all(&path).unwrap();
        assert_eq!(records.len(), 3);
        verify_chain(&records).unwrap();
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn file_log_refuses_tampered_file() {
        let path = std::env::temp_dir().join(format!("otas-audit-{}.jsonl", TaskId::new()));
        {
            let log = FileAuditLog::open(&path).unwrap();
            log.append(entry(0)).unwrap();
            log.append(entry(1)).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replacen("tool.0", "tool.X", 1)).unwrap();
        assert!(matches!(
            FileAuditLog::open(&path),
            Err(AuditError::Broken { .. })
        ));
        std::fs::remove_file(&path).unwrap();
    }
}
