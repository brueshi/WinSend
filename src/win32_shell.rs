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

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    LookupIconIdFromDirectoryEx, PostMessageW, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, HICON, HWND_MESSAGE, LR_DEFAULTCOLOR,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, TPM_LEFTBUTTON, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_HOTKEY, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
};

use crate::hotkey::{Action, Hotkey, Hotkeys};
use crate::shell::{HotkeyReport, Shell, ShellEvent, TrayState, Waker};

/// Asks the thread to reconcile its registrations with the pending set.
const WM_APPLY_HOTKEYS: u32 = WM_APP + 1;
/// Asks the thread to unregister everything and end.
const WM_STOP: u32 = WM_APP + 2;
/// The tray icon's callback. Mouse messages on the icon arrive here.
const WM_TRAY: u32 = WM_APP + 3;
/// Asks the thread to push the pending tray state to the icon.
const WM_SET_TRAY: u32 = WM_APP + 4;

/// Menu command identifiers. Distinct from the hotkey ids, which live in a
/// different namespace, but kept far apart so a mix-up would be obvious.
const MENU_SEND: u32 = 101;
const MENU_RETRIEVE: u32 = 102;
const MENU_SETTINGS: u32 = 103;
const MENU_QUIT: u32 = 104;

/// The icon, embedded rather than compiled in as a resource.
///
/// A `.rc` resource would need a resource compiler in the build, and that is
/// exactly what would break the macOS cross-compile the whole development loop
/// depends on. The cost is that Explorer shows no icon on the .exe itself,
/// which is worth a separate change with its own build-tooling risk.
const ICON_BYTES: &[u8] = include_bytes!("../assets/winsend.ico");

/// The version `CreateIconFromResourceEx` expects for icon resources.
const ICON_RESOURCE_VERSION: u32 = 0x0003_0000;

/// `szTip` is a fixed 128 wide characters including its terminator.
const TOOLTIP_LIMIT: usize = 127;

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

/// Build the tray icon from the embedded `.ico`.
///
/// An `.ico` file is a directory of images; `LookupIconIdFromDirectoryEx` picks
/// the entry matching the current small-icon metrics, and the handle is created
/// from that entry alone rather than the file as a whole.
unsafe fn load_icon() -> Option<HICON> {
    let offset = LookupIconIdFromDirectoryEx(ICON_BYTES.as_ptr(), true, 0, 0, LR_DEFAULTCOLOR);
    let entry = ICON_BYTES.get(offset as usize..)?;
    CreateIconFromResourceEx(entry, true, ICON_RESOURCE_VERSION, 0, 0, LR_DEFAULTCOLOR).ok()
}

fn tooltip_bytes(tooltip: &str) -> [u16; 128] {
    let mut buffer = [0u16; 128];
    for (slot, unit) in buffer
        .iter_mut()
        .zip(tooltip.encode_utf16().take(TOOLTIP_LIMIT))
    {
        *slot = unit;
    }
    buffer
}

/// The notification-area icon. Held so it can be removed on the way out: an
/// icon left behind after the process exits lingers until the user hovers it.
struct Tray {
    data: NOTIFYICONDATAW,
    icon: Option<HICON>,
    added: bool,
}

impl Tray {
    unsafe fn add(hwnd: HWND) -> Self {
        let icon = load_icon();
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            // NIF_ICON is claimed only when there is one to show. Claiming it
            // with a null handle produces an entry that exists but is blank.
            uFlags: NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            szTip: tooltip_bytes("WinSend"),
            ..Default::default()
        };
        if let Some(icon) = icon {
            data.uFlags |= NIF_ICON;
            data.hIcon = icon;
        }

        let added = Shell_NotifyIconW(NIM_ADD, &data).as_bool();
        Self { data, icon, added }
    }

    unsafe fn set_tooltip(&mut self, tooltip: &str) {
        if !self.added {
            return;
        }
        self.data.szTip = tooltip_bytes(tooltip);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data);
    }

    unsafe fn remove(&mut self) {
        if self.added {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
            self.added = false;
        }
        if let Some(icon) = self.icon.take() {
            let _ = DestroyIcon(icon);
        }
    }
}

/// Show the tray menu and return what was chosen.
///
/// `TPM_RETURNCMD` hands the choice straight back rather than posting
/// `WM_COMMAND`, which keeps the whole interaction in one place instead of
/// splitting it across the window procedure.
unsafe fn show_menu(hwnd: HWND, can_retrieve: bool) -> Option<ShellEvent> {
    let menu = CreatePopupMenu().ok()?;

    let item = |flags, id: u32, text: PCWSTR| {
        let _ = AppendMenuW(menu, flags, id as usize, text);
    };
    item(MF_STRING, MENU_SEND, w!("Send to Monitor"));
    // Greyed rather than hidden, mirroring the button: an item that vanishes
    // reads as a bug, one that is greyed reads as "not yet".
    item(
        if can_retrieve { MF_STRING } else { MF_STRING | MF_GRAYED },
        MENU_RETRIEVE,
        w!("Retrieve"),
    );
    item(MF_SEPARATOR, 0, PCWSTR::null());
    item(MF_STRING, MENU_SETTINGS, w!("Settings"));
    item(MF_SEPARATOR, 0, PCWSTR::null());
    item(MF_STRING, MENU_QUIT, w!("Quit WinSend"));

    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);

    // The well-known quirk: a menu owned by a window that is not in the
    // foreground will not dismiss when the user clicks away from it. This is
    // harmless for a message-only window, which has nothing to show.
    let _ = SetForegroundWindow(hwnd);

    let chosen = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTBUTTON,
        cursor.x,
        cursor.y,
        None,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);

    match chosen.0 as u32 {
        MENU_SEND => Some(ShellEvent::Trigger(Action::Send)),
        MENU_RETRIEVE => Some(ShellEvent::Trigger(Action::Retrieve)),
        MENU_SETTINGS => Some(ShellEvent::ShowSettings),
        MENU_QUIT => Some(ShellEvent::Quit),
        // Dismissed without choosing anything.
        _ => None,
    }
}

/// Hand an event to the UI and wake it. Returns false once the UI is gone,
/// which is the thread's cue to stop.
fn emit(events: &Sender<ShellEvent>, waker: &Waker, event: ShellEvent) -> bool {
    if events.send(event).is_err() {
        return false;
    }
    waker();
    true
}

fn run(
    startup: SyncSender<Option<isize>>,
    events: Sender<ShellEvent>,
    waker: Waker,
    pending: Arc<Mutex<Hotkeys>>,
    tray_pending: Arc<Mutex<TrayState>>,
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
        let mut tray = Tray::add(hwnd);
        let mut can_retrieve = false;
        let mut message = MSG::default();

        // `.0 > 0` rather than `as_bool`: GetMessageW returns -1 on error, and
        // treating that as true would spin here forever.
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            match message.message {
                WM_HOTKEY => {
                    if let Some(action) = action_from_id(message.wParam.0) {
                        if !emit(&events, &waker, ShellEvent::Trigger(action)) {
                            break;
                        }
                    }
                }
                WM_APPLY_HOTKEYS => {
                    let wanted = pending.lock().map(|set| *set).unwrap_or_default();
                    let report = reconcile(hwnd, &mut registered, wanted);
                    if !emit(&events, &waker, ShellEvent::HotkeysApplied(report)) {
                        break;
                    }
                }
                WM_SET_TRAY => {
                    let state = tray_pending.lock().map(|s| s.clone()).unwrap_or_default();
                    can_retrieve = state.can_retrieve;
                    tray.set_tooltip(&state.tooltip);
                }
                WM_TRAY => {
                    // With the default icon version the mouse message arrives
                    // in lParam and the icon id in wParam.
                    let chosen = match message.lParam.0 as u32 {
                        WM_LBUTTONUP => Some(ShellEvent::ShowWindow),
                        WM_RBUTTONUP => show_menu(hwnd, can_retrieve),
                        _ => None,
                    };
                    if let Some(event) = chosen {
                        if !emit(&events, &waker, event) {
                            break;
                        }
                    }
                }
                WM_STOP => PostQuitMessage(0),
                _ => {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }

        tray.remove();

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
    tray_pending: Arc<Mutex<TrayState>>,
    /// The last state actually sent, so calling `set_tray_state` every frame
    /// does not mean crossing to the other thread every frame.
    tray_sent: RefCell<TrayState>,
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
        let tray_pending = Arc::new(Mutex::new(TrayState::default()));

        let thread = {
            let pending = Arc::clone(&pending);
            let tray_pending = Arc::clone(&tray_pending);
            std::thread::Builder::new()
                .name("winsend-shell".into())
                .spawn(move || run(startup_out, events_out, waker, pending, tray_pending))
                .ok()
        };

        Self {
            hwnd: thread.as_ref().and_then(|_| startup.recv().ok()).flatten(),
            events,
            pending,
            tray_pending,
            tray_sent: RefCell::new(TrayState::default()),
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

    fn set_tray_state(&self, state: TrayState) {
        if *self.tray_sent.borrow() == state {
            return;
        }
        if let Ok(mut pending) = self.tray_pending.lock() {
            *pending = state.clone();
        }
        // Recorded regardless of whether the post lands. A tray that could not
        // be reached will not be reached next frame either, and retrying every
        // frame would only add noise.
        *self.tray_sent.borrow_mut() = state;
        self.post(WM_SET_TRAY);
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
