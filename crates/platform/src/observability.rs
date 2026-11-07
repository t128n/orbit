use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TracingFormat {
    Compact,
    Json,
}

/// Installs local logging without configuring any external telemetry exporter.
///
/// Writes to `log_file` (created, appended to) when given, else to stdout. A file is required
/// to see logs at all when Orbit runs as a Windows Service: the service has no console, so
/// anything written to stdout goes nowhere. Falls back to stdout when the file cannot be opened.
pub fn init_tracing(
    format: TracingFormat,
    log_file: Option<&Path>,
) -> Result<(), tracing_subscriber::util::TryInitError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file = log_file.and_then(open_log_file);
    match (format, file) {
        (TracingFormat::Compact, Some(file)) => tracing_subscriber::fmt()
            .compact()
            .with_writer(Mutex::new(file))
            .with_env_filter(filter)
            .finish()
            .try_init(),
        (TracingFormat::Compact, None) => tracing_subscriber::fmt()
            .compact()
            .with_env_filter(filter)
            .finish()
            .try_init(),
        (TracingFormat::Json, Some(file)) => tracing_subscriber::fmt()
            .json()
            .with_writer(Mutex::new(file))
            .with_env_filter(filter)
            .finish()
            .try_init(),
        (TracingFormat::Json, None) => tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .finish()
            .try_init(),
    }
}

fn open_log_file(path: &Path) -> Option<fs::File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).ok()?;
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}
