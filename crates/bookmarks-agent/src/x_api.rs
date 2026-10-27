//! Read-only X API source with automatic token refresh.
//!
//! Paid calls are off by default: every entry point fails closed unless
//! `ANCIENTOS_ALLOW_PAID=1`. The user id is fetched once from `/users/me`,
//! then bookmarks pages map to [`Bookmark`](crate::fixture::Bookmark).
use crate::fixture::Bookmark;
use crate::ingest::{BookmarkSource, Page};
use crate::token_store::{StoredTokens, TokenStoreError};
use crate::unroll::ThreadPost;
use std::collections::HashMap;

pub const ME_URL: &str = "https://api.x.com/2/users/me";

/// Bookmarks endpoint for a user id with the fields we map.
pub fn bookmarks_url(user_id: &str) -> String {
    format!(
        "https://api.x.com/2/users/{}/bookmarks?max_results=100\
         &tweet.fields=note_tweet,text,created_at,author_id,lang,attachments&expansions=author_id,{}\
         &media.fields={}&user.fields=username",
        crate::oauth::url_encode(user_id),
        crate::media::MEDIA_EXPANSION,
        crate::media::MEDIA_FIELDS,
    )
}

/// Reject post ids that are empty or could break out of a query string.
fn validate_post_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("post id must be non-empty".to_string());
    }
    if id.contains([',', '&', '=', '?', '#', ' ', '\t', '\n']) {
        return Err("invalid post id".to_string());
    }
    Ok(())
}

/// Batched tweets endpoint for full-text hydration (max 100 ids).
pub fn hydrate_url(ids: &[String]) -> Result<String, String> {
    if ids.is_empty() {
        return Err("no ids to hydrate".to_string());
    }
    if ids.len() > 100 {
        return Err("too many ids to hydrate (max 100)".to_string());
    }
    for id in ids {
        validate_post_id(id)?;
    }
    Ok(format!(
        "https://api.x.com/2/tweets?ids={}&tweet.fields=note_tweet,text",
        ids.join(",")
    ))
}

/// Tweet fields for thread reconstruction: full text, timestamps,
/// authorship, reply linkage, conversation id, and quote references.
pub const THREAD_TWEET_FIELDS: &str =
    "note_tweet,text,created_at,author_id,referenced_tweets,conversation_id,in_reply_to_user_id";
/// Expansions for thread reconstruction: quoted posts plus authors.
pub const THREAD_EXPANSIONS: &str = "referenced_tweets.id,author_id";

/// Single-tweet endpoint for thread reconstruction: conversation id,
/// full text, authorship, and reply linkage, plus the author's username
/// and any quoted post.
pub fn thread_root_url(id: &str) -> Result<String, String> {
    validate_post_id(id)?;
    Ok(format!(
        "https://api.x.com/2/tweets/{id}?\
         tweet.fields={THREAD_TWEET_FIELDS}\
         &expansions={THREAD_EXPANSIONS}&user.fields=username"
    ))
}

/// Build the X recent-search query for a thread's replies by its author.
fn search_query(conversation_id: &str, author_username: &str) -> Result<String, String> {
    validate_post_id(conversation_id).map_err(|_| "invalid conversation id".to_string())?;
    if author_username.is_empty()
        || !author_username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err("invalid author username".to_string());
    }
    Ok(crate::oauth::url_encode(&format!(
        "conversation_id:{conversation_id} from:{author_username}"
    )))
}

/// Recent-search endpoint for one author's posts in a conversation.
/// Recent search only covers the last 7 days; see [`thread_search_all_url`]
/// for the archive fallback. At most two search calls per thread.
pub fn thread_search_url(conversation_id: &str, author_username: &str) -> Result<String, String> {
    let query = search_query(conversation_id, author_username)?;
    Ok(format!(
        "https://api.x.com/2/tweets/search/recent?query={query}&max_results=100\
         &tweet.fields={THREAD_TWEET_FIELDS}\
         &expansions={THREAD_EXPANSIONS}&user.fields=username"
    ))
}

/// Full-archive search endpoint with the same query, fields, and expansions
/// as [`thread_search_url`]. Used ONCE per thread when recent search returns
/// zero matches (e.g. a root older than 7 days). May fail with 403 on access
/// tiers without archive search; callers treat that as search unavailable.
pub fn thread_search_all_url(
    conversation_id: &str,
    author_username: &str,
) -> Result<String, String> {
    let query = search_query(conversation_id, author_username)?;
    Ok(format!(
        "https://api.x.com/2/tweets/search/all?query={query}&max_results=100\
         &tweet.fields={THREAD_TWEET_FIELDS}\
         &expansions={THREAD_EXPANSIONS}&user.fields=username"
    ))
}

/// True when a recent-search response yielded zero usable candidates, so the
/// caller should try the full-archive search once. Zero matches for an older
/// root mean "unknown" (outside the 7-day window), not "no replies".
/// Pure, for tests.
pub fn needs_archive_fallback(candidate_count: usize) -> bool {
    candidate_count == 0
}

/// Full post text: `note_tweet.text` (untruncated long-post text) wins,
/// else `text`. Empty when both are missing.
fn full_text_of(tweet: &serde_json::Value) -> String {
    if let Some(s) = tweet
        .get("note_tweet")
        .and_then(|n| n.get("text"))
        .and_then(|v| v.as_str())
    {
        if !s.is_empty() {
            return s.to_string();
        }
    }
    tweet
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// True if the tweet has a note_tweet (long post) body.
fn had_note_tweet(tweet: &serde_json::Value) -> bool {
    tweet
        .get("note_tweet")
        .and_then(|n| n.get("text"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
}

/// Map a batched `/tweets` response to post_id -> full text. Pure, for tests.
/// `note_tweet.text` is preferred; entries without an id or with empty text
/// are skipped so callers keep their stored text.
pub fn parse_hydrate_response(value: &serde_json::Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(tweets) = value.get("data").and_then(|d| d.as_array()) {
        for t in tweets {
            let Some(id) = t
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let text = full_text_of(t);
            if text.is_empty() {
                continue;
            }
            out.insert(id.to_string(), text);
        }
    }
    out
}

/// Resolve the quoted post `(author_username, text)` for `post` from a
/// thread response's `includes`. Returns `None` unless the post has a
/// `referenced_tweets` entry of type `quoted` whose tweet (in
/// `includes.tweets`) and author (in `includes.users`) both resolve to
/// non-empty values. Pure, for tests.
fn quoted_from_includes(post: &ThreadPost, value: &serde_json::Value) -> Option<(String, String)> {
    let quoted_id = post
        .referenced_tweets
        .iter()
        .find(|r| r.kind == "quoted")
        .map(|r| r.id.as_str())?;
    let tweets = value.get("includes")?.get("tweets")?.as_array()?;
    let quoted = tweets
        .iter()
        .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(quoted_id))?;
    let text = full_text_of(quoted);
    if text.is_empty() {
        return None;
    }
    let author_id = quoted
        .get("author_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let users = value.get("includes")?.get("users")?.as_array()?;
    let username = users
        .iter()
        .find(|u| u.get("id").and_then(|v| v.as_str()) == Some(author_id))?
        .get("username")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    Some((username.to_string(), text))
}

/// Parse a single-tweet response into the thread root plus the author's
/// username (matched by `author_id` against `includes.users`). Attaches the
/// root's quoted post from `includes`, when present. Pure, for tests.
pub fn parse_thread_root(
    value: &serde_json::Value,
) -> Result<(ThreadPost, Option<String>), String> {
    let data = value
        .get("data")
        .ok_or_else(|| "tweet response has no data".to_string())?;
    let mut root = crate::unroll::parse_post(data)
        .ok_or_else(|| "tweet response data is not a usable tweet".to_string())?;
    root.quoted = quoted_from_includes(&root, value);
    let mut username = None;
    if let Some(users) = value
        .get("includes")
        .and_then(|i| i.get("users"))
        .and_then(|u| u.as_array())
    {
        for u in users {
            let id_match = u
                .get("id")
                .and_then(|v| v.as_str())
                .is_some_and(|id| id == root.author_id);
            if id_match {
                if let Some(name) = u
                    .get("username")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    username = Some(name.to_string());
                    break;
                }
            }
        }
    }
    Ok((root, username))
}

/// Parse a search response (recent or archive) into candidate thread posts,
/// skipping entries without an id or author. Attaches each post's quoted
/// post from `includes`, when present. Pure, for tests.
pub fn parse_thread_search(value: &serde_json::Value) -> Vec<ThreadPost> {
    value
        .get("data")
        .and_then(|d| d.as_array())
        .map(|tweets| {
            tweets
                .iter()
                .filter_map(crate::unroll::parse_post)
                .map(|mut p| {
                    p.quoted = quoted_from_includes(&p, value);
                    p
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One fetched thread: the unrolled author chain (`posts[0]` is the root).
/// `search_ok` is false when the conversation search failed or was
/// impossible and only the root post is present; `search_note` then holds
/// the failure (HTTP status, never tokens) for the decision log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadFetch {
    pub root: ThreadPost,
    pub username: Option<String>,
    pub posts: Vec<ThreadPost>,
    pub search_ok: bool,
    pub search_note: Option<String>,
}

/// Error text shown when a paid call is blocked by `ANCIENTOS_ALLOW_PAID`.
fn paid_error() -> String {
    "paid calls are disabled (set ANCIENTOS_ALLOW_PAID=1 to enable)".to_string()
}

/// Fail closed unless `ANCIENTOS_ALLOW_PAID=1`.
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

/// Expanded link URLs from a tweet's `entities.urls` (`expanded_url`).
/// Pure, for tests. Keeps trimmed http(s) URLs up to 2048 chars, deduped in
/// order; non-http(s) values and overlong values are dropped (fail
/// closed). Link fetching in [`crate::link_context`] applies the rest of the
/// skip list (unexpanded `t.co`, X status links) and re-validates every URL
/// before use.
pub fn expanded_urls(tweet: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let urls = tweet
        .get("entities")
        .and_then(|e| e.get("urls"))
        .and_then(|u| u.as_array());
    for u in urls.into_iter().flatten() {
        let Some(raw) = u.get("expanded_url").and_then(|v| v.as_str()) else {
            continue;
        };
        let s = raw.trim();
        if s.is_empty() || s.len() > 2048 {
            continue;
        }
        if !(s.starts_with("http://") || s.starts_with("https://")) {
            continue;
        }
        if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            continue;
        }
        if !out.contains(&s.to_string()) {
            out.push(s.to_string());
        }
    }
    out
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
    let media_index = crate::media::index_includes(value);
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
                text: full_text_of(t),
                author_username: author.map(str::to_string),
                created_at: t
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                folders: None,
                label: None,
                had_note_tweet: had_note_tweet(t),
                media: crate::media::for_tweet(t, &media_index),
                lang: t
                    .get("lang")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string),
                urls: expanded_urls(t),
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

/// GET a URL with a bearer token and parse the JSON body.
fn get_json(url: &str, access_token: &str) -> Result<serde_json::Value, String> {
    ureq::get(url)
        .timeout(std::time::Duration::from_secs(30))
        .set("Authorization", &format!("Bearer {access_token}"))
        .call()
        .map_err(map_ureq_error)?
        .into_json()
        .map_err(|e| format!("bad X API response: {e}"))
}

/// Map a ureq error to a short message without leaking secrets.
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

    /// Refresh the access token if it is near expiry.
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

    /// Authenticated user's id (cached after first lookup).
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

    /// Fetch full post text for up to 100 ids with ONE batched GET to
    /// `/tweets`. Same token handling/refresh as [`fetch`](Self::fetch);
    /// fails closed without the paid switch. Returns post_id -> full text
    /// with `note_tweet.text` preferred.
    pub fn hydrate(&mut self, ids: &[String]) -> Result<HashMap<String, String>, String> {
        require_paid()?;
        let url = hydrate_url(ids)?;
        self.ensure_fresh_token()?;
        let raw = get_json(&url, &self.tokens.access_token.clone())?;
        Ok(parse_hydrate_response(&raw))
    }

    /// Fetch one tweet as a thread root plus the author's username.
    pub fn fetch_root(&mut self, id: &str) -> Result<(ThreadPost, Option<String>), String> {
        require_paid()?;
        let url = thread_root_url(id)?;
        self.ensure_fresh_token()?;
        let raw = get_json(&url, &self.tokens.access_token.clone())?;
        parse_thread_root(&raw)
    }

    /// Run one conversation search at `url`: fetch, parse candidates (with
    /// quoted posts), and unroll the author's self-reply chain. Returns the
    /// chain plus the raw candidate count, so callers can tell "zero
    /// matches" from "matches that did not chain".
    fn run_search(
        &mut self,
        url: &str,
        root: &ThreadPost,
    ) -> Result<(Vec<ThreadPost>, usize), String> {
        self.ensure_fresh_token()?;
        let raw = get_json(url, &self.tokens.access_token.clone())?;
        let candidates = parse_thread_search(&raw);
        let count = candidates.len();
        Ok((crate::unroll::unroll(root, &candidates), count))
    }

    /// Recent search plus ONE full-archive fallback when recent returns zero
    /// matches. Never more than 2 search calls. Any search failure yields the
    /// root post only with `search_ok` false and the error (HTTP status,
    /// never tokens) in `search_note`.
    fn search_with_fallback(
        &mut self,
        root: &ThreadPost,
        username: &str,
    ) -> (Vec<ThreadPost>, bool, Option<String>) {
        let root_only = |note: String| (vec![root.clone()], false, Some(note));
        let cid = match root.conversation_id.as_deref() {
            Some(c) => c,
            None => return root_only("root post has no conversation_id".to_string()),
        };
        let recent_url = match thread_search_url(cid, username) {
            Ok(u) => u,
            Err(e) => return root_only(e),
        };
        let (posts, count) = match self.run_search(&recent_url, root) {
            Ok(r) => r,
            Err(e) => return root_only(e),
        };
        if !needs_archive_fallback(count) {
            return (posts, true, None);
        }
        // Zero matches: recent search only covers 7 days, so try the full
        // archive once with the same query.
        let all_url = match thread_search_all_url(cid, username) {
            Ok(u) => u,
            Err(e) => return root_only(e),
        };
        match self.run_search(&all_url, root) {
            Ok((posts, _)) => (posts, true, None),
            Err(e) => root_only(e),
        }
    }

    /// Fetch one thread: the root tweet, then up to TWO conversation
    /// searches (recent plus one full-archive fallback on zero matches).
    /// The root fetch failing is an error; the search failing yields the
    /// root post only with `search_ok` false and `search_note` set.
    pub fn fetch_thread(&mut self, id: &str) -> Result<ThreadFetch, String> {
        let (root, username) = self.fetch_root(id)?;
        let (posts, search_ok, search_note) = match username.clone() {
            Some(user) => self.search_with_fallback(&root, &user),
            None => (
                vec![root.clone()],
                false,
                Some("root author username unknown; search skipped".to_string()),
            ),
        };
        Ok(ThreadFetch {
            root,
            username,
            posts,
            search_ok,
            search_note,
        })
    }
}

/// Fetch full post text for up to 100 ids with ONE batched GET to
/// `https://api.x.com/2/tweets?ids=...&tweet.fields=note_tweet,text`.
/// Loads tokens from the store (refreshing when expired) and fails closed
/// unless `ANCIENTOS_ALLOW_PAID=1`. Returns post_id -> full text with
/// `note_tweet.text` preferred. Never logs tokens.
pub fn hydrate(ids: &[String]) -> Result<HashMap<String, String>, String> {
    require_paid()?;
    let client_id = std::env::var("X_CLIENT_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "X_CLIENT_ID not set".to_string())?;
    let mut src = XApiSource::load_from_store(client_id)?;
    src.hydrate(ids)
}

/// Build an X API source from the local token store.
fn source_from_store() -> Result<XApiSource, String> {
    require_paid()?;
    let client_id = std::env::var("X_CLIENT_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "X_CLIENT_ID not set".to_string())?;
    XApiSource::load_from_store(client_id)
}

/// Fetch one thread (root tweet plus up to TWO conversation searches:
/// recent plus one full-archive fallback on zero matches) using tokens
/// from the store. Fails closed without the paid switch. Never logs tokens.
pub fn fetch_thread(id: &str) -> Result<ThreadFetch, String> {
    source_from_store()?.fetch_thread(id)
}

/// Fetch one post read-only by id (the single-tweet endpoint with full
/// text, `note_tweet` preferred). Fails closed without the paid switch.
/// Used by `deeper` for quoted/linked posts.
pub fn fetch_post(id: &str) -> Result<ThreadPost, String> {
    let (post, _) = source_from_store()?.fetch_root(id)?;
    Ok(post)
}

/// Fetch the unrolled thread for `id`, but only when `id` is a thread root
/// (`conversation_id` equals the post id) with at least one self-reply.
/// Returns `Ok(None)` for non-roots, single posts, and search failures, so
/// callers show the root post only. At most two search calls per invocation
/// (recent plus one full-archive fallback on zero matches).
pub fn fetch_thread_if_root(id: &str) -> Result<Option<Vec<ThreadPost>>, String> {
    let mut src = source_from_store()?;
    let (root, username) = src.fetch_root(id)?;
    let is_root = root.conversation_id.as_deref() == Some(root.id.as_str());
    if !is_root {
        return Ok(None);
    }
    let Some(user) = username else {
        return Ok(None);
    };
    let (posts, search_ok, _) = src.search_with_fallback(&root, &user);
    if search_ok && posts.len() > 1 {
        Ok(Some(posts))
    } else {
        Ok(None)
    }
}

/// Liveness of one post as seen by the X API. `Unknown` is fail-closed:
/// callers must neither treat it as dead nor as verified live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostLiveness {
    Live,
    Deleted,
    Suspended,
    Protected,
    Unknown,
}

impl PostLiveness {
    /// True only for positively confirmed live posts.
    pub fn is_live(self) -> bool {
        self == Self::Live
    }

    /// True for confirmed-dead posts (deleted, suspended, protected).
    pub fn is_unavailable(self) -> bool {
        matches!(self, Self::Deleted | Self::Suspended | Self::Protected)
    }

    /// Short reason string for logs and cache (`None` for live/unknown).
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Deleted => Some("deleted"),
            Self::Suspended => Some("suspended"),
            Self::Protected => Some("protected"),
            Self::Live | Self::Unknown => None,
        }
    }
}

/// Batched liveness endpoint: same `/tweets?ids=` lookup as [`hydrate_url`]
/// (max 100 ids). Liveness needs the full response (including `errors`),
/// so callers use [`XApiSource::fetch_liveness_raw`] instead of [`hydrate`].
pub fn liveness_url(ids: &[String]) -> Result<String, String> {
    hydrate_url(ids)
}

/// Post ids named in an X API error object.
fn liveness_ids_of_error(err: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(id) = err.get("resource_id").and_then(|v| v.as_str()) {
        if !id.is_empty() {
            out.push(id.to_string());
        }
    }
    if let Some(params) = err.get("parameters") {
        for key in ["ids", "id"] {
            match params.get(key) {
                Some(serde_json::Value::Array(arr)) => {
                    for v in arr {
                        if let Some(s) = v.as_str() {
                            if !s.is_empty() {
                                out.push(s.to_string());
                            }
                        }
                    }
                }
                Some(serde_json::Value::String(s)) if !s.is_empty() => {
                    out.push(s.clone());
                }
                _ => {}
            }
        }
    }
    out
}

/// Classify an X API error as deleted, protected, or unknown liveness.
fn classify_liveness_error(err: &serde_json::Value) -> PostLiveness {
    let text = format!(
        "{} {} {}",
        err.get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        err.get("detail")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        err.get("type").and_then(|v| v.as_str()).unwrap_or_default(),
    )
    .to_lowercase();
    if text.contains("suspend") || text.contains("deactivat") {
        PostLiveness::Suspended
    } else if text.contains("protect")
        || text.contains("not-authorized")
        || text.contains("not authorized")
        || text.contains("authorization")
        || text.contains("forbidden")
        || text.contains("private")
        || text.contains("cannot see")
        || text.contains("not allowed")
    {
        PostLiveness::Protected
    } else if text.contains("not found")
        || text.contains("not-found")
        || text.contains("resource-not-found")
        || text.contains("could not find")
        || text.contains("deleted")
        || text.contains("does not exist")
        || text.contains("no such")
    {
        PostLiveness::Deleted
    } else {
        PostLiveness::Unknown
    }
}

/// Map a batched `/tweets?ids=` response to per-id liveness. Pure, for tests.
/// Ids present in `data` are live; otherwise the `errors` array is searched
/// for a matching `resource_id` / `parameters.ids` entry and classified.
/// Missing ids with no matching dead signal are `Unknown` (fail closed),
/// never `Deleted`, so partial responses cannot fake a removal.
pub fn parse_liveness_response(
    requested: &[String],
    value: &serde_json::Value,
) -> HashMap<String, PostLiveness> {
    use std::collections::HashSet;
    let mut live: HashSet<&str> = HashSet::new();
    if let Some(tweets) = value.get("data").and_then(|d| d.as_array()) {
        for t in tweets {
            if let Some(id) = t.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    live.insert(id);
                }
            }
        }
    }
    let mut errors: HashMap<String, PostLiveness> = HashMap::new();
    if let Some(list) = value.get("errors").and_then(|e| e.as_array()) {
        for err in list {
            let status = classify_liveness_error(err);
            for id in liveness_ids_of_error(err) {
                errors.entry(id).or_insert(status);
            }
        }
    }
    requested
        .iter()
        .map(|id| {
            let status = if live.contains(id.as_str()) {
                PostLiveness::Live
            } else {
                errors.get(id).copied().unwrap_or(PostLiveness::Unknown)
            };
            (id.clone(), status)
        })
        .collect()
}

impl XApiSource {
    /// Fetch the raw batched `/tweets?ids=` response (including `errors`)
    /// for up to 100 ids. Same token handling/refresh as [`hydrate`](Self::hydrate);
    /// fails closed without the paid switch. Callers map it with
    /// [`parse_liveness_response`]; transport/HTTP failures are `Err` and
    /// must be treated as unknown, never dead.
    pub fn fetch_liveness_raw(&mut self, ids: &[String]) -> Result<serde_json::Value, String> {
        require_paid()?;
        let url = liveness_url(ids)?;
        self.ensure_fresh_token()?;
        get_json(&url, &self.tokens.access_token.clone())
    }
}

/// Fetch the raw batched liveness response using tokens from the store.
/// Fails closed without the paid switch. Never logs tokens.
pub fn fetch_liveness_raw(ids: &[String]) -> Result<serde_json::Value, String> {
    source_from_store()?.fetch_liveness_raw(ids)
}

/// Batched liveness check for up to 100 ids: one GET plus
/// [`parse_liveness_response`]. Fails closed without the paid switch;
/// transport/HTTP failures are `Err` (unknown, never dead).
pub fn check_liveness(ids: &[String]) -> Result<HashMap<String, PostLiveness>, String> {
    let raw = fetch_liveness_raw(ids)?;
    Ok(parse_liveness_response(ids, &raw))
}

impl BookmarkSource for XApiSource {
    /// Fetch one page of bookmarks from the X API (paid, gated).
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

    /// Test helper: env lock.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::lock_env()
    }

    /// Test helper: tokens.
    fn tokens() -> StoredTokens {
        StoredTokens::new(
            "access".into(),
            "refresh".into(),
            1_800_000_000,
            "tweet.read".into(),
        )
    }

    #[test]
    fn bookmarks_url_requests_lang_field() {
        let u = bookmarks_url("123");
        assert!(u.contains("tweet.fields="), "{u}");
        assert!(u.contains(",lang,"), "{u}");
    }

    #[test]
    fn parses_bookmarks_page_stores_lang() {
        let raw = serde_json::json!({
            "data": [
                {"id": "1", "text": "hello", "author_id": "u1", "lang": "en"},
                {"id": "2", "text": "hola", "author_id": "u1", "lang": "es"},
                {"id": "3", "text": "no lang here", "author_id": "u1"},
                {"id": "4", "text": "blank lang", "author_id": "u1", "lang": "  "}
            ],
            "includes": {"users": [{"id": "u1", "username": "alice"}]},
            "meta": {}
        });
        let page = parse_bookmarks_page(&raw).unwrap();
        assert_eq!(page.items.len(), 4);
        assert_eq!(page.items[0].lang.as_deref(), Some("en"));
        assert_eq!(page.items[1].lang.as_deref(), Some("es"));
        assert_eq!(page.items[2].lang, None);
        assert_eq!(page.items[3].lang, None);
    }

    #[test]
    fn parses_bookmarks_page_stores_expanded_urls() {
        let raw = serde_json::json!({
            "data": [
                {"id": "1", "text": "synthetic with links", "author_id": "u1",
                 "entities": {"urls": [
                    {"url": "https://t.co/a", "expanded_url": "https://example.invalid/guide"},
                    {"url": "https://t.co/b", "expanded_url": "https://example.invalid/guide"},
                    {"url": "https://t.co/c", "expanded_url": "ftp://example.invalid/nope"},
                    {"url": "https://t.co/d"},
                    {"url": "https://t.co/e", "expanded_url": "  "},
                 ]}},
                {"id": "2", "text": "synthetic no entities", "author_id": "u1"},
            ],
            "includes": {"users": [{"id": "u1", "username": "alice"}]},
            "meta": {}
        });
        let page = parse_bookmarks_page(&raw).unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(
            page.items[0].urls,
            vec!["https://example.invalid/guide".to_string()]
        );
        assert!(page.items[1].urls.is_empty());
    }

    #[test]
    fn bookmarks_url_has_required_params() {
        let u = bookmarks_url("123");
        assert!(u.contains("/2/users/123/bookmarks"), "{u}");
        assert!(u.contains("max_results=100"), "{u}");
        assert!(
            u.contains("tweet.fields=note_tweet,text,created_at,author_id"),
            "{u}"
        );
        assert!(u.contains("expansions=author_id"), "{u}");
        assert!(u.contains("user.fields=username"), "{u}");
    }

    #[test]
    fn bookmarks_url_requests_media_expansion_and_fields() {
        let u = bookmarks_url("123");
        assert!(u.contains(crate::media::MEDIA_EXPANSION), "{u}");
        assert!(
            u.contains(&format!(
                "expansions=author_id,{}",
                crate::media::MEDIA_EXPANSION
            )),
            "{u}"
        );
        assert!(
            u.contains(&format!("media.fields={}", crate::media::MEDIA_FIELDS)),
            "{u}"
        );
        for field in ["type", "url", "preview_image_url", "variants", "alt_text"] {
            assert!(u.contains(field), "{field} missing in {u}");
        }
        assert!(u.contains("attachments"), "{u}");
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
    fn parses_bookmarks_page_attaches_media() {
        let raw = serde_json::json!({
            "data": [
                {"id": "m1", "text": "synthetic with media", "author_id": "u1",
                 "attachments": {"media_keys": ["3_photo", "7_video"]}},
                {"id": "m2", "text": "synthetic without media", "author_id": "u1"}
            ],
            "includes": {
                "users": [{"id": "u1", "username": "syn_user"}],
                "media": [
                    {"media_key": "3_photo", "type": "photo",
                     "url": "https://pbs.twimg.com/media/synthetic_a.jpg",
                     "alt_text": "synthetic alt"},
                    {"media_key": "7_video", "type": "video",
                     "preview_image_url": "https://pbs.twimg.com/media/synthetic_thumb.jpg",
                     "variants": [
                        {"content_type": "video/mp4", "bit_rate": 100, "url": "https://video.twimg.com/synthetic_low.mp4"},
                        {"content_type": "video/mp4", "bit_rate": 500, "url": "https://video.twimg.com/synthetic_high.mp4"}
                     ]}
                ]
            },
            "meta": {}
        });
        let page = parse_bookmarks_page(&raw).unwrap();
        assert_eq!(page.items.len(), 2);
        let media = &page.items[0].media;
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].kind, "photo");
        assert_eq!(
            media[0].url.as_deref(),
            Some("https://pbs.twimg.com/media/synthetic_a.jpg")
        );
        assert_eq!(media[1].kind, "video");
        assert_eq!(
            media[1].video_url.as_deref(),
            Some("https://video.twimg.com/synthetic_high.mp4")
        );
        assert!(page.items[1].media.is_empty());
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

    #[test]
    fn prefers_note_tweet_text_in_bookmarks_parsing() {
        let raw = serde_json::json!({
            "data": [
                {"id": "10", "text": "truncated… https://t.co/abc",
                 "note_tweet": {"text": "synthetic full long-post text"},
                 "author_id": "u1"},
                {"id": "11", "text": "synthetic short text", "author_id": "u1"},
                {"id": "12", "text": "synthetic fallback",
                 "note_tweet": {"text": ""}, "author_id": "u1"}
            ],
            "includes": {"users": [{"id": "u1", "username": "syn_user"}]},
            "meta": {}
        });
        let page = parse_bookmarks_page(&raw).unwrap();
        assert_eq!(page.items.len(), 3);
        assert_eq!(page.items[0].text, "synthetic full long-post text");
        assert!(page.items[0].had_note_tweet);
        assert_eq!(page.items[1].text, "synthetic short text");
        assert!(!page.items[1].had_note_tweet);
        // Empty note_tweet.text falls back to text.
        assert_eq!(page.items[2].text, "synthetic fallback");
        assert!(!page.items[2].had_note_tweet);
    }

    #[test]
    fn hydrate_url_builds_one_batched_request() {
        let ids = vec!["1".to_string(), "2".to_string(), "3".to_string()];
        let u = hydrate_url(&ids).unwrap();
        assert!(u.starts_with("https://api.x.com/2/tweets?ids=1,2,3"), "{u}");
        assert!(u.contains("tweet.fields=note_tweet,text"), "{u}");
        assert!(hydrate_url(&[]).is_err());
        let many: Vec<String> = (0..101).map(|i| i.to_string()).collect();
        assert!(hydrate_url(&many).is_err());
        assert!(hydrate_url(&["".to_string()]).is_err());
        assert!(hydrate_url(&["a,b".to_string()]).is_err());
    }

    #[test]
    fn parses_hydrate_response_preferring_note_tweet() {
        let raw = serde_json::json!({
            "data": [
                {"id": "a", "text": "short",
                 "note_tweet": {"text": "synthetic full a"}},
                {"id": "b", "text": "synthetic plain b"},
                {"id": "", "text": "no id skipped"},
                {"id": "c"},
                {"text": "missing id skipped"}
            ]
        });
        let map = parse_hydrate_response(&raw);
        assert_eq!(map.get("a").map(String::as_str), Some("synthetic full a"));
        assert_eq!(map.get("b").map(String::as_str), Some("synthetic plain b"));
        assert!(!map.contains_key("c"), "empty text skipped: {map:?}");
        assert_eq!(map.len(), 2);
        let empty = parse_hydrate_response(&serde_json::json!({"meta": {}}));
        assert!(empty.is_empty());
    }

    #[test]
    fn thread_urls_have_required_params() {
        let u = thread_root_url("123").unwrap();
        assert!(u.starts_with("https://api.x.com/2/tweets/123?"), "{u}");
        for field in [
            "conversation_id",
            "note_tweet",
            "text",
            "author_id",
            "in_reply_to_user_id",
            "referenced_tweets",
        ] {
            assert!(u.contains(field), "{field} missing in {u}");
        }
        assert!(
            u.contains("expansions=referenced_tweets.id,author_id"),
            "{u}"
        );
        assert!(u.contains("user.fields=username"), "{u}");
        assert!(thread_root_url("").is_err());
        assert!(thread_root_url("a b").is_err());
        assert!(thread_root_url("a&b").is_err());

        let s = thread_search_url("123", "syn_user").unwrap();
        assert!(
            s.starts_with("https://api.x.com/2/tweets/search/recent?"),
            "{s}"
        );
        assert!(s.contains("max_results=100"), "{s}");
        assert!(
            s.contains("query=conversation_id%3A123%20from%3Asyn_user"),
            "{s}"
        );
        for field in [
            "note_tweet",
            "text",
            "created_at",
            "in_reply_to_user_id",
            "referenced_tweets",
            "author_id",
            "conversation_id",
        ] {
            assert!(s.contains(field), "{field} missing in {s}");
        }
        assert!(
            s.contains("expansions=referenced_tweets.id,author_id"),
            "{s}"
        );
        assert!(s.contains("user.fields=username"), "{s}");
        assert!(thread_search_url("", "syn_user").is_err());
        assert!(thread_search_url("123", "").is_err());
        assert!(thread_search_url("123", "not a user").is_err());
        assert!(thread_search_url("12&3", "syn_user").is_err());
    }

    #[test]
    fn archive_search_url_shares_query_fields_and_expansions() {
        let recent = thread_search_url("123", "syn_user").unwrap();
        let all = thread_search_all_url("123", "syn_user").unwrap();
        assert!(
            all.starts_with("https://api.x.com/2/tweets/search/all?"),
            "{all}"
        );
        assert!(all.contains("max_results=100"), "{all}");
        assert!(
            all.contains("query=conversation_id%3A123%20from%3Asyn_user"),
            "{all}"
        );
        // Same fields and expansions as recent search.
        for part in [
            "tweet.fields=",
            "note_tweet",
            "text",
            "created_at",
            "author_id",
            "referenced_tweets",
            "conversation_id",
            "in_reply_to_user_id",
            "expansions=referenced_tweets.id,author_id",
            "user.fields=username",
        ] {
            assert!(all.contains(part), "{part} missing in {all}");
            assert!(recent.contains(part), "{part} missing in {recent}");
        }
        assert!(thread_search_all_url("", "syn_user").is_err());
        assert!(thread_search_all_url("123", "").is_err());
        assert!(thread_search_all_url("123", "not a user").is_err());
    }

    #[test]
    fn archive_fallback_triggers_on_zero_matches_only() {
        assert!(needs_archive_fallback(0));
        assert!(!needs_archive_fallback(1));
        assert!(!needs_archive_fallback(100));
    }

    #[test]
    fn parses_thread_root_with_matched_username() {
        let raw = serde_json::json!({
            "data": {
                "id": "100", "author_id": "u1", "conversation_id": "100",
                "text": "synthetic thread root",
                "referenced_tweets": [],
            },
            "includes": {"users": [
                {"id": "u9", "username": "syn_other"},
                {"id": "u1", "username": "syn_author"},
            ]},
        });
        let (root, username) = parse_thread_root(&raw).unwrap();
        assert_eq!(root.id, "100");
        assert_eq!(root.author_id, "u1");
        assert_eq!(root.conversation_id.as_deref(), Some("100"));
        assert_eq!(username.as_deref(), Some("syn_author"));
        // No username match: still usable, search is skipped later.
        let raw = serde_json::json!({
            "data": {"id": "100", "author_id": "u1", "text": "synthetic"},
            "includes": {"users": [{"id": "u9", "username": "syn_other"}]},
        });
        let (_, username) = parse_thread_root(&raw).unwrap();
        assert_eq!(username, None);
        assert!(parse_thread_root(&serde_json::json!({"meta": {}})).is_err());
        assert!(parse_thread_root(&serde_json::json!({"data": {"text": "x"}})).is_err());
    }

    #[test]
    fn parses_thread_search_skipping_invalid_entries() {
        let raw = serde_json::json!({
            "data": [
                {"id": "101", "author_id": "u1", "text": "synthetic reply",
                 "referenced_tweets": [{"type": "replied_to", "id": "100"}]},
                {"id": "102", "text": "missing author skipped"},
                {"text": "missing id skipped"},
            ],
            "meta": {"result_count": 3},
        });
        let posts = parse_thread_search(&raw);
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].id, "101");
        assert!(parse_thread_search(&serde_json::json!({"meta": {}})).is_empty());
    }

    #[test]
    fn parses_quoted_posts_from_includes() {
        let raw = serde_json::json!({
            "data": [
                {"id": "101", "author_id": "u1", "text": "synthetic setup 1",
                 "conversation_id": "100",
                 "referenced_tweets": [
                    {"type": "replied_to", "id": "100"},
                    {"type": "quoted", "id": "q1"},
                 ]},
                {"id": "102", "author_id": "u1", "text": "synthetic setup 2",
                 "conversation_id": "100",
                 "referenced_tweets": [{"type": "replied_to", "id": "101"}]},
            ],
            "includes": {
                "tweets": [
                    {"id": "q1", "author_id": "u9",
                     "text": "truncated… https://t.co/q",
                     "note_tweet": {"text": "synthetic quoted real content"}},
                ],
                "users": [
                    {"id": "u1", "username": "syn_author"},
                    {"id": "u9", "username": "syn_quoted"},
                ],
            },
            "meta": {"result_count": 2},
        });
        let posts = parse_thread_search(&raw);
        assert_eq!(posts.len(), 2);
        // Quoted text prefers note_tweet.text; username resolves via users.
        assert_eq!(
            posts[0].quoted,
            Some((
                "syn_quoted".to_string(),
                "synthetic quoted real content".to_string()
            ))
        );
        // No quote reference: nothing attached.
        assert_eq!(posts[1].quoted, None);
    }

    #[test]
    fn quoted_posts_fail_closed_on_missing_includes() {
        // Quote reference but no includes at all.
        let raw = serde_json::json!({
            "data": [
                {"id": "101", "author_id": "u1", "text": "synthetic",
                 "referenced_tweets": [{"type": "quoted", "id": "q1"}]},
            ],
        });
        assert_eq!(parse_thread_search(&raw)[0].quoted, None);
        // Quoted tweet id not in includes.tweets.
        let raw = serde_json::json!({
            "data": [
                {"id": "101", "author_id": "u1", "text": "synthetic",
                 "referenced_tweets": [{"type": "quoted", "id": "missing"}]},
            ],
            "includes": {
                "tweets": [{"id": "q1", "author_id": "u9", "text": "synthetic"}],
                "users": [{"id": "u9", "username": "syn_quoted"}],
            },
        });
        assert_eq!(parse_thread_search(&raw)[0].quoted, None);
        // Quoted author not in includes.users: username is never guessed.
        let raw = serde_json::json!({
            "data": [
                {"id": "101", "author_id": "u1", "text": "synthetic",
                 "referenced_tweets": [{"type": "quoted", "id": "q1"}]},
            ],
            "includes": {
                "tweets": [{"id": "q1", "author_id": "u9", "text": "synthetic"}],
                "users": [{"id": "u1", "username": "syn_author"}],
            },
        });
        assert_eq!(parse_thread_search(&raw)[0].quoted, None);
        // Empty quoted text: nothing useful to attach.
        let raw = serde_json::json!({
            "data": [
                {"id": "101", "author_id": "u1", "text": "synthetic",
                 "referenced_tweets": [{"type": "quoted", "id": "q1"}]},
            ],
            "includes": {
                "tweets": [{"id": "q1", "author_id": "u9", "text": ""}],
                "users": [{"id": "u9", "username": "syn_quoted"}],
            },
        });
        assert_eq!(parse_thread_search(&raw)[0].quoted, None);
    }

    #[test]
    fn parses_root_quoted_post_from_includes() {
        let raw = serde_json::json!({
            "data": {
                "id": "100", "author_id": "u1", "conversation_id": "100",
                "text": "synthetic thread root",
                "referenced_tweets": [{"type": "quoted", "id": "q1"}],
            },
            "includes": {
                "tweets": [
                    {"id": "q1", "author_id": "u9",
                     "text": "synthetic quoted root content"},
                ],
                "users": [
                    {"id": "u1", "username": "syn_author"},
                    {"id": "u9", "username": "syn_quoted"},
                ],
            },
        });
        let (root, username) = parse_thread_root(&raw).unwrap();
        assert_eq!(username.as_deref(), Some("syn_author"));
        assert_eq!(
            root.quoted,
            Some((
                "syn_quoted".to_string(),
                "synthetic quoted root content".to_string()
            ))
        );
    }

    #[test]
    fn thread_free_fns_fail_closed_without_paid_switch() {
        let _guard = env_lock();
        let saved_paid = std::env::var("ANCIENTOS_ALLOW_PAID").ok();
        let saved_client = std::env::var("X_CLIENT_ID").ok();
        std::env::remove_var("ANCIENTOS_ALLOW_PAID");
        std::env::set_var("X_CLIENT_ID", "synthetic-client");
        assert!(fetch_thread("123").is_err());
        assert!(fetch_thread_if_root("123").is_err());
        match saved_paid {
            Some(v) => std::env::set_var("ANCIENTOS_ALLOW_PAID", v),
            None => std::env::remove_var("ANCIENTOS_ALLOW_PAID"),
        }
        match saved_client {
            Some(v) => std::env::set_var("X_CLIENT_ID", v),
            None => std::env::remove_var("X_CLIENT_ID"),
        }
    }

    #[test]
    fn liveness_url_reuses_batched_lookup_limit() {
        let ids = vec!["1".to_string(), "2".to_string()];
        let u = liveness_url(&ids).unwrap();
        assert!(u.starts_with("https://api.x.com/2/tweets?ids=1,2"), "{u}");
        assert!(liveness_url(&[]).is_err());
        let many: Vec<String> = (0..101).map(|i| i.to_string()).collect();
        assert!(liveness_url(&many).is_err());
    }

    #[test]
    fn parses_liveness_deleted_suspended_protected_and_live() {
        let requested = vec![
            "live1".to_string(),
            "gone1".to_string(),
            "susp1".to_string(),
            "prot1".to_string(),
            "mystery1".to_string(),
        ];
        let raw = serde_json::json!({
            "data": [{"id": "live1", "text": "synthetic live"}],
            "errors": [
                {"resource_id": "gone1", "title": "Not Found Error",
                 "detail": "Could not find tweet with ids: [gone1].",
                 "type": "https://api.twitter.com/2/problems/resource-not-found"},
                {"parameters": {"ids": ["susp1"]}, "title": "Forbidden",
                 "detail": "User has been suspended.",
                 "type": "https://api.twitter.com/2/problems/suspended"},
                {"resource_id": "prot1", "title": "Authorization Error",
                 "detail": "You are not authorized to see this protected post.",
                 "type": "https://api.twitter.com/2/problems/not-authorized-for-resource"},
                {"title": "Too Many Requests", "detail": "Rate limited",
                 "type": "https://api.twitter.com/2/problems/rate-limit"},
            ],
        });
        let out = parse_liveness_response(&requested, &raw);
        assert_eq!(out.get("live1"), Some(&PostLiveness::Live));
        assert_eq!(out.get("gone1"), Some(&PostLiveness::Deleted));
        assert_eq!(out.get("susp1"), Some(&PostLiveness::Suspended));
        assert_eq!(out.get("prot1"), Some(&PostLiveness::Protected));
        // Missing with no matching dead signal is unknown, never deleted.
        assert_eq!(out.get("mystery1"), Some(&PostLiveness::Unknown));
    }

    #[test]
    fn liveness_url_is_the_read_only_batched_get() {
        // Same endpoint as hydrate: one GET to /tweets?ids=, max 100 ids.
        // No write path exists anywhere on this flow (never un-bookmark).
        let ids = vec!["1".to_string(), "2".to_string()];
        let u = liveness_url(&ids).unwrap();
        assert_eq!(u, hydrate_url(&ids).unwrap());
        assert!(u.starts_with("https://api.x.com/2/tweets?ids=1,2"), "{u}");
        assert!(liveness_url(&[]).is_err());
        let many: Vec<String> = (0..101).map(|i| i.to_string()).collect();
        assert!(liveness_url(&many).is_err());
    }

    #[test]
    fn liveness_parse_is_fail_closed_on_ambiguity() {
        // Id in data wins over a contradictory error entry.
        let raw = serde_json::json!({
            "data": [{"id": "a", "text": "synthetic live"}],
            "errors": [{"resource_id": "a", "title": "Not Found Error",
                        "detail": "Could not find tweet.",
                        "type": "https://api.twitter.com/2/problems/resource-not-found"}],
        });
        let out = parse_liveness_response(&["a".to_string()], &raw);
        assert_eq!(out.get("a"), Some(&PostLiveness::Live));

        // Dead-type error with no attributable id marks nothing dead.
        let raw = serde_json::json!({
            "errors": [{"title": "Not Found Error", "detail": "Could not find tweet.",
                        "type": "https://api.twitter.com/2/problems/resource-not-found"}],
        });
        let out = parse_liveness_response(&["b".to_string()], &raw);
        assert_eq!(out.get("b"), Some(&PostLiveness::Unknown));

        // Attributable but unrecognized error (rate limit) is unknown, not dead.
        let raw = serde_json::json!({
            "errors": [{"resource_id": "c", "title": "Too Many Requests",
                        "detail": "Rate limited",
                        "type": "https://api.twitter.com/2/problems/rate-limit"}],
        });
        let out = parse_liveness_response(&["c".to_string()], &raw);
        assert_eq!(out.get("c"), Some(&PostLiveness::Unknown));

        // Helpers classify only confirmed outcomes.
        assert!(PostLiveness::Live.is_live());
        assert!(!PostLiveness::Deleted.is_live());
        assert!(!PostLiveness::Unknown.is_live());
        assert!(PostLiveness::Suspended.is_unavailable());
        assert!(PostLiveness::Protected.is_unavailable());
        assert!(!PostLiveness::Unknown.is_unavailable());
        assert_eq!(PostLiveness::Deleted.reason(), Some("deleted"));
        assert_eq!(PostLiveness::Live.reason(), None);
        assert_eq!(PostLiveness::Unknown.reason(), None);
    }

    #[test]
    fn liveness_check_fails_closed_without_paid_switch() {
        // No network (and no X call of any kind) without the paid switch.
        let _guard = env_lock();
        let saved_paid = std::env::var("ANCIENTOS_ALLOW_PAID").ok();
        let saved_client = std::env::var("X_CLIENT_ID").ok();
        std::env::remove_var("ANCIENTOS_ALLOW_PAID");
        std::env::set_var("X_CLIENT_ID", "synthetic-client");
        let ids = vec!["1".to_string()];
        assert!(check_liveness(&ids).is_err());
        assert!(fetch_liveness_raw(&ids).is_err());
        match saved_paid {
            Some(v) => std::env::set_var("ANCIENTOS_ALLOW_PAID", v),
            None => std::env::remove_var("ANCIENTOS_ALLOW_PAID"),
        }
        match saved_client {
            Some(v) => std::env::set_var("X_CLIENT_ID", v),
            None => std::env::remove_var("X_CLIENT_ID"),
        }
    }

    #[test]
    fn hydrate_free_fn_fails_closed_without_paid_switch() {
        let _guard = env_lock();
        let saved_paid = std::env::var("ANCIENTOS_ALLOW_PAID").ok();
        let saved_home = std::env::var("ANCIENTOS_HOME").ok();
        let saved_client = std::env::var("X_CLIENT_ID").ok();
        std::env::remove_var("ANCIENTOS_ALLOW_PAID");
        std::env::set_var("X_CLIENT_ID", "synthetic-client");
        let ids = vec!["1".to_string()];
        assert!(hydrate(&ids).is_err());
        match saved_paid {
            Some(v) => std::env::set_var("ANCIENTOS_ALLOW_PAID", v),
            None => std::env::remove_var("ANCIENTOS_ALLOW_PAID"),
        }
        match saved_home {
            Some(v) => std::env::set_var("ANCIENTOS_HOME", v),
            None => std::env::remove_var("ANCIENTOS_HOME"),
        }
        match saved_client {
            Some(v) => std::env::set_var("X_CLIENT_ID", v),
            None => std::env::remove_var("X_CLIENT_ID"),
        }
    }
}
