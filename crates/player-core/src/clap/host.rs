//! The plugin host thread: CLAP's "main thread", one per process and not the UI's. It makes every
//! main-thread call (create, activate, state, the plugin's window, `on_main_thread`) and runs its
//! own loop: jobs other threads hand it, the timers plugins register (`clap.timer-support`), on
//! Linux the file descriptors they register (`clap.posix-fd-support`), on Windows the messages
//! of the plugins' windows.

use super::instance::Instance;
use std::cell::{Cell, RefCell};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock, PoisonError, Weak};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

type Job = Box<dyn FnOnce() + Send>;

/// How long a caller waits for the host thread before it gives up on a plugin.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

struct Host {
    jobs: Mutex<Sender<Job>>,
    thread: ThreadId,
    waker: os::Waker,
}

static HOST: OnceLock<Result<Host, String>> = OnceLock::new();

thread_local! {
    /// Set while this thread processes a plugin (`clap.thread-check`'s audio thread).
    static AUDIO: Cell<bool> = const { Cell::new(false) };
    /// The host thread's timers and descriptors; empty on every other thread.
    static MAIN: RefCell<MainState> = RefCell::new(MainState::default());
}

fn host() -> Result<&'static Host, String> {
    HOST.get_or_init(start).as_ref().map_err(Clone::clone)
}

fn start() -> Result<Host, String> {
    let (jobs, inbox) = mpsc::channel::<Job>();
    let (ready_tx, ready) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("clap-host".into())
        .spawn(move || {
            let waker = os::Waker::new();
            let failed = waker.is_err();
            let _ = ready_tx.send(waker);
            if !failed {
                main_loop(&inbox);
            }
        })
        .map_err(|error| format!("cannot start the plugin host: {error}"))?;
    let waker = ready
        .recv()
        .map_err(|_| "the plugin host did not start".to_string())??;
    Ok(Host {
        jobs: Mutex::new(jobs),
        thread: thread.thread().id(),
        waker,
    })
}

/// Whether this is the host thread.
pub(super) fn is_main_thread() -> bool {
    HOST.get()
        .and_then(|host| host.as_ref().ok())
        .is_some_and(|host| host.thread == std::thread::current().id())
}

pub(super) fn is_audio_thread() -> bool {
    AUDIO.with(Cell::get)
}

/// Runs `f` as the plugins' audio thread.
pub(super) fn as_audio_thread<R>(f: impl FnOnce() -> R) -> R {
    let was = AUDIO.with(|audio| audio.replace(true));
    let result = f();
    AUDIO.with(|audio| audio.set(was));
    result
}

/// The host thread runs (it starts with the first call that needs it).
pub(super) fn ready() -> Result<(), String> {
    host().map(|_| ())
}

/// Hands `job` to the host thread without waiting for it.
pub(super) fn post(job: impl FnOnce() + Send + 'static) {
    if let Ok(host) = host() {
        let sent = host
            .jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(Box::new(job))
            .is_ok();
        if sent {
            host.waker.wake();
        }
    }
}

/// Runs `job` on the host thread and waits for its answer (at once when called there).
pub(super) fn run<R: Send + 'static>(
    job: impl FnOnce() -> R + Send + 'static,
) -> Result<R, String> {
    host()?;
    if is_main_thread() {
        return Ok(job());
    }
    let (answer, receive) = mpsc::sync_channel(1);
    post(move || {
        let _ = answer.send(job());
    });
    receive
        .recv_timeout(ANSWER_TIMEOUT)
        .map_err(|_| "the plugin host is not answering".to_string())
}

#[derive(Default)]
struct MainState {
    timers: Vec<Timer>,
    /// Jobs for later ([`post_after`]), with when they are due.
    later: Vec<(Instant, Job)>,
    next_timer: u32,
    #[cfg(target_os = "linux")]
    fds: Vec<Fd>,
}

struct Timer {
    id: u32,
    owner: Weak<Instance>,
    period: Duration,
    due: Instant,
}

#[cfg(target_os = "linux")]
struct Fd {
    fd: i32,
    flags: u32,
    owner: Weak<Instance>,
}

#[cfg(target_os = "linux")]
impl Fd {
    /// It is `fd` as `owner` registered it.
    fn is(&self, owner: &Weak<Instance>, fd: i32) -> bool {
        self.fd == fd && self.owner.ptr_eq(owner)
    }
}

/// Hands `job` to the host thread to run once `delay` has passed.
pub(super) fn post_after(delay: Duration, job: impl FnOnce() + Send + 'static) {
    let job: Job = Box::new(job);
    post(move || {
        MAIN.with(|main| main.borrow_mut().later.push((Instant::now() + delay, job)));
    });
}

/// `clap.timer-support`'s register, on the host thread: the timer's id.
pub(super) fn register_timer(owner: Weak<Instance>, period_ms: u32) -> Option<u32> {
    if !is_main_thread() {
        return None;
    }
    MAIN.with(|main| {
        let mut main = main.borrow_mut();
        main.next_timer = main.next_timer.wrapping_add(1);
        let id = main.next_timer;
        let period = Duration::from_millis(u64::from(period_ms.max(1)));
        main.timers.push(Timer {
            id,
            owner,
            period,
            due: Instant::now() + period,
        });
        Some(id)
    })
}

pub(super) fn unregister_timer(owner: &Weak<Instance>, id: u32) -> bool {
    is_main_thread()
        && MAIN.with(|main| {
            let timers = &mut main.borrow_mut().timers;
            let before = timers.len();
            timers.retain(|timer| !(timer.id == id && timer.owner.ptr_eq(owner)));
            timers.len() != before
        })
}

/// Forgets every timer and descriptor of an instance being destroyed.
pub(super) fn forget(owner: *const Instance) {
    MAIN.with(|main| {
        let mut main = main.borrow_mut();
        main.timers.retain(|timer| timer.owner.as_ptr() != owner);
        #[cfg(target_os = "linux")]
        main.fds.retain(|fd| fd.owner.as_ptr() != owner);
    });
}

#[cfg(target_os = "linux")]
pub(super) fn register_fd(owner: Weak<Instance>, fd: i32, flags: u32) -> bool {
    is_main_thread()
        && MAIN.with(|main| {
            let fds = &mut main.borrow_mut().fds;
            if fds.iter().any(|known| known.fd == fd) {
                return false;
            }
            fds.push(Fd { fd, flags, owner });
            true
        })
}

#[cfg(target_os = "linux")]
pub(super) fn modify_fd(owner: &Weak<Instance>, fd: i32, flags: u32) -> bool {
    is_main_thread()
        && MAIN.with(|main| {
            let mut main = main.borrow_mut();
            match main.fds.iter_mut().find(|known| known.is(owner, fd)) {
                Some(known) => {
                    known.flags = flags;
                    true
                }
                None => false,
            }
        })
}

#[cfg(target_os = "linux")]
pub(super) fn unregister_fd(owner: &Weak<Instance>, fd: i32) -> bool {
    is_main_thread()
        && MAIN.with(|main| {
            let fds = &mut main.borrow_mut().fds;
            let before = fds.len();
            fds.retain(|known| !(known.is(owner, fd)));
            fds.len() != before
        })
}

fn main_loop(inbox: &Receiver<Job>) {
    loop {
        loop {
            match inbox.try_recv() {
                Ok(job) => job(),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        fire_timers();
        run_later();
        let timeout = MAIN.with(|main| {
            let main = main.borrow();
            let timers = main.timers.iter().map(|timer| timer.due);
            timers
                .chain(main.later.iter().map(|(due, _)| *due))
                .min()
                .map(|due| due.saturating_duration_since(Instant::now()))
        });
        os::wait(timeout);
    }
}

/// Runs every job of [`post_after`] that is due, outside the borrow: one may post more.
fn run_later() {
    let now = Instant::now();
    let due: Vec<Job> = MAIN.with(|main| {
        let later = &mut main.borrow_mut().later;
        let (due, wait): (Vec<_>, Vec<_>) = later.drain(..).partition(|(at, _)| *at <= now);
        *later = wait;
        due.into_iter().map(|(_, job)| job).collect()
    });
    for job in due {
        job();
    }
}

/// Calls every timer that is due, outside the borrow: a plugin's `on_timer` may register more.
fn fire_timers() {
    let now = Instant::now();
    let due: Vec<(Weak<Instance>, u32)> = MAIN.with(|main| {
        let mut main = main.borrow_mut();
        main.timers
            .iter_mut()
            .filter(|timer| timer.due <= now)
            .map(|timer| {
                timer.due = now + timer.period;
                (timer.owner.clone(), timer.id)
            })
            .collect()
    });
    for (owner, id) in due {
        if let Some(instance) = owner.upgrade() {
            instance.on_timer(id);
        }
    }
}

#[cfg(windows)]
mod os {
    use std::time::Duration;
    use windows::Win32::Foundation::{HANDLE, LPARAM, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, PostThreadMessageW,
        TranslateMessage, MSG, PM_NOREMOVE, PM_REMOVE, QS_ALLINPUT, WM_APP,
    };

    /// Wakes the host thread's message wait.
    pub(super) struct Waker(u32);

    impl Waker {
        /// On the host thread: gives it a message queue, so posts to it land.
        pub(super) fn new() -> Result<Waker, String> {
            let mut msg = MSG::default();
            // SAFETY: `PeekMessageW` on this thread creates its queue; `msg` is a valid out.
            let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
            // SAFETY: no preconditions.
            Ok(Waker(unsafe { GetCurrentThreadId() }))
        }

        pub(super) fn wake(&self) {
            // SAFETY: a thread id with a queue; a message with no payload.
            let _ = unsafe { PostThreadMessageW(self.0, WM_APP, WPARAM(0), LPARAM(0)) };
        }
    }

    /// Waits for a message (a job's wake, a plugin window's input) or the timeout, then
    /// dispatches whatever messages there are.
    pub(super) fn wait(timeout: Option<Duration>) {
        const INFINITE: u32 = u32::MAX;
        let millis = timeout.map_or(INFINITE, |timeout| {
            u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1)
        });
        // SAFETY: no handles; waits on this thread's queue.
        let _ = unsafe { MsgWaitForMultipleObjects(None::<&[HANDLE]>, false, millis, QS_ALLINPUT) };
        let mut msg = MSG::default();
        // SAFETY: the standard pump over this thread's messages; `msg` is a valid out.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.hwnd.is_invalid() && msg.message == WM_APP {
                continue;
            }
            // SAFETY: a message just taken from this thread's queue.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod os {
    use super::{MainState, MAIN};
    use clap_sys::ext::posix_fd_support::{
        CLAP_POSIX_FD_ERROR, CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE,
    };
    use std::sync::Weak;
    use std::time::Duration;

    /// The write end of a pipe the host thread polls.
    pub(super) struct Waker(i32);

    thread_local! {
        static READ_END: std::cell::Cell<i32> = const { std::cell::Cell::new(-1) };
    }

    impl Waker {
        pub(super) fn new() -> Result<Waker, String> {
            let mut fds = [0i32; 2];
            // SAFETY: `fds` is two ints for the pipe's ends.
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
                return Err("cannot create the plugin host's pipe".into());
            }
            READ_END.with(|read| read.set(fds[0]));
            Ok(Waker(fds[1]))
        }

        pub(super) fn wake(&self) {
            // SAFETY: one byte to a pipe this waker owns; a full pipe is already a wake.
            let _ = unsafe { libc::write(self.0, [1u8].as_ptr().cast(), 1) };
        }
    }

    /// Polls the wake pipe and the plugins' descriptors until one is ready or the timeout, then
    /// tells each plugin about its ready descriptors.
    pub(super) fn wait(timeout: Option<Duration>) {
        // Events Xlib already read (they would not wake the poll).
        crate::clap::frame::dispatch_pending();
        let read_end = READ_END.with(std::cell::Cell::get);
        let registered: Vec<(i32, u32, Weak<super::Instance>)> = MAIN.with(|main| {
            let main: &MainState = &main.borrow();
            main.fds
                .iter()
                .map(|fd| (fd.fd, fd.flags, fd.owner.clone()))
                .collect()
        });
        let mut polled: Vec<libc::pollfd> = std::iter::once(libc::pollfd {
            fd: read_end,
            events: libc::POLLIN,
            revents: 0,
        })
        .chain(registered.iter().map(|(fd, flags, _)| libc::pollfd {
            fd: *fd,
            events: if flags & CLAP_POSIX_FD_READ != 0 {
                libc::POLLIN
            } else {
                0
            } | if flags & CLAP_POSIX_FD_WRITE != 0 {
                libc::POLLOUT
            } else {
                0
            },
            revents: 0,
        }))
        // The plugin windows' X connection (frame.rs), last.
        .chain(crate::clap::frame::fd().map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        }))
        .collect();
        let millis = timeout.map_or(-1, |timeout| {
            i32::try_from(timeout.as_millis().saturating_add(1)).unwrap_or(i32::MAX)
        });
        // SAFETY: `polled` is a valid array of its own length.
        let ready =
            unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, millis) };
        if ready <= 0 {
            return;
        }
        if polled[0].revents != 0 {
            let mut sink = [0u8; 64];
            // SAFETY: draining a non-blocking pipe this thread owns into `sink`.
            while unsafe { libc::read(read_end, sink.as_mut_ptr().cast(), sink.len()) } > 0 {}
        }
        crate::clap::frame::dispatch_pending();
        for (pollfd, (fd, _, owner)) in polled[1..].iter().zip(&registered) {
            let revents = pollfd.revents;
            let flags = if revents & libc::POLLIN != 0 {
                CLAP_POSIX_FD_READ
            } else {
                0
            } | if revents & libc::POLLOUT != 0 {
                CLAP_POSIX_FD_WRITE
            } else {
                0
            } | if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                CLAP_POSIX_FD_ERROR
            } else {
                0
            };
            if flags != 0 {
                if let Some(instance) = owner.upgrade() {
                    instance.on_fd(*fd, flags);
                }
            }
        }
    }
}
