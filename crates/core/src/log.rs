//! Append-only, hash-chained JSONL decision log.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("chain broken at line {0}")]
    Chain(usize),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub seq: u64,
    pub prev_hash: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub hash: String,
}

const GENESIS: &str = "0";

fn digest(seq: u64, prev: &str, kind: &str, payload: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    h.update(format!("{seq}|{prev}|{kind}|{payload}"));
    hex::encode(h.finalize())
}

#[derive(Debug)]
pub struct DecisionLog {
    path: PathBuf,
    next_seq: u64,
    last_hash: String,
}

impl DecisionLog {
    /// Open (or create) a log, verifying the existing chain.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LogError> {
        let path = path.as_ref().to_path_buf();
        let mut log = Self {
            path,
            next_seq: 0,
            last_hash: GENESIS.into(),
        };
        if log.path.exists() {
            for e in log.verify()? {
                log.next_seq = e.seq + 1;
                log.last_hash = e.hash;
            }
        }
        Ok(log)
    }

    /// Append one entry. The file is only ever opened in append mode.
    pub fn append(&mut self, kind: &str, payload: serde_json::Value) -> Result<Entry, LogError> {
        let hash = digest(self.next_seq, &self.last_hash, kind, &payload);
        let e = Entry {
            seq: self.next_seq,
            prev_hash: self.last_hash.clone(),
            kind: kind.into(),
            payload,
            hash,
        };
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(&e)?)?;
        f.sync_data()?;
        self.next_seq += 1;
        self.last_hash = e.hash.clone();
        Ok(e)
    }

    /// Like `verify`, but a log file that does not exist yet is empty.
    pub fn verify_or_empty(&self) -> Result<Vec<Entry>, LogError> {
        if self.path.exists() {
            self.verify()
        } else {
            Ok(Vec::new())
        }
    }

    /// Read all entries and check the hash chain.
    pub fn verify(&self) -> Result<Vec<Entry>, LogError> {
        let mut prev = GENESIS.to_string();
        let mut out = Vec::new();
        for (i, line) in BufReader::new(File::open(&self.path)?).lines().enumerate() {
            let e: Entry = serde_json::from_str(&line?)?;
            if e.seq != i as u64
                || e.prev_hash != prev
                || e.hash != digest(e.seq, &e.prev_hash, &e.kind, &e.payload)
            {
                return Err(LogError::Chain(i));
            }
            prev = e.hash.clone();
            out.push(e);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn appends_and_reopens_preserving_history() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        log.append("route", json!({"to": "local"})).unwrap();
        drop(log);
        let mut log = DecisionLog::open(&p).unwrap();
        let e = log.append("approve", json!({"id": 1})).unwrap();
        assert_eq!(e.seq, 1);
        assert_eq!(log.verify().unwrap().len(), 2);
    }

    #[test]
    fn detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        log.append("a", json!(1)).unwrap();
        log.append("b", json!(2)).unwrap();
        let s = std::fs::read_to_string(&p)
            .unwrap()
            .replacen("\"a\"", "\"x\"", 1);
        std::fs::write(&p, s).unwrap();
        assert!(matches!(DecisionLog::open(&p), Err(LogError::Chain(0))));
    }
}
