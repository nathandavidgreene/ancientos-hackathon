//! Shot 12 add-on: the web-ui `/accuracy` page rendered offline through
//! web-ui's own handler (the same code the server runs) and printed as text.
//! The page's numbers come from the committed synthetic eval set compiled
//! into the binary; nothing here reads eval-private or calls a model.
//! Semantic ranking in the brief stays opt-in and is reported, not enabled.
use crate::{require_paid_off, Workspace};

/// Text of the rendered `/accuracy` page plus whether the brief would rank
/// by meaning in this environment (the demo expects `false`).
#[derive(Debug, Clone, PartialEq)]
pub struct AccuracyDemo {
    /// Page body lines after the nav bar, tags stripped, table cells joined
    /// with `: ` (never `|`, so the storyboard's markdown table can quote
    /// them).
    pub lines: Vec<String>,
    /// True only when both semantic env settings are set (opt-in).
    pub semantic_in_brief: bool,
}

/// Names of tags that start a new text line when the page is flattened.
const BLOCK_TAGS: &[&str] = &[
    "h1",
    "h2",
    "h3",
    "p",
    "tr",
    "table",
    "div",
    "blockquote",
    "br",
    "hr",
    "nav",
    "li",
    "ul",
];

/// Undo web-ui's HTML escaping for the five characters it escapes.
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Flatten an HTML page to trimmed, non-empty text lines. Block tags break
/// lines; table cells after the first in a row are joined with `: ` (never
/// `|`, so the storyboard's markdown table can quote them). Only the body
/// after the first `<hr>` (web-ui's nav bar separator) is kept,
/// or the whole page when there is none.
pub fn page_text(html: &str) -> Vec<String> {
    let body = html.split_once("<hr>").map_or(html, |(_, b)| b);
    let mut out = String::new();
    let mut rest = body;
    let mut cell_in_row = 0usize;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[start + 1..start + end];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if BLOCK_TAGS.contains(&name.as_str()) {
            out.push('\n');
            if name == "tr" {
                cell_in_row = 0;
            }
        } else if (name == "td" || name == "th") && !closing {
            if cell_in_row > 0 {
                out.push_str(": ");
            }
            cell_in_row += 1;
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    unescape(&out)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

impl Workspace {
    /// Render web-ui's `/accuracy` page for this data dir and flatten it to
    /// text. Paid-off only. A non-200 page is an error (fail closed: no
    /// numbers printed).
    pub fn accuracy_demo(&self) -> Result<AccuracyDemo, String> {
        require_paid_off()?;
        let mut state = web_ui::State::open(&self.dir)?;
        state.phone_token = Some("synthetic-demo-desktop-token-0002".into());
        let resp = web_ui::handle(
            &web_ui::Request::get("/accuracy")
                .with_cookie("ap_k=synthetic-demo-desktop-token-0002"),
            &state,
        );
        if resp.status != 200 {
            return Err(format!("/accuracy returned {}", resp.status));
        }
        Ok(AccuracyDemo {
            lines: page_text(&resp.body),
            semantic_in_brief: bookmarks_agent::semantic::Config::from_env().is_some(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_text_flattens_blocks_and_table_cells() {
        let html = "<nav><a href=\"/\">Brief</a></nav><hr><h1>Accuracy</h1>\
                    <p>A &amp; B &lt;ok&gt; it&#39;s &quot;q&quot;</p>\
                    <table><tr><th>Picks</th><th>P@5</th></tr>\
                    <tr><td>Today</td><td>0.600</td></tr></table>";
        assert_eq!(
            page_text(html),
            vec![
                "Accuracy",
                "A & B <ok> it's \"q\"",
                "Picks: P@5",
                "Today: 0.600"
            ]
        );
    }

    #[test]
    fn page_text_without_nav_keeps_whole_page() {
        assert_eq!(page_text("<p>one</p><p>two</p>"), vec!["one", "two"]);
        assert!(page_text("").is_empty());
        assert_eq!(page_text("<p>cut <b"), vec!["cut"]);
    }

    #[test]
    fn accuracy_demo_renders_synthetic_panel_without_private_data() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let demo = ws.accuracy_demo().unwrap();
        let text = demo.lines.join("\n");
        for want in [
            "Accuracy",
            "committed synthetic eval set",
            "Try-it classification",
            "Brief ranking",
            "Picks: P@5",
            "Votes (personal ranker):",
            "Meaning (local embeddings):",
            "Votes + meaning (blend):",
        ] {
            assert!(text.contains(want), "missing {want}: {text}");
        }
        let (_, cmp) = ancientos_eval::harness::synthetic_panel().unwrap();
        let f3 = |x: f64| format!("{x:.3}");
        for (row, value) in [
            ("Today's order", f3(cmp.baseline_precision_at_5)),
            ("Votes (personal ranker)", f3(cmp.ranked_precision_at_5)),
            (
                "Votes + meaning (blend)",
                f3(cmp.blended_precision_at_5.unwrap()),
            ),
        ] {
            let line = format!("{row}: {value}");
            assert!(demo.lines.contains(&line), "missing {line}: {text}");
        }
        assert!(!text.contains("eval-private"), "{text}");
        assert!(!text.contains("Brief</a>"), "nav must be stripped");
        assert!(!text.contains('<'), "tags must be stripped: {text}");
    }
}
