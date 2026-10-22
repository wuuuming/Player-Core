//! A window of the host's own, for a plugin that only draws embedded (most do: CLAP's floating
//! windows are optional, and JUCE-based plugins offer none). Made, sized, shown and destroyed on
//! the host thread; it floats above the player's window (owned by it on Windows, transient for
//! it on X11), and closing it hides the plugin's view instead of destroying it.

use std::sync::Arc;

/// What closing a frame does: posted to the host thread, never run inside the window's own
/// message handling (it may drop the last reference to the plugin, and with it the frame).
pub(super) type OnClose = Arc<dyn Fn() + Send + Sync>;

pub(super) use os::Frame;
#[cfg(target_os = "linux")]
pub(super) use os::{dispatch_pending, fd};

#[cfg(windows)]
mod os {
    use super::OnClose;
    use std::sync::OnceLock;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, LoadCursorW,
        RegisterClassExW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
        CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_HIDE,
        SW_SHOW, WINDOW_EX_STYLE, WM_CLOSE, WNDCLASSEXW, WS_CAPTION, WS_MINIMIZEBOX, WS_OVERLAPPED,
        WS_SYSMENU,
    };

    const CLASS: PCWSTR = w!("NovaClapFrame");
    const STYLE: windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE =
        windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
            WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0,
        );

    pub(in super::super) struct Frame {
        hwnd: HWND,
        /// The close action, owned here; its address is the window's user data.
        on_close: *mut OnClose,
    }

    fn register() -> Result<(), String> {
        static REGISTERED: OnceLock<bool> = OnceLock::new();
        let ok = *REGISTERED.get_or_init(|| {
            // SAFETY: plain registration of a class whose procedure is below.
            unsafe {
                let class = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(procedure),
                    hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    lpszClassName: CLASS,
                    ..Default::default()
                };
                RegisterClassExW(&class) != 0
            }
        });
        if ok {
            Ok(())
        } else {
            Err("cannot make a window for the plugin".into())
        }
    }

    unsafe extern "system" fn procedure(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CLOSE {
            // SAFETY: the user data is null or the frame's `on_close`, alive until the frame
            // clears it in `Drop`.
            let on_close = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const OnClose;
            // SAFETY: as above.
            if let Some(on_close) = unsafe { on_close.as_ref() } {
                let on_close = on_close.clone();
                crate::clap::host::post(move || on_close());
            }
            return LRESULT(0);
        }
        // SAFETY: the default handling of every other message.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    impl Frame {
        /// A hidden frame titled `title`, owned by `owner` (an HWND; 0 for none).
        pub(in super::super) fn new(
            title: &str,
            owner: isize,
            on_close: OnClose,
        ) -> Result<Frame, String> {
            register()?;
            let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
            let owner = HWND(owner as *mut _);
            // SAFETY: a registered class and a null-terminated title; the owner is the player's
            // window or none.
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    CLASS,
                    PCWSTR(title.as_ptr()),
                    STYLE,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    owner,
                    None,
                    GetModuleHandleW(None).unwrap_or_default(),
                    None,
                )
            }
            .map_err(|error| format!("cannot make a window for the plugin: {}", error.message()))?;
            let on_close = Box::into_raw(Box::new(on_close));
            // SAFETY: the window just made; `on_close` lives until `Drop` clears it.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, on_close as isize) };
            Ok(Frame { hwnd, on_close })
        }

        /// The HWND a plugin embeds into.
        pub(in super::super) fn handle(&self) -> isize {
            self.hwnd.0 as isize
        }

        /// The display scale of the frame's monitor (CLAP `gui.set_scale`).
        pub(in super::super) fn scale(&self) -> f64 {
            f64::from(self.dpi()) / 96.0
        }

        fn dpi(&self) -> u32 {
            // SAFETY: a live window; the system's DPI when the window has none.
            match unsafe { GetDpiForWindow(self.hwnd) } {
                // SAFETY: no preconditions.
                0 => unsafe { GetDpiForSystem() },
                dpi => dpi,
            }
        }

        /// Sizes the frame so its inside is `width` × `height` physical pixels.
        pub(in super::super) fn resize(&self, width: u32, height: u32) {
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            // SAFETY: a valid rectangle; a live window.
            unsafe {
                let _ = AdjustWindowRectExForDpi(
                    &mut rect,
                    STYLE,
                    false,
                    WINDOW_EX_STYLE(0),
                    self.dpi(),
                );
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }

        pub(in super::super) fn show(&self) {
            // SAFETY: a live window.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOW);
                let _ = SetForegroundWindow(self.hwnd);
            }
        }

        pub(in super::super) fn hide(&self) {
            // SAFETY: a live window.
            let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        }
    }

    impl Drop for Frame {
        fn drop(&mut self) {
            // SAFETY: the window and the close action made in `new`, each freed once; the user
            // data is cleared first, so no message reaches the freed action.
            unsafe {
                SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(self.hwnd);
                drop(Box::from_raw(self.on_close));
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod os {
    use super::OnClose;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::CString;
    use x11_dl::xlib;

    /// The host thread's own X connection, opened with its first frame.
    struct Conn {
        xl: xlib::Xlib,
        dpy: *mut xlib::Display,
        protocols: xlib::Atom,
        delete: xlib::Atom,
        frames: HashMap<xlib::Window, OnClose>,
    }

    thread_local! {
        static CONN: RefCell<Option<Conn>> = const { RefCell::new(None) };
    }

    fn open() -> Result<Conn, String> {
        let xl = xlib::Xlib::open().map_err(|_| "no X11 library".to_string())?;
        // SAFETY: opens the default display; null when there is none.
        let dpy = unsafe { (xl.XOpenDisplay)(std::ptr::null()) };
        if dpy.is_null() {
            return Err("no X display for the plugin's window".into());
        }
        // SAFETY: a live display; null-terminated atom names.
        let (protocols, delete) = unsafe {
            (
                (xl.XInternAtom)(dpy, c"WM_PROTOCOLS".as_ptr(), 0),
                (xl.XInternAtom)(dpy, c"WM_DELETE_WINDOW".as_ptr(), 0),
            )
        };
        Ok(Conn {
            xl,
            dpy,
            protocols,
            delete,
            frames: HashMap::new(),
        })
    }

    /// Runs `f` on the host thread's connection, opening it first if need be.
    fn with<R>(f: impl FnOnce(&mut Conn) -> R) -> Result<R, String> {
        CONN.with(|conn| {
            let mut conn = conn.borrow_mut();
            let conn = match &mut *conn {
                Some(conn) => conn,
                none => none.insert(open()?),
            };
            Ok(f(conn))
        })
    }

    pub(in super::super) struct Frame {
        window: xlib::Window,
    }

    impl Frame {
        /// A hidden frame titled `title`, transient for `owner` (an X window; 0 for none).
        pub(in super::super) fn new(
            title: &str,
            owner: isize,
            on_close: OnClose,
        ) -> Result<Frame, String> {
            let title = CString::new(title.replace('\0', " ")).unwrap_or_default();
            with(|c| {
                // SAFETY: a live display; the window and its properties are this connection's.
                unsafe {
                    let root = (c.xl.XDefaultRootWindow)(c.dpy);
                    let window = (c.xl.XCreateSimpleWindow)(c.dpy, root, 0, 0, 320, 240, 0, 0, 0);
                    (c.xl.XStoreName)(c.dpy, window, title.as_ptr());
                    let mut delete = c.delete;
                    (c.xl.XSetWMProtocols)(c.dpy, window, &mut delete, 1);
                    if owner != 0 {
                        (c.xl.XSetTransientForHint)(c.dpy, window, owner as xlib::Window);
                    }
                    (c.xl.XFlush)(c.dpy);
                    c.frames.insert(window, on_close);
                    Frame { window }
                }
            })
        }

        /// The X window a plugin embeds into.
        pub(in super::super) fn handle(&self) -> isize {
            self.window as isize
        }

        pub(in super::super) fn resize(&self, width: u32, height: u32) {
            let _ = with(|c| {
                // SAFETY: a live display and window.
                unsafe {
                    (c.xl.XResizeWindow)(c.dpy, self.window, width.max(1), height.max(1));
                    (c.xl.XFlush)(c.dpy);
                }
            });
        }

        pub(in super::super) fn show(&self) {
            let _ = with(|c| {
                // SAFETY: a live display and window.
                unsafe {
                    (c.xl.XMapRaised)(c.dpy, self.window);
                    (c.xl.XFlush)(c.dpy);
                }
            });
        }

        pub(in super::super) fn hide(&self) {
            let _ = with(|c| {
                // SAFETY: a live display and window.
                unsafe {
                    (c.xl.XUnmapWindow)(c.dpy, self.window);
                    (c.xl.XFlush)(c.dpy);
                }
            });
        }
    }

    impl Drop for Frame {
        fn drop(&mut self) {
            let _ = with(|c| {
                c.frames.remove(&self.window);
                // SAFETY: a live display; the window is this connection's, destroyed once.
                unsafe {
                    (c.xl.XDestroyWindow)(c.dpy, self.window);
                    (c.xl.XFlush)(c.dpy);
                }
            });
        }
    }

    /// The connection's descriptor for the host loop to poll; none before the first frame.
    pub(in super::super) fn fd() -> Option<i32> {
        CONN.with(|conn| {
            conn.borrow()
                .as_ref()
                // SAFETY: a live display.
                .map(|c| unsafe { (c.xl.XConnectionNumber)(c.dpy) })
        })
    }

    /// Handles the events that have arrived: a frame closed by its title bar hides the plugin
    /// (posted, outside the connection's borrow).
    pub(in super::super) fn dispatch_pending() {
        let closed: Vec<OnClose> = CONN.with(|conn| {
            let conn = conn.borrow();
            let Some(c) = conn.as_ref() else {
                return Vec::new();
            };
            let mut closed = Vec::new();
            // SAFETY: a live display; each event is read into a zeroed `XEvent`.
            unsafe {
                while (c.xl.XPending)(c.dpy) > 0 {
                    let mut event: xlib::XEvent = std::mem::zeroed();
                    (c.xl.XNextEvent)(c.dpy, &mut event);
                    if event.get_type() == xlib::ClientMessage {
                        let message = event.client_message;
                        let is_delete = message.message_type == c.protocols
                            && message.data.get_long(0) as xlib::Atom == c.delete;
                        if let Some(on_close) = c.frames.get(&message.window).filter(|_| is_delete)
                        {
                            closed.push(on_close.clone());
                        }
                    }
                }
            }
            closed
        });
        for on_close in closed {
            crate::clap::host::post(move || on_close());
        }
    }
}
