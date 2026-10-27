//! Post media (photos, videos, GIFs) from X API `includes.media`.
//!
//! Only https URLs on `pbs.twimg.com` / `video.twimg.com` are kept; anything
//! else is dropped (fail closed). Alt text is flattened to one line.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Media expansion and fields requested alongside posts.
pub const MEDIA_EXPANSION: &str = "attachments.media_keys";
pub const MEDIA_FIELDS: &str = "type,url,preview_image_url,variants,alt_text";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Media {
    /// `photo`, `video`, or `animated_gif`.
    #[serde(alias = "type")]
    pub kind: String,
    /// Full image URL (photos).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Thumbnail URL (videos and GIFs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<String>,
    /// Best mp4 variant (videos and GIFs): prefers 720p, excludes >1080p.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt_text: Option<String>,
    /// Caption/subtitle track URL for videos, when the stored data carries
    /// one. `None` for old stores and fixtures. Read by
    /// [`crate::link_context`]; fetching it never calls the X API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption_url: Option<String>,
    /// Lowest-bitrate mp4 variant (`<=1080p`) for local transcription.
    /// [`crate::link_context`] prefers this over [`Self::video_url`] (which
    /// prefers 720p at the highest bitrate) so transcription downloads the
    /// smallest file. `None` for old stores, fixtures, and photos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcription_url: Option<String>,
}

/// Accept only https URLs on X's media hosts, with no userinfo or port.
pub fn safe_media_url(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 2048 {
        return None;
    }
    let rest = s.strip_prefix("https://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    if host.contains(['@', ':', ' ', '\t', '\n', '\r']) {
        return None;
    }
    if !host.eq_ignore_ascii_case("pbs.twimg.com") && !host.eq_ignore_ascii_case("video.twimg.com")
    {
        return None;
    }
    if s.chars().any(|c| {
        c.is_whitespace()
            || c.is_control()
            || matches!(
                c,
                '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']' | '`' | '\\'
            )
    }) {
        return None;
    }
    Some(s.to_string())
}

fn clean_alt(s: &str) -> Option<String> {
    let stripped: String = s
        .chars()
        .filter(|c| {
            if c.is_whitespace() {
                true
            } else {
                !c.is_control() && !matches!(c, '[' | ']' | '<' | '>' | '`' | '\\')
            }
        })
        .collect();
    let flat: String = stripped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect();
    let flat = flat.trim().to_string();
    (!flat.is_empty()).then_some(flat)
}

/// Keep a caption/subtitle track URL for storage: trimmed http(s) only,
/// length-capped, with no whitespace or control characters. Full validation
/// happens at fetch time in [`crate::link_context`]; this only drops
/// obviously unusable values. Anything else is `None` (fail closed).
fn clean_caption_url(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 2048 {
        return None;
    }
    if !(s.starts_with("http://") || s.starts_with("https://")) {
        return None;
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    Some(s.to_string())
}

/// Sanitize a media kind for storage and display: lowercase ASCII
/// alphanumerics and underscore only, truncated, defaulting to `media`.
fn clean_kind(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                Some(c.to_ascii_lowercase())
            } else {
                None
            }
        })
        .take(20)
        .collect();
    if cleaned.is_empty() {
        "media".to_string()
    } else {
        cleaned
    }
}

/// Parse the `WxH` resolution (if present) from an X video variant URL path
/// like `/vid/avc1/1280x720/`, returning the shorter side as the "p" value.
fn parse_p_value(url: &str) -> Option<u32> {
    let bytes = url.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'x' || bytes[j] == b'X') {
                let k = j + 1;
                let mut l = k;
                while l < bytes.len() && bytes[l].is_ascii_digit() {
                    l += 1;
                }
                if l > k {
                    if let (Ok(w), Ok(h)) = (url[i..j].parse::<u32>(), url[k..l].parse::<u32>()) {
                        if w > 0 && h > 0 {
                            return Some(w.min(h));
                        }
                    }
                    i = l;
                    continue;
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    None
}

/// Usable mp4 candidates (`content_type video/mp4`, allowlisted URL,
/// `<=1080p`): `(bit_rate, p_value, url)`.
fn mp4_candidates(variants: &[serde_json::Value]) -> Vec<(u64, Option<u32>, String)> {
    variants
        .iter()
        .filter(|v| v.get("content_type").and_then(|c| c.as_str()) == Some("video/mp4"))
        .filter_map(|v| {
            let url = safe_media_url(v.get("url")?.as_str()?)?;
            let br = v.get("bit_rate").and_then(|b| b.as_u64()).unwrap_or(0);
            let p = parse_p_value(&url);
            Some((br, p, url))
        })
        .filter(|(_, p, _)| p.is_none_or(|p| p <= 1080))
        .collect()
}

/// Best mp4 for viewing: prefers 720p, excludes `>1080p` (see `parse_one`).
fn pick_best_mp4(candidates: &[(u64, Option<u32>, String)]) -> Option<String> {
    candidates
        .iter()
        .filter(|(_, p, _)| *p == Some(720))
        .max_by_key(|(br, _, _)| *br)
        .or_else(|| {
            candidates
                .iter()
                .filter(|(_, p, _)| p.is_some())
                .max_by_key(|(br, _, _)| *br)
        })
        .or_else(|| candidates.iter().max_by_key(|(br, _, _)| *br))
        .map(|(_, _, u)| u.clone())
}

/// Smallest mp4 for transcription: the lowest bitrate at `<=1080p` (ties
/// break toward the lower resolution, then the first listed).
fn pick_low_mp4(candidates: &[(u64, Option<u32>, String)]) -> Option<String> {
    candidates
        .iter()
        .min_by_key(|(br, p, _)| (*br, p.unwrap_or(u32::MAX)))
        .map(|(_, _, u)| u.clone())
}

fn parse_one(m: &serde_json::Value) -> Option<Media> {
    let kind = clean_kind(m.get("type")?.as_str()?);
    let get = |k: &str| m.get(k).and_then(|v| v.as_str()).and_then(safe_media_url);
    let candidates = m
        .get("variants")
        .and_then(|v| v.as_array())
        .map(|vs| mp4_candidates(vs))
        .unwrap_or_default();
    let video_url = pick_best_mp4(&candidates);
    let transcription_url = pick_low_mp4(&candidates);
    let media = Media {
        kind,
        url: get("url"),
        preview_url: get("preview_image_url"),
        video_url,
        alt_text: m
            .get("alt_text")
            .and_then(|v| v.as_str())
            .and_then(clean_alt),
        caption_url: m
            .get("caption_url")
            .and_then(|v| v.as_str())
            .and_then(clean_caption_url),
        transcription_url,
    };
    (media.url.is_some() || media.preview_url.is_some() || media.video_url.is_some())
        .then_some(media)
}

/// Index `includes.media` by `media_key`.
pub fn index_includes(value: &serde_json::Value) -> HashMap<String, Media> {
    let mut out = HashMap::new();
    if let Some(arr) = value
        .get("includes")
        .and_then(|i| i.get("media"))
        .and_then(|m| m.as_array())
    {
        for m in arr {
            if let (Some(key), Some(media)) =
                (m.get("media_key").and_then(|k| k.as_str()), parse_one(m))
            {
                out.insert(key.to_string(), media);
            }
        }
    }
    out
}

/// Media for one tweet, in attachment order.
pub fn for_tweet(tweet: &serde_json::Value, index: &HashMap<String, Media>) -> Vec<Media> {
    tweet
        .get("attachments")
        .and_then(|a| a.get("media_keys"))
        .and_then(|k| k.as_array())
        .map(|keys| {
            keys.iter()
                .filter_map(|k| k.as_str())
                .filter_map(|k| index.get(k).cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// Markdown lines for a post's media, re-checking every URL at render time.
pub fn render_markdown(media: &[Media]) -> String {
    let mut lines = Vec::new();
    for m in media {
        let kind = clean_kind(&m.kind);
        let alt = m.alt_text.as_deref().and_then(clean_alt);
        let label = alt.unwrap_or_else(|| kind.clone());
        let url = m.url.as_deref().and_then(safe_media_url);
        let preview = m.preview_url.as_deref().and_then(safe_media_url);
        let video = m.video_url.as_deref().and_then(safe_media_url);
        match (kind.as_str(), url, preview, video) {
            ("photo", Some(u), _, None) => lines.push(format!("- Photo: [![{label}]({u})]({u})")),
            (_, Some(u), _, None) => {
                lines.push(format!("- Media ({kind}): [![{label}]({u})]({u})"))
            }
            (_, _, p, Some(v)) => {
                let display = if kind == "animated_gif" {
                    "GIF"
                } else {
                    "Video"
                };
                match p {
                    Some(p) => lines.push(format!("- {display}: [![{label}]({p})]({v})")),
                    None => lines.push(format!("- {display}: [{label}]({v})")),
                }
            }
            (_, _, Some(p), None) => {
                lines.push(format!("- Media ({kind}): [![{label}]({p})]({p})"))
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("**Media**\n{}\n", lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> serde_json::Value {
        json!({
            "data": [{"id": "1", "attachments": {"media_keys": ["3_a", "7_b", "3_bad"]}}],
            "includes": {"media": [
                {"media_key": "3_a", "type": "photo", "url": "https://pbs.twimg.com/media/a.jpg", "alt_text": "a  chart\nof [things]"},
                {"media_key": "7_b", "type": "video", "preview_image_url": "https://pbs.twimg.com/thumb/b.jpg",
                 "variants": [
                    {"content_type": "application/x-mpegURL", "url": "https://video.twimg.com/b.m3u8"},
                    {"content_type": "video/mp4", "bit_rate": 256000, "url": "https://video.twimg.com/b_low.mp4"},
                    {"content_type": "video/mp4", "bit_rate": 2176000, "url": "https://video.twimg.com/b_high.mp4"},
                    {"content_type": "video/mp4", "bit_rate": 9000000, "url": "https://evil.example/b.mp4"}
                 ]},
                {"media_key": "3_bad", "type": "photo", "url": "http://pbs.twimg.com/media/x.jpg"}
            ]}
        })
    }

    #[test]
    fn url_allowlist() {
        assert!(safe_media_url("https://pbs.twimg.com/media/a.jpg").is_some());
        assert!(safe_media_url("https://video.twimg.com/v.mp4?tag=1").is_some());
        for bad in [
            "http://pbs.twimg.com/a.jpg",
            "https://pbs.twimg.com.evil.com/a.jpg",
            "https://user@pbs.twimg.com/a.jpg",
            "https://pbs.twimg.com:8443/a.jpg",
            "https://evil.com/pbs.twimg.com/a.jpg",
            "javascript:alert(1)",
            "https://pbs.twimg.com/a.jpg)[x](https://evil.com",
            "https://pbs.twimg.com/a b.jpg",
            "",
        ] {
            assert!(safe_media_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn parses_photo_and_best_mp4_and_drops_unsafe() {
        let v = sample();
        let idx = index_includes(&v);
        let media = for_tweet(&v["data"][0], &idx);
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].kind, "photo");
        assert_eq!(media[0].alt_text.as_deref(), Some("a chart of things"));
        assert_eq!(
            media[1].video_url.as_deref(),
            Some("https://video.twimg.com/b_high.mp4")
        );
        assert_eq!(
            media[1].preview_url.as_deref(),
            Some("https://pbs.twimg.com/thumb/b.jpg")
        );
    }

    #[test]
    fn no_attachments_means_no_media() {
        assert!(for_tweet(&json!({"id": "1"}), &HashMap::new()).is_empty());
        assert!(index_includes(&json!({})).is_empty());
    }

    #[test]
    fn renders_markdown_and_rechecks_urls() {
        let v = sample();
        let media = for_tweet(&v["data"][0], &index_includes(&v));
        let md = render_markdown(&media);
        assert!(
            md.contains("- Photo: [![a chart of things](https://pbs.twimg.com/media/a.jpg)]"),
            "{md}"
        );
        assert!(md.contains("- Video: [![video](https://pbs.twimg.com/thumb/b.jpg)](https://video.twimg.com/b_high.mp4)"), "{md}");
        let tampered = vec![Media {
            kind: "photo".into(),
            url: Some("https://evil.com/x.jpg".into()),
            preview_url: None,
            video_url: None,
            alt_text: None,
            caption_url: None,
            transcription_url: None,
        }];
        assert_eq!(render_markdown(&tampered), "");
        assert_eq!(render_markdown(&[]), "");
    }

    #[test]
    fn serde_backward_compatible() {
        let m: Media =
            serde_json::from_str(r#"{"kind":"photo","url":"https://pbs.twimg.com/a.jpg"}"#)
                .unwrap();
        assert!(m.video_url.is_none());
        assert!(m.caption_url.is_none());
        assert!(m.transcription_url.is_none());
        let s = serde_json::to_string(&m).unwrap();
        assert!(!s.contains("video_url"));
        assert!(!s.contains("caption_url"));
        assert!(!s.contains("transcription_url"));
    }

    #[test]
    fn transcription_url_prefers_lowest_bitrate() {
        let v = json!({
            "type": "video",
            "preview_image_url": "https://pbs.twimg.com/thumb/b.jpg",
            "variants": [
                {"content_type": "video/mp4", "bit_rate": 3000000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/1920x1080/b1080.mp4"},
                {"content_type": "video/mp4", "bit_rate": 1000000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/1280x720/b720.mp4"},
                {"content_type": "video/mp4", "bit_rate": 500000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/640x480/b480.mp4"},
            ]
        });
        let m = parse_one(&v).unwrap();
        // Viewing still prefers 720p; transcription takes the smallest.
        assert_eq!(
            m.video_url.as_deref(),
            Some("https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/1280x720/b720.mp4")
        );
        assert_eq!(
            m.transcription_url.as_deref(),
            Some("https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/640x480/b480.mp4")
        );
        // Above-1080p variants are excluded from both picks.
        let only_high = json!({
            "type": "video",
            "preview_image_url": "https://pbs.twimg.com/thumb/b.jpg",
            "variants": [
                {"content_type": "video/mp4", "bit_rate": 10000000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/3840x2160/b2160.mp4"},
            ]
        });
        let m = parse_one(&only_high).unwrap();
        assert!(m.video_url.is_none());
        assert!(m.transcription_url.is_none());
    }

    #[test]
    fn caption_url_kept_when_usable_else_dropped() {
        let v = json!({
            "media_key": "k",
            "type": "video",
            "preview_image_url": "https://pbs.twimg.com/thumb/synthetic.jpg",
            "variants": [
                {"content_type": "video/mp4", "bit_rate": 500000, "url": "https://video.twimg.com/synthetic.mp4"},
            ],
            "caption_url": "https://video.twimg.com/synthetic.vtt",
        });
        let m = parse_one(&v).unwrap();
        assert_eq!(
            m.caption_url.as_deref(),
            Some("https://video.twimg.com/synthetic.vtt")
        );
        for bad in [
            "",
            "  ",
            "ftp://example.invalid/a.vtt",
            "javascript:alert(1)",
        ] {
            let v = json!({
                "media_key": "k",
                "type": "video",
                "preview_image_url": "https://pbs.twimg.com/thumb/synthetic.jpg",
                "variants": [
                    {"content_type": "video/mp4", "bit_rate": 500000, "url": "https://video.twimg.com/synthetic.mp4"},
                ],
                "caption_url": bad,
            });
            assert_eq!(parse_one(&v).unwrap().caption_url, None, "{bad:?}");
        }
        // Missing key stays missing.
        let v = json!({
            "media_key": "k",
            "type": "video",
            "preview_image_url": "https://pbs.twimg.com/thumb/synthetic.jpg",
            "variants": [
                {"content_type": "video/mp4", "bit_rate": 500000, "url": "https://video.twimg.com/synthetic.mp4"},
            ],
        });
        assert_eq!(parse_one(&v).unwrap().caption_url, None);
    }

    #[test]
    fn host_match_is_case_insensitive_and_long_urls_rejected() {
        assert!(safe_media_url("https://PBS.TWIMG.COM/media/a.jpg").is_some());
        assert!(safe_media_url("https://Video.Twimg.Com/v.mp4").is_some());
        let long = format!("https://pbs.twimg.com/{}", "a".repeat(3000));
        assert!(safe_media_url(&long).is_none());
    }

    #[test]
    fn kind_and_alt_are_sanitized_for_markdown() {
        assert_eq!(clean_kind("photo"), "photo");
        assert_eq!(clean_kind("animated_gif"), "animated_gif");
        assert_eq!(clean_kind("photo](evil)[x"), "photoevilx");
        assert_eq!(clean_kind(""), "media");
        assert_eq!(clean_kind(" \n "), "media");
        let v = json!({
            "media_key": "k",
            "type": "photo](evil)[x",
            "url": "https://pbs.twimg.com/media/synthetic.jpg",
            "alt_text": "synthetic \\ escaped \u{0001} text",
        });
        let m = parse_one(&v).unwrap();
        assert_eq!(m.kind, "photoevilx");
        assert_eq!(m.alt_text.as_deref(), Some("synthetic escaped text"));
        let md = render_markdown(&[m]);
        assert!(!md.contains("](evil)"), "{md}");
        assert!(!md.contains('\\'), "{md}");
        // Non-photo with only a direct URL renders generically, not as Photo.
        let video_like = vec![Media {
            kind: "video".into(),
            url: Some("https://pbs.twimg.com/media/synthetic.jpg".into()),
            preview_url: None,
            video_url: None,
            alt_text: None,
            caption_url: None,
            transcription_url: None,
        }];
        let md = render_markdown(&video_like);
        assert!(md.contains("- Media (video):"), "{md}");
    }

    #[test]
    fn prefers_720p_over_1080p() {
        let v = json!({
            "type": "video",
            "preview_image_url": "https://pbs.twimg.com/thumb/b.jpg",
            "variants": [
                {"content_type": "video/mp4", "bit_rate": 3000000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/1920x1080/b1080.mp4"},
                {"content_type": "video/mp4", "bit_rate": 1000000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/1280x720/b720.mp4"},
            ]
        });
        let m = parse_one(&v).unwrap();
        assert_eq!(
            m.video_url.as_deref(),
            Some("https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/1280x720/b720.mp4")
        );
    }

    #[test]
    fn excludes_variants_above_1080p() {
        let v = json!({
            "type": "video",
            "preview_image_url": "https://pbs.twimg.com/thumb/b.jpg",
            "variants": [
                {"content_type": "video/mp4", "bit_rate": 10000000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/3840x2160/b2160.mp4"},
                {"content_type": "video/mp4", "bit_rate": 8000000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/2560x1440/b1440.mp4"},
                {"content_type": "video/mp4", "bit_rate": 500000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/640x480/b480.mp4"},
            ]
        });
        let m = parse_one(&v).unwrap();
        assert_eq!(
            m.video_url.as_deref(),
            Some("https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/640x480/b480.mp4")
        );
        let only_high = json!({
            "type": "video",
            "preview_image_url": "https://pbs.twimg.com/thumb/b.jpg",
            "variants": [
                {"content_type": "video/mp4", "bit_rate": 10000000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/3840x2160/b2160.mp4"},
                {"content_type": "video/mp4", "bit_rate": 8000000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/2560x1440/b1440.mp4"},
            ]
        });
        let m = parse_one(&only_high).unwrap();
        assert!(m.video_url.is_none());
    }

    #[test]
    fn falls_back_when_no_resolution() {
        let v = json!({
            "type": "video",
            "preview_image_url": "https://pbs.twimg.com/thumb/b.jpg",
            "variants": [
                {"content_type": "video/mp4", "bit_rate": 256000, "url": "https://video.twimg.com/b_low.mp4"},
                {"content_type": "video/mp4", "bit_rate": 2176000, "url": "https://video.twimg.com/b_high.mp4"},
            ]
        });
        let m = parse_one(&v).unwrap();
        assert_eq!(
            m.video_url.as_deref(),
            Some("https://video.twimg.com/b_high.mp4")
        );
        let mixed = json!({
            "type": "video",
            "preview_image_url": "https://pbs.twimg.com/thumb/b.jpg",
            "variants": [
                {"content_type": "video/mp4", "bit_rate": 5000000, "url": "https://video.twimg.com/b_nores.mp4"},
                {"content_type": "video/mp4", "bit_rate": 500000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/640x480/b480.mp4"},
            ]
        });
        let m = parse_one(&mixed).unwrap();
        assert_eq!(
            m.video_url.as_deref(),
            Some("https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/640x480/b480.mp4")
        );
    }

    #[test]
    fn vertical_720x1280_counts_as_720p() {
        let v = json!({
            "type": "video",
            "preview_image_url": "https://pbs.twimg.com/thumb/b.jpg",
            "variants": [
                {"content_type": "video/mp4", "bit_rate": 3000000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/1920x1080/b1080.mp4"},
                {"content_type": "video/mp4", "bit_rate": 1000000, "url": "https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/720x1280/bvert.mp4"},
            ]
        });
        let m = parse_one(&v).unwrap();
        assert_eq!(
            m.video_url.as_deref(),
            Some("https://video.twimg.com/ext_tw_video/1/pu/vid/avc1/720x1280/bvert.mp4")
        );
    }
}

#[cfg(test)]
mod demo_fixture_tests {
    #[test]
    fn loads_demo_fixture_media_with_type_key() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/demo/try_it_and_thread_bookmarks.jsonl"
        );
        let rows = crate::fixture::load(path).unwrap();
        let with_media: Vec<_> = rows.iter().filter(|b| !b.media.is_empty()).collect();
        assert!(!with_media.is_empty());
        for b in &with_media {
            let md = super::render_markdown(&b.media);
            assert!(md.starts_with("**Media**"), "{md}");
            assert!(md.contains("https://pbs.twimg.com/"), "{md}");
        }
        assert!(with_media.iter().any(|b| b.media[0].kind == "video"));
    }
}
