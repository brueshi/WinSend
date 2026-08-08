//! Throwaway diagnostic. Dumps the monitor layout and every top-level window
//! with its class, title, owning process, bounds and style bits, so the Zoom
//! dual-monitor video window can be positively identified before any detection
//! logic is written against it.
//!
//! Delete this file once detection is confirmed.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFOEXW,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetClientRect, GetWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed, GWL_EXSTYLE,
    GWL_STYLE, GW_OWNER, MONITORINFOF_PRIMARY, WS_EX_TOPMOST,
};

/// Style bits worth naming in the dump. Chrome-related ones matter most: they
/// tell us how much we would have to strip to get a borderless fill.
const STYLE_BITS: &[(u32, &str)] = &[
    (0x8000_0000, "WS_POPUP"),
    (0x4000_0000, "WS_CHILD"),
    (0x2000_0000, "WS_MINIMIZE"),
    (0x1000_0000, "WS_VISIBLE"),
    (0x0800_0000, "WS_DISABLED"),
    (0x0100_0000, "WS_MAXIMIZE"),
    (0x00C0_0000, "WS_CAPTION"),
    (0x0080_0000, "WS_BORDER"),
    (0x0040_0000, "WS_DLGFRAME"),
    (0x0020_0000, "WS_VSCROLL"),
    (0x0010_0000, "WS_HSCROLL"),
    (0x0008_0000, "WS_SYSMENU"),
    (0x0004_0000, "WS_THICKFRAME"),
    (0x0002_0000, "WS_MINIMIZEBOX"),
    (0x0001_0000, "WS_MAXIMIZEBOX"),
];

const EXSTYLE_BITS: &[(u32, &str)] = &[
    (0x0000_0001, "WS_EX_DLGMODALFRAME"),
    (0x0000_0008, "WS_EX_TOPMOST"),
    (0x0000_0020, "WS_EX_TRANSPARENT"),
    (0x0000_0040, "WS_EX_MDICHILD"),
    (0x0000_0080, "WS_EX_TOOLWINDOW"),
    (0x0000_0100, "WS_EX_WINDOWEDGE"),
    (0x0000_0200, "WS_EX_CLIENTEDGE"),
    (0x0002_0000, "WS_EX_STATICEDGE"),
    (0x0004_0000, "WS_EX_APPWINDOW"),
    (0x0008_0000, "WS_EX_LAYERED"),
    (0x0010_0000, "WS_EX_NOINHERITLAYOUT"),
    (0x0080_0000, "WS_EX_NOACTIVATE"),
];

fn decode(value: u32, table: &[(u32, &str)]) -> String {
    let names: Vec<&str> = table
        .iter()
        .filter(|(bit, _)| value & bit == *bit)
        .map(|(_, name)| *name)
        .collect();
    if names.is_empty() {
        "-".to_string()
    } else {
        names.join(" | ")
    }
}

fn wide_to_string(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    OsString::from_wide(&buffer[..end]).to_string_lossy().into_owned()
}

struct WindowInfo {
    hwnd: HWND,
    pid: u32,
    process_path: String,
    class_name: String,
    title: String,
    rect: RECT,
    client_w: i32,
    client_h: i32,
    monitor: String,
    visible: bool,
    cloaked: bool,
    minimized: bool,
    maximized: bool,
    owner: isize,
    style: u32,
    exstyle: u32,
}

impl WindowInfo {
    fn process_name(&self) -> &str {
        self.process_path
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(&self.process_path)
    }

    /// Anything that plausibly belongs to Zoom. Deliberately loose: it is far
    /// better to show a few unrelated windows than to filter out the target.
    fn looks_zoom_related(&self) -> bool {
        let haystack = format!(
            "{} {} {}",
            self.process_name(),
            self.class_name,
            self.title
        )
        .to_lowercase();
        ["zoom", "zp", "cpthost", "conf", "video"]
            .iter()
            .any(|needle| haystack.contains(needle))
    }

    fn print(&self, index: usize) {
        let RECT { left, top, right, bottom } = self.rect;
        println!("--- [{index}] HWND=0x{:X}", self.hwnd.0 as isize);
        println!("  process      : {} ({})", self.process_name(), self.pid);
        println!("  full path    : {}", self.process_path);
        println!("  class        : {}", self.class_name);
        println!("  title        : {:?}", self.title);
        println!(
            "  window rect  : ({left},{top}) -> ({right},{bottom})  {}x{}",
            right - left,
            bottom - top
        );
        println!("  client size  : {}x{}", self.client_w, self.client_h);
        println!("  monitor      : {}", self.monitor);
        println!(
            "  state        : visible={} cloaked={} minimized={} maximized={}",
            self.visible, self.cloaked, self.minimized, self.maximized
        );
        println!("  owner        : 0x{:X}", self.owner);
        println!("  style        : 0x{:08X}  {}", self.style, decode(self.style, STYLE_BITS));
        println!(
            "  exstyle      : 0x{:08X}  {}",
            self.exstyle,
            decode(self.exstyle, EXSTYLE_BITS)
        );
        println!();
    }
}

unsafe fn process_path_for(pid: u32) -> String {
    let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
        Ok(handle) => handle,
        Err(_) => return "<access denied>".to_string(),
    };
    let mut buffer = [0u16; 512];
    let mut len = buffer.len() as u32;
    let path = match QueryFullProcessImageNameW(
        handle,
        PROCESS_NAME_WIN32,
        windows::core::PWSTR(buffer.as_mut_ptr()),
        &mut len,
    ) {
        Ok(()) => wide_to_string(&buffer[..len as usize]),
        Err(_) => "<unknown>".to_string(),
    };
    let _ = CloseHandle(handle);
    path
}

unsafe fn monitor_name_for(hwnd: HWND) -> String {
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    if monitor.is_invalid() {
        return "<none>".to_string();
    }
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(monitor, &mut info as *mut _ as *mut _).as_bool() {
        wide_to_string(&info.szDevice)
    } else {
        "<unknown>".to_string()
    }
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows_out = &mut *(lparam.0 as *mut Vec<WindowInfo>);

    let mut class_buffer = [0u16; 256];
    let class_len = GetClassNameW(hwnd, &mut class_buffer);
    let class_name = wide_to_string(&class_buffer[..class_len.max(0) as usize]);

    let mut title_buffer = [0u16; 512];
    let title_len = GetWindowTextW(hwnd, &mut title_buffer);
    let title = wide_to_string(&title_buffer[..title_len.max(0) as usize]);

    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));

    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);

    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);

    let mut cloaked_value = 0u32;
    let cloaked = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked_value as *mut _ as *mut _,
        std::mem::size_of::<u32>() as u32,
    )
    .is_ok()
        && cloaked_value != 0;

    windows_out.push(WindowInfo {
        hwnd,
        pid,
        process_path: process_path_for(pid),
        class_name,
        title,
        rect,
        client_w: client.right - client.left,
        client_h: client.bottom - client.top,
        monitor: monitor_name_for(hwnd),
        visible: IsWindowVisible(hwnd).as_bool(),
        cloaked,
        minimized: IsIconic(hwnd).as_bool(),
        maximized: IsZoomed(hwnd).as_bool(),
        owner: GetWindow(hwnd, GW_OWNER).map(|h| h.0 as isize).unwrap_or(0),
        style: GetWindowLongPtrW(hwnd, GWL_STYLE) as u32,
        exstyle: GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32,
    });

    BOOL(1)
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = &mut *(lparam.0 as *mut Vec<(String, RECT, RECT, bool)>);
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(monitor, &mut info as *mut _ as *mut _).as_bool() {
        monitors.push((
            wide_to_string(&info.szDevice),
            info.monitorInfo.rcMonitor,
            info.monitorInfo.rcWork,
            info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        ));
    }
    BOOL(1)
}

fn main() {
    unsafe {
        // Without this, GetWindowRect returns DPI-virtualised coordinates on
        // mixed-scaling setups and every number below would be a lie.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let mut monitors: Vec<(String, RECT, RECT, bool)> = Vec::new();
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut monitors as *mut _ as isize),
        );

        println!("=== MONITORS ({}) ===\n", monitors.len());
        for (index, (device, bounds, work, primary)) in monitors.iter().enumerate() {
            println!(
                "[{index}] {device}{}\n     bounds ({},{}) -> ({},{})  {}x{}\n     work   ({},{}) -> ({},{})",
                if *primary { "  [PRIMARY]" } else { "" },
                bounds.left,
                bounds.top,
                bounds.right,
                bounds.bottom,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                work.left,
                work.top,
                work.right,
                work.bottom,
            );
        }
        println!();

        let mut all: Vec<WindowInfo> = Vec::new();
        let _ = EnumWindows(Some(collect_window), LPARAM(&mut all as *mut _ as isize));

        let zoom_related: Vec<&WindowInfo> =
            all.iter().filter(|w| w.looks_zoom_related()).collect();

        println!("=== ZOOM-RELATED WINDOWS ({}) ===\n", zoom_related.len());
        for (index, window) in zoom_related.iter().enumerate() {
            window.print(index);
        }

        // The filtered list above can miss the target if Zoom hosts the video
        // window in a process whose name gives nothing away, so dump everything
        // real as well.
        let visible: Vec<&WindowInfo> = all
            .iter()
            .filter(|w| w.visible && !w.cloaked && !w.title.is_empty())
            .collect();

        println!("=== ALL VISIBLE TITLED TOP-LEVEL WINDOWS ({}) ===\n", visible.len());
        for (index, window) in visible.iter().enumerate() {
            window.print(index);
        }

        // The compact view. EnumWindows walks the stacking order from the
        // front, so this row order is literally what is in front of what —
        // which is the one question three attempts at the z-order problem kept
        // having to guess at.
        println!("=== STACKING ORDER, FRONT TO BACK ===\n");
        println!(
            "{:>3}  {:<7}  {:<7}  {:<14}  {:<24}  {:<16}  {}",
            "#", "TOPMOST", "STATE", "MONITOR", "BOUNDS", "PROCESS", "TITLE"
        );
        let on_screen: Vec<&WindowInfo> =
            all.iter().filter(|w| w.visible && !w.cloaked).collect();
        for (index, window) in on_screen.iter().enumerate() {
            let RECT { left, top, right, bottom } = window.rect;
            let state = if window.minimized {
                "min"
            } else if window.maximized {
                "max"
            } else {
                "-"
            };
            println!(
                "{index:>3}  {:<7}  {state:<7}  {:<14}  {:<24}  {:<16}  {}",
                if window.exstyle & WS_EX_TOPMOST.0 != 0 { "yes" } else { "no" },
                window.monitor,
                format!("{},{} {}x{}", left, top, right - left, bottom - top),
                window.process_name(),
                // Untitled windows are listed rather than skipped: a
                // full-screen video output window usually has no caption, and
                // it is exactly the one worth finding here.
                if window.title.is_empty() { "<untitled>" } else { &window.title },
            );
        }
        println!();

        println!("=== TOTAL TOP-LEVEL WINDOWS ENUMERATED: {} ===", all.len());
    }
}
