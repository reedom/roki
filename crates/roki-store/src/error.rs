use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("open store at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("migration {version} ({name}) failed: {source}")]
    Migration {
        version: i64,
        name: &'static str,
        #[source]
        source: rusqlite::Error,
    },

    #[error("schema is at version {found}, newer than this binary supports ({max_supported})")]
    SchemaTooNew { found: i64, max_supported: i64 },

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("serialize payload: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("row not found")]
    NotFound,
}

pub type Result<T> = std::result::Result<T, Error>;
