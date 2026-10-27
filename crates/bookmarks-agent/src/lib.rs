//! X bookmarks agent: read-only OAuth2 PKCE, incremental ingest, JSONL fixtures.
pub mod author_filter;
pub mod brief;
pub mod callback;
pub mod deeper;
pub mod delegate;
pub mod digest;
pub mod fixture;
pub mod idea_card;
pub mod ingest;
pub mod language;
pub mod liveness;
pub mod media;
pub mod merge;
pub mod oauth;
pub mod sanitize;
#[cfg(test)]
pub(crate) mod test_support;
pub mod token_store;
pub mod translate;
pub mod try_it;
pub mod unroll;
pub mod vote;
pub mod x_api;
