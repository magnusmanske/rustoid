/// Unified error type for the rustoid parser.
use thiserror::Error;

/// Alias for results using our error type.
pub type Result<T> = std::result::Result<T, RustoidError>;

/// Top-level error variants for the parser.
#[derive(Error, Debug)]
pub enum RustoidError {
    /// I/O or network error when fetching data from a source.
    #[error("data source error: {0}")]
    DataSource(String),

    /// A page or template was not found.
    #[error("page not found: {0}")]
    NotFound(String),

    /// A template exceeded the maximum expansion depth (likely a self-referencing loop).
    #[error("template expansion depth exceeded at `{0}`")]
    RecursionDepthExceeded(String),

    /// The parser encountered unexpected or unparseable wikitext.
    #[error("parse error: {0}")]
    Parse(String),

    /// A Lua/Scribunto runtime error.
    #[error("lua error: {0}")]
    Lua(String),

    /// A Lua script exceeded its execution timeout.
    #[error("lua timeout: {0}")]
    LuaTimeout(String),

    /// Invalid or unsupported parser options.
    #[error("invalid options: {0}")]
    InvalidOptions(String),

    /// An unsupported feature was requested.
    #[error("unsupported feature: {0}")]
    Unsupported(String),

    /// Catch-all for internal errors that should not occur.
    #[error("internal error: {0}")]
    Internal(String),
}

impl RustoidError {
    /// Returns `true` if this error represents a transient condition (e.g., network error)
    /// that may succeed on retry.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            RustoidError::DataSource(_) | RustoidError::LuaTimeout(_)
        )
    }
}

impl From<mlua::Error> for RustoidError {
    fn from(e: mlua::Error) -> Self {
        RustoidError::Lua(e.to_string())
    }
}
