use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::oneshot,
};

pub type FallibleSimplexReader = FallibleReader<tokio::io::ReadHalf<tokio::io::SimplexStream>>;

pub struct FallibleSignal {
    sender: Option<oneshot::Sender<Option<String>>>,
}

impl FallibleSignal {
    #[inline]
    pub fn succeed(mut self) {
        if let Some(sender) = self.sender.take() {
            sender.send(None).ok();
        }
    }

    #[inline]
    pub fn fail(mut self, err: impl std::fmt::Display) {
        if let Some(sender) = self.sender.take() {
            sender.send(Some(err.to_string())).ok();
        }
    }
}

impl Drop for FallibleSignal {
    #[inline]
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            sender
                .send(Some("producer ended without completing the stream".into()))
                .ok();
        }
    }
}

enum Outcome {
    Waiting(oneshot::Receiver<Option<String>>),
    Succeeded,
    Failed(String),
}

pub struct FallibleReader<R> {
    inner: R,
    outcome: Outcome,
}

impl<R> FallibleReader<R> {
    #[inline]
    pub fn new(inner: R) -> (Self, FallibleSignal) {
        let (sender, receiver) = oneshot::channel();

        (
            Self {
                inner,
                outcome: Outcome::Waiting(receiver),
            },
            FallibleSignal {
                sender: Some(sender),
            },
        )
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for FallibleReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        if crate::unlikely(buf.remaining() == 0) {
            return Poll::Ready(Ok(()));
        }

        let filled = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            // Bytes came through, so the producer is still mid-stream and its
            // outcome cannot matter yet. Anything already buffered is handed
            // over before a failure is surfaced.
            Poll::Ready(Ok(())) if buf.filled().len() != filled => return Poll::Ready(Ok(())),
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            // Either EOF, or drained but still open. Both mean the inner stream
            // has nothing left to give right now, so the outcome decides.
            Poll::Ready(Ok(())) | Poll::Pending => {}
        }

        // Note the `Poll::Pending` arm above: dropping a `tokio::io::simplex`
        // write half does *not* close the stream, only `shutdown` does, and the
        // producer failure paths cannot shut down a writer they no longer own.
        // Consulting the outcome on EOF alone would therefore park here forever
        // whenever a producer failed, instead of reporting the failure.
        if let Outcome::Waiting(receiver) = &mut this.outcome {
            let resolved = match Pin::new(receiver).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(None)) => Outcome::Succeeded,
                Poll::Ready(Ok(Some(err))) => Outcome::Failed(err),
                Poll::Ready(Err(_)) => {
                    Outcome::Failed("producer was dropped without completing the stream".into())
                }
            };

            this.outcome = resolved;
        }

        match &this.outcome {
            // Kept, rather than taken, so a failure keeps being reported instead
            // of decaying into a clean end of stream on the next read.
            Outcome::Failed(err) => Poll::Ready(Err(std::io::Error::other(err.clone()))),
            // A producer that reported success has no more bytes to hand over,
            // so a drained stream is the end of the stream.
            _ => Poll::Ready(Ok(())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn stream() -> (
        FallibleSimplexReader,
        tokio::io::WriteHalf<tokio::io::SimplexStream>,
        FallibleSignal,
    ) {
        let (reader, writer) = tokio::io::simplex(64);
        let (reader, signal) = FallibleReader::new(reader);

        (reader, writer, signal)
    }

    #[test]
    fn succeeding_producer_reads_to_eof() {
        tokio_test::block_on(async {
            let (mut reader, mut writer, signal) = stream();

            tokio::spawn(async move {
                writer.write_all(b"archive").await.unwrap();
                drop(writer);
                signal.succeed();
            });

            let mut out = Vec::new();
            reader.read_to_end(&mut out).await.unwrap();

            assert_eq!(out, b"archive");
        });
    }

    #[test]
    fn failing_producer_errors_instead_of_ending() {
        tokio_test::block_on(async {
            let (mut reader, mut writer, signal) = stream();

            tokio::spawn(async move {
                writer.write_all(b"trunc").await.unwrap();
                drop(writer);
                signal.fail("disk went away");
            });

            let mut out = Vec::new();
            let err = reader.read_to_end(&mut out).await.unwrap_err();

            // The bytes already produced still surface; what must not happen is
            // the read completing as if the producer had finished.
            assert_eq!(out, b"trunc");
            assert!(err.to_string().contains("disk went away"), "{err}");
        });
    }

    #[test]
    fn failure_recorded_after_writer_drop_is_still_observed() {
        // The producer releases its writer before recording the outcome, so the
        // reader hits EOF while the outcome is still pending. It must wait
        // rather than report a clean end of stream.
        tokio_test::block_on(async {
            let (mut reader, writer, signal) = stream();

            tokio::spawn(async move {
                drop(writer);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                signal.fail("late failure");
            });

            let mut out = Vec::new();
            let err = reader.read_to_end(&mut out).await.unwrap_err();

            assert!(out.is_empty());
            assert!(err.to_string().contains("late failure"), "{err}");
        });
    }

    #[test]
    fn failure_is_reported_on_every_subsequent_read() {
        tokio_test::block_on(async {
            let (mut reader, writer, signal) = stream();

            tokio::spawn(async move {
                drop(writer);
                signal.fail("disk went away");
            });

            let mut buf = [0; 8];
            let err = reader.read(&mut buf).await.unwrap_err();
            assert!(err.to_string().contains("disk went away"), "{err}");

            // Reading on past the failure must not paper over it by reporting a
            // clean end of stream.
            let err = reader.read(&mut buf).await.unwrap_err();
            assert!(err.to_string().contains("disk went away"), "{err}");
        });
    }

    #[test]
    fn dropped_producer_is_treated_as_failure() {
        tokio_test::block_on(async {
            let (mut reader, writer, signal) = stream();

            tokio::spawn(async move {
                drop(writer);
                drop(signal);
            });

            let mut out = Vec::new();
            let err = reader.read_to_end(&mut out).await.unwrap_err();

            assert!(out.is_empty());
            assert!(err.to_string().contains("without completing"), "{err}");
        });
    }
}
