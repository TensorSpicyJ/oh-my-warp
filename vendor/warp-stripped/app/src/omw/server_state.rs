//! omw-server launcher state — process-wide singleton.
//!
//! Starts an embedded `omw-server` axum HTTP server on `127.0.0.1:8788` so the
//! GUI agent panel and local client can reach the session registry and agent
//! endpoints without any cloud dependency.
//!
//! The server runs on its own dedicated tokio runtime in a background thread,
//! following the same pattern as [`super::remote_state::OmwRemoteState`].
//!
//! v0.3 (wiring 1): minimum viable — spawn the server on app launch, keep it
//! alive for the lifetime of the process, no stop/toggle needed.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::thread;

use parking_lot::Mutex;
use tokio::runtime::Builder;
use tokio::task::JoinHandle;

/// Default bind for the embedded server. Loopback-only — no external exposure.
const DEFAULT_BIND: &str = "127.0.0.1:8788";

pub struct OmwServerState {
    inner: Mutex<Inner>,
}

struct Inner {
    serve_task: Option<JoinHandle<()>>,
    registry: Option<Arc<omw_server::SessionRegistry>>,
    runtime_handle: Option<tokio::runtime::Handle>,
    _runtime_thread: Option<thread::JoinHandle<()>>,
}

static SHARED: OnceLock<Arc<OmwServerState>> = OnceLock::new();

impl OmwServerState {
    /// Process-wide accessor. Lazily constructs on first call.
    pub fn shared() -> Arc<Self> {
        SHARED
            .get_or_init(|| {
                Arc::new(Self {
                    inner: Mutex::new(Inner {
                        serve_task: None,
                        registry: None,
                        runtime_handle: None,
                        _runtime_thread: None,
                    }),
                })
            })
            .clone()
    }

    /// Start the server if not already running. Idempotent.
    pub fn start(&self) -> Result<(), String> {
        let mut g = self.inner.lock();
        if g.serve_task.is_some() {
            return Ok(());
        }

        let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel::<io::Result<tokio::runtime::Handle>>(1);
        let thread_handle = thread::Builder::new()
            .name("omw-server-rt".into())
            .spawn(move || {
                let rt = match Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(2)
                    .thread_name("omw-server-worker")
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = handle_tx.send(Err(io::Error::new(io::ErrorKind::Other, e)));
                        return;
                    }
                };
                let h = rt.handle().clone();
                let _ = handle_tx.send(Ok(h.clone()));
                rt.block_on(std::future::pending::<()>());
            })
            .map_err(|e| format!("spawning omw-server-rt thread: {e}"))?;

        let handle = handle_rx
            .recv()
            .map_err(|e| format!("runtime handle channel closed: {e}"))?
            .map_err(|e| format!("building tokio runtime: {e}"))?;

        let registry = omw_server::SessionRegistry::new();
        let registry_for_task = registry.clone();

        let bind: SocketAddr = DEFAULT_BIND
            .parse()
            .map_err(|e| format!("parsing bind addr {DEFAULT_BIND}: {e}"))?;

        let serve_task = handle.spawn(async move {
            match omw_server::serve(registry_for_task, bind).await {
                Ok(()) => {}
                Err(e) => eprintln!("omw-server: serve loop ended with error: {e}"),
            }
        });

        eprintln!("omw-server listening on {DEFAULT_BIND}");

        g.serve_task = Some(serve_task);
        g.registry = Some(registry);
        g.runtime_handle = Some(handle);
        g._runtime_thread = Some(thread_handle);
        Ok(())
    }

    /// Idempotent convenience: ensures the server is running. Called early in
    /// app startup so the agent panel doesn't need to manage server lifecycle.
    pub fn ensure_running() {
        if let Err(e) = Self::shared().start() {
            eprintln!("omw-server: failed to start: {e}");
        }
    }

    /// Returns the live session registry, when the server is running.
    #[allow(dead_code)]
    pub fn registry(&self) -> Option<Arc<omw_server::SessionRegistry>> {
        self.inner.lock().registry.clone()
    }

    /// Returns the server's tokio runtime handle, when one has been spun up.
    #[allow(dead_code)]
    pub fn runtime_handle(&self) -> Option<tokio::runtime::Handle> {
        self.inner.lock().runtime_handle.clone()
    }
}
