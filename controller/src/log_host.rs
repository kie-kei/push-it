//! `wasi:logging@0.1.0-draft` host function.
//!
//! Components call `log(level, context, message)`; the host appends a
//! structured line to the same per-app `LogStore` that captures stdout, so
//! `pushit logs` shows both.

use std::sync::Arc;

use crate::logs::LogStore;
use crate::runtime::ComponentCtx;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../wit/logging",
        world: "wasi:logging/logging-host",
        async: true,
    });
}

use wasmtime::component::HasSelf;

/// Everything a component invocation needs for wasi:logging.
pub struct LogCtx {
    /// App name — selects the per-app log file.
    pub app: String,
    /// Shared sink owned by `AppState`.
    pub logs: Arc<LogStore>,
}

impl bindings::wasi::logging::logging::Host for ComponentCtx {
    async fn log(
        &mut self,
        level: bindings::wasi::logging::logging::Level,
        context: String,
        message: String,
    ) {
        use bindings::wasi::logging::logging::Level::*;
        let lvl = match level {
            Trace => "TRACE",
            Debug => "DEBUG",
            Info => "INFO",
            Warn => "WARN",
            Error => "ERROR",
            Critical => "CRITICAL",
        };
        let line = format!("[{lvl}] [{context}] {message}\n");
        let _ = self.log.logs.append(&self.log.app, line.as_bytes());
        // LogStore::append prefixes its own timestamp before the line.
    }
}

/// Register wasi:logging imports on the linker.
pub fn add_to_linker(linker: &mut wasmtime::component::Linker<ComponentCtx>) -> anyhow::Result<()> {
    bindings::wasi::logging::logging::add_to_linker::<_, HasSelf<_>>(linker, |d| d)?;
    Ok(())
}
