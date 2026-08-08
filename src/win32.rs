//! The real platform. Everything here is unverified until it runs against a
//! live Zoom session on Windows.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;

use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
    GetMonitorInfoW, MonitorFromWindow, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HDC, HMONITOR, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, IsZoomed, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, GWL_EXSTYLE, GWL_STYLE, HWND_BOTTOM, HWND_NOTOPMOST,
    HWND_TOPMOST, MONITORINFOF_PRIMARY, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SW_MINIMIZE, SW_RESTORE, WS_CAPTION, WS_EX_TOPMOST, WS_MAXIMIZEBOX,
    WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};

use crate::platform::{
    Bounds, MonitorInfo, Placement, Platform, PlatformError, Thumbnail, WindowCandidate,
};

/// Renders the window's full content even when it is occluded or composited by
/// the GPU. Without this flag, hardware-accelerated surfaces capture as black.
const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

const THUMBNAIL_WIDTH: u32 = 192;

pub struct Win32Platform {
    /// Window styles we stripped to go borderless, so they can be put back.
    /// Held here rather than in `Core` because it is a detail of how this
    /// platform achieves a borderless fill, not something the app logic needs.
    stripped_styles: RefCell<HashMap<u64, isize>>,
    /// Whether a window was already topmost before we raised it, so putting it
    /// back does not quietly clear an always-on-top the user set themselves.
    was_topmost: RefCell<HashMap<u64, bool>>,
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
            stripped_styles: RefCell::new(HashMap::new()),
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

        for (handle, style) in self.stripped_styles.borrow().iter() {
            let hwnd = handle_to_hwnd(*handle);
            unsafe {
                if !IsWindow(Some(hwnd)).as_bool() {
                    continue;
                }
                SetWindowLongPtrW(hwnd, GWL_STYLE, *style);
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

    // Cloaked windows are not really on screen. Minimised ones are still
    // reported, flagged, so the confirmed window can be found and restored
    // rather than looking like it vanished; the picker filters them out.
    if !IsWindowVisible(hwnd).as_bool() || is_cloaked(hwnd) {
        return BOOL(1);
    }

    let mut title_buffer = [0u16; 512];
    let title_len = GetWindowTextW(hwnd, &mut title_buffer);
    let title = wide_to_string(&title_buffer[..title_len.max(0) as usize]);
    if title.is_empty() {
        return BOOL(1);
    }

    let mut class_buffer = [0u16; 256];
    let class_len = GetClassNameW(hwnd, &mut class_buffer);
    let class_name = wide_to_string(&class_buffer[..class_len.max(0) as usize]);

    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let process = process_name(pid);

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

fn handle_to_hwnd(handle: u64) -> HWND {
    HWND(handle as *mut c_void)
}

impl Platform for Win32Platform {
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

    fn promote(&self, handle: u64) -> Result<(), PlatformError> {
        let hwnd = handle_to_hwnd(handle);
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(PlatformError::WindowGone);
            }
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
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
                    self.stripped_styles.borrow_mut().insert(handle, current);
                    SetWindowLongPtrW(hwnd, GWL_STYLE, current & !chrome);
                }
            } else if let Some(original) = self.stripped_styles.borrow_mut().remove(&handle) {
                SetWindowLongPtrW(hwnd, GWL_STYLE, original);
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

            SetWindowPos(
                hwnd,
                Some(insert_after),
                placement.bounds.x,
                placement.bounds.y,
                placement.bounds.width,
                placement.bounds.height,
                // No SWP_NOZORDER: changing the band is the point. SWP_NOACTIVATE
                // stays, so whatever is playing underneath keeps focus and is
                // not interrupted by the window arriving over it.
                SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )
            .map_err(|e| {
                // Moving a window owned by an elevated process from a
                // non-elevated one fails here, and the raw message is unhelpful.
                PlatformError::Denied(format!(
                    "{e}. If Zoom is running as administrator, WinSend must be too."
                ))
            })?;
        }

        Ok(())
    }
}
