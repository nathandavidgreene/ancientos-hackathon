//! JSONL fixture loader (public fields only).
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Label {
    pub parent_label: Option<String>,
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    pub post_id: String,
    pub url: String,
    pub text: String,
    pub author_username: Option<String>,
    pub created_at: Option<String>,
    #[serde(default)]
    pub folders: Option<Vec<String>>,
    #[serde(default)]
    pub label: Option<Label>,
}

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("line {line}: {source}")]
    Parse {
        line: usize,
        source: serde_json::Error,
    },
}

pub fn parse_jsonl(s: &str) -> Result<Vec<Bookmark>, FixtureError> {
    s.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str(l).map_err(|source| FixtureError::Parse {
                line: i + 1,
                source,
            })
        })
        .collect()
}

pub fn load(path: impl AsRef<Path>) -> Result<Vec<Bookmark>, FixtureError> {
    parse_jsonl(&std::fs::read_to_string(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_fixture_parses() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/sample_bookmarks.jsonl"
        );
        let rows = load(path).unwrap();
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|b| !b.post_id.is_empty()));
    }

    #[test]
    fn reports_bad_line() {
        let err =
            parse_jsonl("{\"post_id\":\"1\",\"url\":\"u\",\"text\":\"t\"}\nnot json").unwrap_err();
        assert!(matches!(err, FixtureError::Parse { line: 2, .. }));
    }
}
