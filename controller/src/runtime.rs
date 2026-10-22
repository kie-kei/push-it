use anyhow::{bail, Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::p2::{IoView, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::bindings::ProxyPre;
use wasmtime_wasi_http::body::HyperOutgoingBody;
use wasmtime_wasi_http::types::HostFutureIncomingResponse;
use wasmtime_wasi_http::bindings::http::types::Scheme;
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpView};
use crate::blob_host::{self, BlobCtx};
use crate::log_host::{self, LogCtx};
use crate::sql_host::{self, SqlCtx};

/// Host capabilities for one component invocation, resolved by the caller
/// (manifest grants + resource maps + the app log sink).
pub struct HostCaps {
    pub allow_http_out: bool,
    pub sql_allowed: Vec<String>,
    pub sql_dsns: HashMap<String, String>,
    pub blob_allowed: Vec<String>,
    pub blob_stores: HashMap<String, blob_host::BlobEntry>,
    pub log: LogCtx,
}

pub struct ComponentCtx {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    allow_http_out: bool,
    pub sql: SqlCtx,
    pub blob: BlobCtx,
    pub log: LogCtx,
}

impl IoView for ComponentCtx {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}
impl WasiView for ComponentCtx {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
}
impl WasiHttpView for ComponentCtx {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.http
    }

    fn send_request(
        &mut self,
        request: hyper::Request<HyperOutgoingBody>,
        config: wasmtime_wasi_http::types::OutgoingRequestConfig,
    ) -> wasmtime_wasi_http::HttpResult<HostFutureIncomingResponse> {
        if !self.allow_http_out {
            return Err(wasmtime_wasi_http::bindings::http::types::ErrorCode::HttpRequestDenied
                .into());
        }
        Ok(wasmtime_wasi_http::types::default_send_request(request, config))
    }
}

/// Wasmtime host: one engine, cached `ProxyPre` per artifact dir.
pub struct Runtime {
    engine: Engine,
    /// (revision-dir, connection-name) → backend pool, shared across calls.
    sql_pools: Arc<Mutex<HashMap<(PathBuf, String), Arc<sql_host::Backend>>>>,
    pre: Mutex<HashMap<PathBuf, Arc<ProxyPre<ComponentCtx>>>>,
    pub timeout: Duration,
}

pub struct CallResult {
    pub response: Option<hyper::Response<HyperOutgoingBody>>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Runtime {
    pub fn new(timeout: Duration) -> Result<Self> {
        let mut cfg = Config::new();
        cfg.async_support(true).epoch_interruption(true);
        let engine = Engine::new(&cfg)?;
        // epoch ticker: 10ms ticks; deadline set in ticks per request
        let eng = engine.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(10));
            eng.increment_epoch();
        });
        sqlx::any::install_default_drivers();
        Ok(Runtime {
            engine,
            sql_pools: Arc::new(Mutex::new(HashMap::new())),
            pre: Mutex::new(HashMap::new()),
            timeout,
        })
    }

    /// Compile + pre-instantiate; used at deploy to validate the component
    /// exports wasi:http/incoming-handler.
    pub fn load(&self, dir: &Path) -> Result<Arc<ProxyPre<ComponentCtx>>> {
        let dir = dir.to_path_buf();
        if let Some(p) = self.pre.lock().get(&dir) {
            return Ok(p.clone());
        }
        let wasm = dir.join("app.wasm");
        let component = Component::from_file(&self.engine, &wasm)
            .with_context(|| format!("compiling {}", wasm.display()))?;
        let mut linker: Linker<ComponentCtx> = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::add_only_http_to_linker_async(&mut linker)?;
        sql_host::add_to_linker(&mut linker)?;
        blob_host::add_to_linker(&mut linker)?;
        log_host::add_to_linker(&mut linker)?;
        let pre = linker
            .instantiate_pre(&component)
            .context("linking component (missing/unresolved imports)")?;
        let pre = ProxyPre::new(pre)
            .context("component does not export wasi:http/incoming-handler@0.2")?;
        let pre = Arc::new(pre);
        self.pre.lock().insert(dir, pre.clone());
        Ok(pre)
    }

    /// Invoke incoming-handler for one request. `env` is manifest env +
    /// secrets (already resolved).
    pub async fn call(
        &self,
        dir: &Path,
        env: Vec<(String, String)>,
        caps: HostCaps,
        req: hyper::Request<Full<Bytes>>,
    ) -> Result<CallResult> {
        let pre = self.load(dir)?;
        let stdout = MemoryOutputPipe::new(1 << 20);
        let stderr = MemoryOutputPipe::new(1 << 20);
        let mut wb = WasiCtxBuilder::new();
        wb.envs(&env)
            .stdout(stdout.clone())
            .stderr(stderr.clone())
            .allow_ip_name_lookup(caps.allow_http_out);
        let mut store = Store::new(
            &self.engine,
            ComponentCtx {
                table: ResourceTable::new(),
                wasi: wb.build(),
                http: WasiHttpCtx::new(),
                allow_http_out: caps.allow_http_out,
                sql: SqlCtx {
                    dir: dir.to_path_buf(),
                    allowed: caps.sql_allowed,
                    dsns: caps.sql_dsns,
                    pools: self.sql_pools.clone(),
                },
                blob: BlobCtx {
                    allowed: caps.blob_allowed,
                    stores: caps.blob_stores,
                },
                log: caps.log,
            },
        );
        // epoch deadline: 10ms ticks
        store.epoch_deadline_trap();
        store.set_epoch_deadline((self.timeout.as_millis() / 10) as u64);

        let (tx, rx) = tokio::sync::oneshot::channel();
        let req = store
            .data_mut()
            .new_incoming_request(Scheme::Http, req.map(|b| b.map_err(|e| match e {})))?;
        let out = store.data_mut().new_response_outparam(tx)?;

        let task = tokio::task::spawn(async move {
            let proxy = pre.instantiate_async(&mut store).await?;
            proxy
                .wasi_http_incoming_handler()
                .call_handle(store, req, out)
                .await?;
            anyhow::Ok(())
        });

        let response = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(Ok(resp))) => Some(resp),
            Ok(Ok(Err(e))) => bail!("guest response error: {e}"),
            Ok(Err(_)) => {
                let e = match task.await {
                    Ok(Ok(())) => anyhow::anyhow!("guest never set response-outparam"),
                    Ok(Err(e)) => e,
                    Err(e) => e.into(),
                };
                bail!("handler failed: {e:#}");
            }
            Err(_) => bail!("handler timed out after {:?}", self.timeout),
        };

        // wait briefly for the handler task to finish writing logs
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        Ok(CallResult {
            response,
            stdout: stdout.contents().to_vec(),
            stderr: stderr.contents().to_vec(),
        })
    }
}
