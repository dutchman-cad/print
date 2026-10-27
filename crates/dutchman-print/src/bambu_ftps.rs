//! Bounded implicit FTPS upload with protected, resumed passive data connections.

use crate::backend::Error;
use rustls::pki_types::ServerName;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::{client::TlsStream, TlsConnector};

type Control = BufReader<crate::progress::Stream<TlsStream<TcpStream>>>;

fn refused(message: &str) -> Error {
    Error::Refused(message.into())
}

async fn reply(control: &mut Control) -> Result<(u16, String), Error> {
    let mut first = None;
    // Bound both individual lines and multiline replies from the server.
    for _ in 0..32 {
        let mut bytes = Vec::new();
        let count = (&mut *control)
            .take(4097)
            .read_until(b'\n', &mut bytes)
            .await
            .map_err(|_| Error::Unavailable)?;
        if count == 0 || count > 4096 || !bytes.ends_with(b"\r\n") {
            return Err(refused(
                "Cannot read the FTPS reply. Check printer compatibility.",
            ));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| Error::Unavailable)?;
        let code = text.get(..3).and_then(|s| s.parse::<u16>().ok());
        if first.is_none() {
            first = code;
            if first.is_none() || !matches!(bytes.get(3), Some(b' ' | b'-')) {
                return Err(Error::Unavailable);
            }
        }
        if code == first && bytes.get(3) == Some(&b' ') {
            return Ok((code.ok_or(Error::Unavailable)?, text.trim_end().into()));
        }
    }
    Err(refused(
        "FTPS reply is too long. Check printer compatibility.",
    ))
}

async fn command(control: &mut Control, text: &str) -> Result<(u16, String), Error> {
    control
        .get_mut()
        .write_all(format!("{text}\r\n").as_bytes())
        .await
        .map_err(|_| Error::Unavailable)?;
    control
        .get_mut()
        .flush()
        .await
        .map_err(|_| Error::Unavailable)?;
    reply(control).await
}

fn accepted(code: u16, expected: &[u16]) -> Result<(), Error> {
    if matches!(code, 530 | 532) {
        Err(Error::Authentication)
    } else if expected.contains(&code) {
        Ok(())
    } else {
        // Replies may echo credentials or paths; keep server text out of diagnostics.
        Err(refused("Printer refused the FTPS operation. Check storage, LAN control permission, and printer compatibility."))
    }
}

fn passive_port(text: &str) -> Result<u16, Error> {
    let fields = text
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(fields, _)| fields)
        .ok_or(Error::Unavailable)?;
    let values = fields
        .split(',')
        .map(|v| v.trim().parse::<u8>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Error::Unavailable)?;
    match values.as_slice() {
        [_, _, _, _, high, low] => {
            let port = u16::from(*high) * 256 + u16::from(*low);
            if port == 0 {
                return Err(Error::Unavailable);
            }
            Ok(port)
        }
        _ => Err(Error::Unavailable),
    }
}

pub(super) async fn upload(
    config: &crate::config::Printer,
    filename: &str,
    bytes: &[u8],
) -> Result<(), Error> {
    upload_with_idle(
        config,
        filename,
        bytes,
        Duration::from_secs(30),
        crate::progress::REPLY,
    )
    .await
}

async fn upload_with_idle(
    config: &crate::config::Printer,
    filename: &str,
    bytes: &[u8],
    idle: Duration,
    reply_budget: Duration,
) -> Result<(), Error> {
    // A basename cannot inject FTP commands, traverse storage, or alter URL semantics.
    if filename.is_empty()
        || filename.len() > 128
        || !filename.ends_with(".gcode.3mf")
        || !filename
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        || filename.contains("..")
        || bytes.is_empty()
        || bytes.len() > 200 << 20
    {
        return Err(refused(
            "Use a sliced .gcode.3mf basename and at most 200 MiB of prepared bytes.",
        ));
    }
    let mut staged = false;
    let (progress, updates) = crate::progress::channel();
    let operation = async {
        let host = config.host.as_deref().ok_or(Error::Unavailable)?;
        let serial = ServerName::try_from(config.serial.clone().unwrap_or_default())
            .map_err(|_| Error::Unavailable)?;
        // One configuration and serial name share the control session with the data socket.
        // This cache is private to the upload, never shared with another printer or MQTT.
        let connector = TlsConnector::from(Arc::new(super::tls(config).map_err(Error::Refused)?));
        let socket = TcpStream::connect((host, config.ftps_port.unwrap_or(990)))
            .await
            .map_err(|_| Error::Unavailable)?;
        let peer = socket.peer_addr().map_err(|_| Error::Unavailable)?.ip();
        let socket = connector.connect(serial.clone(), socket).await.map_err(|_| {
            refused("FTPS TLS identity could not be verified. Check this printer's CA or fingerprint.")
        })?;
        let mut control = BufReader::new(crate::progress::Stream::new(socket, progress.clone()));
        accepted(reply(&mut control).await?.0, &[220])?;
        let user = command(&mut control, "USER bblp").await?.0;
        accepted(user, &[230, 331])?;
        if user == 331 {
            let password = config.access_code.as_deref().ok_or(Error::Authentication)?;
            accepted(
                command(&mut control, &format!("PASS {password}")).await?.0,
                &[230],
            )?;
        }
        accepted(command(&mut control, "PBSZ 0").await?.0, &[200])?;
        accepted(command(&mut control, "PROT P").await?.0, &[200])?;
        accepted(command(&mut control, "TYPE I").await?.0, &[200])?;
        let (code, text) = command(&mut control, "PASV").await?;
        accepted(code, &[227])?;
        // Ignore the advertised address: a passive reply must not redirect credentials or bytes.
        let data = TcpStream::connect((peer, passive_port(&text)?))
            .await
            .map_err(|_| Error::Unavailable)?;
        staged = true;
        accepted(
            command(&mut control, &format!("STOR {filename}")).await?.0,
            &[125, 150],
        )?;
        let data = connector
            .connect(serial, data)
            .await
            .map_err(|_| refused("FTPS data TLS could not be verified. No print was started."))?;
        if data.get_ref().1.handshake_kind() != Some(rustls::HandshakeKind::Resumed) {
            return Err(refused(
                "FTPS data TLS did not resume the control session. No file bytes were sent.",
            ));
        }
        let mut data = crate::progress::Stream::new(data, progress.clone());
        data.write_all(bytes)
            .await
            .map_err(|_| Error::Unavailable)?;
        data.flush().await.map_err(|_| Error::Unavailable)?;
        data.shutdown().await.map_err(|_| Error::Unavailable)?;
        // Keep the data socket alive until the server confirms receipt of all bytes.
        accepted(reply(&mut control).await?.0, &[226])?;
        drop(data);
        let (code, size) = command(&mut control, &format!("SIZE {filename}")).await?;
        accepted(code, &[213])?;
        if size.get(4..).and_then(|s| s.trim().parse::<u64>().ok()) != Some(bytes.len() as u64) {
            return Err(refused(
                "Uploaded file size differs from the prepared bytes. No print was started.",
            ));
        }
        accepted(command(&mut control, "QUIT").await?.0, &[221])?;
        Ok(())
    };
    let result =
        crate::progress::timeout(idle, reply_budget, updates, bytes.len() as u64, operation)
            .await
            .map_err(|_| refused("FTPS upload stopped making progress. No print was started."))
            .and_then(|result| result);
    if let Err(error) = result {
        if !staged {
            return Err(error);
        }
        if tokio::time::timeout(Duration::from_secs(15), delete(config, filename))
            .await
            .is_ok_and(|r| r.is_ok())
        {
            return Err(error);
        }
        return Err(refused("FTPS upload failed and cleanup is not confirmed. Inspect storage and remove the staged file before preparing again. No print was started."));
    }
    Ok(())
}

async fn delete(config: &crate::config::Printer, filename: &str) -> Result<(), Error> {
    let host = config.host.as_deref().ok_or(Error::Unavailable)?;
    let serial = ServerName::try_from(config.serial.clone().unwrap_or_default())
        .map_err(|_| Error::Unavailable)?;
    let connector = TlsConnector::from(Arc::new(super::tls(config).map_err(Error::Refused)?));
    let socket = TcpStream::connect((host, config.ftps_port.unwrap_or(990)))
        .await
        .map_err(|_| Error::Unavailable)?;
    let socket = connector
        .connect(serial, socket)
        .await
        .map_err(|_| Error::Unavailable)?;
    let (progress, _) = crate::progress::channel();
    let mut control = BufReader::new(crate::progress::Stream::new(socket, progress));
    accepted(reply(&mut control).await?.0, &[220])?;
    let user = command(&mut control, "USER bblp").await?.0;
    accepted(user, &[230, 331])?;
    if user == 331 {
        accepted(
            command(
                &mut control,
                &format!(
                    "PASS {}",
                    config.access_code.as_deref().ok_or(Error::Authentication)?
                ),
            )
            .await?
            .0,
            &[230],
        )?;
    }
    accepted(
        command(&mut control, &format!("DELE {filename}")).await?.0,
        &[250],
    )
}

#[cfg(test)]
#[path = "bambu_ftps_tests.rs"]
mod tests;
