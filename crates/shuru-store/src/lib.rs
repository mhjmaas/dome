mod backend;
pub mod cas;
mod nbd;

pub use backend::FlatFileBackend;
pub use cas::{CasBackend, ChunkIndex, ChunkStore, LocalChunkStore};
pub use nbd::NbdBackend;

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

/// Handle to a running NBD server. Dropping it shuts the server down.
pub struct NbdHandle {
    socket_path: String,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// CAS backend reference for saving checkpoints. None for flat file mode.
    cas_backend: Option<Arc<CasBackend>>,
}

impl NbdHandle {
    /// NBD URI for VZNetworkBlockDeviceStorageDeviceAttachment.
    pub fn uri(&self) -> String {
        format!("nbd+unix:///export?socket={}", self.socket_path)
    }

    /// Save the current disk state as a checkpoint index. Only works with CAS backend.
    pub fn save_checkpoint(&self, index_path: &str) -> Result<()> {
        let backend = self.cas_backend.as_ref()
            .ok_or_else(|| anyhow::anyhow!("save_checkpoint requires CAS backend"))?;
        backend.save_index(index_path)
    }
}

impl Drop for NbdHandle {
    fn drop(&mut self) {
        // Flush CAS backend before shutdown
        if let Some(ref backend) = self.cas_backend {
            let _ = backend.flush();
        }
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = std::os::unix::net::UnixStream::connect(&self.socket_path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn start_nbd_with_backend(
    backend: Arc<dyn NbdBackend>,
    socket_path: &str,
    cas_backend: Option<Arc<CasBackend>>,
) -> Result<NbdHandle> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind NBD socket: {}", socket_path))?;
    // Blocking accept — shutdown unblocks via dummy connect in Drop
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    let socket_path_owned = socket_path.to_string();

    let thread = std::thread::Builder::new()
        .name("shuru-nbd".into())
        .spawn(move || {
            info!("NBD server listening on {}", socket_path_owned);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if shutdown_rx.try_recv().is_ok() {
                            debug!("NBD server shutting down");
                            break;
                        }
                        // Timeout so the command loop exits promptly on VM shutdown
                        stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok();
                        info!("NBD client connected");
                        if let Err(e) = nbd::handle_client(stream, backend.clone()) {
                            warn!("NBD client session ended: {}", e);
                        }
                        debug!("NBD client disconnected, waiting for reconnect...");
                    }
                    Err(e) => {
                        if shutdown_rx.try_recv().is_ok() {
                            break;
                        }
                        warn!("NBD accept error: {}", e);
                    }
                }
            }
            info!("NBD server stopped");
        })?;

    Ok(NbdHandle {
        socket_path: socket_path.to_string(),
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
        cas_backend,
    })
}

/// Start an NBD server backed by the content-addressable chunk store.
/// Chunks are loaded lazily from the flat file on first access — no upfront ingestion.
pub fn start_cas_nbd_server(
    rootfs_path: &str,
    cas_dir: &str,
    index_path: &str,
    socket_path: &str,
    disk_size: u64,
) -> Result<NbdHandle> {
    let store: Box<dyn ChunkStore> = Box::new(LocalChunkStore::open(cas_dir)?);

    let (index, fallback, source_idx) = if Path::new(index_path).exists() {
        info!("loading CAS index from {}", index_path);
        let idx = ChunkIndex::load(index_path)?;
        // If the index has a fallback_path, open it for chain resolution
        let fb = idx.fallback_path.as_ref().and_then(|p| {
            FlatFileBackend::open(p).ok()
        });
        (idx, fb, Some(index_path.to_string()))
    } else {
        // No index yet — create empty index with fallback to flat file for lazy ingestion
        let fb = FlatFileBackend::open(rootfs_path)
            .with_context(|| format!("failed to open rootfs for lazy ingestion: {}", rootfs_path))?;
        let disk_size = fb.size();
        info!("CAS: lazy mode, {} MB rootfs", disk_size / (1024 * 1024));
        (ChunkIndex::new(disk_size), Some(fb), None)
    };

    let mut backend = if let Some(fb) = fallback {
        CasBackend::with_fallback(store, index, fb)
    } else {
        CasBackend::new(store, index)
    };
    backend.source_index_path = source_idx;
    if disk_size > 0 {
        backend.set_disk_size(disk_size);
    }
    let cas = Arc::new(backend);
    start_nbd_with_backend(cas.clone(), socket_path, Some(cas))
}
