//! Error type for the comparison harness.

use std::path::PathBuf;

/// Errors from fetching, caching, or comparing wiki content.
#[derive(Debug, thiserror::Error)]
pub enum CompareError {
    /// A cache read/write failed.
    #[error("cache error at {path}: {message}")]
    Cache { path: PathBuf, message: String },

    /// An HTTP request failed, or returned a non-success status.
    #[error("http error for {url}: {message}")]
    Http { url: String, message: String },

    /// The wiki returned a body we could not parse.
    #[error("malformed response from {url}: {message}")]
    Response { url: String, message: String },

    /// The page does not exist on that wiki.
    #[error("page not found: {title}")]
    NotFound { title: String },

    /// rustoid failed to parse the wikitext.
    #[error("parse error: {0}")]
    Parse(String),
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
