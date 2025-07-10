use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[must_use]
pub(crate) fn initialize_tracing() -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish())
}

/// small TCP mockserver. Stores everything it receives.
pub(crate) struct MockTcpServer {
    addr: SocketAddr,
    data: Arc<Mutex<Vec<u8>>>,
    _task: JoinHandle<()>,
    _cancel_token: CancellationToken,
}

impl MockTcpServer {
    /// Start server, return handle; reads one connection then stops.
    pub async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let data = Arc::new(Mutex::new(Vec::new()));

        let cancel_token = CancellationToken::new();

        let task = tokio::spawn({
            let cancel_token = cancel_token.clone();
            let data = data.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = cancel_token.cancelled() => break,
                        socket = listener.accept() => match socket {
                            Ok((mut socket, _)) => {
                                let mut buf = [0u8; 1024];
                                loop {
                                    match socket.read(&mut buf).await {
                                        Ok(0) => break,
                                        Ok(n) => {
                                            data.lock().unwrap().extend_from_slice(&buf[..n]);
                                        }
                                        Err(_) => break,
                                    }
                                }
                            }
                            Err(_) => continue,
                        }
                    }
                }
            }
        });

        MockTcpServer {
            addr,
            data,
            _task: task,
            _cancel_token: cancel_token,
        }
    }

    /// Where clients should connect.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// stop the server, collect the data, and return the collected data.
    pub async fn into_data(self) -> Vec<u8> {
        self._cancel_token.cancel();
        self._task.await.unwrap();

        self.data.lock().unwrap().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn recv_bytes_from_client() {
        let server = MockTcpServer::start().await;

        for _ in 0..3 {
            let mut stream = TcpStream::connect(server.addr()).await.unwrap();
            stream.write_all(b"ping!").await.unwrap();
            stream.
            // drop to trigger EOF
            drop(stream);
        }

        // tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(server.into_data().await, b"ping!ping!ping!");
    }
}
