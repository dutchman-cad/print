#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::{hash, PrinterConfig};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

#[allow(dead_code)]
pub struct Fake {
    pub config: PrinterConfig,
    pub pem: String,
    pub credentials: Arc<AtomicUsize>,
    pub received: Arc<AtomicUsize>,
    pub resumed: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn server(tls12: bool) -> (rustls::ServerConfig, Vec<u8>, String) {
    let key = rcgen::generate_simple_self_signed(vec!["TESTSERIAL".into()]).unwrap();
    let cert = key.cert.der().clone();
    let pem = key.cert.pem();
    let versions = if tls12 {
        vec![&rustls::version::TLS12]
    } else {
        vec![&rustls::version::TLS13]
    };
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&versions)
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(key.signing_key.serialize_der()).into(),
    )
    .unwrap();
    (config, cert.to_vec(), pem)
}

#[allow(dead_code)]
impl Fake {
    pub async fn new(tls12: bool, wrong_size: bool, data_mode: u8) -> Self {
        let (server, cert, pem) = server(tls12);
        Self::with_server(server, cert, pem, tls12, wrong_size, data_mode).await
    }
    pub async fn with_server(
        server: rustls::ServerConfig,
        cert: Vec<u8>,
        pem: String,
        tls12: bool,
        wrong_size: bool,
        data_mode: u8,
    ) -> Self {
        let mut independent = server.clone();
        independent.session_storage = rustls::server::ServerSessionMemoryCache::new(256);
        let acceptor = TlsAcceptor::from(Arc::new(server));
        let data_acceptor = match data_mode {
            1 => TlsAcceptor::from(Arc::new(self::server(tls12).0)),
            2 => TlsAcceptor::from(Arc::new(independent)),
            _ => acceptor.clone(),
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let credentials = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(AtomicUsize::new(0));
        let resumed = Arc::new(AtomicUsize::new(0));
        let (auth, length, reuse) = (credentials.clone(), received.clone(), resumed.clone());
        let task = tokio::spawn(async move {
            loop {
                async {
                    let (socket, _) = listener.accept().await.unwrap();
                    let Ok(socket) = acceptor.accept(socket).await else {
                        return;
                    };
                    let mut control = BufReader::new(socket);
                    if control
                        .get_mut()
                        .write_all(b"220 Synthetic FTPS\r\n")
                        .await
                        .is_err()
                    {
                        return;
                    }
                    let mut passive = None;
                    loop {
                        let mut line = String::new();
                        if control.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let response = match line.trim_end() {
                            "USER bblp" => {
                                auth.fetch_add(1, Ordering::SeqCst);
                                "331 Password\r\n".to_string()
                            }
                            "PASS test-access" => "230 Accepted\r\n".into(),
                            command if command.starts_with("PASS ") => {
                                "530 Access denied\r\n".into()
                            }
                            "PBSZ 0" | "PROT P" | "TYPE I" => "200 Accepted\r\n".into(),
                            "PASV" => {
                                let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
                                let port = listener.local_addr().unwrap().port();
                                passive = Some(listener);
                                // A foreign address is deliberately advertised; the client uses the control peer.
                                format!("227 Passive (192,0,2,1,{},{})\r\n", port / 256, port % 256)
                            }
                            command if command.starts_with("STOR ") => {
                                control
                                    .get_mut()
                                    .write_all(b"150 Send file\r\n")
                                    .await
                                    .unwrap();
                                let (socket, _) = passive.take().unwrap().accept().await.unwrap();
                                let Ok(mut data) = data_acceptor.accept(socket).await else {
                                    return;
                                };
                                if data.get_ref().1.handshake_kind()
                                    == Some(rustls::HandshakeKind::Resumed)
                                {
                                    reuse.fetch_add(1, Ordering::SeqCst);
                                }
                                let mut bytes = Vec::new();
                                let _ = data.read_to_end(&mut bytes).await;
                                length.store(bytes.len(), Ordering::SeqCst);
                                "226 Stored\r\n".into()
                            }
                            command if command.starts_with("SIZE ") => format!(
                                "213 {}\r\n",
                                length.load(Ordering::SeqCst) + usize::from(wrong_size)
                            ),
                            "QUIT" => {
                                control.get_mut().write_all(b"221 Bye\r\n").await.unwrap();
                                return;
                            }
                            other => panic!("Unexpected command {other}"),
                        };
                        if control
                            .get_mut()
                            .write_all(response.as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                .await;
            }
        });
        Self {
            config: PrinterConfig {
                host: Some("127.0.0.1".into()),
                ftps_port: Some(port),
                serial: Some("TESTSERIAL".into()),
                access_code: Some("test-access".into()),
                cert_sha256: Some(hash(&cert)),
                ..Default::default()
            },
            pem,
            credentials,
            received,
            resumed,
            task,
        }
    }
}
