//! Editor hosting that mirrors what a normal VST host does: the plug-in's
//! editor is embedded as a **child** of a window this mixer owns, instead of
//! being handed a bare top-level window.
//!
//! That matters in practice. vst3-host's `PluginWindow` creates a standalone
//! top-level window and never sends `IPlugView::onFocus`, which leaves some
//! plug-ins (VSTGUI ones such as LoudMax) with a correct-looking GUI whose
//! meters never update, and makes file-dialog-based preset menus (TDR) refuse
//! to open. Embedding into a window we create ourselves is the arrangement
//! VSTHost uses and is what those plug-ins expect.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use raw_window_handle::{RawWindowHandle, Win32WindowHandle};
use vst3_host::embed::{EditorRect, EmbeddedEditor};
use vst3_host::Plugin;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetWindowLongPtrW, PeekMessageW, RegisterClassExW, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    GWLP_USERDATA, IDC_ARROW, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_HIDE,
    SW_SHOW, WM_ACTIVATE, WM_CLOSE, WM_KILLFOCUS, WM_SETFOCUS, WNDCLASSEXW, WS_MAXIMIZEBOX,
    WS_OVERLAPPEDWINDOW, WS_THICKFRAME,
};

const CLASS_NAME: &str = "MusicRemoverEditorHost";

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_ACTIVATE || msg == WM_SETFOCUS || msg == WM_KILLFOCUS {
        crate::diag_log(format!("editor: host msg {msg:#06x} wparam {wp} (hwnd {hwnd:p})"));
    }
    if msg == WM_CLOSE {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const AtomicBool;
        if !raw.is_null() {
            (*raw).store(true, Ordering::Relaxed);
        }
        // Hiding rather than destroying keeps the plug-in's child window alive
        // until the editor has been detached in the normal teardown path.
        ShowWindow(hwnd, SW_HIDE);
        return 0;
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn first_child(parent: HWND) -> Option<HWND> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumChildWindows, IsWindowVisible};
    unsafe extern "system" fn visit(hwnd: HWND, lp: LPARAM) -> i32 {
        let out = &mut *(lp as *mut Option<HWND>);
        if out.is_none() && unsafe { IsWindowVisible(hwnd) } != 0 {
            *out = Some(hwnd);
        }
        1
    }
    let mut found: Option<HWND> = None;
    unsafe {
        EnumChildWindows(
            parent,
            Some(visit),
            &mut found as *mut Option<HWND> as LPARAM,
        )
    };
    found
}

fn ensure_class() {
    static REGISTERED: std::sync::Once = std::sync::Once::new();
    REGISTERED.call_once(|| unsafe {
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(host_proc);
        wc.hInstance = GetModuleHandleW(std::ptr::null());
        wc.hCursor = windows_sys::Win32::UI::WindowsAndMessaging::LoadCursorW(
            std::ptr::null_mut(),
            IDC_ARROW,
        );
        let name: Vec<u16> = CLASS_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        wc.lpszClassName = name.as_ptr();
        RegisterClassExW(&wc);
    });
}

/// A top-level window this mixer owns, with the plug-in editor embedded in it.
pub struct EditorHost {
    hwnd: isize,
    editor: Option<EmbeddedEditor>,
    closed: Arc<AtomicBool>,
    title: Vec<u16>,
    size: (i32, i32),
    plugin: Arc<std::sync::Mutex<Plugin>>,
    /// Last focus state handed to the view. VST 3 requires the host to call
    /// `IPlugView::onFocus`, and plug-ins run or park their meter timers on it.
    focused: bool,
}

unsafe impl Send for EditorHost {}

impl EditorHost {
    /// Create the host window sized to the editor and embed the plug-in into it.
    pub fn open(plugin: Arc<std::sync::Mutex<Plugin>>, title: &str) -> Result<Self, String> {
        let (width, height) = {
            let mut guard = plugin
                .lock()
                .map_err(|_| "The VST slot is unavailable".to_string())?;
            guard.get_editor_size().unwrap_or((640, 480))
        };
        let (width, height) = (width.max(120), height.max(80));

        ensure_class();
        let style = WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX);
        let mut rect = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        unsafe {
            AdjustWindowRectEx(&mut rect, style, 0, 0);
        }
        let outer_w = rect.right - rect.left;
        let outer_h = rect.bottom - rect.top;

        let class: Vec<u16> = CLASS_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let title: Vec<u16> = format!("{title} - VST3")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let closed = Arc::new(AtomicBool::new(false));
        let user = Arc::into_raw(closed.clone()) as isize;

        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                outer_w,
                outer_h,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            unsafe { drop(Arc::from_raw(user as *const AtomicBool)) };
            return Err("Could not create the plug-in's window".to_string());
        }
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, user) };

        let Some(raw_hwnd) = std::num::NonZeroIsize::new(hwnd as isize) else {
            unsafe { DestroyWindow(hwnd) };
            return Err("Invalid window handle".to_string());
        };
        let handle = RawWindowHandle::Win32(Win32WindowHandle::new(raw_hwnd));

        let shared = plugin.clone();
        let editor = match EmbeddedEditor::embed(
            plugin,
            handle,
            EditorRect {
                x: 0.0,
                y: 0.0,
                width: width as f32,
                height: height as f32,
            },
        ) {
            Ok(editor) => editor,
            Err(err) => {
                unsafe { DestroyWindow(hwnd) };
                return Err(format!("Could not open the plug-in's own screen: {err}"));
            }
        };

        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
            // Hand keyboard focus to the plug-in's own child window. Without
            // this the top-level window keeps focus and plug-ins that run their
            // meters only while focused never start.
            if let Some(child) = first_child(hwnd) {
                windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(child);
            }
        }

        // Tell the view it has focus right away. Plug-ins start their meter
        // timers from `IPlugView::onFocus`, and waiting for a real foreground
        // change never arrives here (Windows refuses our SetForegroundWindow),
        // which left editors looking correct with permanently dead meters until
        // the person right-clicked the title bar and activated the window.
        let mut focused = false;
        if let Ok(mut plugin) = shared.lock() {
            focused = plugin.set_editor_focus(true).is_ok();
        }

        Ok(Self {
            hwnd: hwnd as isize,
            editor: Some(editor),
            closed,
            title,
            size: (width, height),
            plugin: shared,
            focused,
        })
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    /// Apply the plug-in's own resize requests and keep the window matched to
    /// the editor. Returns false once the person closed the window.
    pub fn service(&mut self) -> bool {
        if self.closed.load(Ordering::Relaxed) {
            return false;
        }
        self.report_focus();
        let Some(editor) = self.editor.as_ref() else {
            return false;
        };
        if let Some((width, height)) = editor.take_resize_request() {
            let (w, h) = (width.max(120), height.max(80));
            let style = WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX);
            let mut rect = windows_sys::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            unsafe {
                AdjustWindowRectEx(&mut rect, style, 0, 0);
                SetWindowPos(
                    self.hwnd as HWND,
                    std::ptr::null_mut(),
                    0,
                    0,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            editor.set_rect(EditorRect {
                x: 0.0,
                y: 0.0,
                width: w as f32,
                height: h as f32,
            });
            self.size = (w, h);
        }
        true
    }

    /// Tell the view whether its window is focused, once per UI frame. The
    /// audio thread holds the plug-in lock per block, so a `try_lock` that
    /// loses is simply retried next frame.
    fn report_focus(&mut self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        let foreground = unsafe { GetForegroundWindow() as isize };
        let focused = foreground == self.hwnd;
        if focused == self.focused || !focused {
            return;
        }
        match self.plugin.try_lock() {
            Ok(mut plugin) => match plugin.set_editor_focus(focused) {
                Ok(()) => {
                    crate::diag_log(format!(
                        "editor: onFocus({focused}) sent (foreground {foreground:#x}, host {:#x})",
                        self.hwnd
                    ));
                    self.focused = focused;
                }
                Err(err) => {
                    crate::diag_log(format!("editor: onFocus({focused}) rejected: {err}"));
                }
            },
            Err(_) => {}
        }
    }

    pub fn size(&self) -> (i32, i32) {
        self.size
    }

    pub fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Relaxed) && self.editor.is_some()
    }

    /// Place the window next to the mixer and pin its frame, the same geometry
    /// the standalone window path used.
    pub fn place_beside(&mut self, mixer: isize) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetSystemMetrics, GetWindowRect, SM_CXVIRTUALSCREEN,
            SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        };
        let parent = if mixer != 0 {
            mixer
        } else {
            unsafe { GetForegroundWindow() as isize }
        };
        if parent == 0 {
            return;
        }
        let mut rect = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if unsafe { GetWindowRect(parent as HWND, &mut rect) } == 0 {
            return;
        }
        let (left, top, width, height) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let x = (rect.left + 48).clamp(left, left + width - 240);
        let y = (rect.top + 72).clamp(top, top + height - 120);
        let mut client = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe { GetClientRect(self.hwnd as HWND, &mut client) };
        let style = WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX);
        let mut outer = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: client.right,
            bottom: client.bottom,
        };
        unsafe {
            AdjustWindowRectEx(&mut outer, style, 0, 0);
            SetWindowPos(
                self.hwnd as HWND,
                std::ptr::null_mut(),
                x,
                y,
                outer.right - outer.left,
                outer.bottom - outer.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    pub fn close(&mut self) {
        self.editor = None;
        unsafe {
            let raw = GetWindowLongPtrW(self.hwnd as HWND, GWLP_USERDATA) as *const AtomicBool;
            SetWindowLongPtrW(self.hwnd as HWND, GWLP_USERDATA, 0);
            DestroyWindow(self.hwnd as HWND);
            if !raw.is_null() {
                drop(Arc::from_raw(raw));
            }
        }
        self.hwnd = 0;
    }

    /// Write the editor's client area to a BMP. Used by `--vst-shot` so the
    /// meters can be inspected without another process stealing focus.
    pub fn snapshot(&self, path: &str) -> Result<(), String> {
        use windows_sys::Win32::Graphics::Gdi::{
            BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
            BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
        };
        let mut client = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe { GetClientRect(self.hwnd as HWND, &mut client) };
        let (w, h) = (client.right.max(1), client.bottom.max(1));

        unsafe {
            let dc = CreateCompatibleDC(std::ptr::null_mut());
            if dc.is_null() {
                return Err("no memory DC".to_string());
            }
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if dib.is_null() || bits.is_null() {
                DeleteDC(dc);
                return Err("no DIB".to_string());
            }
            let old = SelectObject(dc, dib);
            // Ask the window to render itself into the DC; this is the same
            // path a screenshot tool would use and does not need focus.
            let ok = windows_sys::Win32::Storage::Xps::PrintWindow(self.hwnd as HWND, dc, 2);
            if ok == 0 {
                let _ = BitBlt(dc, 0, 0, w, h, dc, 0, 0, SRCCOPY);
            }
            let mut data = vec![0u8; (w * h * 4) as usize];
            std::ptr::copy_nonoverlapping(bits as *const u8, data.as_mut_ptr(), data.len());
            SelectObject(dc, old);
            DeleteObject(dib);
            DeleteDC(dc);

            // 24-bit bottom-up BMP.
            let row = (w * 3) as usize;
            let pad = (4 - row % 4) % 4;
            let mut out = Vec::with_capacity(54 + (row + pad) * h as usize);
            out.extend_from_slice(b"BM");
            let size = 54 + ((row + pad) * h as usize) as u32;
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&[0, 0, 0, 0]);
            out.extend_from_slice(&54u32.to_le_bytes());
            out.extend_from_slice(&40u32.to_le_bytes());
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(&1u16.to_le_bytes());
            out.extend_from_slice(&24u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(((row + pad) * h as usize) as u32).to_le_bytes());
            out.extend_from_slice(&[0u8; 16]);
            for y in (0..h).rev() {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    out.push(data[i]);
                    out.push(data[i + 1]);
                    out.push(data[i + 2]);
                }
                out.extend(std::iter::repeat(0u8).take(pad));
            }
            std::fs::write(path, out).map_err(|e| e.to_string())
        }
    }

    /// Pump this thread's message queue. The mixer's own event loop usually
    /// does this, but plug-in editors that open their own modal dialogs need
    /// the pump to run while they are up.
    pub fn pump_messages() {
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        unsafe {
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl Drop for EditorHost {
    fn drop(&mut self) {
        if self.hwnd != 0 {
            let _ = &self.title;
            self.close();
        }
    }
}

