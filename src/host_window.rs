//! A plain top-level window owned by the mixer, plus a message pump and a
//! window-to-BMP capture. CLAP's GUI extension attaches the plug-in's view to a
//! window the host owns, so this is the CLAP counterpart of `editor_host`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetAncestor, GetClientRect, GetWindowLongPtrW, IsIconic, PeekMessageW, RegisterClassExW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, CS_HREDRAW,
    CS_VREDRAW, CW_USEDEFAULT, GA_ROOT, GWLP_USERDATA, HWND_TOP, IDC_ARROW, MSG, PM_NOREMOVE,
    PM_REMOVE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_RESTORE,
    SW_SHOW, WM_CLOSE, WNDCLASSEXW, WS_MAXIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_THICKFRAME,
};

const CLASS_NAME: &str = "MusicRemoverHostWindow";

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_CLOSE {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const AtomicBool;
        if !raw.is_null() {
            (*raw).store(true, Ordering::Relaxed);
        }
        ShowWindow(hwnd, windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE);
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
        wc.lpfnWndProc = Some(window_proc);
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

pub struct HostWindow {
    hwnd: isize,
    closed: Arc<AtomicBool>,
    title: Vec<u16>,
}

unsafe impl Send for HostWindow {}

impl HostWindow {
    pub fn open(title: &str, width: i32, height: i32) -> Option<Self> {
        ensure_class();
        let (width, height) = (width.max(120), height.max(80));
        let style = WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX);
        let mut rect = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        unsafe { AdjustWindowRectEx(&mut rect, style, 0, 0) };

        let class: Vec<u16> = CLASS_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
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
                rect.right - rect.left,
                rect.bottom - rect.top,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            unsafe { drop(Arc::from_raw(user as *const AtomicBool)) };
            return None;
        }
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, user);
            ShowWindow(hwnd, SW_SHOW);
        }
        Some(Self {
            hwnd: hwnd as isize,
            closed,
            title,
        })
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Bring the window to the front, restoring it if it was minimised. Used
    /// when the person clicks a strip's insert panel while its editor sits
    /// behind another window.
    pub fn raise(&self) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        let hwnd = self.hwnd as HWND;
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            } else {
                ShowWindow(hwnd, SW_SHOW);
            }
            SetWindowPos(
                hwnd,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            SetForegroundWindow(hwnd);
        }
    }

    pub fn set_client_size(&mut self, width: i32, height: i32) {
        let style = WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX);
        let mut rect = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: width.max(80),
            bottom: height.max(60),
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
    }

    pub fn client_size(&self) -> (i32, i32) {
        let mut client = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe { GetClientRect(self.hwnd as HWND, &mut client) };
        (client.right, client.bottom)
    }

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
        let (cw, ch) = self.client_size();
        let style = WS_OVERLAPPEDWINDOW & !(WS_THICKFRAME | WS_MAXIMIZEBOX);
        let mut outer = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: cw,
            bottom: ch,
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
        if self.hwnd == 0 {
            return;
        }
        unsafe { DestroyWindow(self.hwnd as HWND) };
        self.hwnd = 0;
    }
}

impl Drop for HostWindow {
    fn drop(&mut self) {
        if self.hwnd != 0 {
            let _ = &self.title;
            self.close();
        }
    }
}

/// Drain messages destined for this host window (and its children).
///
/// Plug-in GUIs paint and animate from window messages, and a host that never
/// dispatches them leaves the window blank or frozen. Only this window's
/// messages are dispatched: draining the whole thread queue also fed messages
/// to the mixer's own OpenGL window from inside `update()`, re-entering
/// painting through `wglWndProc -> SwapBuffers`, which froze or spun the mixer.
pub fn pump_messages(root: isize) {
    if root == 0 {
        return;
    }
    let root = root as HWND;
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        loop {
            if PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE) == 0 {
                break;
            }
            if GetAncestor(msg.hwnd, GA_ROOT) != root {
                break;
            }
            if PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) == 0 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Write any window's client area to a BMP, for verifying a GUI painted.
pub fn snapshot_hwnd(hwnd: isize, path: &str) -> Result<(), String> {
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    let mut client = windows_sys::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetClientRect(hwnd as HWND, &mut client) };
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
        let dib = CreateDIBSection(
            dc,
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if dib.is_null() || bits.is_null() {
            DeleteDC(dc);
            return Err("no DIB".to_string());
        }
        let old = SelectObject(dc, dib);
        let ok = windows_sys::Win32::Storage::Xps::PrintWindow(hwnd as HWND, dc, 2);
        if ok == 0 {
            SelectObject(dc, old);
            DeleteObject(dib);
            DeleteDC(dc);
            return Err("PrintWindow failed".to_string());
        }
        let mut data = vec![0u8; (w * h * 4) as usize];
        std::ptr::copy_nonoverlapping(bits as *const u8, data.as_mut_ptr(), data.len());
        SelectObject(dc, old);
        DeleteObject(dib);
        DeleteDC(dc);

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
