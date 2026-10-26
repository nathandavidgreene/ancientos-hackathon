//! JSONL fixture loader (public fields only).
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Label {
    #[serde(default)]
    pub parent_label: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub is_foldered: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    pub post_id: String,
    pub url: String,
    pub text: String,
    #[serde(default)]
    pub author_username: Option<String>,
    #[serde(default)]
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
    fn ignores_unknown_fields_and_null_flags() {
        let line = r#"{"post_id":"s1","url":"https://x.com/i/status/s1","text":"synthetic","author_username":"syn","created_at":"2026-01-01T00:00:00+00:00","folders":["A"],"label":{"parent_label":"T","confidence":0.9,"method":"jev","is_foldered":true,"extra":"drop"},"still_bookmarked":null,"another_unknown":42}"#;
        let rows = parse_jsonl(line).unwrap();
        assert_eq!(rows.len(), 1);
        let label = rows[0].label.as_ref().unwrap();
        assert_eq!(label.method.as_deref(), Some("jev"));
        assert_eq!(label.is_foldered, Some(true));
    }

    #[test]
    fn reports_bad_line() {
        let err =
            parse_jsonl("{\"post_id\":\"1\",\"url\":\"u\",\"text\":\"t\"}\nnot json").unwrap_err();
        assert!(matches!(err, FixtureError::Parse { line: 2, .. }));
    }
}
