//! Structured logging bootstrap for the binary and library entry points.

use tracing_subscriber::EnvFilter;

/// Install the global tracing subscriber (idempotent enough for one-shot main).
pub fn init() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
