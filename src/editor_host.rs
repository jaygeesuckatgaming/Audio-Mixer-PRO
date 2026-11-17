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
    GetClientRect, GetWindowLongPtrW, MessageBoxW, PeekMessageW, RegisterClassExW,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOZORDER, SW_HIDE, SW_SHOW, WM_CLOSE, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WNDCLASSEXW, WS_MAXIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_THICKFRAME,
};

const CLASS_NAME: &str = "MusicRemoverEditorHost";

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
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
            // Bring the editor forward so the plug-in is told it has focus
            // straight away; meters stay parked until `onFocus(true)` arrives.
            windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
        }

        Ok(Self {
            hwnd: hwnd as isize,
            editor: Some(editor),
            closed,
            title,
            size: (width, height),
            plugin: shared,
            focused: false,
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
        let focused = unsafe { GetForegroundWindow() as isize } == self.hwnd;
        if focused == self.focused {
            return;
        }
        if let Ok(mut plugin) = self.plugin.try_lock() {
            if plugin.set_editor_focus(focused).is_ok() {
                self.focused = focused;
            }
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

