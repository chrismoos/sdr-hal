//! Noticing when a control connection closes.
//!
//! A client is identified by its control connection, and that connection lives exactly
//! as long as the client does. Watching it closing is what notices a client that only
//! receives: its data plane can be silent UDP, which reports nothing, but no client
//! outlives its control connection.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use log::info;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tonic::transport::server::{Connected, TcpConnectInfo};

use super::composite::ClientRegistry;
use super::sessions::Sessions;

pub(crate) struct ClientWatch {
    sessions: Arc<Sessions>,
    registry: Option<Arc<Mutex<ClientRegistry>>>,
    left: mpsc::UnboundedSender<SocketAddr>,
}

impl ClientWatch {
    pub(crate) fn new(
        sessions: Arc<Sessions>,
        registry: Option<Arc<Mutex<ClientRegistry>>>,
        left: mpsc::UnboundedSender<SocketAddr>,
    ) -> Arc<Self> {
        Arc::new(Self {
            sessions,
            registry,
            left,
        })
    }

    fn client_left(&self, peer: SocketAddr) {
        let Some(registry) = &self.registry else {
            let _ = self.left.send(peer);
            return;
        };
        let Some(session_id) = registry.lock().forget(peer) else {
            return;
        };
        match session_id {
            Some(session_id) => {
                info!(
                    "radiod: composite client {peer} closed its control connection, releasing session {session_id:#010x}"
                );
                self.sessions.remove(session_id);
            }
            None => info!("radiod: composite client {peer} left before opening a session"),
        }
    }
}

/// A control connection that reports its own closing.
///
/// Delegates every IO operation to the stream it wraps. `TcpStream` is `Unpin`, so the
/// wrapper needs no projection of its own.
pub(crate) struct WatchedStream {
    inner: TcpStream,
    peer: Option<SocketAddr>,
    watch: Arc<ClientWatch>,
}

impl WatchedStream {
    pub(crate) fn new(inner: TcpStream, watch: Arc<ClientWatch>) -> Self {
        let peer = inner.peer_addr().ok();
        Self { inner, peer, watch }
    }
}

impl Drop for WatchedStream {
    fn drop(&mut self) {
        if let Some(peer) = self.peer {
            self.watch.client_left(peer);
        }
    }
}
impl Connected for WatchedStream {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.inner.connect_info()
    }
}

impl AsyncRead for WatchedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for WatchedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}
