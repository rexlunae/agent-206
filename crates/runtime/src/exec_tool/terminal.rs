//! The pseudo-terminal of a background session started with `terminal`.
//!
//! The gateway opens the pair; the command gets the terminal side as its
//! stdin, stdout and stderr and as its controlling terminal (set up by the
//! sandbox, `a206_sandbox`), so programs see a terminal and its interrupt
//! and suspend keys work. The gateway keeps only the controlling side, read
//! and written without blocking.
use std::{
    io,
    os::{fd::OwnedFd, unix::ffi::OsStrExt},
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A new terminal pair before the command starts.
pub(super) struct Pair {
    pub(super) controller: Controller,
    /// The command's side; the gateway's copies are dropped once it starts,
    /// so the controller sees the end when the command's last copy closes.
    pub(super) device: OwnedFd,
    /// The device's path, for the macOS sandbox policy.
    pub(super) path: PathBuf,
}

/// Why a terminal could not be prepared.
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("could not open a pseudo-terminal")]
    Open(#[source] io::Error),
    #[error("could not name the pseudo-terminal")]
    Name(#[source] io::Error),
    #[error("could not prepare the pseudo-terminal for the gateway")]
    Register(#[source] io::Error),
}

impl Pair {
    pub(super) fn open(size: super::Size) -> Result<Self, TerminalError> {
        use rustix::{fs::OFlags, pty::OpenptFlags};
        let open = |errno: rustix::io::Errno| TerminalError::Open(errno.into());
        // Close-on-exec from the start where the platform allows, so no
        // other program the gateway starts meanwhile inherits the controller.
        #[cfg(target_os = "linux")]
        let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY;
        let controller = rustix::pty::openpt(flags).map_err(open)?;
        #[cfg(not(target_os = "linux"))]
        rustix::io::fcntl_setfd(&controller, rustix::io::FdFlags::CLOEXEC).map_err(open)?;
        rustix::pty::grantpt(&controller).map_err(open)?;
        rustix::pty::unlockpt(&controller).map_err(open)?;
        let name = rustix::pty::ptsname(&controller, Vec::new())
            .map_err(|errno| TerminalError::Name(errno.into()))?;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(name.as_bytes()));
        let device = rustix::fs::open(
            &path,
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(open)?;
        rustix::termios::tcsetwinsize(&device, winsize(size)).map_err(open)?;
        let register = |errno: rustix::io::Errno| TerminalError::Register(errno.into());
        rustix::io::ioctl_fionbio(&controller, true).map_err(register)?;
        let controller = async_io::Async::new_nonblocking(std::fs::File::from(controller))
            .map_err(TerminalError::Register)?;
        let path = std::fs::canonicalize(&path).map_err(TerminalError::Name)?;
        Ok(Self {
            controller: Controller(Arc::new(controller)),
            device,
            path,
        })
    }
}

/// The gateway's side of a session's terminal: reading gives the command's
/// output, writing gives it keystrokes. Clones share the one descriptor.
#[derive(Clone)]
pub(super) struct Controller(Arc<async_io::Async<std::fs::File>>);

impl Controller {
    /// Give the terminal a new window size; its foreground process group
    /// gets `SIGWINCH`, as from a resized terminal window.
    pub(super) fn resize(&self, size: super::Size) -> io::Result<()> {
        Ok(rustix::termios::tcsetwinsize(
            self.0.get_ref(),
            winsize(size),
        )?)
    }
}

fn winsize(size: super::Size) -> rustix::termios::Winsize {
    rustix::termios::Winsize {
        ws_row: size.rows,
        ws_col: size.columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Linux reports a terminal whose every device copy closed as `EIO`; that
/// is the end of its output, not a failure.
fn ended(error: &io::Error) -> bool {
    error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error())
}

impl AsyncRead for Controller {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        use std::io::Read;
        loop {
            match self.0.get_ref().read(buf.initialize_unfilled()) {
                Ok(read) => {
                    buf.advance(read);
                    return Poll::Ready(Ok(()));
                }
                Err(error) if ended(&error) => return Poll::Ready(Ok(())),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    ready!(self.0.poll_readable(cx))?;
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
}

impl AsyncWrite for Controller {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        use std::io::Write;
        loop {
            match self.0.get_ref().write(data) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    ready!(self.0.poll_writable(cx))?;
                }
                result => return Poll::Ready(result),
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    /// A terminal has no separate input to close; callers send its
    /// end-of-file key instead.
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
