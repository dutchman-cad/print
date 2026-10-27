//! Transport progress bounds uploads without imposing a minimum average transfer rate.
//!
//! While the request is still being sent, any byte moved in either direction resets the idle
//! budget. Once every body byte has been handed to the socket, the reply gets its own budget:
//! a large socket buffer, or a printer storing the file, can delay the reply without any
//! further byte moving.
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::watch,
    time::Instant,
};

/// The time bytes last moved, and how many bytes have been written so far.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Progress {
    pub(crate) at: Instant,
    pub(crate) written: u64,
}
pub(crate) fn channel() -> (watch::Sender<Progress>, watch::Receiver<Progress>) {
    watch::channel(Progress {
        at: Instant::now(),
        written: 0,
    })
}
/// Restarts the idle budget, for example before reusing a connection.
pub(crate) fn touch(progress: &watch::Sender<Progress>) {
    progress.send_modify(|p| p.at = Instant::now());
}
/// How long a reply may take once the whole request body has been written.
pub(crate) const REPLY: Duration = Duration::from_secs(60);

pub(crate) struct Stream<S> {
    inner: S,
    progress: watch::Sender<Progress>,
}
impl<S> Stream<S> {
    pub(crate) fn new(inner: S, progress: watch::Sender<Progress>) -> Self {
        Self { inner, progress }
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for Stream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            touch(&self.progress);
        }
        result
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for Stream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if let Poll::Ready(Ok(n)) = result {
            if n > 0 {
                self.progress.send_modify(|p| {
                    p.at = Instant::now();
                    p.written += n as u64;
                });
            }
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
/// Runs `future` until no byte moves for `idle` while `body` bytes are still to be written,
/// or for `reply` after they have all been written.
pub(crate) async fn timeout<F: Future>(
    idle: Duration,
    reply: Duration,
    mut progress: watch::Receiver<Progress>,
    body: u64,
    future: F,
) -> Result<F::Output, ()> {
    tokio::pin!(future);
    let target = progress.borrow().written + body;
    let limit = |p: &Progress| if p.written >= target { reply } else { idle };
    loop {
        let current = *progress.borrow_and_update();
        let deadline = current.at + limit(&current);
        tokio::select! {
            biased;
            value = &mut future => return Ok(value),
            _ = tokio::time::sleep_until(deadline) => {
                let current = *progress.borrow();
                if current.at.elapsed() >= limit(&current) { return Err(()); }
            }
            result = progress.changed() => { if result.is_err() { return Err(()); } }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn progressing_transfer_outlasts_idle_budget_and_stall_expires() {
        let (socket, mut peer) = tokio::io::duplex(16);
        let (tx, rx) = channel();
        let mut stream = Stream::new(socket, tx);
        let producer = async {
            for _ in 0..10 {
                peer.write_all(&[1; 16]).await.unwrap();
                // Deliberately model a link that makes progress slowly.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let consumer = timeout(Duration::from_millis(100), REPLY, rx, 160, async {
            let mut bytes = [0; 160];
            stream.read_exact(&mut bytes).await.unwrap();
        });
        let ((), result) = tokio::join!(producer, consumer);
        assert!(result.is_ok());
        let (tx, rx) = channel();
        let _keep_channel = tx;
        assert!(timeout(
            Duration::from_millis(20),
            REPLY,
            rx,
            1,
            std::future::pending::<()>()
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn reply_after_the_whole_body_gets_its_own_budget() {
        let (socket, mut peer) = tokio::io::duplex(1 << 16);
        let (tx, rx) = channel();
        let mut stream = Stream::new(socket, tx);
        let request = timeout(
            Duration::from_millis(50),
            Duration::from_secs(5),
            rx,
            32,
            async {
                stream.write_all(&[1; 32]).await.unwrap();
                let mut reply = [0; 2];
                stream.read_exact(&mut reply).await.unwrap();
                reply
            },
        );
        let printer = async {
            let mut body = [0; 32];
            peer.read_exact(&mut body).await.unwrap();
            // The reply comes well after the idle budget, with no bytes moving meanwhile.
            tokio::time::sleep(Duration::from_millis(200)).await;
            peer.write_all(b"ok").await.unwrap();
        };
        let (result, ()) = tokio::join!(request, printer);
        assert_eq!(result.unwrap(), *b"ok");
    }
}
