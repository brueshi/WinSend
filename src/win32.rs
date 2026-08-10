//! The real platform. Everything here is unverified until it runs against a
//! live Zoom session on Windows.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;

use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, COLORREF, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CLOAKED,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
    GetMonitorInfoW, MonitorFromWindow, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HDC, HMONITOR, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    DPI_AWARENESS_PER_MONITOR_AWARE, DPI_AWARENESS_SYSTEM_AWARE, DPI_AWARENESS_UNAWARE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowTextW, SetForegroundWindow,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed,
    SetLayeredWindowAttributes, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, GWL_EXSTYLE, GWL_STYLE, HWND_BOTTOM, HWND_NOTOPMOST, HWND_TOP,
    HWND_TOPMOST, LWA_ALPHA, MONITORINFOF_PRIMARY, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_MINIMIZE, SW_RESTORE, SW_SHOW, WS_CAPTION, WS_EX_LAYERED,
    WS_EX_TOPMOST, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};

use crate::hotkey::KeyChord;
use crate::platform::{
    Bounds, MonitorInfo, Placement, Platform, PlatformError, Thumbnail, WindowCandidate,
};

/// Renders the window's full content even when it is occluded or composited by
/// the GPU. Without this flag, hardware-accelerated surfaces capture as black.
const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

const THUMBNAIL_WIDTH: u32 = 192;

pub struct Win32Platform {
    /// The style bits taken off a window to go borderless, so exactly those can
    /// be put back.
    ///
    /// The bits removed, not the style word they came from. Snapshotting the
    /// whole word meant restoring it discarded anything the application changed
    /// in the meantime, and Zoom does change its own styles — it enters its own
    /// full-screen mode when the video window is moved. Putting back a stale
    /// snapshot is what left odd chrome around it.
    ///
    /// Held here rather than in `Core` because it is a detail of how this
    /// platform achieves a borderless fill, not something the app logic needs.
    cleared_styles: RefCell<HashMap<u64, isize>>,
    /// Whether a window was already topmost before we raised it, so putting it
    /// back does not quietly clear an always-on-top the user set themselves.
    was_topmost: RefCell<HashMap<u64, bool>>,
    /// Windows this process put into the layered band in order to fade them.
    ///
    /// The same rule as `cleared_styles`, for the same reason: record the bit
    /// that was set and take back exactly that. A window that was already
    /// layered before we touched it is not in here and keeps its style, since
    /// clearing `WS_EX_LAYERED` from a window that arrived with it would break
    /// however that application draws itself.
    layered: RefCell<HashSet<u64>>,
}

/// Whether the window currently sits in the always-on-top band.
unsafe fn is_topmost(hwnd: HWND) -> bool {
    GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0
}

impl Win32Platform {
    pub fn new() -> Self {
        // Must happen before any window exists. Without per-monitor v2, window
        // rects come back DPI-virtualised on mixed-scaling setups and the
        // window lands in the wrong place or the wrong size.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        Self {
            cleared_styles: RefCell::new(HashMap::new()),
            layered: RefCell::new(HashSet::new()),
            was_topmost: RefCell::new(HashMap::new()),
        }
    }
}

impl Default for Win32Platform {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Win32Platform {
    /// Undo everything done to another application's window.
    ///
    /// Retrieve already does this, but quitting while a window is still sent
    /// would otherwise leave Zoom frameless and pinned above everything until
    /// it is restarted — changes to another application that outlive this
    /// process.
    fn drop(&mut self) {
        // First, because it is the one that shows. Quitting part-way through a
        // fade would otherwise leave another application's window translucent
        // after this process has gone, with nothing left able to put it back.
        for handle in self.layered.borrow().iter().copied() {
            let hwnd = handle_to_hwnd(handle);
            unsafe {
                if !IsWindow(Some(hwnd)).as_bool() {
                    continue;
                }
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
                let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, current & !(WS_EX_LAYERED.0 as isize));
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
        }

        for (handle, was_topmost) in self.was_topmost.borrow().iter() {
            if *was_topmost {
                continue;
            }
            let hwnd = handle_to_hwnd(*handle);
            unsafe {
                if !IsWindow(Some(hwnd)).as_bool() {
                    continue;
                }
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_NOTOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }

        for (handle, cleared) in self.cleared_styles.borrow().iter() {
            let hwnd = handle_to_hwnd(*handle);
            unsafe {
                if !IsWindow(Some(hwnd)).as_bool() {
                    continue;
                }
                let current = GetWindowLongPtrW(hwnd, GWL_STYLE);
                SetWindowLongPtrW(hwnd, GWL_STYLE, current | *cleared);
                // The frame does not come back until the window is told to
                // recalculate it.
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
        }
    }
}

fn wide_to_string(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    OsString::from_wide(&buffer[..end]).to_string_lossy().into_owned()
}

fn rect_to_bounds(rect: RECT) -> Bounds {
    Bounds::new(
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    )
}

/// Loose on purpose. This only sorts the picker, so a false positive costs
/// nothing while a false negative could hide the window the user is looking for.
fn looks_zoom_related(process: &str, class: &str, title: &str) -> bool {
    let haystack = format!("{process} {class} {title}").to_lowercase();
    ["zoom", "cpthost", "zp"]
        .iter()
        .any(|needle| haystack.contains(needle))
}

unsafe fn process_name(pid: u32) -> String {
    let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return String::new();
    };
    let mut buffer = [0u16; 512];
    let mut len = buffer.len() as u32;
    let name = match QueryFullProcessImageNameW(
        handle,
        PROCESS_NAME_WIN32,
        windows::core::PWSTR(buffer.as_mut_ptr()),
        &mut len,
    ) {
        Ok(()) => {
            let full = wide_to_string(&buffer[..len as usize]);
            full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
        }
        Err(_) => String::new(),
    };
    let _ = CloseHandle(handle);
    name
}

unsafe fn monitor_id(hwnd: HWND) -> String {
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    if monitor.is_invalid() {
        return String::new();
    }
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(monitor, &mut info as *mut _ as *mut _).as_bool() {
        wide_to_string(&info.szDevice)
    } else {
        String::new()
    }
}

unsafe fn is_cloaked(hwnd: HWND) -> bool {
    let mut value = 0u32;
    DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut value as *mut _ as *mut c_void,
        std::mem::size_of::<u32>() as u32,
    )
    .is_ok()
        && value != 0
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<WindowCandidate>);

    // Cloaked windows are not really on screen. Everything else is reported
    // and flagged, and the filtering happens above this layer, because
    // different callers want different subsets.
    //
    // Untitled windows in particular must be here. A media player putting
    // video on a second display does it with a bare popup that has no caption
    // text at all, and dropping those made it invisible to the code that works
    // out what is covering a monitor — which is exactly the window that needed
    // moving. The picker still hides them, since a window with no title cannot
    // be identified in a list.
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }

    let mut title_buffer = [0u16; 512];
    let title_len = GetWindowTextW(hwnd, &mut title_buffer);
    let title = wide_to_string(&title_buffer[..title_len.max(0) as usize]);

    let mut class_buffer = [0u16; 256];
    let class_len = GetClassNameW(hwnd, &mut class_buffer);
    let class_name = wide_to_string(&class_buffer[..class_len.max(0) as usize]);

    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let process = process_name(pid);
    let own_process = pid == GetCurrentProcessId();

    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }

    out.push(WindowCandidate {
        handle: hwnd.0 as u64,
        likely_zoom: looks_zoom_related(&process, &class_name, &title),
        process_name: process,
        class_name,
        title,
        bounds: rect_to_bounds(rect),
        monitor_id: monitor_id(hwnd),
        minimized: IsIconic(hwnd).as_bool(),
        topmost: is_topmost(hwnd),
        own_process,
        cloaked: is_cloaked(hwnd),
        // EnumWindows walks the stacking order from the front, so the position
        // a window arrives in is its depth.
        z_order: out.len(),
    });

    BOOL(1)
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<MonitorInfo>);
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(monitor, &mut info as *mut _ as *mut _).as_bool() {
        out.push(MonitorInfo {
            id: wide_to_string(&info.szDevice),
            bounds: rect_to_bounds(info.monitorInfo.rcMonitor),
            work_area: rect_to_bounds(info.monitorInfo.rcWork),
            is_primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    BOOL(1)
}

/// Capture a window into RGBA via `PrintWindow`, downsampled in Rust rather
/// than with `StretchBlt` to keep the GDI handling to a single bitmap.
///
/// Returns `None` rather than a black image when capture fails — Zoom's video
/// window may well refuse, in which case the picker falls back to a placeholder
/// and the user identifies it by title, size and monitor instead.
unsafe fn capture(hwnd: HWND) -> Option<Thumbnail> {
    let mut rect = RECT::default();
    GetWindowRect(hwnd, &mut rect).ok()?;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return None;
    }

    let screen_dc = GetDC(None);
    if screen_dc.is_invalid() {
        return None;
    }
    let mem_dc = CreateCompatibleDC(Some(screen_dc));
    if mem_dc.is_invalid() {
        ReleaseDC(None, screen_dc);
        return None;
    }

    let mut info = BITMAPINFO::default();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width;
    // Negative height requests a top-down DIB, matching RGBA row order.
    info.bmiHeader.biHeight = -height;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB.0;

    let mut bits: *mut c_void = std::ptr::null_mut();
    let bitmap = CreateDIBSection(Some(mem_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0);

    let result = (|| {
        let bitmap = bitmap.ok()?;
        if bits.is_null() {
            return None;
        }
        let previous = SelectObject(mem_dc, bitmap.into());

        let printed = PrintWindow(hwnd, mem_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();

        let thumbnail = if printed {
            let pixel_count = (width * height) as usize;
            let source = std::slice::from_raw_parts(bits as *const u8, pixel_count * 4);
            Some(downsample_bgra(source, width as u32, height as u32))
        } else {
            None
        };

        SelectObject(mem_dc, previous);
        let _ = DeleteObject(bitmap.into());
        thumbnail
    })();

    let _ = DeleteDC(mem_dc);
    ReleaseDC(None, screen_dc);
    result
}

/// Box-filter downsample from BGRA (GDI's order) to RGBA at a fixed width.
fn downsample_bgra(source: &[u8], width: u32, height: u32) -> Thumbnail {
    let target_width = THUMBNAIL_WIDTH.min(width).max(1);
    let target_height = ((height as f32 / width as f32) * target_width as f32).round().max(1.0) as u32;

    let mut rgba = Vec::with_capacity((target_width * target_height * 4) as usize);
    for ty in 0..target_height {
        for tx in 0..target_width {
            // Sample the block of source pixels this output pixel covers.
            let x0 = tx * width / target_width;
            let x1 = ((tx + 1) * width / target_width).max(x0 + 1).min(width);
            let y0 = ty * height / target_height;
            let y1 = ((ty + 1) * height / target_height).max(y0 + 1).min(height);

            let (mut r, mut g, mut b, mut count) = (0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let index = ((y * width + x) * 4) as usize;
                    if index + 3 >= source.len() {
                        continue;
                    }
                    b += source[index] as u32;
                    g += source[index + 1] as u32;
                    r += source[index + 2] as u32;
                    count += 1;
                }
            }
            if count == 0 {
                rgba.extend_from_slice(&[0, 0, 0, 255]);
            } else {
                rgba.extend_from_slice(&[
                    (r / count) as u8,
                    (g / count) as u8,
                    (b / count) as u8,
                    255,
                ]);
            }
        }
    }

    Thumbnail { width: target_width, height: target_height, rgba }
}

/// What to ask for, given what was asked for and what arrived.
///
/// Position is corrected by the difference and size by the ratio, because the
/// two go wrong in different ways: an offset is added to a position, while a
/// scale multiplies a size. Asking for the square of the request over the
/// result cancels a scale factor exactly in one step, where adding the
/// difference would only close part of the gap.
fn corrected_request(wanted: Bounds, actual: Bounds) -> Bounds {
    let scale = |wanted: i32, actual: i32| {
        if actual <= 0 || wanted <= 0 {
            wanted
        } else {
            ((wanted as i64 * wanted as i64) / actual as i64) as i32
        }
    };
    Bounds::new(
        wanted.x + (wanted.x - actual.x),
        wanted.y + (wanted.y - actual.y),
        scale(wanted.width, actual.width),
        scale(wanted.height, actual.height),
    )
}

fn handle_to_hwnd(handle: u64) -> HWND {
    HWND(handle as *mut c_void)
}

impl Platform for Win32Platform {
    /// DPI awareness, because a process that is not per-monitor aware is told
    /// scaled coordinates and its windows land at the wrong size.
    fn diagnostic_notes(&self) -> Vec<String> {
        let awareness = unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) };
        let described = match awareness {
            DPI_AWARENESS_UNAWARE => "unaware (coordinates will be scaled)",
            DPI_AWARENESS_SYSTEM_AWARE => "system aware (scaled on other displays)",
            DPI_AWARENESS_PER_MONITOR_AWARE => "per-monitor aware",
            _ => "invalid or unknown",
        };
        vec![format!("process DPI awareness: {described}")]
    }

    /// Dark title bar and rounded corners, both introduced with Windows 11.
    ///
    /// Errors are dropped on purpose rather than out of laziness. Windows 10
    /// does not recognise either attribute and answers `E_INVALIDARG`, which
    /// is not a failure: it means the frame stays as it was, which is what
    /// running on Windows 10 looks like. There is nothing for the user to do
    /// about it and nothing worth putting in the status strip.
    ///
    /// Mica (`DWMWA_SYSTEMBACKDROP_TYPE`) is deliberately absent. The backdrop
    /// only shows through a transparent window background, so egui's opaque
    /// `panel_fill` would have to go translucent — and during a broadcast that
    /// means whatever happens to be behind the window shows through it.
    fn apply_window_chrome(&self, handle: u64) {
        let hwnd = handle_to_hwnd(handle);
        unsafe {
            let dark = BOOL::from(true);
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                std::ptr::addr_of!(dark).cast(),
                std::mem::size_of::<BOOL>() as u32,
            );

            let rounded = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                std::ptr::addr_of!(rounded).cast(),
                std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            );
        }
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        let mut monitors = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(
                None,
                None,
                Some(collect_monitor),
                LPARAM(&mut monitors as *mut _ as isize),
            );
        }
        monitors
    }

    fn candidate_windows(&self) -> Vec<WindowCandidate> {
        let mut windows = Vec::new();
        unsafe {
            let _ = EnumWindows(
                Some(collect_window),
                LPARAM(&mut windows as *mut _ as isize),
            );
        }
        windows
    }

    fn thumbnail(&self, handle: u64) -> Option<Thumbnail> {
        unsafe { capture(handle_to_hwnd(handle)) }
    }

    fn unminimize(&self, handle: u64) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(PlatformError::WindowGone);
            }
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
        }
        Ok(())
    }

    /// Clear always-on-top and drop to the back, without telling the
    /// application anything. A video playing behind the sent window keeps
    /// playing, which a minimise could not promise.
    fn demote(&self, handle: u64) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(PlatformError::WindowGone);
            }
            // Two calls rather than one: leaving the topmost band and moving
            // within the ordinary band are separate placements, and asking for
            // both at once leaves the window at the top of the wrong one.
            let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
            let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
            let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 0, 0, flags);
        }
        Ok(())
    }

    fn raise(&self, handle: u64, topmost: bool) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(PlatformError::WindowGone);
            }
            let _ = SetWindowPos(
                hwnd,
                Some(if topmost { HWND_TOPMOST } else { HWND_TOP }),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        Ok(())
    }

    fn minimize(&self, handle: u64) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(PlatformError::WindowGone);
            }
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
        Ok(())
    }

    /// Take the foreground, the way clicking a window does.
    ///
    /// Windows refuses `SetForegroundWindow` to a process that does not
    /// already own the foreground. Attaching to the foreground thread's input
    /// queue is the documented way round that, and pressing Send is precisely
    /// the explicit user request the restriction exists to distinguish from an
    /// application grabbing attention on its own.
    fn activate(&self, handle: u64) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(PlatformError::WindowGone);
            }

            let foreground = GetForegroundWindow();
            let foreground_thread = GetWindowThreadProcessId(foreground, None);
            let ours = GetCurrentThreadId();

            let attached = foreground_thread != 0
                && foreground_thread != ours
                && AttachThreadInput(ours, foreground_thread, true).as_bool();

            let _ = SetForegroundWindow(hwnd);
            let _ = BringWindowToTop(hwnd);

            if attached {
                let _ = AttachThreadInput(ours, foreground_thread, false);
            }
        }
        Ok(())
    }

    /// Press the chord with `SendInput`, the way typing it does.
    ///
    /// Injected input goes to whatever has focus, not to a window of the
    /// caller's choosing, so the foreground check is not an optimisation — it
    /// is the difference between toggling the player and typing an F into
    /// someone's document.
    fn send_key(&self, handle: u64, chord: KeyChord) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(PlatformError::WindowGone);
            }
            if GetForegroundWindow() != hwnd {
                return Err(PlatformError::Denied(
                    "the window is not in the foreground".into(),
                ));
            }

            // A modifier the user is physically holding — most likely the
            // Retrieve hotkey they just pressed — combines with the injected
            // events and turns F into Ctrl+Alt+F. Refuse rather than send a
            // corrupted chord; by the next attempt the keys are released.
            let pressed = |vk: VIRTUAL_KEY| GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0;
            let corrupting = [
                (!chord.ctrl && pressed(VK_CONTROL), "Ctrl"),
                (!chord.alt && pressed(VK_MENU), "Alt"),
                (!chord.shift && pressed(VK_SHIFT), "Shift"),
                (pressed(VK_LWIN) || pressed(VK_RWIN), "Win"),
            ];
            if let Some((_, name)) = corrupting.iter().find(|(held, _)| *held) {
                return Err(PlatformError::Denied(format!(
                    "{name} is still held down and would corrupt the keystroke"
                )));
            }

            let key_event = |vk: u16, up: bool| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        wScan: 0,
                        dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };

            // Modifiers down, key down, key up, modifiers up in reverse — the
            // order the same chord arrives in when typed.
            let modifiers: Vec<u16> = [
                (chord.ctrl, VK_CONTROL),
                (chord.alt, VK_MENU),
                (chord.shift, VK_SHIFT),
            ]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, vk)| vk.0)
            .collect();

            let mut inputs = Vec::with_capacity(modifiers.len() * 2 + 2);
            inputs.extend(modifiers.iter().map(|&vk| key_event(vk, false)));
            inputs.push(key_event(chord.key.virtual_key(), false));
            inputs.push(key_event(chord.key.virtual_key(), true));
            inputs.extend(modifiers.iter().rev().map(|&vk| key_event(vk, true)));

            let injected = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
            if injected as usize != inputs.len() {
                // UIPI swallows input aimed at a higher-integrity process and
                // reports it as blocked. Name the fix rather than the API.
                return Err(PlatformError::Denied(
                    "the input was blocked. If the player runs as administrator, WinSend must too"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    fn hide(&self, handle: u64) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(PlatformError::WindowGone);
            }
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        Ok(())
    }

    fn set_window_opacity(&self, handle: u64, alpha: f32) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(PlatformError::WindowGone);
            }

            // Only add the bit if it is not already there, and remember that
            // we were the ones who added it. A window that arrived layered
            // keeps its style when we are done with it.
            let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            if current & WS_EX_LAYERED.0 as isize == 0 {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, current | WS_EX_LAYERED.0 as isize);
                self.layered.borrow_mut().insert(handle);
            }

            let opacity = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
            SetLayeredWindowAttributes(hwnd, COLORREF(0), opacity, LWA_ALPHA)
                .map_err(|e| PlatformError::Denied(e.to_string()))
        }
    }

    fn clear_window_opacity(&self, handle: u64) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        let we_layered_it = self.layered.borrow_mut().remove(&handle);

        unsafe {
            // A window that has closed has nothing left to put back, and
            // nothing left translucent either. Not an error: this runs on
            // every failure path, and the window closing is one of them.
            if !IsWindow(Some(hwnd)).as_bool() {
                return Ok(());
            }

            // Full opacity first, so a window that arrived already layered —
            // and therefore keeps its style below — still ends up opaque.
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);

            if we_layered_it {
                let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, current & !(WS_EX_LAYERED.0 as isize));
                // Leaving the band is a frame change like any other, and the
                // window does not repaint out of it until it is told to.
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
        }
        Ok(())
    }

    fn show(&self, handle: u64) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(PlatformError::WindowGone);
            }
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        Ok(())
    }

    fn window_bounds(&self, handle: u64) -> Result<Bounds, PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        let mut rect = RECT::default();
        unsafe {
            GetWindowRect(hwnd, &mut rect).map_err(|_| PlatformError::WindowGone)?;
        }
        Ok(rect_to_bounds(rect))
    }

    fn place_window(&self, handle: u64, placement: Placement) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);

        unsafe {
            // A maximised window ignores explicit sizing, so drop it out of
            // that state before positioning it.
            if IsZoomed(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }

            if placement.borderless {
                let current = GetWindowLongPtrW(hwnd, GWL_STYLE);
                if current != 0 {
                    let chrome = (WS_CAPTION.0 | WS_THICKFRAME.0 | WS_MINIMIZEBOX.0
                        | WS_MAXIMIZEBOX.0
                        | WS_SYSMENU.0) as isize;
                    // Only the bits actually present, accumulated rather than
                    // replaced. Sending twice used to record the already
                    // stripped style as the thing to restore, which put the
                    // window back frameless and permanently.
                    let clearing = current & chrome;
                    if clearing != 0 {
                        *self.cleared_styles.borrow_mut().entry(handle).or_insert(0) |= clearing;
                    }
                    SetWindowLongPtrW(hwnd, GWL_STYLE, current & !chrome);
                }
            } else if let Some(cleared) = self.cleared_styles.borrow_mut().remove(&handle) {
                // Put back what was taken, on top of whatever the style is now,
                // so anything the application changed in between survives.
                let current = GetWindowLongPtrW(hwnd, GWL_STYLE);
                SetWindowLongPtrW(hwnd, GWL_STYLE, current | cleared);
            }

            // Which band to place the window in, and what to put back.
            //
            // Retrieve must not assume the window started out ordinary: Zoom
            // has its own always-on-top option, and clearing it silently would
            // be changing a setting the user made in another application.
            let insert_after = if placement.topmost {
                self.was_topmost
                    .borrow_mut()
                    .entry(handle)
                    .or_insert_with(|| is_topmost(hwnd));
                HWND_TOPMOST
            } else if self.was_topmost.borrow_mut().remove(&handle).unwrap_or(false) {
                HWND_TOPMOST
            } else {
                HWND_NOTOPMOST
            };

            let flags = SWP_NOACTIVATE | SWP_FRAMECHANGED;
            SetWindowPos(
                hwnd,
                Some(insert_after),
                placement.bounds.x,
                placement.bounds.y,
                placement.bounds.width,
                placement.bounds.height,
                // No SWP_NOZORDER: changing the band is the point.
                flags,
            )
            .map_err(|e| {
                // Moving a window owned by an elevated process from a
                // non-elevated one fails here, and the raw message is unhelpful.
                PlatformError::Denied(format!(
                    "{e}. If Zoom is running as administrator, WinSend must be too."
                ))
            })?;

            // Ask, measure, and correct once.
            //
            // A window does not always end up where it was put: coordinates can
            // be scaled on a display whose DPI differs from the one the process
            // was told about, and an application can resize itself in response
            // to the move. Correcting by the observed error puts it right
            // whichever it was, and one attempt avoids fighting an application
            // that is determined to have its own way.
            let mut actual = RECT::default();
            if GetWindowRect(hwnd, &mut actual).is_ok() {
                let actual = rect_to_bounds(actual);
                if actual != placement.bounds {
                    let corrected = corrected_request(placement.bounds, actual);
                    let _ = SetWindowPos(
                        hwnd,
                        Some(insert_after),
                        corrected.x,
                        corrected.y,
                        corrected.width,
                        corrected.height,
                        flags,
                    );
                }
            }
        }

        Ok(())
    }
}
