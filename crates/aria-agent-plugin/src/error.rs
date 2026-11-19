//! Assembly failures.
//!
//! Every variant is a **startup** error: the runtime refuses to boot on an
//! inconsistent plugin graph rather than degrading at request time (ADR-0014
//! §fail-closed).

use thiserror::Error;

/// Result alias for plugin assembly.
pub type PluginResult<T> = Result<T, PluginError>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PluginError {
    #[error("plugin id is empty")]
    EmptyPluginId,
    #[error("duplicate plugin id: {id}")]
    DuplicatePlugin { id: String },
    #[error("plugin `{id}` declares seam `{seam}` twice")]
    DuplicateManifestSeam { id: String, seam: &'static str },
    #[error("seam `{seam}` already has a provider named `{name}`")]
    DuplicateProvider {
        seam: &'static str,
        name: &'static str,
    },
    #[error("plugin `{plugin}` requires seam `{seam}` but no provider is registered")]
    MissingDependency { plugin: String, seam: &'static str },
    #[error("dependency cycle between plugins: {chain}")]
    Cycle { chain: String },
    #[error("core seam `{seam}` has {count} providers ({providers}); exactly one is allowed")]
    CoreConflict {
        seam: &'static str,
        count: usize,
        providers: String,
    },
    #[error("no provider registered for seam `{seam}`")]
    NoProvider { seam: &'static str },
    #[error("seam `{seam}` is a service, not a swappable provider slot")]
    ServiceIsNotAProvider { seam: &'static str },
    #[error("seam `{seam}` has no default provider")]
    MissingDefault { seam: &'static str },
    #[error("seam `{seam}` provider `{name}` has the wrong capability type")]
    TypeMismatch {
        seam: &'static str,
        name: &'static str,
    },
    #[error("plugin `{plugin}` declared seam `{seam}` but installed no provider for it")]
    UnfulfilledProvide { plugin: String, seam: &'static str },
    #[error("unknown seam: {seam}")]
    UnknownSeam { seam: String },
    #[error("invalid plugin manifest: {0}")]
    Manifest(String),
}
