//! Test-owned TLS MQTT endpoint. All certificates and messages are synthetic.
use bytes::BytesMut;
use rumqttc::mqttbytes::v4::{
    ConnAck, ConnectReturnCode, Packet, Publish, SubAck, SubscribeReasonCode,
};
use rumqttc::QoS;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, Mutex};

pub struct Fake {
    pub port: u16,
    pub cert: rustls::pki_types::CertificateDer<'static>,
    pub pem: String,
    pub reports: broadcast::Sender<serde_json::Value>,
    pub connections: Arc<AtomicUsize>,
    pub requests: Arc<AtomicUsize>,
    pub disconnects: Arc<AtomicUsize>,
    pub credentials: Arc<AtomicUsize>,
    pub tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
        if let Ok(tasks) = self.tasks.try_lock() {
            for task in tasks.iter() {
                task.abort();
            }
        }
    }
}
impl Fake {
    pub async fn new() -> Self {
        let key = rcgen::generate_simple_self_signed(vec!["TESTSERIAL".into()]).unwrap();
        let cert = key.cert.der().clone();
        let pem = key.cert.pem();
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key.signing_key.serialize_der()).into(),
        )
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (reports, _) = broadcast::channel::<serde_json::Value>(64);
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let disconnects = Arc::new(AtomicUsize::new(0));
        let credentials = Arc::new(AtomicUsize::new(0));
        let tasks = Arc::new(Mutex::new(Vec::new()));
        let task = {
            let (reports, connections, requests, disconnects, credentials, tasks) = (
                reports.clone(),
                connections.clone(),
                requests.clone(),
                disconnects.clone(),
                credentials.clone(),
                tasks.clone(),
            );
            tokio::spawn(async move {
                loop {
                    let (socket, _) = listener.accept().await.unwrap();
                    connections.fetch_add(1, Ordering::SeqCst);
                    let (acceptor, mut reports, requests, disconnects, credentials) = (
                        acceptor.clone(),
                        reports.subscribe(),
                        requests.clone(),
                        disconnects.clone(),
                        credentials.clone(),
                    );
                    let task = tokio::spawn(async move {
                        let Ok(mut socket) = acceptor.accept(socket).await else {
                            return;
                        };
                        let mut buffer = BytesMut::new();
                        let mut ignore_ping = false;
                        loop {
                            tokio::select! {
                                result = reports.recv() => {
                                    let Ok(value) = result else { return; };
                                    if value.is_null() { return; }
                                    if value.get("test_ignore_ping").is_some() { ignore_ping = true; continue; }
                                    if send(&mut socket, Packet::Publish(Publish::new("device/TESTSERIAL/report", QoS::AtMostOnce, value.to_string()))).await.is_err() { return; }
                                }
                                packet = read(&mut socket, &mut buffer) => {
                                    let Ok(packet) = packet else { return; };
                                    let reply = match packet {
                                        Packet::Connect(c) => {
                                            credentials.fetch_add(1, Ordering::SeqCst);
                                            let ok = c.login.is_some_and(|l| l.username == "bblp" && l.password == "test-access");
                                            Packet::ConnAck(ConnAck::new(if ok { ConnectReturnCode::Success } else { ConnectReturnCode::BadUserNamePassword }, false))
                                        }
                                        Packet::Subscribe(s) => {
                                            assert_eq!(s.filters.len(), 1);
                                            assert_eq!(s.filters[0].path, "device/TESTSERIAL/report");
                                            Packet::SubAck(SubAck::new(s.pkid, vec![SubscribeReasonCode::Success(QoS::AtMostOnce)]))
                                        }
                                        Packet::Publish(p) => {
                                            assert_eq!(p.topic, "device/TESTSERIAL/request");
                                            let value: serde_json::Value = serde_json::from_slice(&p.payload).unwrap();
                                            assert_eq!(value, serde_json::json!({"pushing":{"command":"pushall","sequence_id":"0","version":1,"push_target":1}}));
                                            requests.fetch_add(1, Ordering::SeqCst);
                                            Packet::Publish(Publish::new("device/TESTSERIAL/report", QoS::AtMostOnce, serde_json::json!({"print":{"gcode_state":"IDLE","mc_percent":20,"mc_remaining_time":3,"nozzle_temper":210,"bed_temper":55,"subtask_name":"Test cube"}}).to_string()))
                                        }
                                        Packet::PingReq if ignore_ping => continue,
                                        Packet::PingReq => Packet::PingResp,
                                        Packet::Disconnect => { disconnects.fetch_add(1, Ordering::SeqCst); return; }
                                        _ => panic!("Unexpected MQTT packet"),
                                    };
                                    if send(&mut socket, reply).await.is_err() { return; }
                                }
                            }
                        }
                    });
                    tasks.lock().await.push(task);
                }
            })
        };
        Self {
            port,
            cert,
            pem,
            reports,
            connections,
            requests,
            disconnects,
            credentials,
            tasks,
            task,
        }
    }
    pub async fn wait(counter: &AtomicUsize, n: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while counter.load(Ordering::SeqCst) < n {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}
async fn read<S: tokio::io::AsyncRead + Unpin>(s: &mut S, b: &mut BytesMut) -> Result<Packet, ()> {
    loop {
        if let Ok(p) = Packet::read(b, 1 << 20) {
            return Ok(p);
        }
        if s.read_buf(b).await.map_err(|_| ())? == 0 {
            return Err(());
        }
    }
}
async fn send<S: tokio::io::AsyncWrite + Unpin>(s: &mut S, p: Packet) -> Result<(), ()> {
    let mut b = BytesMut::new();
    p.write(&mut b, 1 << 20).unwrap();
    s.write_all(&b).await.map_err(|_| ())
}
