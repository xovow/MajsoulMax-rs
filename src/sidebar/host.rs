use crate::sidebar::{
    GuiEvent, InitialValues, app::SidebarApp, raster::SoftwareRenderer,
};
use anyhow::{Context, Result};
use egui::{
    CursorIcon, Event, FontData, FontDefinitions, FontFamily, FontId, FullOutput, ImeEvent,
    Modifiers, MouseWheelUnit, OutputCommand, PointerButton, Pos2, RawInput, Rect, TouchPhase,
    ViewportInfo, pos2, vec2,
};
use egui::viewport::{ViewportId, ViewportIdMap};
use std::{
    ffi::c_void,
    mem::size_of,
    ptr,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tao::event_loop::EventLoopProxy;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, DIB_RGB_COLORS, EndPaint,
        InvalidateRect, PAINTSTRUCT, SRCCOPY, StretchDIBits,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::{GetCapture, GetKeyState, ReleaseCapture, SetCapture, SetFocus},
        WindowsAndMessaging::{
            CREATESTRUCTW, CreateWindowExW, DefWindowProcW,
            DestroyWindow, GetClassInfoW, GetClientRect, GetCursorPos, HTCLIENT, HWND_TOP,
            IDC_ARROW, IDC_HAND, IDC_IBEAM, IDC_NO, IDC_SIZENS, IDC_SIZEWE, IsWindow, KillTimer,
            LoadCursorW, RegisterClassW, SWP_NOACTIVATE, SWP_NOCOPYBITS, SWP_NOREDRAW,
            SWP_SHOWWINDOW, SetCursor, SetTimer, SetWindowPos, WM_CANCELMODE, WM_CAPTURECHANGED,
            WM_CHAR, WM_ERASEBKGND, WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION,
            WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN,
            WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,
            WM_SETCURSOR, WM_SETFOCUS, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSW, WS_CHILD,
            WS_CLIPSIBLINGS, WS_VISIBLE,
        },
    },
};

const SIDEBAR_CLASS: &str = "MajsoulMaxEguiSidebar";
const WM_MOUSELEAVE: u32 = 0x02A3;
const REPAINT_TIMER: usize = 1;
const DRAG_TIMER: usize = 2;
const DRAG_FRAME_INTERVAL_MS: u32 = 16;
const GCS_COMPSTR: u32 = 0x0008;
const GCS_RESULTSTR: u32 = 0x0800;
const CFS_POINT: u32 = 0x0002;
const TME_LEAVE: u32 = 0x0002;
const GWLP_USER_DATA: i32 = -21;

type Himc = isize;

#[link(name = "imm32")]
unsafe extern "system" {
    fn ImmGetContext(hwnd: HWND) -> Himc;
    fn ImmReleaseContext(hwnd: HWND, himc: Himc) -> i32;
    fn ImmGetCompositionStringW(himc: Himc, index: u32, buf: *mut u16, buf_len: u32) -> i32;
    fn ImmSetCompositionWindow(himc: Himc, form: *const CompositionForm) -> i32;
}

#[repr(C)]
struct CompositionForm {
    dw_style: u32,
    pt_current_pos: POINT,
    rc_area: RECT,
}

#[repr(C)]
struct TrackMouseEvent {
    cb_size: u32,
    dw_flags: u32,
    hwnd_track: HWND,
    dw_hover_time: u32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: HWND, index: i32, value: isize) -> isize;
    fn TrackMouseEvent(event: *mut TrackMouseEvent) -> i32;
    fn OpenClipboard(hwnd: HWND) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn GetClipboardData(format: u32) -> isize;
    fn SetClipboardData(format: u32, mem: isize) -> isize;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> isize;
    fn GlobalLock(mem: isize) -> *mut u8;
    fn GlobalUnlock(mem: isize) -> i32;
    fn GlobalSize(mem: isize) -> usize;
}

const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

static CLASS_NAME: OnceLock<Vec<u16>> = OnceLock::new();

pub struct Host {
    pub hwnd: HWND,
    ctx: egui::Context,
    pub app: SidebarApp,
    renderer: SoftwareRenderer,
    proxy: EventLoopProxy<GuiEvent>,
    events: Vec<Event>,
    pub pixels_per_point: f32,
    focused: bool,
    modifiers: Modifiers,
    pointer_pos: Option<Pos2>,
    ime_composing: bool,
    tracking_leave: bool,
    dragging_splitter: bool,
    drag_x: i32,
    drag_timer_armed: bool,
    cursor: CursorIcon,
    started: Instant,
}

pub fn attach(
    parent: HWND,
    proxy: EventLoopProxy<GuiEvent>,
    initial: &InitialValues,
    scale_factor: f64,
) -> Result<HWND> {
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    if instance == 0 as _ {
        return Err(std::io::Error::last_os_error()).context("Failed to get module handle");
    }
    register_class(instance)?;

    let ctx = egui::Context::default();
    ctx.set_os(egui::os::OperatingSystem::Windows);
    install_cjk_fonts(&ctx);
    apply_theme(&ctx);

    let host = Box::new(Host {
        hwnd: 0 as HWND,
        ctx,
        app: SidebarApp::from_initial(initial),
        renderer: SoftwareRenderer::new(),
        proxy,
        events: Vec::with_capacity(16),
        pixels_per_point: scale_factor.max(0.5) as f32,
        focused: false,
        modifiers: Modifiers::default(),
        pointer_pos: None,
        ime_composing: false,
        tracking_leave: false,
        dragging_splitter: false,
        drag_x: 0,
        drag_timer_armed: false,
        cursor: CursorIcon::Default,
        started: Instant::now(),
    });
    let raw = Box::into_raw(host);
    let class = CLASS_NAME.get().expect("class name registered");
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            ptr::null(),
            // The sidebar is a single painted surface; there are no native
            // child controls to clip or tab through.
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            parent,
            0 as _,
            instance,
            raw as *const c_void,
        )
    };
    if hwnd == 0 as _ {
        unsafe { drop(Box::from_raw(raw)) };
        return Err(std::io::Error::last_os_error()).context("Failed to create egui sidebar");
    }
    Ok(hwnd)
}

pub fn with_host<T>(hwnd: HWND, f: impl FnOnce(&mut Host) -> T) -> Option<T> {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USER_DATA) } as *mut Host;
    if ptr.is_null() {
        None
    } else {
        Some(f(unsafe { &mut *ptr }))
    }
}

pub fn request_repaint(hwnd: HWND) {
    unsafe {
        InvalidateRect(hwnd, ptr::null(), 0);
    }
}

pub fn resize(hwnd: HWND, width: i32, height: i32) {
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            width.max(1),
            height.max(1),
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOREDRAW | SWP_NOCOPYBITS,
        );
    }
}

pub fn destroy(hwnd: HWND) {
    if unsafe { IsWindow(hwnd) } != 0 {
        unsafe { DestroyWindow(hwnd) };
    }
}

fn register_class(instance: windows_sys::Win32::Foundation::HINSTANCE) -> Result<()> {
    let class_name = CLASS_NAME.get_or_init(|| wide(SIDEBAR_CLASS));
    let mut existing = unsafe { std::mem::zeroed() };
    if unsafe { GetClassInfoW(instance, class_name.as_ptr(), &mut existing) } != 0 {
        return Ok(());
    }
    let class = WNDCLASSW {
        // Repaints are explicitly scheduled after resize/input events.  The
        // old native-control class flags are unnecessary for this one-surface
        // egui host and would also turn the second click into an unhandled
        // WM_LBUTTONDBLCLK message.
        style: 0,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: 0 as _,
        hCursor: unsafe { LoadCursorW(0 as _, IDC_ARROW) },
        hbrBackground: 0 as _,
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(1410) {
            return Err(error).context("Failed to register egui sidebar class");
        }
    }
    Ok(())
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    message: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(l_param as *const CREATESTRUCTW) };
        let host = create.lpCreateParams as *mut Host;
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USER_DATA, host as isize);
            (*host).hwnd = hwnd;
        }
        return unsafe { DefWindowProcW(hwnd, message, w_param, l_param) };
    }
    if message == WM_NCDESTROY {
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USER_DATA) } as *mut Host;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USER_DATA, 0) };
        if !ptr.is_null() {
            unsafe { drop(Box::from_raw(ptr)) };
        }
        return unsafe { DefWindowProcW(hwnd, message, w_param, l_param) };
    }

    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USER_DATA) } as *mut Host;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, w_param, l_param) };
    }
    let host = unsafe { &mut *ptr };
    match handle_message(hwnd, host, message, w_param, l_param) {
        Some(result) => result,
        None => unsafe { DefWindowProcW(hwnd, message, w_param, l_param) },
    }
}

fn handle_message(
    hwnd: HWND,
    host: &mut Host,
    message: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> Option<LRESULT> {
    match message {
        WM_ERASEBKGND => Some(1),
        WM_PAINT => {
            paint(hwnd, host);
            Some(0)
        }
        WM_LBUTTONDOWN => {
            let pos = pointer_pos(l_param, host.pixels_per_point);
            if in_splitter(hwnd, pos, host) {
                start_splitter_drag(hwnd, host);
            } else {
                unsafe { SetFocus(hwnd) };
                push_modifiers(host);
                host.pointer_pos = Some(pos);
                host.events.push(Event::PointerMoved(pos));
                host.events.push(Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: host.modifiers,
                });
                request_repaint(hwnd);
            }
            Some(0)
        }
        WM_LBUTTONUP => {
            if host.dragging_splitter {
                finish_splitter_drag(hwnd, host);
            } else {
                let pos = pointer_pos(l_param, host.pixels_per_point);
                host.pointer_pos = Some(pos);
                host.events.push(Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: host.modifiers,
                });
                request_repaint(hwnd);
            }
            Some(0)
        }
        WM_MOUSEMOVE => {
            track_mouse_leave(hwnd, host);
            if host.dragging_splitter && unsafe { GetCapture() } == hwnd {
                if let Some(x) = cursor_x() {
                    host.drag_x = x;
                }
                arm_drag_timer(hwnd, host);
            } else {
                let pos = pointer_pos(l_param, host.pixels_per_point);
                // WM_MOUSEMOVE can repeat the same logical point many times;
                // avoid queuing redundant egui frames for it.
                if host.pointer_pos != Some(pos) {
                    host.pointer_pos = Some(pos);
                    host.events.push(Event::PointerMoved(pos));
                    request_repaint(hwnd);
                }
            }
            Some(0)
        }
        WM_MOUSELEAVE => {
            host.tracking_leave = false;
            if !host.dragging_splitter {
                host.pointer_pos = None;
                host.events.push(Event::PointerGone);
                request_repaint(hwnd);
            }
            Some(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((w_param >> 16) as u16) as i16 as f32 / 120.0;
            push_modifiers(host);
            host.events.push(Event::MouseWheel {
                unit: MouseWheelUnit::Line,
                delta: vec2(0.0, delta),
                phase: TouchPhase::Move,
                modifiers: host.modifiers,
            });
            request_repaint(hwnd);
            Some(0)
        }
        WM_KEYDOWN => {
            handle_key(hwnd, host, w_param, l_param, true);
            Some(0)
        }
        WM_SYSKEYDOWN => {
            handle_key(hwnd, host, w_param, l_param, true);
            None
        }
        WM_KEYUP => {
            handle_key(hwnd, host, w_param, l_param, false);
            Some(0)
        }
        WM_SYSKEYUP => {
            handle_key(hwnd, host, w_param, l_param, false);
            None
        }
        WM_CHAR => {
            if !host.ime_composing
                && let Some(ch) = char::from_u32(w_param as u32)
                && !ch.is_control()
            {
                host.events.push(Event::Text(ch.to_string()));
                request_repaint(hwnd);
            }
            Some(0)
        }
        WM_IME_STARTCOMPOSITION => {
            host.ime_composing = true;
            request_repaint(hwnd);
            None
        }
        WM_IME_ENDCOMPOSITION => {
            host.ime_composing = false;
            request_repaint(hwnd);
            None
        }
        WM_IME_COMPOSITION => {
            handle_ime_composition(hwnd, host, l_param);
            None
        }
        WM_SETFOCUS => {
            host.focused = true;
            host.events.push(Event::WindowFocused(true));
            request_repaint(hwnd);
            Some(0)
        }
        WM_KILLFOCUS => {
            host.focused = false;
            host.events.push(Event::WindowFocused(false));
            request_repaint(hwnd);
            Some(0)
        }
        WM_SETCURSOR => {
            if (l_param as u32 & 0xffff) == HTCLIENT as u32 {
                let cursor = if host.dragging_splitter || hovering_splitter(hwnd, host) {
                    IDC_SIZEWE
                } else {
                    cursor_id(host.cursor)
                };
                unsafe { SetCursor(LoadCursorW(0 as _, cursor)) };
                return Some(1);
            }
            None
        }
        WM_TIMER if w_param == REPAINT_TIMER => {
            unsafe { KillTimer(hwnd, REPAINT_TIMER) };
            request_repaint(hwnd);
            Some(0)
        }
        WM_TIMER if w_param == DRAG_TIMER => {
            unsafe { KillTimer(hwnd, DRAG_TIMER) };
            host.drag_timer_armed = false;
            if host.dragging_splitter && unsafe { GetCapture() } == hwnd {
                if let Some(x) = cursor_x() {
                    host.drag_x = x;
                }
                let _ = host.proxy.send_event(GuiEvent::SidebarDrag(host.drag_x));
            }
            Some(0)
        }
        WM_CAPTURECHANGED | WM_CANCELMODE => {
            if host.dragging_splitter {
                finish_splitter_drag(hwnd, host);
            }
            Some(0)
        }
        _ => None,
    }
}

fn paint(hwnd: HWND, host: &mut Host) {
    let mut paint = unsafe { std::mem::zeroed::<PAINTSTRUCT>() };
    let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
    if hdc == 0 as _ {
        return;
    }
    let (width, height) = client_size(hwnd);
    if width > 0 && height > 0 {
        host.renderer.resize(width as usize, height as usize);
        let ppp = host.pixels_per_point.max(0.5);
        host.ctx.set_pixels_per_point(ppp);
        let logical = vec2(width as f32 / ppp, height as f32 / ppp);
        let screen = Rect::from_min_size(Pos2::ZERO, logical);
        let mut viewports = ViewportIdMap::default();
        viewports.insert(
            ViewportId::ROOT,
            ViewportInfo {
                native_pixels_per_point: Some(ppp),
                inner_rect: Some(screen),
                focused: Some(host.focused),
                ..Default::default()
            },
        );
        let input = RawInput {
            viewport_id: ViewportId::ROOT,
            viewports,
            screen_rect: Some(screen),
            max_texture_side: Some(2048),
            time: Some(host.started.elapsed().as_secs_f64()),
            predicted_dt: 1.0 / 60.0,
            events: {
                let mut events = Vec::with_capacity(host.events.len());
                events.append(&mut host.events);
                events
            },
            focused: host.focused,
            ..Default::default()
        };
        // Borrow the context and app fields independently for the frame.  A
        // cloned `egui::Context` is cheap, but it is still an unnecessary
        // atomic reference-count operation on every paint.
        let output = {
            let ctx = &host.ctx;
            let app = &mut host.app;
            let proxy = &host.proxy;
            ctx.run_ui(input, |ui| app.ui(ui, proxy))
        };
        handle_platform_output(hwnd, host, &output, ppp);
        let delay = repaint_delay(&output);
        let pixels_per_point = output.pixels_per_point;
        host.renderer.apply_textures(output.textures_delta);
        let primitives = host.ctx.tessellate(output.shapes, pixels_per_point);
        host.renderer.render(&primitives, pixels_per_point);
        blit(hdc, width, height, host.renderer.bgra());
        schedule_repaint(hwnd, delay);
    }
    unsafe { EndPaint(hwnd, &paint) };
}

fn handle_platform_output(hwnd: HWND, host: &mut Host, output: &FullOutput, ppp: f32) {
    host.cursor = output.platform_output.cursor_icon;
    for command in &output.platform_output.commands {
        match command {
            OutputCommand::CopyText(text) => set_clipboard(text),
            OutputCommand::CopyImage(_) | OutputCommand::OpenUrl(_) => {}
        }
    }
    if let Some(ime) = output.platform_output.ime.as_ref() {
        set_ime_position(hwnd, ime.cursor_rect, ppp);
    }
}

fn repaint_delay(output: &FullOutput) -> Duration {
    output
        .viewport_output
        .get(&ViewportId::ROOT)
        .map(|viewport| viewport.repaint_delay)
        .unwrap_or(Duration::MAX)
}

fn schedule_repaint(hwnd: HWND, delay: Duration) {
    unsafe {
        if delay.is_zero() {
            SetTimer(hwnd, REPAINT_TIMER, 16, None);
        } else if delay == Duration::MAX {
            KillTimer(hwnd, REPAINT_TIMER);
        } else {
            let ms = delay.as_millis().clamp(1, 60_000) as u32;
            SetTimer(hwnd, REPAINT_TIMER, ms, None);
        }
    }
}

fn blit(hdc: windows_sys::Win32::Graphics::Gdi::HDC, width: i32, height: i32, pixels: &[u32]) {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    unsafe {
        StretchDIBits(
            hdc,
            0,
            0,
            width,
            height,
            0,
            0,
            width,
            height,
            pixels.as_ptr().cast(),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
}

fn handle_key(hwnd: HWND, host: &mut Host, w_param: WPARAM, l_param: LPARAM, pressed: bool) {
    push_modifiers(host);
    let vk = w_param as i32;
    let Some(key) = map_key(vk) else {
        return;
    };
    let repeat = pressed && (l_param & 0x4000_0000) != 0;
    if pressed && host.modifiers.command {
        match key {
            egui::Key::C => host.events.push(Event::Copy),
            egui::Key::X => host.events.push(Event::Cut),
            egui::Key::V => {
                if let Some(text) = clipboard_text() {
                    host.events.push(Event::Paste(text));
                }
            }
            _ => {}
        }
    }
    host.events.push(Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat,
        modifiers: host.modifiers,
    });
    request_repaint(hwnd);
}

fn handle_ime_composition(hwnd: HWND, host: &mut Host, l_param: LPARAM) {
    let himc = unsafe { ImmGetContext(hwnd) };
    if himc == 0 {
        return;
    }
    if l_param as u32 & GCS_RESULTSTR != 0
        && let Some(text) = ime_string(himc, GCS_RESULTSTR)
        && !text.is_empty()
    {
        host.ime_composing = false;
        host.events.push(Event::Ime(ImeEvent::Commit(text)));
        request_repaint(hwnd);
    } else if l_param as u32 & GCS_COMPSTR != 0 {
        let text = ime_string(himc, GCS_COMPSTR).unwrap_or_default();
        host.ime_composing = !text.is_empty();
        host.events.push(Event::Ime(ImeEvent::Preedit {
            text,
            active_range_chars: None,
        }));
        request_repaint(hwnd);
    }
    unsafe { ImmReleaseContext(hwnd, himc) };
}

fn set_ime_position(hwnd: HWND, cursor: Rect, ppp: f32) {
    let himc = unsafe { ImmGetContext(hwnd) };
    if himc == 0 {
        return;
    }
    let form = CompositionForm {
        dw_style: CFS_POINT,
        pt_current_pos: POINT {
            x: (cursor.min.x * ppp).round() as i32,
            y: (cursor.max.y * ppp).round() as i32,
        },
        rc_area: RECT {
            left: (cursor.min.x * ppp).round() as i32,
            top: (cursor.min.y * ppp).round() as i32,
            right: (cursor.max.x * ppp).round() as i32,
            bottom: (cursor.max.y * ppp).round() as i32,
        },
    };
    unsafe {
        ImmSetCompositionWindow(himc, &form);
        ImmReleaseContext(hwnd, himc);
    }
}

fn ime_string(himc: Himc, index: u32) -> Option<String> {
    let bytes = unsafe { ImmGetCompositionStringW(himc, index, ptr::null_mut(), 0) };
    if bytes <= 0 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let written = unsafe { ImmGetCompositionStringW(himc, index, buf.as_mut_ptr(), bytes as u32) };
    if written <= 0 {
        return None;
    }
    buf.truncate((written as usize) / 2);
    Some(String::from_utf16_lossy(&buf))
}

fn start_splitter_drag(hwnd: HWND, host: &mut Host) {
    unsafe { SetCapture(hwnd) };
    if unsafe { GetCapture() } != hwnd {
        return;
    }
    let x = cursor_x().unwrap_or(0);
    host.dragging_splitter = true;
    host.drag_x = x;
    let _ = host.proxy.send_event(GuiEvent::SidebarDragStart(x));
}

fn finish_splitter_drag(hwnd: HWND, host: &mut Host) {
    stop_drag_timer(hwnd, host);
    if !host.dragging_splitter {
        return;
    }
    host.dragging_splitter = false;
    if unsafe { GetCapture() } == hwnd {
        unsafe { ReleaseCapture() };
    }
    if let Some(x) = cursor_x() {
        host.drag_x = x;
    }
    let _ = host.proxy.send_event(GuiEvent::SidebarDrag(host.drag_x));
    let _ = host.proxy.send_event(GuiEvent::SidebarDragEnd);
}

fn arm_drag_timer(hwnd: HWND, host: &mut Host) {
    if host.drag_timer_armed {
        return;
    }
    host.drag_timer_armed = true;
    if unsafe { SetTimer(hwnd, DRAG_TIMER, DRAG_FRAME_INTERVAL_MS, None) } == 0 {
        host.drag_timer_armed = false;
        let _ = host.proxy.send_event(GuiEvent::SidebarDrag(host.drag_x));
    }
}

fn stop_drag_timer(hwnd: HWND, host: &mut Host) {
    unsafe { KillTimer(hwnd, DRAG_TIMER) };
    host.drag_timer_armed = false;
}

fn in_splitter(hwnd: HWND, pos: Pos2, host: &Host) -> bool {
    if host.app.collapsed {
        return false;
    }
    let (width, _) = client_size(hwnd);
    let logical_width = width as f32 / host.pixels_per_point.max(0.5);
    pos.x >= logical_width - 6.0
}

fn hovering_splitter(hwnd: HWND, host: &Host) -> bool {
    host.pointer_pos.is_some_and(|pos| in_splitter(hwnd, pos, host))
}

fn track_mouse_leave(hwnd: HWND, host: &mut Host) {
    if host.tracking_leave {
        return;
    }
    let mut event = TrackMouseEvent {
        cb_size: size_of::<TrackMouseEvent>() as u32,
        dw_flags: TME_LEAVE,
        hwnd_track: hwnd,
        dw_hover_time: 0,
    };
    if unsafe { TrackMouseEvent(&mut event) } != 0 {
        host.tracking_leave = true;
    }
}

fn pointer_pos(l_param: LPARAM, ppp: f32) -> Pos2 {
    let x = (l_param as u32 & 0xffff) as i16 as f32;
    let y = ((l_param as u32 >> 16) & 0xffff) as i16 as f32;
    pos2(x / ppp.max(0.5), y / ppp.max(0.5))
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetClientRect(hwnd, &mut rect) };
    ((rect.right - rect.left).max(0), (rect.bottom - rect.top).max(0))
}

fn cursor_x() -> Option<i32> {
    let mut point = POINT { x: 0, y: 0 };
    (unsafe { GetCursorPos(&mut point) } != 0).then_some(point.x)
}

fn push_modifiers(host: &mut Host) {
    let modifiers = current_modifiers();
    if modifiers != host.modifiers {
        host.modifiers = modifiers;
        host.events.push(Event::ModifiersChanged(modifiers));
    } else {
        host.modifiers = modifiers;
    }
}

fn current_modifiers() -> Modifiers {
    let ctrl = key_down(0x11);
    let shift = key_down(0x10);
    let alt = key_down(0x12);
    Modifiers {
        alt,
        ctrl,
        shift,
        mac_cmd: false,
        command: ctrl,
    }
}

fn key_down(vk: i32) -> bool {
    unsafe { GetKeyState(vk) as u16 & 0x8000 != 0 }
}

fn map_key(vk: i32) -> Option<egui::Key> {
    use egui::Key;
    Some(match vk {
        0x08 => Key::Backspace,
        0x09 => Key::Tab,
        0x0D => Key::Enter,
        0x1B => Key::Escape,
        0x20 => Key::Space,
        0x25 => Key::ArrowLeft,
        0x26 => Key::ArrowUp,
        0x27 => Key::ArrowRight,
        0x28 => Key::ArrowDown,
        0x2D => Key::Insert,
        0x2E => Key::Delete,
        0x24 => Key::Home,
        0x23 => Key::End,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0x30 => Key::Num0,
        0x31 => Key::Num1,
        0x32 => Key::Num2,
        0x33 => Key::Num3,
        0x34 => Key::Num4,
        0x35 => Key::Num5,
        0x36 => Key::Num6,
        0x37 => Key::Num7,
        0x38 => Key::Num8,
        0x39 => Key::Num9,
        0x41 => Key::A,
        0x42 => Key::B,
        0x43 => Key::C,
        0x44 => Key::D,
        0x45 => Key::E,
        0x46 => Key::F,
        0x47 => Key::G,
        0x48 => Key::H,
        0x49 => Key::I,
        0x4A => Key::J,
        0x4B => Key::K,
        0x4C => Key::L,
        0x4D => Key::M,
        0x4E => Key::N,
        0x4F => Key::O,
        0x50 => Key::P,
        0x51 => Key::Q,
        0x52 => Key::R,
        0x53 => Key::S,
        0x54 => Key::T,
        0x55 => Key::U,
        0x56 => Key::V,
        0x57 => Key::W,
        0x58 => Key::X,
        0x59 => Key::Y,
        0x5A => Key::Z,
        _ => return None,
    })
}

fn cursor_id(icon: CursorIcon) -> *const u16 {
    match icon {
        CursorIcon::Text | CursorIcon::VerticalText => IDC_IBEAM,
        CursorIcon::ResizeHorizontal
        | CursorIcon::ResizeEast
        | CursorIcon::ResizeWest
        | CursorIcon::ResizeColumn => IDC_SIZEWE,
        CursorIcon::ResizeVertical
        | CursorIcon::ResizeNorth
        | CursorIcon::ResizeSouth
        | CursorIcon::ResizeRow => IDC_SIZENS,
        CursorIcon::PointingHand => IDC_HAND,
        CursorIcon::NotAllowed | CursorIcon::NoDrop => IDC_NO,
        _ => IDC_ARROW,
    }
}

fn clipboard_text() -> Option<String> {
    if unsafe { OpenClipboard(0 as HWND) } == 0 {
        return None;
    }
    let mem = unsafe { GetClipboardData(CF_UNICODETEXT) };
    let text = (mem != 0).then(|| unsafe {
        let ptr = GlobalLock(mem) as *const u16;
        if ptr.is_null() {
            None
        } else {
            let bytes = GlobalSize(mem);
            let len = bytes / 2;
            let slice = std::slice::from_raw_parts(ptr, len);
            let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
            let owned = String::from_utf16_lossy(&slice[..end]);
            GlobalUnlock(mem);
            Some(owned)
        }
    });
    unsafe { CloseClipboard() };
    text.flatten()
}

fn set_clipboard(text: &str) {
    let mut encoded: Vec<u16> = text.encode_utf16().collect();
    encoded.push(0);
    let bytes = encoded.len() * 2;
    if unsafe { OpenClipboard(0 as HWND) } == 0 {
        return;
    }
    unsafe { EmptyClipboard() };
    let mem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
    if mem != 0 {
        let ptr = unsafe { GlobalLock(mem) };
        if !ptr.is_null() {
            unsafe {
                ptr::copy_nonoverlapping(encoded.as_ptr().cast::<u8>(), ptr, bytes);
                GlobalUnlock(mem);
                SetClipboardData(CF_UNICODETEXT, mem);
            }
        }
    }
    unsafe { CloseClipboard() };
}

fn install_cjk_fonts(ctx: &egui::Context) {
    let Some(bytes) = load_cjk_font() else {
        return;
    };
    let mut fonts = FontDefinitions::empty();
    fonts
        .font_data
        .insert("cjk".to_owned(), Arc::new(FontData::from_owned(bytes)));
    fonts
        .families
        .insert(FontFamily::Proportional, vec!["cjk".to_owned()]);
    fonts
        .families
        .insert(FontFamily::Monospace, vec!["cjk".to_owned()]);
    ctx.set_fonts(fonts);
}

fn load_cjk_font() -> Option<Vec<u8>> {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyhl.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
    ];
    CANDIDATES.iter().find_map(|path| std::fs::read(path).ok())
}

fn apply_theme(ctx: &egui::Context) {
    ctx.set_visuals(egui::style::Visuals::light());
    ctx.all_styles_mut(|style| {
        style.visuals.panel_fill = egui::Color32::WHITE;
        style.visuals.window_fill = egui::Color32::WHITE;
        // The sidebar lays out its own horizontal padding.  Keeping egui's
        // implicit horizontal gap at zero prevents a second gap from being
        // inserted between a row label, switch, and edge padding.
        style.spacing.item_spacing = vec2(0.0, 10.0);
        style.spacing.button_padding = vec2(12.0, 8.0);
        style.text_styles.insert(egui::style::TextStyle::Body, FontId::proportional(14.0));
        style.text_styles.insert(egui::style::TextStyle::Button, FontId::proportional(14.0));
        style.text_styles.insert(egui::style::TextStyle::Heading, FontId::proportional(19.0));
        style.text_styles.insert(egui::style::TextStyle::Small, FontId::proportional(12.5));
        style.text_styles.insert(
            egui::style::TextStyle::Monospace,
            FontId::proportional(13.0),
        );
    });
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
