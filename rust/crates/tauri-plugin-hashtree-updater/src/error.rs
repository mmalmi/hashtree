use serde::{Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Updater(#[from] hashtree_updater::UpdateError),
    #[error("hashtree resolver: {0}")]
    Resolver(#[from] hashtree_resolver::ResolverError),
    #[error("plugin not configured: {0}")]
    Config(String),
    #[error("install destination missing for kind {0}")]
    MissingDestination(String),
    #[error(transparent)]
    Tauri(#[from] tauri::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Serialize for Error {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
