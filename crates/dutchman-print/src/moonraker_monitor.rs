//! A lazy, single monitoring socket. Commands never enter this transport.
use super::{Client, Error, Stream, STATUS_TIMEOUT};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::{watch, Mutex};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest as _, protocol::WebSocketConfig, Message,
};

const MAX_MESSAGE: usize = 1 << 20;
const FRESH: Duration = Duration::from_secs(30);

#[derive(Clone, Default)]
struct State {
    data: Option<(Value, Value)>,
    received: Option<Instant>,
    fault: bool,
    generation: u64,
}
impl State {
    fn cached(&self) -> Result<Option<(Value, Value, u64)>, Error> {
        if self.fault {
            return Err(Error::Unavailable);
        }
        Ok(self
            .received
            .filter(|at| at.elapsed() < FRESH)
            .and_then(|_| self.data.clone())
            .map(|(server, objects)| (server, objects, self.generation)))
    }
}
pub(super) struct Monitor {
    state: watch::Sender<State>,
    stop: watch::Sender<bool>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Drop for Monitor {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.get_mut().take() {
            worker.abort();
        }
    }
}
impl Monitor {
    pub fn new() -> Self {
        Self {
            state: watch::channel(State::default()).0,
            stop: watch::channel(false).0,
            worker: Mutex::new(None),
        }
    }
    pub async fn shutdown(&self) {
        let mut worker = self.worker.lock().await;
        self.stop.send_replace(true);
        if let Some(mut task) = worker.take() {
            if tokio::time::timeout(Duration::from_secs(1), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        self.state.send_replace(State::default());
    }
    pub fn check(&self, generation: Option<u64>) -> Result<(), Error> {
        let state = self.state.borrow();
        if *self.stop.borrow()
            || state.fault
            || generation.is_some_and(|g| {
                g != state.generation
                    || state.received.is_none_or(|at| at.elapsed() >= FRESH)
                    || state.data.is_none()
            })
        {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
    pub async fn read(&self, client: &Client) -> Result<Option<(Value, Value, u64)>, Error> {
        let mut worker = self.worker.lock().await;
        if *self.stop.borrow() {
            return Err(Error::Unavailable);
        }
        if worker.is_none() {
            let transport = Client {
                config: client.config.clone(),
                uri: client.uri.clone(),
                tls: client.tls.clone(),
                observation: Mutex::new(None),
                changed: std::sync::atomic::AtomicBool::new(false),
                monitor: Monitor::new(),
            };
            *worker = Some(tokio::spawn(run(
                transport,
                self.state.clone(),
                self.stop.subscribe(),
            )));
        }
        self.state.borrow().cached()
    }
}

/// Object fields are replaced as units: a nested config value in a delta replaces
/// that field, while omitted fields and omitted objects retain their baseline.
fn merge(objects: &mut Value, delta: &Value, names: &[String]) -> Result<(), Error> {
    let target = objects.as_object_mut().ok_or(Error::Unavailable)?;
    for (name, fields) in delta.as_object().ok_or(Error::Unavailable)? {
        if !names.contains(name) {
            continue;
        }
        let fields = fields.as_object().ok_or(Error::Unavailable)?;
        let object = target.entry(name.clone()).or_insert_with(|| json!({}));
        let object = object.as_object_mut().ok_or(Error::Unavailable)?;
        for (key, value) in fields {
            object.insert(key.clone(), value.clone());
        }
    }
    // The incoming frame is bounded too; this also caps accumulation over many deltas.
    if serde_json::to_vec(objects)
        .map_err(|_| Error::Unavailable)?
        .len()
        > MAX_MESSAGE
    {
        return Err(Error::Unavailable);
    }
    Ok(())
}
pub(super) fn baseline(data: &Value, names: &[String]) -> Result<Value, Error> {
    eventtime(data.get("eventtime"))?;
    let mut objects = json!({});
    merge(
        &mut objects,
        data.get("status").ok_or(Error::Unavailable)?,
        names,
    )?;
    Ok(objects)
}
fn eventtime(value: Option<&Value>) -> Result<f64, Error> {
    value
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .ok_or(Error::Unavailable)
}
type Socket = tokio_tungstenite::WebSocketStream<Box<dyn Stream>>;
async fn connect(client: &Client) -> Result<Socket, Error> {
    let https = client.uri.scheme_str() == Some("https");
    let host = client
        .uri
        .host()
        .ok_or(Error::Unavailable)?
        .trim_matches(['[', ']']);
    let tcp = tokio::net::TcpStream::connect((
        host,
        client
            .uri
            .port_u16()
            .unwrap_or(if https { 443 } else { 80 }),
    ))
    .await
    .map_err(|_| Error::Unavailable)?;
    let stream: Box<dyn Stream> = if https {
        let name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|_| Error::Unavailable)?;
        Box::new(
            tokio_rustls::TlsConnector::from(client.tls.clone())
                .connect(name, tcp)
                .await
                .map_err(|_| Error::Unavailable)?,
        )
    } else {
        Box::new(tcp)
    };
    let url = format!(
        "{}://{}{}/websocket",
        if https { "wss" } else { "ws" },
        client.uri.authority().ok_or(Error::Unavailable)?,
        client.uri.path().trim_end_matches('/')
    );
    let mut request = url.into_client_request().map_err(|_| Error::Unavailable)?;
    if let Some(key) = &client.config.api_key {
        request
            .headers_mut()
            .insert("X-Api-Key", key.parse().map_err(|_| Error::Unavailable)?);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    tokio_tungstenite::client_async_with_config(request, stream, Some(config))
        .await
        .map(|(socket, _)| socket)
        .map_err(|_| Error::Unavailable)
}
async fn run(client: Client, state: watch::Sender<State>, mut stop: watch::Receiver<bool>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            break;
        }
        let generation = state.borrow().generation.wrapping_add(1);
        state.send_modify(|s| s.generation = generation);
        let operation = async {
            let mut socket = tokio::time::timeout(STATUS_TIMEOUT, connect(&client))
                .await
                .map_err(|_| Error::Unavailable)??;
            let setup = async {
                let server = client.json("/server/info").await?;
                if server.get("klippy_connected").and_then(Value::as_bool) != Some(true)
                    || server.get("klippy_state").and_then(Value::as_str) != Some("ready")
                {
                    return Err(Error::Unavailable);
                }
                let names = client.status_objects().await?;
                let objects = names
                    .iter()
                    .map(|n| (n.clone(), Value::Null))
                    .collect::<serde_json::Map<_, _>>();
                socket.send(Message::Text(json!({"jsonrpc":"2.0","id":1,"method":"printer.objects.subscribe","params":{"objects":objects}}).to_string().into())).await.map_err(|_| Error::Unavailable)?;
                Ok((server, names))
            };
            let (server, names) = tokio::time::timeout(STATUS_TIMEOUT, setup)
                .await
                .map_err(|_| Error::Unavailable)??;
            let mut objects = None;
            let mut time = 0.0;
            let baseline_deadline = Instant::now() + STATUS_TIMEOUT;
            loop {
                let message = tokio::select! {
                    _ = stop.changed() => {
                        let _ = tokio::time::timeout(STATUS_TIMEOUT, socket.close(None)).await;
                        return Ok::<(), Error>(());
                    }
                    message = tokio::time::timeout_at(if objects.is_some() {Instant::now() + FRESH} else {baseline_deadline}, socket.next()) => message.map_err(|_| Error::Unavailable)?.ok_or(Error::Unavailable)?.map_err(|_| Error::Unavailable)?,
                };
                let value: Value = match message {
                    Message::Text(text) => {
                        serde_json::from_str(&text).map_err(|_| Error::Unavailable)?
                    }
                    Message::Ping(bytes) => {
                        tokio::time::timeout(STATUS_TIMEOUT, socket.send(Message::Pong(bytes)))
                            .await
                            .map_err(|_| Error::Unavailable)?
                            .map_err(|_| Error::Unavailable)?;
                        state.send_modify(|s| {
                            if s.data.is_some() {
                                s.received = Some(Instant::now());
                            }
                        });
                        continue;
                    }
                    Message::Pong(_) => {
                        state.send_modify(|s| {
                            if s.data.is_some() {
                                s.received = Some(Instant::now());
                            }
                        });
                        continue;
                    }
                    Message::Close(_) => return Err(Error::Unavailable),
                    _ => return Err(Error::Unavailable),
                };
                match value.get("method").and_then(Value::as_str) {
                    Some("notify_klippy_disconnected" | "notify_klippy_shutdown") => {
                        state.send_replace(State {
                            fault: true,
                            generation,
                            ..State::default()
                        });
                        return Err(Error::Unavailable);
                    }
                    Some("notify_status_update") => {
                        if let Some(objects) = &mut objects {
                            let params = value
                                .get("params")
                                .and_then(Value::as_array)
                                .ok_or(Error::Unavailable)?;
                            let next = eventtime(params.get(1))?;
                            if next < time {
                                return Err(Error::Unavailable);
                            }
                            merge(objects, params.first().ok_or(Error::Unavailable)?, &names)?;
                            time = next;
                        }
                    }
                    _ if value.get("id").and_then(Value::as_u64) == Some(1)
                        && objects.is_none() =>
                    {
                        let result = value.get("result").ok_or(Error::Unavailable)?;
                        objects = Some(baseline(result, &names)?);
                        time = eventtime(result.get("eventtime"))?;
                        backoff = Duration::from_secs(1);
                    }
                    _ => {}
                }
                if let Some(objects) = &objects {
                    state.send_replace(State {
                        data: Some((server.clone(), objects.clone())),
                        received: Some(Instant::now()),
                        fault: false,
                        generation,
                    });
                }
            }
        };
        let _ = operation.await;
        state.send_modify(|s| {
            s.data = None;
            s.received = None;
            s.generation = s.generation.wrapping_add(1);
        });
        if *stop.borrow() {
            break;
        }
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = backoff.saturating_mul(2).min(Duration::from_secs(30));
    }
    state.send_replace(State::default());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn freshness_requires_a_baseline_and_recent_message_and_faults_block_fallback() {
        let mut state = State::default();
        assert!(state.cached().unwrap().is_none());
        state.data = Some((json!({}), json!({})));
        assert!(state.cached().unwrap().is_none());
        state.received = Some(Instant::now());
        assert!(state.cached().unwrap().is_some());
        state.received = Some(Instant::now() - FRESH);
        assert!(state.cached().unwrap().is_none());
        state.fault = true;
        assert!(state.cached().is_err());
    }

    #[test]
    fn merges_fields_without_inventing_objects_or_retaining_replaced_nested_values() {
        let names = vec![
            "print_stats".into(),
            "heater_bed".into(),
            "configfile".into(),
        ];
        let mut objects = baseline(
            &json!({"eventtime":1,"status":{
                "print_stats":{"state":"paused","filename":"test.gcode"},
                "heater_bed":{"temperature":59,"target":60},
                "configfile":{"settings":{"old":true}}
            }}),
            &names,
        )
        .unwrap();
        merge(&mut objects, &json!({"heater_bed":{"temperature":60},"extruder9":{"temperature":999},"configfile":{"settings":{"new":true}}}), &names).unwrap();
        assert_eq!(
            objects.pointer("/print_stats/filename"),
            Some(&json!("test.gcode"))
        );
        assert_eq!(
            objects.pointer("/print_stats/state"),
            Some(&json!("paused"))
        );
        assert_eq!(objects.pointer("/heater_bed/target"), Some(&json!(60)));
        assert!(objects.get("extruder9").is_none());
        assert_eq!(
            objects.pointer("/configfile/settings"),
            Some(&json!({"new":true}))
        );
    }

    #[test]
    fn bounds_accumulation_and_rejects_malformed_baselines_and_deltas() {
        let names = vec!["print_stats".into()];
        for value in [
            json!({"status":{}}),
            json!({"eventtime":-1,"status":{}}),
            json!({"eventtime":0,"status":[]}),
            json!({"eventtime":0,"status":{"print_stats":false}}),
        ] {
            assert!(baseline(&value, &names).is_err());
        }
        let mut objects = json!({});
        merge(
            &mut objects,
            &json!({"print_stats":{"first":"a".repeat(MAX_MESSAGE / 2)}}),
            &names,
        )
        .unwrap();
        assert!(merge(
            &mut objects,
            &json!({"print_stats":{"second":"b".repeat(MAX_MESSAGE / 2)}}),
            &names
        )
        .is_err());
    }
}
