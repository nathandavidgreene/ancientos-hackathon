//! Read-only X API source with automatic token refresh.
//!
//! Paid calls are off by default: every entry point fails closed unless
//! `ANCIENTOS_ALLOW_PAID=1`. The user id is fetched once from `/users/me`,
//! then bookmarks pages map to [`Bookmark`](crate::fixture::Bookmark).
use crate::fixture::Bookmark;
use crate::ingest::{BookmarkSource, Page};
use crate::token_store::{StoredTokens, TokenStoreError};
use std::collections::HashMap;

pub const ME_URL: &str = "https://api.x.com/2/users/me";

/// Bookmarks endpoint for a user id with the fields we map.
pub fn bookmarks_url(user_id: &str) -> String {
    format!(
        "https://api.x.com/2/users/{user_id}/bookmarks?max_results=100\
         &tweet.fields=created_at,author_id&expansions=author_id&user.fields=username"
    )
}

fn paid_error() -> String {
    "paid calls are disabled (set ANCIENTOS_ALLOW_PAID=1 to enable)".to_string()
}

fn require_paid() -> Result<(), String> {
    match std::env::var("ANCIENTOS_ALLOW_PAID") {
        Ok(v) if v == "1" => Ok(()),
        _ => Err(paid_error()),
    }
}

/// Extract the user id from a `/users/me` response. Pure, for tests.
pub fn parse_user_id(value: &serde_json::Value) -> Result<String, String> {
    value
        .get("data")
        .and_then(|d| d.get("id"))
        .and_then(|id| id.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "users/me response has no data.id".to_string())
}

/// Map one bookmarks page to fixtures. Pure, for tests.
pub fn parse_bookmarks_page(value: &serde_json::Value) -> Result<Page, String> {
    let mut usernames: HashMap<&str, &str> = HashMap::new();
    if let Some(users) = value
        .get("includes")
        .and_then(|i| i.get("users"))
        .and_then(|u| u.as_array())
    {
        for u in users {
            if let (Some(id), Some(name)) = (
                u.get("id").and_then(|v| v.as_str()),
                u.get("username").and_then(|v| v.as_str()),
            ) {
                usernames.insert(id, name);
            }
        }
    }
    let mut items = Vec::new();
    if let Some(tweets) = value.get("data").and_then(|d| d.as_array()) {
        for t in tweets {
            let id = t
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "bookmark tweet has no id".to_string())?;
            let author = t
                .get("author_id")
                .and_then(|v| v.as_str())
                .and_then(|aid| usernames.get(aid).copied());
            let url = match author {
                Some(name) => format!("https://x.com/{name}/status/{id}"),
                None => format!("https://x.com/i/status/{id}"),
            };
            items.push(Bookmark {
                post_id: id.to_string(),
                url,
                text: t
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                author_username: author.map(str::to_string),
                created_at: t
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                folders: None,
                label: None,
            });
        }
    }
    let next_token = value
        .get("meta")
        .and_then(|m| m.get("next_token"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(Page { items, next_token })
}

fn get_json(url: &str, access_token: &str) -> Result<serde_json::Value, String> {
    ureq::get(url)
        .timeout(std::time::Duration::from_secs(30))
        .set("Authorization", &format!("Bearer {access_token}"))
        .call()
        .map_err(map_ureq_error)?
        .into_json()
        .map_err(|e| format!("bad X API response: {e}"))
}

fn map_ureq_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(300).collect();
            format!("X API http status {code}: {snippet}")
        }
        other => format!("X API request failed: {other}"),
    }
}

/// Live X bookmarks source. Holds tokens in memory only; refreshed tokens are
/// persisted through [`token_store`](crate::token_store).
#[derive(Debug)]
pub struct XApiSource {
    tokens: StoredTokens,
    client_id: String,
    user_id: Option<String>,
}

impl XApiSource {
    /// Build from tokens. Fails closed without the paid switch.
    /// The client id is needed to refresh an expired access token.
    pub fn new(tokens: StoredTokens, client_id: String) -> Result<Self, String> {
        require_paid()?;
        if client_id.is_empty() {
            return Err("X client id must be non-empty".to_string());
        }
        if tokens.access_token.is_empty() || tokens.refresh_token.is_empty() {
            return Err("stored tokens are incomplete; run login again".to_string());
        }
        Ok(Self {
            tokens,
            client_id,
            user_id: None,
        })
    }

    /// Load tokens from the token store. Fails closed without the paid switch.
    pub fn load_from_store(client_id: String) -> Result<Self, String> {
        require_paid()?;
        let tokens = crate::token_store::load().map_err(|e| e.to_string())?;
        Self::new(tokens, client_id)
    }

    fn ensure_fresh_token(&mut self) -> Result<(), String> {
        if !self.tokens.is_expired() {
            return Ok(());
        }
        let resp = crate::oauth::refresh_tokens(&self.client_id, &self.tokens.refresh_token)
            .map_err(|e| e.to_string())?;
        let now = crate::token_store::unix_now().map_err(|e| match e {
            TokenStoreError::Clock(msg) => format!("clock error: {msg}"),
            other => other.to_string(),
        })?;
        // A refresh may omit the new refresh token; keep the old one then.
        let refresh = resp
            .refresh_token
            .unwrap_or_else(|| self.tokens.refresh_token.clone());
        let scope = resp.scope.unwrap_or_else(|| self.tokens.scope.clone());
        self.tokens = StoredTokens::new(
            resp.access_token,
            refresh,
            now.saturating_add(resp.expires_in.unwrap_or(0)),
            scope,
        );
        crate::token_store::save(&self.tokens).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn user_id(&mut self) -> Result<String, String> {
        if let Some(id) = &self.user_id {
            return Ok(id.clone());
        }
        self.ensure_fresh_token()?;
        let raw = get_json(ME_URL, &self.tokens.access_token.clone())?;
        let id = parse_user_id(&raw)?;
        self.user_id = Some(id.clone());
        Ok(id)
    }
}

impl BookmarkSource for XApiSource {
    fn fetch(&mut self, pagination_token: Option<&str>) -> Result<Page, String> {
        require_paid()?;
        let uid = self.user_id()?;
        self.ensure_fresh_token()?;
        let mut url = bookmarks_url(&uid);
        if let Some(t) = pagination_token {
            url.push_str("&pagination_token=");
            url.push_str(&crate::oauth::url_encode(t));
        }
        let raw = get_json(&url, &self.tokens.access_token.clone())?;
        parse_bookmarks_page(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    fn env_lock() -> MutexGuard<'static, ()> {
        static ENV_MUTEX: Mutex<()> = Mutex::new(());
        ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn tokens() -> StoredTokens {
        StoredTokens::new(
            "access".into(),
            "refresh".into(),
            1_800_000_000,
            "tweet.read".into(),
        )
    }

    #[test]
    fn bookmarks_url_has_required_params() {
        let u = bookmarks_url("123");
        assert!(u.contains("/2/users/123/bookmarks"), "{u}");
        assert!(u.contains("max_results=100"), "{u}");
        assert!(u.contains("tweet.fields=created_at,author_id"), "{u}");
        assert!(u.contains("expansions=author_id"), "{u}");
        assert!(u.contains("user.fields=username"), "{u}");
    }

    #[test]
    fn parses_user_id() {
        let raw = serde_json::json!({"data": {"id": "42", "username": "me"}});
        assert_eq!(parse_user_id(&raw).unwrap(), "42");
        assert!(parse_user_id(&serde_json::json!({})).is_err());
        assert!(parse_user_id(&serde_json::json!({"data": {}})).is_err());
    }

    #[test]
    fn parses_bookmarks_page_with_authors() {
        let raw = serde_json::json!({
            "data": [
                {"id": "1", "text": "hello", "author_id": "u1",
                 "created_at": "2026-01-01T00:00:00Z"},
                {"id": "2", "text": "no author match", "author_id": "u9"}
            ],
            "includes": {"users": [{"id": "u1", "username": "alice"}]},
            "meta": {"next_token": "tok123", "result_count": 2}
        });
        let page = parse_bookmarks_page(&raw).unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.next_token.as_deref(), Some("tok123"));
        assert_eq!(page.items[0].post_id, "1");
        assert_eq!(page.items[0].author_username.as_deref(), Some("alice"));
        assert_eq!(page.items[0].url, "https://x.com/alice/status/1");
        assert_eq!(
            page.items[0].created_at.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert_eq!(page.items[1].author_username, None);
        assert_eq!(page.items[1].url, "https://x.com/i/status/2");
    }

    #[test]
    fn empty_page_parses() {
        let page = parse_bookmarks_page(&serde_json::json!({"meta": {}})).unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.next_token, None);
    }

    #[test]
    fn paid_switch_is_required() {
        let _guard = env_lock();
        let saved = std::env::var("ANCIENTOS_ALLOW_PAID").ok();
        std::env::remove_var("ANCIENTOS_ALLOW_PAID");
        assert!(XApiSource::new(tokens(), "cid".into()).is_err());
        assert!(XApiSource::load_from_store("cid".into()).is_err());

        std::env::set_var("ANCIENTOS_ALLOW_PAID", "1");
        assert!(XApiSource::new(tokens(), "cid".into()).is_ok());
        assert!(XApiSource::new(tokens(), "".into()).is_err());

        match saved {
            Some(v) => std::env::set_var("ANCIENTOS_ALLOW_PAID", v),
            None => std::env::remove_var("ANCIENTOS_ALLOW_PAID"),
        }
    }

    #[test]
    fn debug_output_redacts_tokens() {
        let _guard = env_lock();
        let saved = std::env::var("ANCIENTOS_ALLOW_PAID").ok();
        std::env::set_var("ANCIENTOS_ALLOW_PAID", "1");
        let src = XApiSource::new(
            StoredTokens::new("tok-access".into(), "tok-refresh".into(), 1, "s".into()),
            "cid".into(),
        )
        .unwrap();
        let dbg = format!("{src:?}");
        assert!(!dbg.contains("tok-access"), "leaked: {dbg}");
        assert!(!dbg.contains("tok-refresh"), "leaked: {dbg}");

        match saved {
            Some(v) => std::env::set_var("ANCIENTOS_ALLOW_PAID", v),
            None => std::env::remove_var("ANCIENTOS_ALLOW_PAID"),
        }
    }
}
