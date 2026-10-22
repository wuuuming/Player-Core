//! Cancellable child ownership. Never wait on a child while holding an engine/UI lock.
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Waits until one of `fds` is ready for what it asks (or hung up, or in error), or until `until`
/// (`None`: no deadline). Answers whether one is; either way the caller looks again.
pub(super) fn ready(fds: &mut [libc::pollfd], until: Option<Instant>) -> io::Result<bool> {
    let timeout = until.map_or(-1, |until| {
        let left = until.saturating_duration_since(Instant::now());
        // Whole milliseconds rounded up, so the wait does not end before `until`.
        i32::try_from(left.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX)
    });
    // SAFETY: `fds` is a live, writable slice of `pollfd`s for the call.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(e)
        };
    }
    Ok(n > 0)
}

/// [`ready`] for one descriptor.
pub(super) fn ready_one(
    fd: &impl AsRawFd,
    events: libc::c_short,
    until: Option<Instant>,
) -> io::Result<bool> {
    ready(
        &mut [libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        }],
        until,
    )
}

#[derive(Default)]
pub(super) struct ChildSlot(Mutex<Option<Child>>);

impl ChildSlot {
    pub fn terminate(&self) {
        // Taking ownership, rather than retaining a pid, prevents killing a reused pid.
        if let Some(mut child) = self.0.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The worker owns cleanup even after its controller has gone away. The controller can also
/// kill/reap the child to unblock pipe reads/writes without joining a PulseAudio call.
pub(super) struct OwnedChild {
    slot: Arc<ChildSlot>,
}

impl OwnedChild {
    pub fn install(child: Child, slot: Arc<ChildSlot>) -> Self {
        *slot.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(child);
        Self { slot }
    }

    pub fn terminate(&self) {
        self.slot.terminate();
    }

    pub fn wait(&self, cancelled: impl Fn() -> bool) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + IO_TIMEOUT;
        let exited = self.exit_fd()?;
        loop {
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "decoder cancelled",
                ));
            }
            let status = self
                .slot
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
                .ok_or_else(|| io::Error::new(io::ErrorKind::Interrupted, "decoder terminated"))?
                .try_wait()?;
            if let Some(status) = status {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "decoder exit timed out",
                ));
            }
            // Until it exits (its pidfd turns readable; a `terminate` from the controller exits
            // it too) or the deadline.
            ready_one(&exited, libc::POLLIN, Some(deadline))?;
        }
    }

    /// A descriptor that turns readable when the child exits (`pidfd_open`).
    fn exit_fd(&self) -> io::Result<OwnedFd> {
        let slot = self.slot.0.lock().unwrap_or_else(PoisonError::into_inner);
        let child = slot
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Interrupted, "decoder terminated"))?;
        let pid = libc::pid_t::try_from(child.id()).map_err(io::Error::other)?;
        // SAFETY: plain values. The child is in the slot, so not reaped: the pid is still its own.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = RawFd::try_from(fd).map_err(io::Error::other)?;
        // SAFETY: a new descriptor this function owns alone.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.slot.terminate();
    }
}

/// Drain stderr independently of PulseAudio writes and child exit waits. Memory is bounded
/// to the final 16 KiB. The reader uses nonblocking IO; only the decoder worker joins it.
/// Killing the child on controller cancellation closes its pipe even if PulseAudio is stuck.
pub(super) struct StderrTail {
    /// The write end of a pipe the reader waits on with stderr: dropping it (`finish`) hangs
    /// that pipe up, which tells the reader to stop.
    stop: Option<OwnedFd>,
    reader: Option<std::thread::JoinHandle<Vec<u8>>>,
}

impl StderrTail {
    pub fn start(mut pipe: std::process::ChildStderr) -> io::Result<Self> {
        nonblocking(&pipe)?;
        let (done, stop) = self_pipe()?;
        let reader = std::thread::Builder::new()
            .name("ffmpeg-stderr".into())
            .spawn(move || {
                const MAX: usize = 16 * 1024;
                let mut tail = Vec::with_capacity(MAX);
                let mut buf = [0; 4096];
                let mut stopping: Option<Instant> = None;
                loop {
                    // A descendant retaining stderr must not keep cleanup alive indefinitely.
                    if stopping.is_some_and(|since| since.elapsed() >= Duration::from_millis(100)) {
                        break;
                    }
                    match pipe.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let remove = (tail.len() + n).saturating_sub(MAX);
                            tail.drain(..remove);
                            tail.extend_from_slice(&buf[..n]);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            if stopping.is_some() {
                                break;
                            }
                            // Until stderr has more, or `finish` hangs the stop pipe up.
                            let mut fds = [
                                libc::pollfd {
                                    fd: pipe.as_raw_fd(),
                                    events: libc::POLLIN,
                                    revents: 0,
                                },
                                libc::pollfd {
                                    fd: done.as_raw_fd(),
                                    events: libc::POLLIN,
                                    revents: 0,
                                },
                            ];
                            if ready(&mut fds, None).is_err() {
                                break;
                            }
                            if fds[1].revents != 0 {
                                stopping = Some(Instant::now());
                            }
                        }
                        Err(_) => break,
                    }
                }
                tail
            })?;
        Ok(Self {
            stop: Some(stop),
            reader: Some(reader),
        })
    }

    /// Call after reaping the decoder, so all its final diagnostics are already in the pipe.
    pub fn finish(&mut self) -> String {
        drop(self.stop.take());
        let bytes = self
            .reader
            .take()
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Drop for StderrTail {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// A pipe (read end, write end); dropping the write end wakes a [`ready`] wait on the read end.
fn self_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` is a live array of two descriptors for the call to fill.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: two new descriptors this function owns alone.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

pub(super) fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: plain values on a descriptor `pipe` owns and keeps open for the call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Capture bounded stdout without a reader thread or unbounded Command::output(). Deadline
/// covers spawn, pipe reads and child exit; errors/cancellation always kill and reap.
pub fn capture(
    command: &mut Command,
    cancelled: impl Fn() -> bool,
    timeout: Duration,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    let start = Instant::now();
    if cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "probe cancelled",
        ));
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    #[allow(
        clippy::expect_used,
        reason = "the child was spawned with piped stdout"
    )]
    let mut pipe = child.stdout.take().expect("piped stdout");
    let child = OwnedChild::install(child, Arc::default());
    nonblocking(&pipe)?;
    let mut bytes = Vec::new();
    let mut buf = [0; 8192];
    loop {
        if cancelled() || start.elapsed() >= timeout {
            return Err(io::Error::new(
                if cancelled() {
                    io::ErrorKind::Interrupted
                } else {
                    io::ErrorKind::TimedOut
                },
                "probe cancelled or timed out",
            ));
        }
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if bytes.len().saturating_add(n) > max_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "probe output too large",
                    ));
                }
                bytes.extend_from_slice(&buf[..n]);
            }
            // Until ffprobe writes, exits, or the deadline; a cancel is seen then.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                ready_one(&pipe, libc::POLLIN, Some(start + timeout))?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let status = child.wait(|| cancelled() || start.elapsed() >= timeout)?;
    if !status.success() {
        return Err(io::Error::other("probe failed"));
    }
    Ok(bytes)
}
