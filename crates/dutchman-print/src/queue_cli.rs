//! Thin client of the running server; queue commands never open backend connections.
use super::{confirm, output};
use axum::body::Body;
use clap::{Args, Subcommand};
use hyper::{Method, Request, Uri};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

#[derive(Args)]
pub(super) struct Flags {
    #[arg(long, default_value = "http://127.0.0.1:8730")]
    server: String,
    #[arg(long, default_value = "dutchman")]
    user: String,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Upload and prepare a 3MF, then append it to the queue.
    Add {
        file: PathBuf,
        /// Printer whose presets are used for the initial preparation.
        #[arg(long)]
        printer: String,
        #[arg(long, default_value = "any")]
        target: String,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        kind: Option<String>,
    },
    List,
    Remove {
        id: String,
    },
    /// Move an entry to a zero-based position.
    Move {
        id: String,
        position: usize,
    },
    Proposals,
    Dispatch {
        id: String,
        #[arg(long)]
        printer: String,
        /// Separately confirm bed clearance before requesting Start.
        #[arg(long)]
        bed_clear: bool,
        #[arg(long)]
        yes: bool,
    },
}
fn segment(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
struct Client {
    server: String,
    user: String,
}
impl Client {
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
        upload: bool,
        confirmed: bool,
    ) -> Result<Value, String> {
        let uri: Uri = format!("{}{path}", self.server.trim_end_matches('/'))
            .parse()
            .map_err(|_| "Use an HTTP server URL.")?;
        if uri.scheme_str() != Some("http") || uri.authority().is_none() {
            return Err("Use an HTTP server URL on a trusted local network.".into());
        }
        let host = uri.host().ok_or("Use a server hostname.")?;
        let port = uri.port_u16().unwrap_or(80);
        let task = async {
            let socket = tokio::net::TcpStream::connect((host, port))
                .await
                .map_err(|_| "Cannot connect to the queue server.")?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(socket))
                    .await
                    .map_err(|_| "Cannot connect to the queue server.")?;
            let connection = tokio::spawn(async {
                let _ = connection.await;
            });
            let result = async {
                let mut request = Request::builder()
                    .method(method)
                    .uri(uri.path_and_query().map_or("/", |p| p.as_str()))
                    .header(
                        "host",
                        uri.authority().ok_or("Use a server hostname.")?.as_str(),
                    )
                    .header(
                        "content-type",
                        if upload {
                            "model/3mf"
                        } else {
                            "application/json"
                        },
                    );
                if let Ok(password) = std::env::var("DUTCHMAN_PRINT_PASSWORD") {
                    use base64::Engine as _;
                    request = request.header(
                        "authorization",
                        format!(
                            "Basic {}",
                            base64::engine::general_purpose::STANDARD
                                .encode(format!("{}:{password}", self.user))
                        ),
                    );
                }
                if confirmed {
                    request = request.header(
                        "Plugin-Confirmation",
                        format!(
                            "{}{}",
                            dutchman_print::new_job_id()?,
                            dutchman_print::new_job_id()?
                        ),
                    );
                }
                let response = sender
                    .send_request(
                        request
                            .body(Body::from(body))
                            .map_err(|_| "Cannot create the queue request.")?,
                    )
                    .await
                    .map_err(|_| "Queue request failed. Read status before another dispatch.")?;
                let status = response.status();
                let bytes = axum::body::to_bytes(Body::new(response.into_body()), 16 << 20)
                    .await
                    .map_err(|_| "Cannot read the queue response.")?;
                let value: Value = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).map_err(|_| "Cannot read queue JSON.")?
                };
                if !status.is_success() {
                    return Err(value
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("The queue server refused the request.")
                        .to_string());
                }
                Ok(value)
            }
            .await;
            connection.abort();
            result
        };
        tokio::time::timeout(Duration::from_secs(900), task)
            .await
            .map_err(|_| {
                "Queue request timed out. Read status before another dispatch.".to_string()
            })?
    }
    async fn json(
        &self,
        method: Method,
        path: &str,
        value: Value,
        confirmed: bool,
    ) -> Result<Value, String> {
        self.request(
            method,
            path,
            serde_json::to_vec(&value).map_err(|_| "Cannot encode queue fields.")?,
            false,
            confirmed,
        )
        .await
    }
}
pub(super) async fn run(flags: Flags) -> Result<(), String> {
    let client = Client {
        server: flags.server,
        user: flags.user,
    };
    let result = match flags.command {
        Command::List => {
            client
                .json(Method::GET, "/api/v1/queue", Value::Null, false)
                .await?
        }
        Command::Proposals => {
            client
                .json(Method::GET, "/api/v1/queue/proposals", Value::Null, false)
                .await?
        }
        Command::Remove { id } => {
            client
                .json(
                    Method::DELETE,
                    &format!("/api/v1/queue/{}", segment(&id)),
                    Value::Null,
                    false,
                )
                .await?
        }
        Command::Move { id, position } => {
            client
                .json(
                    Method::POST,
                    &format!("/api/v1/queue/{}/move", segment(&id)),
                    json!({"position":position}),
                    false,
                )
                .await?
        }
        Command::Add {
            file,
            printer,
            target,
            profile,
            kind,
        } => {
            use tokio::io::AsyncReadExt as _;
            let mut bytes = Vec::new();
            tokio::fs::File::open(file)
                .await
                .map_err(|_| "Cannot open the 3MF file.")?
                .take((200 << 20) + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| "Cannot read the 3MF file.")?;
            if bytes.len() > 200 << 20 {
                return Err("Choose a 3MF file of at most 200 MiB.".into());
            }
            let job = client
                .request(
                    Method::POST,
                    &format!("/api/v1/jobs?printer={}", segment(&printer)),
                    bytes,
                    true,
                    false,
                )
                .await?;
            client.json(Method::POST, "/api/v1/queue", json!({"job":job.get("id").ok_or("Cannot read the prepared job id.")?,"target":target,"profile":profile,"kind":kind}), false).await?
        }
        Command::Dispatch {
            id,
            printer,
            bed_clear,
            yes,
        } => {
            if bed_clear {
                let challenge = client
                    .json(
                        Method::POST,
                        "/api/v1/confirmations",
                        json!({"action":"clear","printer":printer}),
                        false,
                    )
                    .await?;
                let summary = challenge
                    .get("summary")
                    .and_then(Value::as_str)
                    .ok_or("Cannot read confirmation summary.")?;
                if yes {
                    eprintln!("{summary}");
                }
                if !confirm(summary, yes)? {
                    return Ok(());
                }
                client.json(Method::POST, &format!("/api/v1/printers/{}/clear", segment(&printer)), json!({"confirmation":challenge.get("id").ok_or("Cannot read the confirmation id.")?}), true).await?;
            }
            let path = format!("/api/v1/queue/{}", segment(&id));
            let challenge = client
                .json(
                    Method::POST,
                    &format!("{path}/confirmations"),
                    json!({"printer":printer}),
                    false,
                )
                .await?;
            let summary = challenge
                .get("summary")
                .and_then(Value::as_str)
                .ok_or("Cannot read confirmation summary.")?;
            if yes {
                eprintln!("{summary}");
            }
            if !confirm(summary, yes)? {
                return Ok(());
            }
            client.json(Method::POST, &format!("{path}/dispatch"), json!({"confirmation":challenge.get("id").ok_or("Cannot read the confirmation id.")?}), true).await?
        }
    };
    output(&result)
}
