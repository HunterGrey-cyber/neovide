//! This module contains adaptations of the functions found in
//! https://github.com/KillTheMule/nvim-rs/blob/master/src/create/tokio.rs

#[cfg(debug_assertions)]
use core::fmt;
#[cfg(target_os = "windows")]
use std::process::Child;
use std::{
    io::{Error, ErrorKind, Result},
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex},
    task::{Context as TaskContext, Poll},
};

use anyhow::Context;
use nvim_rs::{Handler, error::LoopError, neovim::Neovim};
#[cfg(not(target_os = "windows"))]
use tokio::process::Child;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, split},
    net::TcpStream,
    process::Command,
    spawn,
    task::JoinHandle,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

pub type NeovimWriter = Box<dyn futures::AsyncWrite + Send + Unpin + 'static>;

type BoxedReader = Box<dyn AsyncRead + Send + Unpin + 'static>;
type BoxedWriter = Box<dyn AsyncWrite + Send + Unpin + 'static>;

/// Closes nvim's stdin while every clone of the connection still holds its writer (neovibe).
///
/// An embedding ends nvim without ever sending it `:qa!`: closing its stdin makes nvim exit on EOF
/// keeping the swap files of modified buffers (`preserve_exit`), where `:qa!` deletes them. The
/// writer nvim-rs holds is shared by every clone of the `Neovim` handle, so dropping handles cannot
/// close it; this owns the real writer and hands nvim-rs a proxy, so [`HangUp::hang_up`] can drop
/// the real one -- closing the pipe -- whatever still holds the proxy. Writes after it fail with
/// `BrokenPipe`.
#[derive(Clone)]
pub struct HangUp(Arc<Mutex<Option<BoxedWriter>>>);

impl HangUp {
    fn new(writer: BoxedWriter) -> Self {
        Self(Arc::new(Mutex::new(Some(writer))))
    }

    fn writer(&self) -> HangUpWriter {
        HangUpWriter(self.0.clone())
    }

    /// Closes nvim's stdin. Idempotent.
    pub fn hang_up(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

struct HangUpWriter(Arc<Mutex<Option<BoxedWriter>>>);

impl HangUpWriter {
    fn with<T>(
        &self,
        f: impl FnOnce(Pin<&mut BoxedWriter>) -> Poll<Result<T>>,
        hung_up: impl FnOnce() -> Poll<Result<T>>,
    ) -> Poll<Result<T>> {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(writer) => f(Pin::new(writer)),
            None => hung_up(),
        }
    }
}

impl AsyncWrite for HangUpWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        self.with(|w| w.poll_write(cx, buf), || Poll::Ready(Err(ErrorKind::BrokenPipe.into())))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<()>> {
        self.with(|w| w.poll_flush(cx), || Poll::Ready(Err(ErrorKind::BrokenPipe.into())))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<()>> {
        self.with(|w| w.poll_shutdown(cx), || Poll::Ready(Ok(())))
    }
}

pub struct NeovimSession {
    pub neovim: Neovim<NeovimWriter>,
    /// Closes nvim's stdin (neovibe; [`HangUp`]).
    pub hang_up: HangUp,
    pub io_handle: JoinHandle<std::result::Result<(), Box<LoopError>>>,
    pub neovim_process: Option<Child>,
    pub stderr_task: Option<JoinHandle<Vec<String>>>,
    #[cfg(not(target_os = "windows"))]
    pub stdin_fd: Option<rustix::fd::OwnedFd>,
}

#[cfg(debug_assertions)]
impl fmt::Debug for NeovimSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NeovimSession").field("io_handle", &self.io_handle).finish()
    }
}

impl NeovimSession {
    pub async fn new(
        instance: NeovimInstance,
        handler: impl Handler<Writer = NeovimWriter>,
    ) -> anyhow::Result<Self> {
        // This needs to be done before the process is spawned, since the file descriptors are
        // inherited on unix-like systems
        #[cfg(not(target_os = "windows"))]
        let stdin_fd = instance.forward_stdin();
        let (reader, writer, stderr_reader, neovim_process) = instance.connect().await?;
        let hang_up = HangUp::new(writer);
        let writer = hang_up.writer();
        // Spawn a background task to read from stderr
        let stderr_task = stderr_reader.map(|reader| {
            tokio::spawn(async move {
                let mut lines = Vec::new();
                let mut reader = BufReader::new(reader).lines();
                while let Some(line) = reader.next_line().await.unwrap_or_default() {
                    log::error!("{line}");
                    lines.push(line);
                }
                lines
            })
        });
        let handshake_message = "NeovideToNeovimMagicHandshakeMessage";

        let handshake_res = Neovim::<NeovimWriter>::handshake(
            reader.compat(),
            Box::new(writer.compat_write()),
            handler,
            handshake_message,
        )
        .await;
        match handshake_res {
            Err(err) => {
                if let Some(stderr_task) = stderr_task {
                    let stderr = "stderr output:\n".to_owned() + &stderr_task.await?.join("\n");
                    Err(err).context(stderr)
                } else {
                    Err(err.into())
                }
            }
            Ok((neovim, io)) => {
                let io_handle = spawn(io);

                Ok(Self {
                    neovim,
                    hang_up,
                    io_handle,
                    neovim_process,
                    stderr_task,
                    #[cfg(not(target_os = "windows"))]
                    stdin_fd,
                })
            }
        }
    }
}

/// An existing or future Neovim instance along with a means for establishing a connection.
#[derive(Debug)]
pub enum NeovimInstance {
    /// A new embedded instance to be spawned by the given command.
    Embedded(Command),

    /// An existing instance listening on `address`.
    ///
    /// Interprets `address` in the same way as `:help --server`: If it contains a `:` it's
    /// interpreted as a TCP/IPv4/IPv6 address. Otherwise it's interpreted as a named pipe or Unix
    /// domain socket path. Spawns and connects to an embedded Neovim instance.
    Server { address: String },
}

impl NeovimInstance {
    async fn connect(
        self,
    ) -> Result<(BoxedReader, BoxedWriter, Option<BoxedReader>, Option<Child>)> {
        match self {
            NeovimInstance::Embedded(cmd) => Self::spawn_process(cmd).await,
            NeovimInstance::Server { address } => Self::connect_to_server(address)
                .await
                .map(|(reader, writer)| (reader, writer, None, None)),
        }
    }

    async fn spawn_process(
        #[cfg(not(target_os = "windows"))] mut cmd: Command,
        #[cfg(target_os = "windows")] cmd: Command,
    ) -> Result<(BoxedReader, BoxedWriter, Option<BoxedReader>, Option<Child>)> {
        log::debug!("Starting neovim with: {cmd:?}");

        // On Windows, the stdio pipes we get for a spawned child are overlapped
        // handles. if we pass those handles through tokio's process wrapper, it
        // turns them into Blocking<ArcFile>, which can eventually call
        // std::sys::pal::windows::handle::Handle::synchronous_read.
        //
        // Rust intentionally aborts on that path when an overlapped operation is
        // still pending instead of completing synchronously.
        //
        // See https://github.com/rust-lang/rust/issues/81357
        //
        // We avoid that path by using std::process::Command directly on
        // Windows, taking the raw pipe handles, and wrapping them in
        // NamedPipeServer, which supports overlapped pipe I/O.
        #[cfg(target_os = "windows")]
        let mut cmd = cmd.into_std();

        let mut child =
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let reader_inner = child.stdout.take().ok_or_else(|| Error::other("Can't open stdout"))?;
        let writer_inner = child.stdin.take().ok_or_else(|| Error::other("Can't open stdin"))?;
        let stderr_reader_inner =
            child.stderr.take().ok_or_else(|| Error::other("Can't open stderr"))?;

        let reader: BoxedReader;
        let writer: BoxedWriter;
        let stderr_reader: BoxedReader;

        #[cfg(not(target_os = "windows"))]
        {
            reader = Box::new(reader_inner);
            writer = Box::new(writer_inner);
            stderr_reader = Box::new(stderr_reader_inner);
        }

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::io::IntoRawHandle;
            use tokio::net::windows::named_pipe::NamedPipeServer;
            reader = Box::new(unsafe {
                NamedPipeServer::from_raw_handle(reader_inner.into_raw_handle())
            }?);
            writer = Box::new(unsafe {
                NamedPipeServer::from_raw_handle(writer_inner.into_raw_handle())
            }?);
            stderr_reader = Box::new(unsafe {
                NamedPipeServer::from_raw_handle(stderr_reader_inner.into_raw_handle())
            }?);
        }

        Ok((reader, writer, Some(stderr_reader), Some(child)))
    }

    async fn connect_to_server(address: String) -> Result<(BoxedReader, BoxedWriter)> {
        if address.contains(':') {
            Ok(Self::split(TcpStream::connect(address).await?))
        } else {
            #[cfg(unix)]
            return Ok(Self::split(tokio::net::UnixStream::connect(address).await?));

            #[cfg(windows)]
            {
                // Fixup the address if the pipe on windows does not start with \\.\pipe\.
                let address = if address.starts_with("\\\\.\\pipe\\") {
                    address
                } else {
                    format!("\\\\.\\pipe\\{address}")
                };
                Ok(Self::split(
                    tokio::net::windows::named_pipe::ClientOptions::new().open(address)?,
                ))
            }

            #[cfg(not(any(unix, windows)))]
            Err(Error::new(
                ErrorKind::Unsupported,
                "Unix Domain Sockets and Named Pipes are not supported on this platform",
            ))
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn forward_stdin(&self) -> Option<rustix::fd::OwnedFd> {
        use rustix::fs::{FileType, fstat};
        use std::os::fd::AsFd;

        // stdin should be forwarded only in embedded mode when stdio is piped or redirected
        match self {
            Self::Embedded(..) => {
                let stdin = std::io::stdin();
                let should_forward = fstat(stdin.as_fd())
                    .map(|stat| match FileType::from_raw_mode(stat.st_mode) {
                        FileType::RegularFile => true,
                        #[cfg(not(target_os = "wasi"))]
                        FileType::Fifo | FileType::Socket => true,
                        _ => false,
                    })
                    .unwrap_or(false);

                // We have to use rustix here, since the Rust standard library currently sets O_CLOEXEC
                // on all file handles. And there's no way to pass file handles to subprocesses.
                // See [Tracking Issue for std::os::fd::CommandExt::fd](https://github.com/rust-lang/rust/issues/144989)
                should_forward.then(|| rustix::io::dup(stdin).ok()).flatten()
            }
            Self::Server { .. } => None,
        }
    }

    fn split(
        stream: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
    ) -> (BoxedReader, BoxedWriter) {
        let (reader, writer) = split(stream);
        (Box::new(reader), Box::new(writer))
    }
}
