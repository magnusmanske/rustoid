//! Error type for the comparison harness.

use std::path::PathBuf;

/// Errors from fetching, caching, or comparing wiki content.
#[derive(Debug, thiserror::Error)]
pub enum CompareError {
    /// A cache read/write failed.
    #[error("cache error at {path}: {message}")]
    Cache { path: PathBuf, message: String },

    /// An HTTP request failed, or returned a non-success status.
    ///
    /// `status` is the server's status when it answered with one, and `0` when
    /// the request never completed. A 404 is a real answer for some endpoints
    /// (a missing Wikidata entity, say), so callers must be able to tell it from
    /// a transport failure.
    #[error("http error for {url}: {message}")]
    Http {
        url: String,
        message: String,
        status: u16,
    },

    /// The wiki returned a body we could not parse.
    #[error("malformed response from {url}: {message}")]
    Response { url: String, message: String },

    /// The page does not exist on that wiki.
    #[error("page not found: {title}")]
    NotFound { title: String },

    /// rustoid failed to parse the wikitext.
    #[error("parse error: {0}")]
    Parse(String),

    /// Something was needed that is neither cached nor reachable because the
    /// run was explicitly offline.
    #[error("offline: {0}")]
    Offline(String),

    /// A corpus file could not be read or did not parse.
    #[error("corpus error: {0}")]
    Corpus(String),
}

impl CompareError {
    pub(crate) fn cache(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Cache {
            path: path.into(),
            message: message.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, CompareError>;
