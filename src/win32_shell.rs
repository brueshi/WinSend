//! The real shell. Like `win32.rs`, everything here is unverified until it runs
//! on Windows.
//!
//! `RegisterHotKey` delivers `WM_HOTKEY` to a message queue, and eframe owns
//! the main one without exposing raw Windows messages. So this runs a thread of
//! its own with a message-only window and its own loop, and forwards what it
//! sees to the UI over a channel.
//!
//! The window is created even though hotkeys alone would not need one —
//! `WM_HOTKEY` is intercepted in the loop before dispatch. It exists because
//! `Shell_NotifyIcon` genuinely does need a window to send its callbacks to,
//! and standing up a second thread for that later would be the wrong shape.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassW, TranslateMessage, HWND_MESSAGE, MSG, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_HOTKEY, WNDCLASSW,
};

use crate::hotkey::{Action, Hotkey, Hotkeys};
use crate::shell::{HotkeyReport, Shell, ShellEvent, Waker};

/// Asks the thread to reconcile its registrations with the pending set.
const WM_APPLY_HOTKEYS: u32 = WM_APP + 1;
/// Asks the thread to unregister everything and end.
const WM_STOP: u32 = WM_APP + 2;

/// `RegisterHotKey` identifiers, scoped to our window and so ours to choose.
fn hotkey_id(action: Action) -> i32 {
    match action {
        Action::Send => 1,
        Action::Retrieve => 2,
    }
}

fn action_from_id(id: usize) -> Option<Action> {
    Action::ALL.into_iter().find(|a| hotkey_id(*a) as usize == id)
}

/// `MOD_NOREPEAT` is not optional. Without it, holding the key down repeats the
/// action for as long as it is held, which during a broadcast means a window
/// flung across monitors dozens of times from one lean on the keyboard.
fn modifiers_of(hotkey: Hotkey) -> HOT_KEY_MODIFIERS {
    let mut modifiers = MOD_NOREPEAT;
    if hotkey.ctrl {
        modifiers |= MOD_CONTROL;
    }
    if hotkey.alt {
        modifiers |= MOD_ALT;
    }
    if hotkey.shift {
        modifiers |= MOD_SHIFT;
    }
    if hotkey.win {
        modifiers |= MOD_WIN;
    }
    modifiers
}

/// Nothing but the default behaviour yet. It exists because a window needs a
/// procedure at all, and because the tray icon's callbacks will arrive here.
unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, message, wparam, lparam)
}

unsafe fn create_message_window() -> Option<HWND> {
    let class_name = w!("WinSendShellWindow");
    let instance = GetModuleHandleW(None).ok()?;

    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    // A failure here is only worth reacting to if the window then fails to
    // create, which is checked directly.
    RegisterClassW(&class);

    // HWND_MESSAGE gives a window with no presence at all: not shown, not
    // enumerated, not in the taskbar. It exists purely to own a message queue.
    CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class_name,
        w!("WinSend"),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        Some(instance.into()),
        None,
    )
    .ok()
}

/// Bring registrations in line with `wanted`, reporting whatever the OS
/// refused. Everything is dropped first rather than diffed: reconciling two
/// sets in place would be more code to get subtly wrong, and re-registering a
/// handful of keys costs nothing at the speed a person changes them.
unsafe fn reconcile(hwnd: HWND, registered: &mut Hotkeys, wanted: Hotkeys) -> HotkeyReport {
    for action in Action::ALL {
        if registered.binding(action).is_some() {
            let _ = UnregisterHotKey(Some(hwnd), hotkey_id(action));
        }
    }
    *registered = Hotkeys::default();

    let mut report = HotkeyReport::default();
    for action in Action::ALL {
        let Some(hotkey) = wanted.binding(action) else {
            continue;
        };
        let registration = RegisterHotKey(
            Some(hwnd),
            hotkey_id(action),
            modifiers_of(hotkey),
            u32::from(hotkey.key.virtual_key()),
        );
        match registration {
            // Cannot fail: `wanted` already satisfies the binding rules.
            Ok(()) => {
                let _ = registered.set(action, Some(hotkey));
            }
            // The only failure worth distinguishing is the common one, and
            // Windows does not tell them apart usefully anyway.
            Err(_) => report.rejected.push((
                action,
                format!("{hotkey} is already taken by another application"),
            )),
        }
    }
    report
}

fn run(
    startup: SyncSender<Option<isize>>,
    events: Sender<ShellEvent>,
    waker: Waker,
    pending: Arc<Mutex<Hotkeys>>,
) {
    unsafe {
        let Some(hwnd) = create_message_window() else {
            let _ = startup.send(None);
            return;
        };
        // An isize rather than the HWND itself, which is not `Send`. Only
        // `PostMessageW` is called with it from the other thread, and that is
        // documented as safe to call across threads.
        let _ = startup.send(Some(hwnd.0 as isize));

        let mut registered = Hotkeys::default();
        let mut message = MSG::default();

        // `.0 > 0` rather than `as_bool`: GetMessageW returns -1 on error, and
        // treating that as true would spin here forever.
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            match message.message {
                WM_HOTKEY => {
                    if let Some(action) = action_from_id(message.wParam.0) {
                        if events.send(ShellEvent::Trigger(action)).is_err() {
                            break;
                        }
                        waker();
                    }
                }
                WM_APPLY_HOTKEYS => {
                    let wanted = pending.lock().map(|set| *set).unwrap_or_default();
                    let report = reconcile(hwnd, &mut registered, wanted);
                    if events.send(ShellEvent::HotkeysApplied(report)).is_err() {
                        break;
                    }
                    waker();
                }
                WM_STOP => PostQuitMessage(0),
                _ => {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }

        // Registrations belong to the window and would go with it, but they are
        // released explicitly so the order is not something to rely on.
        for action in Action::ALL {
            if registered.binding(action).is_some() {
                let _ = UnregisterHotKey(Some(hwnd), hotkey_id(action));
            }
        }
        let _ = DestroyWindow(hwnd);
    }
}

pub struct Win32Shell {
    /// `None` when the thread could not create its window, which leaves the
    /// app fully usable by mouse and only the hotkeys gone.
    hwnd: Option<isize>,
    events: Receiver<ShellEvent>,
    pending: Arc<Mutex<Hotkeys>>,
    thread: Option<JoinHandle<()>>,
    /// A set that could not be handed to the thread, held until `poll` can
    /// answer for it. No waker is needed: this is only ever set from the UI
    /// thread while it is already awake and about to poll.
    undeliverable: Cell<Option<Hotkeys>>,
}

impl Win32Shell {
    pub fn new(waker: Waker) -> Self {
        let (events_out, events) = channel();
        // Rendezvous rather than buffered: startup is over in microseconds and
        // the HWND is needed before anything can be posted to it.
        let (startup_out, startup) = sync_channel(0);
        let pending = Arc::new(Mutex::new(Hotkeys::default()));

        let thread = {
            let pending = Arc::clone(&pending);
            std::thread::Builder::new()
                .name("winsend-shell".into())
                .spawn(move || run(startup_out, events_out, waker, pending))
                .ok()
        };

        Self {
            hwnd: thread.as_ref().and_then(|_| startup.recv().ok()).flatten(),
            events,
            pending,
            thread,
            undeliverable: Cell::new(None),
        }
    }

    fn post(&self, message: u32) -> bool {
        let Some(hwnd) = self.hwnd else {
            return false;
        };
        unsafe { PostMessageW(Some(HWND(hwnd as *mut c_void)), message, WPARAM(0), LPARAM(0)) }
            .is_ok()
    }
}

impl Shell for Win32Shell {
    fn apply_hotkeys(&self, hotkeys: Hotkeys) {
        // Written before the message is posted, so the thread always reads the
        // newest set. Two rapid changes collapse into one reconcile, which is
        // the desired outcome rather than a compromise.
        if let Ok(mut pending) = self.pending.lock() {
            *pending = hotkeys;
        }
        if !self.post(WM_APPLY_HOTKEYS) {
            self.undeliverable.set(Some(hotkeys));
        }
    }

    fn poll(&self) -> Vec<ShellEvent> {
        let mut events: Vec<ShellEvent> = self.events.try_iter().collect();

        if let Some(hotkeys) = self.undeliverable.take() {
            let mut report = HotkeyReport::default();
            for action in Action::ALL {
                if hotkeys.binding(action).is_some() {
                    report.rejected.push((
                        action,
                        "the hotkey listener could not be started".to_string(),
                    ));
                }
            }
            events.push(ShellEvent::HotkeysApplied(report));
        }

        events
    }
}

impl Drop for Win32Shell {
    /// Unregister on exit. Windows would release them with the process, but a
    /// combination still held after the app is gone is exactly the kind of
    /// thing that outlives a debugging session.
    fn drop(&mut self) {
        self.post(WM_STOP);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
