//! What can go wrong while reading a PDF, with the file's own answer where there is one.

use std::path::PathBuf;

/// A read failure. Every variant is a message a user can act on; nothing in this crate panics.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("non è un file PDF: nessuna intestazione %PDF- nei primi 1024 byte")]
    NotPdf,
    #[error("il file è protetto da una password: la decrittazione non è supportata")]
    Encrypted,
    #[error("la struttura del PDF è corrotta: {0}")]
    Corrupt(String),
    #[error("il file supera i limiti di sicurezza: {0}")]
    Limit(String),
    #[error("il lavoro è stato annullato")]
    Cancelled,
    /// Something the person asked for cannot be done: an impossible cut plan, a name that is
    /// already taken, a page that is not in the document.
    #[error("{0}")]
    Invalid(String),
}

impl Error {
    /// An I/O error carrying the path it happened on.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io { path: path.into(), source }
    }
}
