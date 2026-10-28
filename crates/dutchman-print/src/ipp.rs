//! IPP document transport and attributes, handwritten from RFC 8010 and RFC 8011.
use crate::backend::{Error, Options, Printer, PrinterState};
pub use crate::ipp_codec::{Attribute, Group, Message};
use axum::{
    body::Bytes,
    http::{Request, StatusCode, Uri},
};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_util::rt::TokioIo;
use std::{
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc,
    },
    time::Duration,
};

const MAX_REPLY: usize = 4 << 20;
const STATUS_TIMEOUT: Duration = Duration::from_secs(4);
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

pub struct Client {
    config: crate::config::Printer,
    uri: Uri,
    printer_uri: String,
    tls: Arc<rustls::ClientConfig>,
    next: AtomicI32,
}
pub struct Snapshot {
    pub printer: Printer,
    pub attributes: Group,
}
fn refused(message: &str) -> Error {
    Error::Refused(message.into())
}
fn endpoint(config: &crate::config::Printer) -> Result<(Uri, String), String> {
    let source = config.url.as_deref().unwrap_or_default();
    if source.len() > 2048 || source.contains(['#', '\\']) {
        return Err("Set an IPP URL without credentials, query, or fragment.".into());
    }
    let (scheme, rest) = source
        .split_once("://")
        .ok_or("Set an ipp, ipps, http, or https printer URL.")?;
    let tls = matches!(scheme, "ipps" | "https");
    if !matches!(scheme, "ipp" | "ipps" | "http" | "https") {
        return Err("Set an ipp, ipps, http, or https printer URL.".into());
    }
    let parsed: Uri = format!("{}://{rest}", if tls { "https" } else { "http" })
        .parse()
        .map_err(|_| "Set a valid IPP URL.")?;
    let authority = parsed.authority().ok_or("Set an IPP printer hostname.")?;
    let suffix = authority
        .as_str()
        .strip_prefix(authority.host())
        .unwrap_or_default();
    if authority.as_str().contains('@')
        || parsed.host().is_none()
        || parsed.path_and_query().is_some_and(|p| p.query().is_some())
        || (!suffix.is_empty()
            && suffix
                .strip_prefix(':')
                .and_then(|s| s.parse::<u16>().ok())
                .is_none_or(|p| p == 0))
    {
        return Err("Set an IPP URL without credentials or query, with a valid port.".into());
    }
    let port = parsed.port_u16().unwrap_or(631);
    let path = parsed.path();
    let transport: Uri = format!(
        "{}://{}:{port}{path}",
        if tls { "https" } else { "http" },
        authority.host()
    )
    .parse()
    .map_err(|_| "Set a valid IPP URL.")?;
    let target = format!(
        "{}://{}:{port}{path}",
        if tls { "ipps" } else { "ipp" },
        authority.host()
    );
    Ok((transport, target))
}
pub fn validate(config: &crate::config::Printer) -> Result<(), String> {
    let (uri, _) = endpoint(config)?;
    if config.username.is_some() != config.password.is_some() {
        return Err("Set both username and password for IPP HTTP Basic authentication.".into());
    }
    if config.username.is_some() && uri.scheme_str() != Some("https") {
        return Err("Use ipps or https for IPP credentials. HTTP Basic requires TLS.".into());
    }
    for secret in [&config.username, &config.password].into_iter().flatten() {
        if secret.is_empty() || secret.len() > 1024 || secret.chars().any(char::is_control) {
            return Err(
                "Use nonempty IPP credentials of at most 1024 bytes without control characters."
                    .into(),
            );
        }
    }
    if config.username.as_ref().is_some_and(|u| u.contains(':'))
        || config.api_key.is_some()
        || config.ca_file.is_some()
        || config.cert_sha256.is_some()
    {
        return Err("IPP supports optional username and password over normally trusted TLS. Leave out API keys and certificate overrides.".into());
    }
    if config.slicer.is_some()
        || config.printer_profile.is_some()
        || config.print_profile.is_some()
        || config.filament_profile.is_some()
    {
        return Err(
            "IPP prints PDF documents directly. Leave out slicer and profile settings.".into(),
        );
    }
    Ok(())
}
impl Client {
    pub fn new(config: crate::config::Printer) -> Result<Self, String> {
        validate(&config)?;
        let (uri, printer_uri) = endpoint(&config)?;
        let mut roots = rustls::RootCertStore::empty();
        if uri.scheme_str() == Some("https") {
            for cert in rustls_native_certs::load_native_certs().certs {
                roots
                    .add(cert)
                    .map_err(|_| "Cannot load system certificate trust.")?;
            }
            if roots.is_empty() {
                return Err("No system certificate trust is available for IPP TLS.".into());
            }
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| "Cannot configure IPP TLS.")?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(Self {
            config,
            uri,
            printer_uri,
            tls: Arc::new(tls),
            next: AtomicI32::new(1),
        })
    }
    pub fn operation(&self) -> Vec<Attribute> {
        vec![
            Attribute::text("attributes-charset", 0x47, "utf-8"),
            Attribute::text("attributes-natural-language", 0x48, "en"),
            Attribute::text("printer-uri", 0x45, &self.printer_uri),
        ]
    }
    pub async fn call(
        &self,
        code: u16,
        attributes: Vec<Attribute>,
        template: Vec<Attribute>,
        document: &[u8],
    ) -> Result<Message, Error> {
        if document.len() > 200 << 20 {
            return Err(refused("PDF exceeds 200 MiB. Choose a smaller document."));
        }
        // IPP request ids are positive; the counter wraps and maps into 1..=i32::MAX.
        let id = self
            .next
            .fetch_add(1, Ordering::Relaxed)
            .rem_euclid(i32::MAX)
            + 1;
        let mut groups = vec![Group { tag: 1, attributes }];
        if !template.is_empty() {
            groups.push(Group {
                tag: 2,
                attributes: template,
            });
        }
        let mut body = Message {
            code,
            request_id: id,
            groups,
        }
        .encode()?;
        body.extend_from_slice(document);
        let (status, bytes) = self.request(body.into(), code == 2).await?;
        if status != StatusCode::OK {
            return Err(Error::Unavailable);
        }
        let (message, end) = Message::decode(&bytes)?;
        if end != bytes.len() || message.request_id != id {
            return Err(Error::Unavailable);
        }
        let operation = message.groups.first().ok_or(Error::Unavailable)?;
        if operation.attributes.first().is_none_or(|a| {
            a.name != "attributes-charset" || a.texts(0x47).ok().as_deref() != Some(&["utf-8"])
        }) || operation
            .attributes
            .get(1)
            .is_none_or(|a| a.name != "attributes-natural-language" || a.texts(0x48).is_err())
        {
            return Err(Error::Unavailable);
        }
        if message.code >= 0x0400 {
            return Err(if matches!(message.code, 0x0401..=0x0403) {
                Error::Authentication
            } else {
                refused("IPP refused the request. Check supported document settings and read fresh status.")
            });
        }
        // Strict fidelity: never silently accept substituted job settings.
        if matches!(code, 2 | 4) && message.code != 0 {
            return Err(refused(
                "IPP did not accept the exact document settings. Prepare with supported values.",
            ));
        }
        Ok(message)
    }
    async fn request(&self, body: Bytes, upload: bool) -> Result<(StatusCode, Bytes), Error> {
        let (progress, updates) = crate::progress::channel();
        let size = body.len() as u64;
        let operation = async {
            let host = self
                .uri
                .host()
                .ok_or(Error::Unavailable)?
                .trim_matches(['[', ']']);
            let https = self.uri.scheme_str() == Some("https");
            let socket = tokio::net::TcpStream::connect((host, self.uri.port_u16().unwrap_or(631)))
                .await
                .map_err(|_| Error::Unavailable)?;
            let stream: Box<dyn Stream> = if https {
                let name = rustls::pki_types::ServerName::try_from(host.to_owned())
                    .map_err(|_| Error::Unavailable)?;
                Box::new(
                    tokio_rustls::TlsConnector::from(self.tls.clone())
                        .connect(name, socket)
                        .await
                        .map_err(|_| Error::Unavailable)?,
                )
            } else {
                Box::new(socket)
            };
            let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(
                crate::progress::Stream::new(stream, progress),
            ))
            .await
            .map_err(|_| Error::Unavailable)?;
            struct Driver(tokio::task::JoinHandle<()>);
            impl Drop for Driver {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _driver = Driver(tokio::spawn(async move {
                let _ = connection.await;
            }));
            let mut request = Request::builder()
                .method("POST")
                .uri(self.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"))
                .header(
                    "Host",
                    self.uri.authority().ok_or(Error::Unavailable)?.as_str(),
                )
                .header("Content-Type", "application/ipp")
                .header("Accept", "application/ipp")
                .header("Content-Length", body.len());
            if let (Some(username), Some(password)) = (&self.config.username, &self.config.password)
            {
                use base64::Engine as _;
                request = request.header(
                    "Authorization",
                    format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD
                            .encode(format!("{username}:{password}"))
                    ),
                );
            }
            let request = request
                .body(Full::new(body))
                .map_err(|_| Error::Unavailable)?;
            let response = sender
                .send_request(request)
                .await
                .map_err(|_| Error::Unavailable)?;
            let code = response.status();
            // Backend bodies and redirects are never forwarded to callers or followed.
            if matches!(code, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                return Err(Error::Authentication);
            }
            if code == StatusCode::OK
                && response
                    .headers()
                    .get("Content-Type")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.split(';').next())
                    .is_none_or(|s| !s.trim().eq_ignore_ascii_case("application/ipp"))
            {
                return Err(Error::Unavailable);
            }
            let bytes = Limited::new(response.into_body(), MAX_REPLY)
                .collect()
                .await
                .map_err(|_| Error::Unavailable)?
                .to_bytes();
            Ok((code, bytes))
        };
        if upload {
            crate::progress::timeout(
                Duration::from_secs(30),
                crate::progress::REPLY,
                updates,
                size,
                operation,
            )
            .await
            .map_err(|_| Error::Unavailable)?
        } else {
            tokio::time::timeout(STATUS_TIMEOUT, operation)
                .await
                .map_err(|_| Error::Unavailable)?
        }
    }
    pub async fn snapshot(&self) -> Result<Snapshot, Error> {
        let mut op = self.operation();
        op.push(requested(&[
            "printer-state",
            "printer-state-reasons",
            "printer-is-accepting-jobs",
            "document-format-supported",
            "media-ready",
            "copies-supported",
            "sides-supported",
            "media-supported",
        ]));
        let reply = self.call(0x000b, op, vec![], &[]).await?;
        let mut groups = reply.groups.into_iter().filter(|g| g.tag == 4);
        let attributes = groups.next().ok_or(Error::Unavailable)?;
        if groups.next().is_some() {
            return Err(Error::Unavailable);
        }
        let state = attributes
            .get("printer-state")
            .ok_or(Error::Unavailable)?
            .number(0x23)?;
        let accepting = attributes
            .get("printer-is-accepting-jobs")
            .ok_or(Error::Unavailable)?;
        let accepting = match accepting.values.as_slice() {
            [(0x22, b)] if b.as_slice() == [1] => true,
            [(0x22, b)] if b.as_slice() == [0] => false,
            _ => return Err(Error::Unavailable),
        };
        let reasons = attributes
            .get("printer-state-reasons")
            .ok_or(Error::Unavailable)?
            .texts(0x44)?;
        if reasons.is_empty() {
            return Err(Error::Unavailable);
        }
        let mut printer = self.report(
            map_state(state, accepting, &reasons),
            Some(format!(
                "{}; reasons: {}",
                match state {
                    3 => "idle",
                    4 => "processing",
                    5 => "stopped",
                    _ => "unknown",
                },
                self.safe_values(&reasons)
            )),
        );
        if !accepting {
            printer.detail = Some(format!(
                "not accepting jobs; {}",
                printer.detail.unwrap_or_default()
            ));
        }
        // Include bounded capabilities in status without exposing arbitrary printer text or URIs.
        for (name, tag) in [("document-format-supported", 0x49), ("media-ready", 0x44)] {
            if let Some(a) = attributes.get(name) {
                let values = if name == "media-ready" {
                    a.keyword_or_name_values()
                        .map(|values| values.into_iter().map(|(_, s)| s).collect())
                } else {
                    a.texts(tag)
                };
                if let Ok(values) = values {
                    let detail = printer.detail.get_or_insert_default();
                    detail.push_str(&format!("; {name}: {}", self.safe_values(&values)));
                }
            }
        }
        Ok(Snapshot {
            printer,
            attributes,
        })
    }
    fn safe_values(&self, values: &[&str]) -> String {
        let mut shown = display_values(values);
        for private in [
            self.config.username.as_deref(),
            self.config.password.as_deref(),
            self.config.url.as_deref(),
            self.uri.host(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        {
            shown = shown.replace(private, "[redacted]");
        }
        shown
    }
    fn report(&self, state: PrinterState, detail: Option<String>) -> Printer {
        Printer {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            model: self.config.model.clone(),
            state,
            controls: vec!["start".into(), "cancel".into()],
            detail,
            needs_clearing: Some(false),
            job_path: None,
            job_id: None,
            job_name: None,
            progress: None,
            seconds_left: None,
            nozzle_temperature: None,
            bed_temperature: None,
        }
    }
    pub fn offline(&self, error: &Error) -> Printer {
        self.report(
            PrinterState::Offline,
            Some(
                if matches!(error, Error::Authentication) {
                    "IPP authentication failed. Check the configured login."
                } else {
                    "Fresh IPP status unavailable. Check the printer connection."
                }
                .into(),
            ),
        )
    }
    pub fn check_options(&self, attributes: &Group, options: &Options) -> Result<(), Error> {
        let formats = attributes
            .get("document-format-supported")
            .ok_or(Error::Unavailable)?
            .texts(0x49)?;
        if !formats.contains(&"application/pdf") {
            return Err(Error::Refused(format!("This IPP printer does not support application/pdf. Supported formats: {}. Choose a printer that accepts PDF.", self.safe_values(&formats))));
        }
        if let Some(copies) = options.copies {
            let supported = attributes.get("copies-supported").is_some_and(|a| a.values.iter().any(|(tag, bytes)| {
                if *tag != 0x33 { return false; }
                let low = bytes.get(..4).and_then(|b| <[u8;4]>::try_from(b).ok()).map(i32::from_be_bytes);
                let high = bytes.get(4..).and_then(|b| <[u8;4]>::try_from(b).ok()).map(i32::from_be_bytes);
                matches!((low, high), (Some(l), Some(h)) if i64::from(l) <= i64::from(copies) && i64::from(copies) <= i64::from(h))
            }));
            if !supported {
                return Err(refused("The printer does not list that copies value as supported. Choose a supported copy count."));
            }
        }
        for (name, selected) in [("sides", &options.sides), ("media", &options.media)] {
            if let Some(selected) = selected {
                let supported = attributes
                    .get(&format!("{name}-supported"))
                    .is_some_and(|a| {
                        if name == "media" {
                            a.keyword_or_name_values()
                                .is_ok_and(|values| values.iter().any(|(_, s)| *s == selected))
                        } else {
                            a.texts(0x44)
                                .is_ok_and(|values| values.contains(&selected.as_str()))
                        }
                    });
                if !supported {
                    return Err(Error::Refused(format!("The printer does not list that {name} value as supported. Choose a supported value.")));
                }
            }
        }
        Ok(())
    }
    pub fn template(&self, attributes: &Group, options: &Options) -> Result<Vec<Attribute>, Error> {
        self.check_options(attributes, options)?;
        let mut template = vec![];
        if let Some(copies) = options.copies {
            template.push(Attribute::integer(
                "copies",
                0x21,
                i32::try_from(copies).map_err(|_| refused("Choose a smaller copy count."))?,
            ));
        }
        if let Some(sides) = &options.sides {
            template.push(Attribute::text("sides", 0x44, sides));
        }
        if let Some(media) = &options.media {
            let supported = attributes
                .get("media-supported")
                .ok_or(Error::Unavailable)?;
            let tag = supported
                .keyword_or_name_values()?
                .into_iter()
                .find(|(_, s)| *s == media)
                .map(|(tag, _)| tag)
                .ok_or(Error::Unavailable)?;
            template.push(Attribute::text("media", tag, media));
        }
        Ok(template)
    }
    pub async fn submit(
        &self,
        operation: u16,
        name: &str,
        template: Vec<Attribute>,
        document: &[u8],
    ) -> Result<Message, Error> {
        let mut op = self.operation();
        op.push(Attribute::text("job-name", 0x42, name));
        op.push(Attribute::boolean("ipp-attribute-fidelity", true));
        op.push(Attribute::text("document-format", 0x49, "application/pdf"));
        self.call(operation, op, template, document).await
    }
    pub async fn jobs(&self) -> Result<Vec<Group>, Error> {
        let mut jobs: Vec<Group> = vec![];
        // IPP/1.1 requires these two filters; do not depend on an extension's 'all'.
        for which in ["not-completed", "completed"] {
            let mut op = self.operation();
            op.push(Attribute::text("which-jobs", 0x44, which));
            op.push(Attribute::integer("limit", 0x21, 5000));
            op.push(requested(&["job-id", "job-name", "job-state"]));
            let reply = self.call(0x000a, op, vec![], &[]).await?;
            let returned: Vec<_> = reply.groups.into_iter().filter(|g| g.tag == 2).collect();
            if returned.len() >= 5000 {
                return Err(Error::Unavailable);
            }
            for group in returned {
                let id = group
                    .get("job-id")
                    .ok_or(Error::Unavailable)?
                    .number(0x21)?;
                if id <= 0 {
                    return Err(Error::Unavailable);
                }
                if let Some(previous) = jobs
                    .iter_mut()
                    .find(|g| g.get("job-id").and_then(|a| a.number(0x21).ok()) == Some(id))
                {
                    if previous.get("job-name") != group.get("job-name") {
                        return Err(Error::Unavailable);
                    }
                    // A job may finish between the two list requests. Retain its later state.
                    *previous = group;
                } else {
                    jobs.push(group);
                }
                if jobs.len() > 5000 {
                    return Err(Error::Unavailable);
                }
            }
        }
        Ok(jobs)
    }
    pub async fn job(&self, id: i32) -> Result<Group, Error> {
        if id <= 0 {
            return Err(Error::Unavailable);
        }
        let mut op = self.operation();
        op.push(Attribute::integer("job-id", 0x21, id));
        op.push(requested(&["job-id", "job-name", "job-state"]));
        let reply = self.call(9, op, vec![], &[]).await?;
        let mut groups = reply.groups.into_iter().filter(|g| g.tag == 2);
        let group = groups.next().ok_or(Error::Unavailable)?;
        if groups.next().is_some() {
            return Err(Error::Unavailable);
        }
        Ok(group)
    }
    pub async fn cancel(&self, id: i32) -> Result<(), Error> {
        if id <= 0 {
            return Err(Error::Unavailable);
        }
        let mut op = self.operation();
        op.push(Attribute::integer("job-id", 0x21, id));
        self.call(8, op, vec![], &[]).await?;
        Ok(())
    }
}
pub fn requested(names: &[&str]) -> Attribute {
    Attribute {
        name: "requested-attributes".into(),
        values: names
            .iter()
            .map(|n| (0x44, n.as_bytes().to_vec()))
            .collect(),
    }
}
fn display_values(values: &[&str]) -> String {
    values
        .iter()
        .take(16)
        .map(|s| {
            if s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_/+. ".contains(&b))
            {
                s.to_string()
            } else {
                "invalid-value".into()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
pub fn map_state(state: i32, accepting: bool, reasons: &[&str]) -> PrinterState {
    if !accepting {
        return PrinterState::Busy;
    }
    match state {
        3 => PrinterState::Idle,
        4 => PrinterState::Printing,
        5 => {
            let mut attention = false;
            for reason in reasons {
                let base = ["-error", "-warning", "-report"]
                    .into_iter()
                    .find_map(|suffix| reason.strip_suffix(suffix))
                    .unwrap_or(reason);
                if base == "none" {
                    continue;
                }
                if matches!(
                    base,
                    "media-empty"
                        | "media-jam"
                        | "media-needed"
                        | "media-low"
                        | "door-open"
                        | "cover-open"
                        | "interlock-open"
                        | "input-tray-missing"
                        | "output-tray-missing"
                        | "output-area-full"
                        | "output-area-almost-full"
                        | "marker-supply-empty"
                        | "marker-supply-low"
                        | "marker-waste-full"
                        | "marker-waste-almost-full"
                        | "toner-empty"
                        | "toner-low"
                        | "developer-empty"
                        | "developer-low"
                ) {
                    attention = true;
                } else {
                    return PrinterState::Error;
                }
            }
            if attention {
                PrinterState::Attention
            } else {
                PrinterState::Error
            }
        }
        _ => PrinterState::Error,
    }
}
/// The PDF input check deliberately does not execute or render the document.
pub fn check_pdf(bytes: &[u8]) -> Result<(), Error> {
    if bytes.len() > 200 << 20 {
        return Err(refused("PDF exceeds 200 MiB. Choose a smaller document."));
    }
    if !bytes.starts_with(b"%PDF-") {
        return Err(refused(
            "Choose a PDF document beginning with %PDF-. IPP printers cannot print a 3MF model.",
        ));
    }
    Ok(())
}
pub fn ready(snapshot: &Snapshot) -> Result<(), Error> {
    if snapshot.printer.state != PrinterState::Idle {
        return Err(refused("The document printer is not idle and accepting jobs. Read fresh status before starting."));
    }
    Ok(())
}
