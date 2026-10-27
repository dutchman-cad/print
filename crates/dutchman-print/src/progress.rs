//! Transport progress bounds uploads without imposing a minimum average transfer rate.
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

pub(crate) struct Stream<S> {
    inner: S,
    progress: watch::Sender<Instant>,
}
impl<S> Stream<S> {
    pub(crate) fn new(inner: S, progress: watch::Sender<Instant>) -> Self {
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
            self.progress.send_replace(Instant::now());
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
        if matches!(result, Poll::Ready(Ok(n)) if n > 0) {
            self.progress.send_replace(Instant::now());
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
pub(crate) async fn timeout<F: Future>(
    duration: Duration,
    mut progress: watch::Receiver<Instant>,
    future: F,
) -> Result<F::Output, ()> {
    tokio::pin!(future);
    loop {
        let deadline = *progress.borrow_and_update() + duration;
        tokio::select! {
            biased;
            value = &mut future => return Ok(value),
            _ = tokio::time::sleep_until(deadline) => {
                if progress.borrow().elapsed() >= duration { return Err(()); }
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
        let (tx, rx) = watch::channel(Instant::now());
        let mut stream = Stream::new(socket, tx);
        let producer = async {
            for _ in 0..10 {
                peer.write_all(&[1; 16]).await.unwrap();
                // Deliberately model a link that makes progress slowly.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let consumer = timeout(Duration::from_millis(100), rx, async {
            let mut bytes = [0; 160];
            stream.read_exact(&mut bytes).await.unwrap();
        });
        let ((), result) = tokio::join!(producer, consumer);
        assert!(result.is_ok());
        let (tx, rx) = watch::channel(Instant::now());
        let _keep_channel = tx;
        assert!(
            timeout(Duration::from_millis(20), rx, std::future::pending::<()>())
                .await
                .is_err()
        );
    }
}
