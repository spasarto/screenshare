//! Best-effort visuals for the setup screen's window picker: a live still of
//! a window's current contents, and its icon as an instant fallback for
//! windows that refuse to render into an off-screen bitmap (protected
//! content, some elevated processes).
//!
//! This deliberately uses GDI (`PrintWindow`) rather than spinning up a
//! Windows.Graphics.Capture session per window: WGC's async, one-capture-
//! session-per-item lifecycle is built for a single long-lived capture, not
//! for cheaply refreshing a grid of N thumbnails on a timer. `PrintWindow`
//! is a single synchronous call, which is what a periodic refresh wants.
//!
//! "Synchronous" is the catch, and why [`Service`] exists: `PrintWindow`
//! blocks until the *target* process's message loop re-renders itself
//! through DWM, measured at 20-36 ms per window on a fast machine and
//! bounded by the other app, not by local CPU. Calling it inline on the
//! render thread for a grid of windows stalls the UI for most of a second
//! at a time and leaves our own window undraggable, so the waiting happens
//! on a worker thread and the render thread only uploads finished pixels.

use std::ffi::c_void;
use std::sync::mpsc;
use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, SelectObject,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW};
use windows::Win32::UI::WindowsAndMessaging::{
    GCLP_HICON, GCLP_HICONSM, GetClassLongPtrW, GetIconInfo, GetWindowRect, GetWindowThreadProcessId,
    HICON, ICON_BIG, ICON_SMALL, ICON_SMALL2, ICONINFO, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
    WM_GETICON,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32,
};
use windows::core::PWSTR;

pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    /// Tightly packed, top-down, straight-alpha RGBA8.
    pub rgba: Vec<u8>,
}

/// Longest edge of a picker thumbnail. Tiles are ~156x72 logical px, so
/// this leaves headroom for a HiDPI scale factor without uploading a
/// window-sized texture per tile.
pub const THUMB_MAX_DIM: u32 = 320;

/// One tile's worth of work. `PrintWindow` is retried every cycle — a live
/// still is the whole point — but the icon fallback is one-shot: once a
/// tile has an icon there's no reason to pay `WM_GETICON`'s timeouts and a
/// shell-icon lookup again, so the caller clears `need_icon` once it has one.
pub struct Request {
    /// Raw `HWND` as a `usize`, because `HWND` isn't `Send`. Only ever
    /// turned back into a handle on the worker.
    pub hwnd: usize,
    pub need_icon: bool,
}

pub enum Art {
    Thumb(CapturedImage),
    Icon(CapturedImage),
}

pub struct Ready {
    pub hwnd: usize,
    pub art: Art,
}

enum Msg {
    Ready(Ready),
    BatchDone,
}

/// Captures picker thumbnails off the render thread.
///
/// Requests are batched per refresh tick and results stream back as each
/// window finishes, so tiles fill in progressively instead of appearing all
/// at once after the whole batch. A batch is only ever sent when the
/// previous one has finished ([`Service::is_busy`]), which keeps a slow
/// desktop from queueing refreshes faster than it can serve them.
pub struct Service {
    req_tx: mpsc::Sender<Vec<Request>>,
    res_rx: mpsc::Receiver<Msg>,
    in_flight: bool,
}

impl Service {
    pub fn new() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<Vec<Request>>();
        let (res_tx, res_rx) = mpsc::channel::<Msg>();
        std::thread::Builder::new()
            .name("picker-thumbs".into())
            .spawn(move || worker(&req_rx, &res_tx))
            .expect("Failed to spawn thumbnail worker thread");
        Self {
            req_tx,
            res_rx,
            in_flight: false,
        }
    }

    /// True while the worker still owes results for the last batch.
    pub fn is_busy(&self) -> bool {
        self.in_flight
    }

    pub fn request(&mut self, batch: Vec<Request>) {
        if batch.is_empty() {
            return;
        }
        if self.req_tx.send(batch).is_ok() {
            self.in_flight = true;
        }
    }

    /// Non-blocking: hands back whatever finished since the last call.
    /// Safe to call every frame.
    pub fn poll(&mut self) -> Vec<Ready> {
        let mut out = Vec::new();
        while let Ok(msg) = self.res_rx.try_recv() {
            match msg {
                Msg::Ready(ready) => out.push(ready),
                Msg::BatchDone => self.in_flight = false,
            }
        }
        out
    }
}

fn worker(req_rx: &mpsc::Receiver<Vec<Request>>, res_tx: &mpsc::Sender<Msg>) {
    // `SHGetFileInfoW`'s icon lookup (the fallback in `window_icon`) needs a
    // COM-initialised thread; the render thread gets this from winit, this
    // one has to ask for it. A failure here only costs us shell icons.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    // Both channels disconnect when the picker is dropped, which is how this
    // thread exits.
    while let Ok(batch) = req_rx.recv() {
        for req in batch {
            let hwnd = req.hwnd as *mut c_void;
            let art = match capture_window(hwnd, THUMB_MAX_DIM) {
                Some(img) => Some(Art::Thumb(img)),
                None if req.need_icon => window_icon(hwnd).map(Art::Icon),
                None => None,
            };
            if let Some(art) = art
                && res_tx
                    .send(Msg::Ready(Ready {
                        hwnd: req.hwnd,
                        art,
                    }))
                    .is_err()
            {
                return;
            }
        }
        if res_tx.send(Msg::BatchDone).is_err() {
            return;
        }
    }
}

/// Grab a still of `hwnd`'s current contents via `PrintWindow`, downscaled
/// so a tile's texture upload stays cheap. Returns `None` for windows that
/// refuse to render off-screen (DRM'd players, some elevated apps) — the
/// caller falls back to [`window_icon`].
pub fn capture_window(hwnd: *mut c_void, max_dim: u32) -> Option<CapturedImage> {
    let hwnd = HWND(hwnd);
    unsafe {
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        let width = (rect.right - rect.left).max(0) as u32;
        let height = (rect.bottom - rect.top).max(0) as u32;
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return None;
        }

        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return None;
        }
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let bitmap = CreateCompatibleBitmap(screen_dc, width as i32, height as i32);
        ReleaseDC(None, screen_dc);
        if mem_dc.is_invalid() || bitmap.is_invalid() {
            let _ = DeleteDC(mem_dc);
            return None;
        }
        let old = SelectObject(mem_dc, bitmap.into());

        // 2 = PW_RENDERFULLCONTENT; this binding only exports PW_CLIENTONLY
        // (1) as a named `PRINT_WINDOW_FLAGS`, so the full-content flag is
        // constructed by value.
        let ok = PrintWindow(hwnd, mem_dc, PRINT_WINDOW_FLAGS(2)).as_bool();
        let image = if ok {
            read_dib(mem_dc, bitmap.into(), width, height, true)
        } else {
            None
        };

        SelectObject(mem_dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem_dc);

        image.map(|img| downscale(img, max_dim))
    }
}

/// The window's own icon (the same one the taskbar shows), falling back to
/// the owning executable's shell icon when the window doesn't answer
/// `WM_GETICON`/`GCLP_HICON` — common for console apps and some dialogs.
pub fn window_icon(hwnd: *mut c_void) -> Option<CapturedImage> {
    let hwnd = HWND(hwnd);
    unsafe {
        if let Some(icon) = window_declared_icon(hwnd) {
            let img = hicon_to_image(icon);
            // Not ours to destroy: WM_GETICON/GCLP_HICON return a handle
            // owned by the window/class, not a copy.
            return img;
        }

        if let Some(icon) = shell_icon_for_process(hwnd) {
            let img = hicon_to_image(icon);
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(icon);
            return img;
        }

        None
    }
}

unsafe fn window_declared_icon(hwnd: HWND) -> Option<HICON> {
    unsafe {
        for icon_kind in [ICON_BIG, ICON_SMALL2, ICON_SMALL] {
            let mut result = 0usize;
            let sent = SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                windows::Win32::Foundation::WPARAM(icon_kind as usize),
                windows::Win32::Foundation::LPARAM(0),
                SMTO_ABORTIFHUNG,
                100,
                Some(&mut result),
            );
            if sent.0 != 0 && result != 0 {
                return Some(HICON(result as *mut c_void));
            }
        }

        for class_field in [GCLP_HICON, GCLP_HICONSM] {
            let value = GetClassLongPtrW(hwnd, class_field);
            if value != 0 {
                return Some(HICON(value as *mut c_void));
            }
        }

        None
    }
}

unsafe fn shell_icon_for_process(hwnd: HWND) -> Option<HICON> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }

        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut path_buf = [0u16; 260];
        let mut len = path_buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path_buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        ok.ok()?;

        let mut shfi = SHFILEINFOW::default();
        let result = SHGetFileInfoW(
            windows::core::PCWSTR(path_buf.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut shfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if result == 0 || shfi.hIcon.is_invalid() {
            return None;
        }
        Some(shfi.hIcon)
    }
}

unsafe fn hicon_to_image(icon: HICON) -> Option<CapturedImage> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).ok()?;
        let color_bitmap = info.hbmColor;
        let mask_bitmap = info.hbmMask;

        if color_bitmap.is_invalid() {
            let _ = DeleteObject(mask_bitmap.into());
            return None;
        }

        let mut bmp = BITMAP::default();
        GetObjectW(
            color_bitmap.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut c_void),
        );
        let width = bmp.bmWidth.max(0) as u32;
        let height = bmp.bmHeight.max(0) as u32;

        let screen_dc = GetDC(None);
        let image = if width > 0 && height > 0 && !screen_dc.is_invalid() {
            let mem_dc = CreateCompatibleDC(Some(screen_dc));
            read_dib(mem_dc, color_bitmap.into(), width, height, false)
        } else {
            None
        };
        ReleaseDC(None, screen_dc);

        let _ = DeleteObject(color_bitmap.into());
        let _ = DeleteObject(mask_bitmap.into());

        image
    }
}

/// Pull a top-down 32bpp RGBA copy of `bitmap` (already selected into, or
/// selectable into, `dc`) via `GetDIBits`. `opaque` forces alpha to 255 —
/// `PrintWindow` output rarely carries a meaningful alpha channel, but real
/// icons (from `GetIconInfo`) do and should keep theirs.
unsafe fn read_dib(
    dc: windows::Win32::Graphics::Gdi::HDC,
    bitmap: windows::Win32::Graphics::Gdi::HGDIOBJ,
    width: u32,
    height: u32,
    opaque: bool,
) -> Option<CapturedImage> {
    unsafe {
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut buf = vec![0u8; (width * height * 4) as usize];
        let scanlines = GetDIBits(
            dc,
            windows::Win32::Graphics::Gdi::HBITMAP(bitmap.0),
            0,
            height,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        if scanlines == 0 {
            return None;
        }

        // BGRA -> RGBA in place.
        #[allow(clippy::chunks_exact_to_as_chunks)]
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            if opaque {
                px[3] = 255;
            }
        }

        Some(CapturedImage {
            width,
            height,
            rgba: buf,
        })
    }
}

fn downscale(img: CapturedImage, max_dim: u32) -> CapturedImage {
    if img.width <= max_dim && img.height <= max_dim {
        return img;
    }
    let scale = (max_dim as f32 / img.width as f32).min(max_dim as f32 / img.height as f32);
    let dst_w = ((img.width as f32 * scale).round() as u32).max(1);
    let dst_h = ((img.height as f32 * scale).round() as u32).max(1);

    let buf = image::RgbaImage::from_raw(img.width, img.height, img.rgba)
        .expect("buffer sized to width*height*4");
    let resized = image::imageops::thumbnail(&buf, dst_w, dst_h);

    CapturedImage {
        width: dst_w,
        height: dst_h,
        rgba: resized.into_raw(),
    }
}
