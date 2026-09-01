use anyhow::{Context, Result, bail};
use majsoul_max_rs::{LiqiUpdatePhase, LiqiUpdateStatus, Settings};
use std::{
    ffi::c_void,
    mem::size_of,
    ptr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};
use tao::event_loop::EventLoopProxy;
use windows_sys::Win32::{
    Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{
        BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreateSolidBrush,
        DEFAULT_CHARSET, DEFAULT_PITCH, DeleteObject, Ellipse, EndPaint, FF_DONTCARE, FillRect,
        GetStockObject, HDC, HFONT, HRGN, NULL_PEN, OPAQUE, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
        RoundRect, SelectObject, SetBkColor, SetBkMode, SetTextColor, TRANSPARENT, WHITE_BRUSH,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
};

type SubclassProc =
    Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM, usize, usize) -> LRESULT>;

#[link(name = "comctl32")]
unsafe extern "system" {
    #[link_name = "SetWindowSubclass"]
    fn set_window_subclass(
        hwnd: HWND,
        callback: SubclassProc,
        subclass_id: usize,
        reference_data: usize,
    ) -> i32;
    #[link_name = "RemoveWindowSubclass"]
    fn remove_window_subclass(hwnd: HWND, callback: SubclassProc, subclass_id: usize) -> i32;
    #[link_name = "DefSubclassProc"]
    fn def_subclass_proc(hwnd: HWND, message: u32, w_param: WPARAM, l_param: LPARAM) -> LRESULT;
}

#[link(name = "user32")]
unsafe extern "system" {
    #[link_name = "EnableWindow"]
    fn enable_window(hwnd: HWND, enable: i32) -> i32;
    #[link_name = "SetScrollInfo"]
    fn set_scroll_info(hwnd: HWND, bar: i32, info: *const SCROLLINFO, redraw: i32) -> i32;
    #[link_name = "SetCapture"]
    fn set_capture(hwnd: HWND) -> HWND;
    #[link_name = "GetCapture"]
    fn get_capture() -> HWND;
    #[link_name = "ReleaseCapture"]
    fn release_capture() -> i32;
    #[link_name = "InvalidateRect"]
    fn invalidate_rect(hwnd: HWND, rect: *const RECT, erase: i32) -> i32;
    #[link_name = "RedrawWindow"]
    fn redraw_window(hwnd: HWND, rect: *const RECT, region: HRGN, flags: u32) -> i32;
    #[link_name = "SetWindowPos"]
    fn set_window_pos(
        hwnd: HWND,
        insert_after: HWND,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> i32;
    #[link_name = "DrawTextW"]
    fn draw_text_w(hdc: HDC, text: *const u16, length: i32, rect: *mut RECT, format: u32) -> i32;
    #[link_name = "GetDC"]
    fn get_dc(hwnd: HWND) -> HDC;
    #[link_name = "ReleaseDC"]
    fn release_dc(hwnd: HWND, hdc: HDC) -> i32;
}

#[repr(C)]
struct GdiplusStartupInput {
    version: u32,
    debug_callback: *const c_void,
    suppress_background_thread: i32,
    suppress_external_codecs: i32,
}

#[link(name = "gdiplus")]
unsafe extern "system" {
    #[link_name = "GdiplusStartup"]
    fn gdiplus_startup(
        token: *mut usize,
        input: *const GdiplusStartupInput,
        output: *mut c_void,
    ) -> i32;
    #[link_name = "GdipCreateFromHDC"]
    fn gdip_create_from_hdc(hdc: HDC, graphics: *mut *mut c_void) -> i32;
    #[link_name = "GdipDeleteGraphics"]
    fn gdip_delete_graphics(graphics: *mut c_void) -> i32;
    #[link_name = "GdipSetSmoothingMode"]
    fn gdip_set_smoothing_mode(graphics: *mut c_void, mode: i32) -> i32;
    #[link_name = "GdipCreateSolidFill"]
    fn gdip_create_solid_fill(color: u32, brush: *mut *mut c_void) -> i32;
    #[link_name = "GdipDeleteBrush"]
    fn gdip_delete_brush(brush: *mut c_void) -> i32;
    #[link_name = "GdipFillRectangleI"]
    fn gdip_fill_rectangle(
        graphics: *mut c_void,
        brush: *mut c_void,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> i32;
    #[link_name = "GdipFillEllipseI"]
    fn gdip_fill_ellipse(
        graphics: *mut c_void,
        brush: *mut c_void,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> i32;
}

const EDIT_SET_LIMIT_TEXT: u32 = 0x00c5;
const STATIC_LEFT: u32 = 0x0000;
const STATIC_NOTIFY: u32 = 0x0100;
const SET_WINDOW_POS_NO_ACTIVATE: u32 = 0x0010;
const SET_WINDOW_POS_SHOW: u32 = 0x0040;
const SET_WINDOW_POS_NO_REDRAW: u32 = 0x0008;
const SET_WINDOW_POS_NO_MOVE: u32 = 0x0002;
const SET_WINDOW_POS_NO_SIZE: u32 = 0x0001;
const SET_WINDOW_POS_NO_COPY_BITS: u32 = 0x0100;
const REDRAW_INVALIDATE: u32 = 0x0001;
const REDRAW_ERASE: u32 = 0x0004;
const REDRAW_ALL_CHILDREN: u32 = 0x0080;
const REDRAW_FRAME: u32 = 0x0400;
const DRAW_ITEM_SELECTED: u32 = 0x0001;
const DRAW_ITEM_DISABLED: u32 = 0x0004;
const DRAW_TEXT_CENTER: u32 = 0x0001;
const DRAW_TEXT_WORDBREAK: u32 = 0x0010;
const DRAW_TEXT_NOPREFIX: u32 = 0x0800;
const DRAW_TEXT_CALCRECT: u32 = 0x0400;
const GDIPLUS_OK: i32 = 0;
const SMOOTHING_MODE_ANTI_ALIAS_8X8: i32 = 6;
static GDIPLUS_TOKEN: OnceLock<usize> = OnceLock::new();

#[repr(C)]
struct NativeDrawItem {
    control_type: u32,
    control_id: u32,
    item_id: u32,
    item_action: u32,
    item_state: u32,
    hwnd: HWND,
    hdc: HDC,
    rect: RECT,
    item_data: usize,
}

const ID_MOD_SWITCH: i32 = 1001;
const ID_AUTO_UPDATE: i32 = 1002;
const ID_NICKNAME: i32 = 1003;
const ID_SHOW_SERVER: i32 = 1004;
const ID_CHECK_UPDATE: i32 = 1005;
const ID_RESTART: i32 = 1006;
const ID_COLLAPSE: i32 = 1007;
const ID_ANTI_NICKNAME_CENSORSHIP: i32 = 1008;
const ID_EMOJI_SWITCH: i32 = 1009;
const ID_REQ_PROXY: i32 = 1010;
const ID_GITHUB_PREFIX: i32 = 1011;
const ID_SCROLLBAR: i32 = 1012;
const ID_HINT_SWITCH: i32 = 1013;
const ID_PROXY_STATUS: i32 = 1014;
const ID_CURRENT_VERSION: i32 = 1015;
const ID_LATEST_VERSION: i32 = 1016;
const ID_MESSAGE: i32 = 1017;
const TEXT_TIMER_ID: usize = 2001;
// Resize the WebView2 surface at most once per display frame.  A raw
// WM_MOUSEMOVE stream can be several times faster than the WebView2
// compositor and leaves a queue of stale bounds updates behind.
const DRAG_TIMER_ID: usize = 2002;
const DRAG_FRAME_INTERVAL_MS: u32 = 16;

const SIDEBAR_SUBCLASS_ID: usize = 1;
const SPLITTER_SUBCLASS_ID: usize = 2;
const CHILD_WHEEL_SUBCLASS_ID: usize = 3;
const SIDEBAR_CLASS: &str = "MajsoulMaxSidebar";
static SIDEBAR_CLASS_NAME: OnceLock<Vec<u16>> = OnceLock::new();

const TOGGLE_IDS: [i32; 6] = [
    ID_MOD_SWITCH,
    ID_AUTO_UPDATE,
    ID_SHOW_SERVER,
    ID_ANTI_NICKNAME_CENSORSHIP,
    ID_EMOJI_SWITCH,
    ID_HINT_SWITCH,
];
const TOGGLE_CHANGES: [fn(bool) -> SettingChange; 6] = [
    SettingChange::ModSwitch,
    SettingChange::AutoUpdate,
    SettingChange::ShowServer,
    SettingChange::AntiNicknameCensorship,
    SettingChange::EmojiSwitch,
    SettingChange::HintSwitch,
];
const TEXT_IDS: [i32; 3] = [ID_NICKNAME, ID_REQ_PROXY, ID_GITHUB_PREFIX];
const TEXT_CHANGES: [fn(String) -> SettingChange; 3] = [
    SettingChange::Nickname,
    SettingChange::ReqProxy,
    SettingChange::GithubPrefix,
];
const RELOAD_LOCK_IDS: [i32; 11] = [
    ID_MOD_SWITCH,
    ID_AUTO_UPDATE,
    ID_REQ_PROXY,
    ID_GITHUB_PREFIX,
    ID_NICKNAME,
    ID_SHOW_SERVER,
    ID_ANTI_NICKNAME_CENSORSHIP,
    ID_EMOJI_SWITCH,
    ID_HINT_SWITCH,
    ID_CHECK_UPDATE,
    ID_RESTART,
];

#[derive(Debug, Clone)]
pub enum SettingChange {
    ModSwitch(bool),
    AutoUpdate(bool),
    Nickname(String),
    ShowServer(bool),
    AntiNicknameCensorship(bool),
    EmojiSwitch(bool),
    HintSwitch(bool),
    ReqProxy(String),
    GithubPrefix(String),
}

#[derive(Debug)]
pub enum GuiEvent {
    SettingChanged(SettingChange),
    CheckUpdate,
    UpdateProgress(LiqiUpdatePhase),
    LatestVersion(LiqiUpdateStatus),
    StartupUpdateFinished(LiqiUpdateStatus),
    ProxyReloaded(std::result::Result<ReloadedSettings, String>),
    Restart,
    ToggleSidebar,
    SidebarDragStart(i32),
    SidebarDrag(i32),
    SidebarDragEnd,
    ScrollTo(i32),
}

pub struct InitialValues {
    pub mod_switch: bool,
    pub auto_update: bool,
    pub nickname: String,
    pub show_server: bool,
    pub anti_nickname_censorship: bool,
    pub emoji_switch: bool,
    pub hint_switch: bool,
    pub req_proxy: String,
    pub github_prefix: String,
    pub liqi_version: String,
}

#[derive(Debug)]
pub struct ReloadedSettings {
    pub settings: Arc<Settings>,
}

#[derive(Clone, Copy)]
enum WidthKind {
    Title,
    Heading,
    Content,
}

enum Row {
    Band {
        hwnd: HWND,
        x: i32,
        width: WidthKind,
        min_height: i32,
        gap: i32,
    },
    Toggle {
        label: HWND,
        switch: HWND,
        gap: i32,
    },
    Field {
        label: HWND,
        field: HWND,
        extra_gap: i32,
    },
    Button {
        hwnd: HWND,
        gap: i32,
    },
    Message(HWND),
}

struct CallbackContext {
    proxy: EventLoopProxy<GuiEvent>,
    scrollbar: HWND,
    toggles: [AtomicBool; 6],
    text_dirty: [AtomicBool; 3],
    // `drag_active`/`drag_x` are updated by the splitter window procedure,
    // while `dragging` is owned by the event-loop side and controls painting.
    drag_active: AtomicBool,
    drag_x: AtomicI32,
    drag_timer_armed: AtomicBool,
    dragging: AtomicBool,
    collapsed: AtomicBool,
    scroll_pos: AtomicI32,
    max_scroll: AtomicI32,
    page_size: AtomicI32,
    scale_x1000: AtomicI32,
}

impl CallbackContext {
    fn is_checked(&self, id: i32) -> bool {
        toggle_index(id).is_some_and(|index| self.toggles[index].load(Ordering::Relaxed))
    }

    fn set_checked(&self, id: i32, checked: bool) {
        if let Some(index) = toggle_index(id) {
            self.toggles[index].store(checked, Ordering::Relaxed);
        }
    }

    fn toggle(&self, id: i32) -> bool {
        toggle_index(id)
            .map(|index| !self.toggles[index].fetch_xor(true, Ordering::Relaxed))
            .unwrap_or(false)
    }
}

pub struct NativeSidebar {
    hwnd: HWND,
    splitter: HWND,
    scrollbar: HWND,
    collapse: HWND,
    rows: Vec<Row>,
    callback_context: Arc<CallbackContext>,
    last_width: AtomicI32,
    last_height: AtomicI32,
    _fonts: [HFONT; 3],
}

pub fn show_error_dialog(message: &str) {
    let title = wide("MajsoulMax 启动失败");
    let message = wide(message);
    unsafe {
        MessageBoxW(
            0 as _,
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

impl NativeSidebar {
    pub fn new(
        parent: HWND,
        proxy: EventLoopProxy<GuiEvent>,
        initial: &InitialValues,
        scale_factor: f64,
    ) -> Result<Self> {
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        if instance == 0 as _ {
            return Err(std::io::Error::last_os_error()).context("Failed to get module handle");
        }
        register_sidebar_class(instance)?;

        let hwnd = create_control(
            WS_EX_CONTROLPARENT,
            SIDEBAR_CLASS,
            "",
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            0,
            parent,
            instance,
        )?;
        let normal_font = create_font(14.0, 400, scale_factor)?;
        let heading_font = create_font(15.0, 600, scale_factor)?;
        let title_font = create_font(19.0, 600, scale_factor)?;

        let mut content = ContentBuilder {
            parent: hwnd,
            instance,
            normal_font,
            heading_font,
            rows: Vec::new(),
        };
        content.add_band("MajsoulMax", 0, title_font, 16, WidthKind::Title, 24, 6)?;
        content.add_band(
            "● 正在启动…",
            ID_PROXY_STATUS,
            normal_font,
            16,
            WidthKind::Heading,
            22,
            16,
        )?;
        let collapse = create_owner_button(hwnd, instance, "‹", ID_COLLAPSE)?;
        set_font(collapse, normal_font);
        content.add_heading("常规设置")?;
        content.add_toggle(ID_MOD_SWITCH, "启用 Mod", 12)?;
        content.add_toggle(ID_AUTO_UPDATE, "自动更新协议数据", 16)?;
        content.add_field(ID_REQ_PROXY, "Github代理", &initial.req_proxy, 256, 0)?;
        content.add_field(
            ID_GITHUB_PREFIX,
            "GitHub前缀",
            &initial.github_prefix,
            256,
            8,
        )?;
        content.add_heading("Mod 设置")?;
        content.add_field(ID_NICKNAME, "本地昵称", &initial.nickname, 64, 0)?;
        content.add_toggle(ID_SHOW_SERVER, "显示服务器", 12)?;
        content.add_toggle(ID_ANTI_NICKNAME_CENSORSHIP, "反昵称审查", 12)?;
        content.add_toggle(ID_EMOJI_SWITCH, "额外表情", 12)?;
        content.add_toggle(ID_HINT_SWITCH, "王座便捷提示", 20)?;
        content.add_heading("协议数据版本")?;
        content.add_band(
            &format!("当前：{}", initial.liqi_version),
            ID_CURRENT_VERSION,
            normal_font,
            22,
            WidthKind::Content,
            20,
            6,
        )?;
        content.add_band(
            "最新：尚未检查",
            ID_LATEST_VERSION,
            normal_font,
            22,
            WidthKind::Content,
            20,
            10,
        )?;
        content.add_button(ID_CHECK_UPDATE, "检查更新", 16)?;
        content.add_band(
            "GitHub 代理、前缀和已开启的 Mod 项修改后立即生效。mod 开关与自动更新需重新加载。",
            0,
            normal_font,
            22,
            WidthKind::Content,
            22,
            10,
        )?;
        content.add_button(ID_RESTART, "重新加载配置并刷新网页", 12)?;
        content.add_message()?;

        let splitter = create_control(
            0,
            "STATIC",
            "",
            WS_CHILD | WS_VISIBLE | STATIC_NOTIFY,
            0,
            hwnd,
            instance,
        )?;
        let scrollbar = create_control(
            0,
            "SCROLLBAR",
            "",
            WS_CHILD | SBS_VERT as u32,
            ID_SCROLLBAR,
            hwnd,
            instance,
        )?;

        let context = Arc::new(CallbackContext {
            proxy,
            scrollbar,
            toggles: toggle_values(initial).map(AtomicBool::new),
            text_dirty: [false, false, false].map(AtomicBool::new),
            drag_active: AtomicBool::new(false),
            drag_x: AtomicI32::new(0),
            drag_timer_armed: AtomicBool::new(false),
            dragging: AtomicBool::new(false),
            collapsed: AtomicBool::new(false),
            scroll_pos: AtomicI32::new(0),
            max_scroll: AtomicI32::new(0),
            page_size: AtomicI32::new(0),
            scale_x1000: AtomicI32::new(1000),
        });
        attach_subclass(
            hwnd,
            Some(sidebar_subclass_proc),
            SIDEBAR_SUBCLASS_ID,
            &context,
            "native sidebar",
        )?;
        attach_subclass(
            splitter,
            Some(splitter_subclass_proc),
            SPLITTER_SUBCLASS_ID,
            &context,
            "sidebar splitter",
        )?;
        subclass_children_for_wheel(hwnd);

        Ok(Self {
            hwnd,
            splitter,
            scrollbar,
            collapse,
            rows: content.rows,
            callback_context: context,
            last_width: AtomicI32::new(0),
            last_height: AtomicI32::new(0),
            _fonts: [normal_font, heading_font, title_font],
        })
    }

    pub fn take_pending_changes(&self) -> Vec<SettingChange> {
        let mut changes = Vec::new();
        for (index, (&id, make)) in TEXT_IDS.iter().zip(TEXT_CHANGES).enumerate() {
            if self.callback_context.text_dirty[index].swap(false, Ordering::Relaxed) {
                changes.push(make(window_text(self.item(id))));
            }
        }
        changes
    }

    pub fn apply_values(&self, values: &InitialValues) {
        for (&id, value) in TOGGLE_IDS.iter().zip(toggle_values(values)) {
            self.callback_context.set_checked(id, value);
            repaint_control(self.item(id));
        }
        for (&id, value) in TEXT_IDS.iter().zip(text_values(values)) {
            set_text(self.item(id), value);
        }
        for dirty in &self.callback_context.text_dirty {
            dirty.store(false, Ordering::Relaxed);
        }
        set_text(
            self.item(ID_CURRENT_VERSION),
            &format!("当前：{}", values.liqi_version),
        );
        self.refresh_layout();
    }

    /// Mark a live splitter resize.  While the pointer is down we avoid a
    /// background erase on every frame (which otherwise exposes the old
    /// WebView2 surface for a frame).  One full repaint on release cleans up
    /// any pixels left by a child window whose bounds just changed.
    pub fn set_dragging(&self, dragging: bool) {
        let was_dragging = self
            .callback_context
            .dragging
            .swap(dragging, Ordering::Relaxed);
        if was_dragging && !dragging {
            self.redraw(true);
        }
    }

    pub fn layout(
        &self,
        width: i32,
        height: i32,
        scale_factor: f64,
        collapsed: bool,
        requested_scroll: i32,
    ) -> i32 {
        self.resize_host(width, height);
        let metrics = LayoutMetrics::new(width, height, scale_factor);
        let was_collapsed = self
            .callback_context
            .collapsed
            .swap(collapsed, Ordering::Relaxed);
        self.callback_context
            .scale_x1000
            .store((scale_factor * 1000.0).round() as i32, Ordering::Relaxed);

        if collapsed {
            if !was_collapsed {
                self.for_each_content(|control| show_window(control, false));
                show_window(self.scrollbar, false);
                show_window(self.splitter, false);
                set_text(self.collapse, "›");
            }
            move_window(
                self.collapse,
                scaled(8, scale_factor),
                scaled(12, scale_factor),
                scaled(32, scale_factor),
                scaled(30, scale_factor),
            );
            self.raise_overlays();
            self.callback_context.scroll_pos.store(0, Ordering::Relaxed);
            self.callback_context.max_scroll.store(0, Ordering::Relaxed);
            self.redraw(true);
            if !was_collapsed {
                let parent = unsafe { GetParent(self.hwnd) };
                if parent != 0 as _ {
                    unsafe { invalidate_rect(parent, ptr::null(), 1) };
                }
            }
            return 0;
        }

        if was_collapsed {
            self.for_each_content(|control| show_window(control, true));
            set_text(self.collapse, "‹");
        }

        let viewport_height = metrics.viewport_height();
        let content_without_bar =
            self.flow_content(&metrics, metrics.reserved_right(false), 0, false);
        let mut show_scrollbar = content_without_bar > viewport_height;
        let content_height = if show_scrollbar {
            self.flow_content(&metrics, metrics.reserved_right(true), 0, false)
        } else {
            content_without_bar
        };
        show_scrollbar = content_height > viewport_height;
        let reserved_right = metrics.reserved_right(show_scrollbar);
        let max_scroll = (content_height - viewport_height).max(0);
        let scroll = requested_scroll.clamp(0, max_scroll);
        self.callback_context
            .page_size
            .store(viewport_height.max(1), Ordering::Relaxed);
        self.callback_context
            .max_scroll
            .store(max_scroll, Ordering::Relaxed);
        self.callback_context
            .scroll_pos
            .store(scroll, Ordering::Relaxed);
        self.update_scrollbar(viewport_height, content_height, scroll, show_scrollbar);
        self.place_chrome(height, &metrics, show_scrollbar);

        let right_edge = (metrics.logical_width - reserved_right).max(48);
        move_window(
            self.collapse,
            scaled(right_edge - 36, scale_factor),
            scaled(12, scale_factor),
            scaled(32, scale_factor),
            scaled(30, scale_factor),
        );
        self.flow_content(&metrics, reserved_right, scroll, true);
        self.raise_overlays();
        let erase = !self.callback_context.dragging.load(Ordering::Relaxed);
        self.redraw(erase);
        scroll
    }

    pub fn scroll_to(&self, requested_scroll: i32) -> i32 {
        if self.callback_context.collapsed.load(Ordering::Relaxed) {
            return 0;
        }
        let max_scroll = self
            .callback_context
            .max_scroll
            .load(Ordering::Relaxed)
            .max(0);
        let scroll = requested_scroll.clamp(0, max_scroll);
        let current = self.callback_context.scroll_pos.load(Ordering::Relaxed);
        if scroll == current {
            return current;
        }
        self.callback_context
            .scroll_pos
            .store(scroll, Ordering::Relaxed);
        let Some(metrics) = self.current_metrics() else {
            return scroll;
        };

        let reserved_right = metrics.reserved_right(max_scroll > 0);
        self.update_scrollbar_position(scroll);
        self.flow_content(&metrics, reserved_right, scroll, true);
        self.raise_overlays();
        self.redraw(false);
        scroll
    }

    pub fn set_message(&self, message: &str) {
        set_text(self.item(ID_MESSAGE), message);
        self.refresh_layout();
    }

    pub fn set_checking(&self, checking: bool) {
        self.set_update_phase(checking.then_some(LiqiUpdatePhase::Checking));
    }

    pub fn set_update_phase(&self, phase: Option<LiqiUpdatePhase>) {
        let busy = phase.is_some();
        set_enabled(self.item(ID_CHECK_UPDATE), !busy);
        set_enabled(self.item(ID_RESTART), !busy);
        set_text(
            self.item(ID_CHECK_UPDATE),
            match phase {
                Some(LiqiUpdatePhase::Checking) => "正在检查…",
                Some(LiqiUpdatePhase::Downloading) => "正在更新…",
                None => "检查更新",
            },
        );
        match phase {
            Some(LiqiUpdatePhase::Checking) => {
                set_text(self.item(ID_LATEST_VERSION), "最新：检查更新中");
            }
            Some(LiqiUpdatePhase::Downloading) => {
                set_text(self.item(ID_LATEST_VERSION), "最新：正在更新");
            }
            None => {}
        }
        self.refresh_layout();
    }

    pub fn set_reloading(&self, reloading: bool) {
        for id in RELOAD_LOCK_IDS {
            set_enabled(self.item(id), !reloading);
        }
        set_text(
            self.item(ID_RESTART),
            if reloading {
                "正在重新加载…"
            } else {
                "重新加载配置并刷新网页"
            },
        );
        self.refresh_layout();
    }

    pub fn set_proxy_status(&self, message: &str) {
        set_text(self.item(ID_PROXY_STATUS), message);
        self.refresh_layout();
    }

    pub fn set_proxy_reload_failed(&self) {
        self.set_proxy_status("● 代理重新加载失败");
    }

    pub fn set_latest_version(&self, status: &LiqiUpdateStatus) {
        self.set_checking(false);
        match status {
            LiqiUpdateStatus::Latest(version) => {
                set_text(
                    self.item(ID_LATEST_VERSION),
                    &format!("最新：{version} （已是最新）"),
                );
                self.set_message("");
            }
            LiqiUpdateStatus::Updated(version) => {
                set_text(
                    self.item(ID_LATEST_VERSION),
                    &format!("最新：{version} （已更新，需重启读取）"),
                );
                self.set_message("");
            }
            LiqiUpdateStatus::Failed(error) => {
                set_text(self.item(ID_LATEST_VERSION), "最新：检查失败");
                self.set_message(&format!("检查失败：{error}"));
            }
        }
    }

    fn item(&self, id: i32) -> HWND {
        dlg_item(self.hwnd, id)
    }

    fn for_each_content(&self, mut visit: impl FnMut(HWND)) {
        for row in &self.rows {
            match row {
                Row::Band { hwnd, .. } | Row::Button { hwnd, .. } | Row::Message(hwnd) => {
                    visit(*hwnd)
                }
                Row::Toggle { label, switch, .. } => {
                    visit(*label);
                    visit(*switch);
                }
                Row::Field { label, field, .. } => {
                    visit(*label);
                    visit(*field);
                }
            }
        }
    }

    fn update_scrollbar(
        &self,
        viewport_height: i32,
        content_height: i32,
        position: i32,
        visible: bool,
    ) {
        let mut info: SCROLLINFO = unsafe { std::mem::zeroed() };
        info.cbSize = size_of::<SCROLLINFO>() as u32;
        info.fMask = SIF_RANGE | SIF_PAGE | SIF_POS;
        info.nMin = 0;
        info.nMax = (content_height - 1).max(0);
        info.nPage = viewport_height.max(1) as u32;
        info.nPos = position;
        show_window(self.scrollbar, visible);
        unsafe { set_scroll_info(self.scrollbar, SB_CTL, &info, 1) };
    }

    fn place_chrome(&self, height: i32, metrics: &LayoutMetrics, show_scrollbar: bool) {
        let client = client_rect(self.hwnd);
        let splitter_x = (client.right - metrics.splitter_width).max(0);
        if show_scrollbar {
            let bar_x = (splitter_x - metrics.scrollbar_width).max(0);
            move_window(self.scrollbar, bar_x, 0, metrics.scrollbar_width, height);
            show_window(self.scrollbar, true);
        } else {
            show_window(self.scrollbar, false);
        }
        move_window(self.splitter, splitter_x, 0, metrics.splitter_width, height);
        show_window(self.splitter, true);
    }

    fn raise_overlays(&self) {
        // WebView2 is created after the sidebar and therefore normally owns
        // the top sibling z-order.  Keep the whole opaque sidebar above it
        // while its right edge is moving; otherwise the compositor can show
        // an old game frame through the newly exposed strip.
        bring_to_front(self.hwnd);
        bring_to_front(self.scrollbar);
        bring_to_front(self.splitter);
        bring_to_front(self.collapse);
    }

    fn update_scrollbar_position(&self, position: i32) {
        let mut info: SCROLLINFO = unsafe { std::mem::zeroed() };
        info.cbSize = size_of::<SCROLLINFO>() as u32;
        info.fMask = SIF_POS;
        info.nPos = position;
        unsafe { set_scroll_info(self.scrollbar, SB_CTL, &info, 1) };
    }

    fn redraw(&self, erase: bool) {
        let flags = if erase {
            REDRAW_INVALIDATE | REDRAW_ERASE | REDRAW_ALL_CHILDREN | REDRAW_FRAME
        } else {
            REDRAW_INVALIDATE | REDRAW_ALL_CHILDREN
        };
        unsafe { redraw_window(self.hwnd, ptr::null(), 0 as _, flags) };
    }

    fn refresh_layout(&self) {
        let Some(metrics) = self.current_metrics() else {
            return;
        };
        let collapsed = self.callback_context.collapsed.load(Ordering::Relaxed);
        let scroll = self.callback_context.scroll_pos.load(Ordering::Relaxed);
        let _ = self.layout(
            metrics.width,
            metrics.height,
            metrics.scale,
            collapsed,
            scroll,
        );
    }

    fn resize_host(&self, width: i32, height: i32) {
        let prev_width = self.last_width.swap(width, Ordering::Relaxed);
        let prev_height = self.last_height.swap(height, Ordering::Relaxed);
        if prev_width != width || prev_height != height {
            unsafe {
                set_window_pos(
                    self.hwnd,
                    0 as _,
                    0,
                    0,
                    width.max(1),
                    height.max(1),
                    SET_WINDOW_POS_NO_ACTIVATE
                        | SET_WINDOW_POS_SHOW
                        | SET_WINDOW_POS_NO_REDRAW
                        | SET_WINDOW_POS_NO_COPY_BITS,
                );
            }
        }
    }

    fn current_metrics(&self) -> Option<LayoutMetrics> {
        let width = self.last_width.load(Ordering::Relaxed);
        let height = self.last_height.load(Ordering::Relaxed);
        let scale = self.callback_context.scale_x1000.load(Ordering::Relaxed) as f64 / 1000.0;
        (width > 0 && height > 0 && scale > 0.0).then(|| LayoutMetrics::new(width, height, scale))
    }

    fn flow_content(
        &self,
        metrics: &LayoutMetrics,
        reserved_right: i32,
        scroll: i32,
        place: bool,
    ) -> i32 {
        let dc = ScopedDc::new(self.hwnd);
        let ctx = FlowCtx::new(metrics, reserved_right, scroll, dc.hdc);
        let mut y = 16;
        for row in &self.rows {
            y = layout_row(row, y, &ctx, place);
        }
        y + 16
    }
}

struct ContentBuilder {
    parent: HWND,
    instance: HINSTANCE,
    normal_font: HFONT,
    heading_font: HFONT,
    rows: Vec<Row>,
}

impl ContentBuilder {
    fn add_band(
        &mut self,
        text: &str,
        id: i32,
        font: HFONT,
        x: i32,
        width: WidthKind,
        min_height: i32,
        gap: i32,
    ) -> Result<HWND> {
        let hwnd = create_static(self.parent, self.instance, text, id)?;
        set_font(hwnd, font);
        self.rows.push(Row::Band {
            hwnd,
            x,
            width,
            min_height,
            gap,
        });
        Ok(hwnd)
    }

    fn add_heading(&mut self, text: &str) -> Result<()> {
        self.add_band(text, 0, self.heading_font, 16, WidthKind::Heading, 22, 10)?;
        Ok(())
    }

    fn add_toggle(&mut self, id: i32, label: &str, gap: i32) -> Result<()> {
        let caption = create_static(self.parent, self.instance, label, 0)?;
        set_font(caption, self.normal_font);
        let switch = create_owner_button(self.parent, self.instance, "", id)?;
        set_font(switch, self.normal_font);
        self.rows.push(Row::Toggle {
            label: caption,
            switch,
            gap,
        });
        Ok(())
    }

    fn add_field(
        &mut self,
        id: i32,
        label: &str,
        text: &str,
        limit: usize,
        extra_gap: i32,
    ) -> Result<()> {
        let caption = create_static(self.parent, self.instance, label, 0)?;
        set_font(caption, self.normal_font);
        let field = create_edit(self.parent, self.instance, text, id, limit)?;
        set_font(field, self.normal_font);
        self.rows.push(Row::Field {
            label: caption,
            field,
            extra_gap,
        });
        Ok(())
    }

    fn add_button(&mut self, id: i32, text: &str, gap: i32) -> Result<()> {
        let hwnd = create_owner_button(self.parent, self.instance, text, id)?;
        set_font(hwnd, self.normal_font);
        self.rows.push(Row::Button { hwnd, gap });
        Ok(())
    }

    fn add_message(&mut self) -> Result<()> {
        let hwnd = create_static(self.parent, self.instance, "", ID_MESSAGE)?;
        set_font(hwnd, self.normal_font);
        self.rows.push(Row::Message(hwnd));
        Ok(())
    }
}

struct LayoutMetrics {
    scale: f64,
    width: i32,
    height: i32,
    splitter_width: i32,
    scrollbar_width: i32,
    logical_width: i32,
    logical_height: i32,
}

impl LayoutMetrics {
    fn new(width: i32, height: i32, scale: f64) -> Self {
        Self {
            scale,
            width,
            height,
            splitter_width: scaled(6, scale),
            scrollbar_width: unsafe { GetSystemMetrics(SM_CXVSCROLL) }.max(scaled(10, scale)),
            logical_width: (width as f64 / scale).round() as i32,
            logical_height: (height as f64 / scale).round() as i32,
        }
    }

    fn reserved_right(&self, show_bar: bool) -> i32 {
        8 + ((self.splitter_width + if show_bar { self.scrollbar_width } else { 0 }) as f64
            / self.scale)
            .round() as i32
    }

    fn viewport_height(&self) -> i32 {
        self.logical_height.max(0)
    }
}

struct FlowCtx {
    hdc: HDC,
    scale: f64,
    scroll: i32,
    toggle_width: i32,
    toggle_x: i32,
    text_width: i32,
    content_width: i32,
    heading_width: i32,
    title_width: i32,
}

impl FlowCtx {
    fn new(metrics: &LayoutMetrics, reserved_right: i32, scroll: i32, hdc: HDC) -> Self {
        let right_edge = (metrics.logical_width - reserved_right).max(48);
        let toggle_width = 52;
        let toggle_x = (right_edge - 8 - toggle_width).max(22);
        Self {
            hdc,
            scale: metrics.scale,
            scroll,
            toggle_width,
            toggle_x,
            text_width: (toggle_x - 30).max(48),
            content_width: (right_edge - 30).max(48),
            heading_width: (right_edge - 24).max(48),
            title_width: (right_edge - 56).max(48),
        }
    }

    fn width(&self, kind: WidthKind) -> i32 {
        match kind {
            WidthKind::Title => self.title_width,
            WidthKind::Heading => self.heading_width,
            WidthKind::Content => self.content_width,
        }
    }
}

struct ScopedDc {
    hwnd: HWND,
    hdc: HDC,
}

impl ScopedDc {
    fn new(hwnd: HWND) -> Self {
        Self {
            hwnd,
            hdc: unsafe { get_dc(hwnd) },
        }
    }
}

impl Drop for ScopedDc {
    fn drop(&mut self) {
        if self.hdc != 0 as _ {
            unsafe { release_dc(self.hwnd, self.hdc) };
        }
    }
}

fn layout_row(row: &Row, y: i32, ctx: &FlowCtx, place: bool) -> i32 {
    match row {
        Row::Band {
            hwnd,
            x,
            width,
            min_height,
            gap,
        } => {
            let width = ctx.width(*width);
            let height = measure_text_logical(ctx.hdc, *hwnd, width, ctx.scale).max(*min_height);
            place_block(ctx, *hwnd, *x, y, width, height, place);
            y + height + *gap
        }
        Row::Toggle { label, switch, gap } => {
            y + place_toggle_row(ctx, *label, *switch, y, place) + *gap
        }
        Row::Field {
            label,
            field,
            extra_gap,
        } => place_field(ctx, *label, *field, y, place) + *extra_gap,
        Row::Button { hwnd, gap } => {
            let height = measure_button_logical(ctx.hdc, *hwnd, ctx.content_width, ctx.scale);
            place_block(ctx, *hwnd, 22, y, ctx.content_width, height, place);
            y + height + *gap
        }
        Row::Message(hwnd) => {
            let height = measure_text_logical(ctx.hdc, *hwnd, ctx.content_width, ctx.scale);
            if height == 0 {
                if place {
                    show_window(*hwnd, false);
                }
                y
            } else {
                let height = height.max(22);
                if place {
                    show_window(*hwnd, true);
                }
                place_block(ctx, *hwnd, 22, y, ctx.content_width, height, place);
                y + height
            }
        }
    }
}

fn toggle_values(values: &InitialValues) -> [bool; 6] {
    [
        values.mod_switch,
        values.auto_update,
        values.show_server,
        values.anti_nickname_censorship,
        values.emoji_switch,
        values.hint_switch,
    ]
}

fn text_values(values: &InitialValues) -> [&str; 3] {
    [
        values.nickname.as_str(),
        values.req_proxy.as_str(),
        values.github_prefix.as_str(),
    ]
}

fn toggle_index(id: i32) -> Option<usize> {
    TOGGLE_IDS.iter().position(|&item| item == id)
}

fn text_index(id: i32) -> Option<usize> {
    TEXT_IDS.iter().position(|&item| item == id)
}

fn action_event(id: i32) -> Option<GuiEvent> {
    match id {
        ID_CHECK_UPDATE => Some(GuiEvent::CheckUpdate),
        ID_RESTART => Some(GuiEvent::Restart),
        ID_COLLAPSE => Some(GuiEvent::ToggleSidebar),
        _ => None,
    }
}

fn send_event(context: &CallbackContext, event: GuiEvent) {
    let _ = context.proxy.send_event(event);
}

fn flush_text_setting(hwnd: HWND, context: &CallbackContext, index: usize) {
    let edit = dlg_item(hwnd, TEXT_IDS[index]);
    if edit != 0 as _ && context.text_dirty[index].swap(false, Ordering::Relaxed) {
        send_event(
            context,
            GuiEvent::SettingChanged(TEXT_CHANGES[index](window_text(edit))),
        );
    }
}

fn flush_all_text_settings(hwnd: HWND, context: &CallbackContext) {
    for index in 0..TEXT_IDS.len() {
        flush_text_setting(hwnd, context, index);
    }
}

fn handle_command(hwnd: HWND, context: &CallbackContext, w_param: WPARAM, l_param: LPARAM) {
    let id = (w_param & 0xffff) as i32;
    let notification = ((w_param >> 16) & 0xffff) as u32;
    if notification == BN_CLICKED as u32 {
        if let Some(index) = toggle_index(id) {
            let checked = context.toggle(id);
            repaint_control(l_param as HWND);
            send_event(
                context,
                GuiEvent::SettingChanged(TOGGLE_CHANGES[index](checked)),
            );
        } else if let Some(event) = action_event(id) {
            send_event(context, event);
        }
    } else if notification == EN_CHANGE {
        if let Some(index) = text_index(id) {
            context.text_dirty[index].store(true, Ordering::Relaxed);
            unsafe { SetTimer(hwnd, TEXT_TIMER_ID, 450, None) };
        }
    } else if notification == EN_KILLFOCUS
        && let Some(index) = text_index(id)
    {
        flush_text_setting(hwnd, context, index);
    }
}

fn handle_draw_item(context: &CallbackContext, l_param: LPARAM) -> Option<LRESULT> {
    if l_param == 0 {
        return None;
    }
    let item = unsafe { &*(l_param as *const NativeDrawItem) };
    let id = item.control_id as i32;
    if toggle_index(id).is_some() {
        draw_toggle(item, context.is_checked(id));
        return Some(1);
    }
    if matches!(id, ID_CHECK_UPDATE | ID_COLLAPSE | ID_RESTART) {
        draw_action_button(item, id == ID_RESTART);
        return Some(1);
    }
    None
}

unsafe extern "system" fn sidebar_subclass_proc(
    hwnd: HWND,
    message: u32,
    w_param: WPARAM,
    l_param: LPARAM,
    subclass_id: usize,
    reference_data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        unsafe {
            KillTimer(hwnd, TEXT_TIMER_ID);
            detach_subclass(
                hwnd,
                Some(sidebar_subclass_proc),
                subclass_id,
                reference_data,
                true,
            );
            return def_subclass_proc(hwnd, message, w_param, l_param);
        }
    }

    let context = unsafe { &*(reference_data as *const CallbackContext) };
    if let Some(result) = handle_paint(hwnd, message, w_param) {
        return result;
    }
    match message {
        WM_COMMAND => handle_command(hwnd, context, w_param, l_param),
        WM_TIMER if w_param == TEXT_TIMER_ID => {
            unsafe { KillTimer(hwnd, TEXT_TIMER_ID) };
            flush_all_text_settings(hwnd, context);
        }
        WM_DRAWITEM => {
            if let Some(result) = handle_draw_item(context, l_param) {
                return result;
            }
        }
        WM_CTLCOLORSTATIC => unsafe {
            SetBkMode(w_param as HDC, OPAQUE as i32);
            SetBkColor(w_param as HDC, rgb(255, 255, 255));
            SetTextColor(w_param as HDC, rgb(35, 39, 47));
            return GetStockObject(WHITE_BRUSH) as LRESULT;
        },
        WM_MOUSEWHEEL => {
            let delta = ((w_param >> 16) as u16) as i16 as i32;
            let amount = -(delta / 120) * 48;
            if amount != 0 {
                queue_scroll(context, context.scroll_pos.load(Ordering::Relaxed) + amount);
            }
            return 0;
        }
        WM_VSCROLL => {
            if let Some(position) = scroll_request(context, w_param, l_param) {
                queue_scroll(context, position);
            }
            return 0;
        }
        _ => {}
    }
    unsafe { def_subclass_proc(hwnd, message, w_param, l_param) }
}

unsafe extern "system" fn splitter_subclass_proc(
    hwnd: HWND,
    message: u32,
    w_param: WPARAM,
    l_param: LPARAM,
    subclass_id: usize,
    reference_data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        let context = unsafe { &*(reference_data as *const CallbackContext) };
        stop_drag_timer(hwnd, context);
        context.drag_active.store(false, Ordering::Release);
        unsafe {
            detach_subclass(
                hwnd,
                Some(splitter_subclass_proc),
                subclass_id,
                reference_data,
                true,
            );
            return def_subclass_proc(hwnd, message, w_param, l_param);
        }
    }

    let context = unsafe { &*(reference_data as *const CallbackContext) };
    if let Some(result) = handle_paint(hwnd, message, w_param) {
        return result;
    }
    match message {
        WM_LBUTTONDOWN => {
            unsafe { set_capture(hwnd) };
            if unsafe { get_capture() } == hwnd
                && let Some(x) = cursor_x()
            {
                context.drag_x.store(x, Ordering::Relaxed);
                if !context.drag_active.swap(true, Ordering::AcqRel) {
                    send_event(context, GuiEvent::SidebarDragStart(x));
                }
            }
            return 0;
        }
        WM_MOUSEMOVE => {
            if context.drag_active.load(Ordering::Acquire)
                && unsafe { get_capture() } == hwnd
                && let Some(x) = cursor_x()
            {
                context.drag_x.store(x, Ordering::Relaxed);
                arm_drag_timer(hwnd, context);
            }
            return 0;
        }
        WM_LBUTTONUP => {
            if unsafe { get_capture() } == hwnd {
                if let Some(x) = cursor_x() {
                    context.drag_x.store(x, Ordering::Relaxed);
                }
                stop_drag_timer(hwnd, context);
                unsafe { release_capture() };
                finish_drag(hwnd, context);
            }
            return 0;
        }
        WM_CAPTURECHANGED => {
            finish_drag(hwnd, context);
            return 0;
        }
        WM_CANCELMODE => {
            finish_drag(hwnd, context);
            return 0;
        }
        WM_TIMER if w_param == DRAG_TIMER_ID => {
            unsafe { KillTimer(hwnd, DRAG_TIMER_ID) };
            context.drag_timer_armed.store(false, Ordering::Release);
            if context.drag_active.load(Ordering::Acquire) && unsafe { get_capture() } == hwnd {
                if let Some(x) = cursor_x() {
                    context.drag_x.store(x, Ordering::Relaxed);
                }
                let x = context.drag_x.load(Ordering::Acquire);
                send_event(context, GuiEvent::SidebarDrag(x));
            }
            return 0;
        }
        WM_MOUSEWHEEL => {
            let parent = unsafe { GetParent(hwnd) };
            if parent != 0 as _ {
                unsafe { SendMessageW(parent, message, w_param, l_param) };
            }
            return 0;
        }
        WM_SETCURSOR => {
            unsafe { SetCursor(LoadCursorW(0 as _, IDC_SIZEWE)) };
            return 1;
        }
        _ => {}
    }
    unsafe { def_subclass_proc(hwnd, message, w_param, l_param) }
}

fn arm_drag_timer(hwnd: HWND, context: &CallbackContext) {
    if !context.drag_timer_armed.swap(true, Ordering::AcqRel) {
        let timer_created = unsafe { SetTimer(hwnd, DRAG_TIMER_ID, DRAG_FRAME_INTERVAL_MS, None) };
        if timer_created == 0 {
            // Keep dragging usable even if the thread cannot allocate a
            // window timer; the release path still applies the final width.
            context.drag_timer_armed.store(false, Ordering::Release);
            send_event(
                context,
                GuiEvent::SidebarDrag(context.drag_x.load(Ordering::Acquire)),
            );
        }
    }
}

fn stop_drag_timer(hwnd: HWND, context: &CallbackContext) {
    unsafe { KillTimer(hwnd, DRAG_TIMER_ID) };
    context.drag_timer_armed.store(false, Ordering::Release);
}

fn finish_drag(hwnd: HWND, context: &CallbackContext) {
    stop_drag_timer(hwnd, context);
    if context.drag_active.swap(false, Ordering::AcqRel) {
        let x = context.drag_x.load(Ordering::Acquire);
        // Queue the last pointer position before the end marker.  The event
        // loop then commits the exact final width even when the last
        // WM_MOUSEMOVE arrived between two timer ticks.
        send_event(context, GuiEvent::SidebarDrag(x));
        send_event(context, GuiEvent::SidebarDragEnd);
    }
}

fn handle_paint(hwnd: HWND, message: u32, w_param: WPARAM) -> Option<LRESULT> {
    match message {
        WM_ERASEBKGND => {
            fill_white(w_param as HDC, &client_rect(hwnd));
            Some(1)
        }
        WM_PAINT => {
            paint_white_panel(hwnd);
            Some(0)
        }
        _ => None,
    }
}

fn register_sidebar_class(instance: HINSTANCE) -> Result<()> {
    let class_name = SIDEBAR_CLASS_NAME.get_or_init(|| wide(SIDEBAR_CLASS));
    let mut existing: WNDCLASSW = unsafe { std::mem::zeroed() };
    if unsafe { GetClassInfoW(instance, class_name.as_ptr(), &mut existing) } != 0 {
        return Ok(());
    }
    let class = WNDCLASSW {
        style: CS_DBLCLKS | CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(DefWindowProcW),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: 0 as _,
        hCursor: unsafe { LoadCursorW(0 as _, IDC_ARROW) },
        hbrBackground: unsafe { GetStockObject(WHITE_BRUSH) as _ },
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(1410) {
            return Err(error).context("Failed to register sidebar window class");
        }
    }
    Ok(())
}

fn attach_subclass(
    hwnd: HWND,
    callback: SubclassProc,
    subclass_id: usize,
    context: &Arc<CallbackContext>,
    what: &str,
) -> Result<()> {
    let raw = Arc::into_raw(Arc::clone(context)) as usize;
    if unsafe { set_window_subclass(hwnd, callback, subclass_id, raw) } == 0 {
        unsafe { drop(Arc::from_raw(raw as *const CallbackContext)) };
        bail!("Failed to subclass {what}");
    }
    Ok(())
}

unsafe fn detach_subclass(
    hwnd: HWND,
    callback: SubclassProc,
    subclass_id: usize,
    reference_data: usize,
    drop_context: bool,
) {
    unsafe {
        remove_window_subclass(hwnd, callback, subclass_id);
        if drop_context {
            drop(Arc::from_raw(reference_data as *const CallbackContext));
        }
    }
}

fn subclass_children_for_wheel(parent: HWND) {
    unsafe {
        EnumChildWindows(parent, Some(enum_subclass_child_wheel), 0);
    }
}

unsafe extern "system" fn enum_subclass_child_wheel(hwnd: HWND, _lparam: LPARAM) -> i32 {
    unsafe {
        set_window_subclass(
            hwnd,
            Some(child_wheel_subclass_proc),
            CHILD_WHEEL_SUBCLASS_ID,
            0,
        );
    }
    1
}

unsafe extern "system" fn child_wheel_subclass_proc(
    hwnd: HWND,
    message: u32,
    w_param: WPARAM,
    l_param: LPARAM,
    subclass_id: usize,
    _reference_data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        unsafe {
            detach_subclass(hwnd, Some(child_wheel_subclass_proc), subclass_id, 0, false);
            return def_subclass_proc(hwnd, message, w_param, l_param);
        }
    }
    if message == WM_MOUSEWHEEL {
        let parent = unsafe { GetParent(hwnd) };
        if parent != 0 as _ {
            return unsafe { SendMessageW(parent, message, w_param, l_param) };
        }
    }
    unsafe { def_subclass_proc(hwnd, message, w_param, l_param) }
}

fn queue_scroll(context: &CallbackContext, requested: i32) {
    let max_scroll = context.max_scroll.load(Ordering::Relaxed).max(0);
    let current = context.scroll_pos.load(Ordering::Relaxed);
    let scroll = requested.clamp(0, max_scroll);
    if scroll != current {
        send_event(context, GuiEvent::ScrollTo(scroll));
    }
}

fn scroll_request(context: &CallbackContext, w_param: WPARAM, l_param: LPARAM) -> Option<i32> {
    let current = context.scroll_pos.load(Ordering::Relaxed);
    let page = context.page_size.load(Ordering::Relaxed).max(1);
    let code = (w_param & 0xffff) as u32;
    Some(match code {
        value if value == SB_LINEUP as u32 => current - 32,
        value if value == SB_LINEDOWN as u32 => current + 32,
        value if value == SB_PAGEUP as u32 => current - page,
        value if value == SB_PAGEDOWN as u32 => current + page,
        value if value == SB_TOP as u32 => 0,
        value if value == SB_BOTTOM as u32 => i32::MAX,
        value if value == SB_THUMBPOSITION as u32 || value == SB_THUMBTRACK as u32 => {
            let mut info: SCROLLINFO = unsafe { std::mem::zeroed() };
            info.cbSize = size_of::<SCROLLINFO>() as u32;
            info.fMask = SIF_TRACKPOS;
            let bar = if l_param != 0 {
                l_param as HWND
            } else {
                context.scrollbar
            };
            unsafe { GetScrollInfo(bar, SB_CTL, &mut info) };
            info.nTrackPos
        }
        _ => return None,
    })
}

fn paint_white_panel(hwnd: HWND) {
    let mut paint: PAINTSTRUCT = unsafe { std::mem::zeroed() };
    let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
    if hdc != 0 as _ {
        let client = client_rect(hwnd);
        fill_white(hdc, &client);
        draw_divider(hdc, &client);
        unsafe { EndPaint(hwnd, &paint) };
    }
}

fn draw_divider(hdc: HDC, client: &RECT) {
    if client.right - client.left <= 0 {
        return;
    }
    let brush = unsafe { CreateSolidBrush(rgb(226, 230, 236)) };
    let line = RECT {
        left: client.right - 1,
        top: client.top,
        right: client.right,
        bottom: client.bottom,
    };
    unsafe {
        FillRect(hdc, &line, brush as _);
        DeleteObject(brush as _);
    }
}

fn scaled(value: i32, scale_factor: f64) -> i32 {
    (value as f64 * scale_factor).round() as i32
}

fn place_block(ctx: &FlowCtx, hwnd: HWND, x: i32, y: i32, width: i32, height: i32, place: bool) {
    if place {
        move_window(
            hwnd,
            scaled(x, ctx.scale),
            scaled(y - ctx.scroll, ctx.scale),
            scaled(width, ctx.scale),
            scaled(height, ctx.scale),
        );
    }
}

fn place_field(ctx: &FlowCtx, label: HWND, field: HWND, y: i32, place: bool) -> i32 {
    let width = ctx.content_width;
    let label_height = measure_text_logical(ctx.hdc, label, width, ctx.scale).max(20);
    place_block(ctx, label, 22, y, width, label_height, place);
    let field_y = y + label_height + 4;
    place_block(ctx, field, 22, field_y, width, 32, place);
    field_y + 32 + 12
}

fn place_toggle_row(ctx: &FlowCtx, label: HWND, switch: HWND, y: i32, place: bool) -> i32 {
    let label_height = measure_text_logical(ctx.hdc, label, ctx.text_width, ctx.scale).max(22);
    let row_height = label_height.max(28);
    let label_y = y + (row_height - label_height).max(0) / 2;
    let switch_y = y + (row_height - 28).max(0) / 2;
    place_block(ctx, label, 22, label_y, ctx.text_width, label_height, place);
    place_block(
        ctx,
        switch,
        ctx.toggle_x,
        switch_y,
        ctx.toggle_width,
        28,
        place,
    );
    row_height
}

fn measure_button_logical(hdc: HDC, hwnd: HWND, width_logical: i32, scale_factor: f64) -> i32 {
    let text_width = (width_logical - 16).max(32);
    (measure_text_logical(hdc, hwnd, text_width, scale_factor) + 12).max(34)
}

fn measure_text_logical(hdc: HDC, hwnd: HWND, width_logical: i32, scale_factor: f64) -> i32 {
    let text = window_text_wide(hwnd);
    if text.len() <= 1 {
        return 0;
    }
    if scale_factor <= 0.0 || hdc == 0 as _ {
        return 22;
    }
    let width_px = scaled(width_logical, scale_factor).max(1);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width_px,
        bottom: 0,
    };
    unsafe {
        let font = SendMessageW(hwnd, WM_GETFONT, 0, 0);
        let old_font = (font != 0).then(|| SelectObject(hdc, font as _));
        draw_wrapped_text(hdc, &text, &mut rect, DRAW_TEXT_CALCRECT);
        if let Some(old_font) = old_font {
            SelectObject(hdc, old_font);
        }
    }
    let pixel_height = (rect.bottom - rect.top).max(1);
    ((pixel_height as f64 / scale_factor).ceil() as i32 + 2).max(1)
}

fn create_static(parent: HWND, instance: HINSTANCE, text: &str, id: i32) -> Result<HWND> {
    create_control(
        0,
        "STATIC",
        text,
        WS_CHILD | WS_VISIBLE | STATIC_LEFT,
        id,
        parent,
        instance,
    )
}

fn create_edit(
    parent: HWND,
    instance: HINSTANCE,
    text: &str,
    id: i32,
    limit: usize,
) -> Result<HWND> {
    let hwnd = create_control(
        WS_EX_CLIENTEDGE,
        "EDIT",
        text,
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
        id,
        parent,
        instance,
    )?;
    unsafe {
        SendMessageW(hwnd, EDIT_SET_LIMIT_TEXT, limit, 0);
    }
    Ok(hwnd)
}

fn create_owner_button(parent: HWND, instance: HINSTANCE, text: &str, id: i32) -> Result<HWND> {
    create_control(
        0,
        "BUTTON",
        text,
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_OWNERDRAW as u32,
        id,
        parent,
        instance,
    )
}

fn create_control(
    ex_style: u32,
    class_name: &str,
    text: &str,
    style: u32,
    id: i32,
    parent: HWND,
    instance: HINSTANCE,
) -> Result<HWND> {
    let class = wide(class_name);
    let text = wide(text);
    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            class.as_ptr(),
            text.as_ptr(),
            style | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            parent,
            id as isize as _,
            instance,
            ptr::null(),
        )
    };
    if hwnd == 0 as _ {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Failed to create native {class_name} control"));
    }
    Ok(hwnd)
}

fn create_font(point_size: f64, weight: i32, scale_factor: f64) -> Result<HFONT> {
    let face = wide("Segoe UI");
    let pixel_height = -(point_size * scale_factor).round() as i32;
    let font = unsafe {
        CreateFontW(
            pixel_height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET.into(),
            OUT_DEFAULT_PRECIS.into(),
            CLIP_DEFAULT_PRECIS.into(),
            CLEARTYPE_QUALITY.into(),
            (DEFAULT_PITCH | FF_DONTCARE).into(),
            face.as_ptr(),
        )
    };
    if font == 0 as _ {
        return Err(std::io::Error::last_os_error()).context("Failed to create sidebar font");
    }
    Ok(font)
}

fn set_font(hwnd: HWND, font: HFONT) {
    unsafe {
        SendMessageW(hwnd, WM_SETFONT, font as usize, 1);
    }
}

fn move_window(hwnd: HWND, x: i32, y: i32, width: i32, height: i32) {
    unsafe {
        MoveWindow(hwnd, x, y, width.max(1), height.max(1), 0);
    }
}

fn bring_to_front(hwnd: HWND) {
    unsafe {
        set_window_pos(
            hwnd,
            0 as _,
            0,
            0,
            0,
            0,
            SET_WINDOW_POS_NO_MOVE
                | SET_WINDOW_POS_NO_SIZE
                | SET_WINDOW_POS_NO_ACTIVATE
                | SET_WINDOW_POS_NO_REDRAW,
        );
    }
}

fn draw_toggle(item: &NativeDrawItem, checked: bool) {
    let rect = item.rect;
    let disabled = item.item_state & DRAW_ITEM_DISABLED != 0;
    fill_white(item.hdc, &rect);
    if draw_toggle_antialiased(item.hdc, &rect, checked, disabled) {
        return;
    }

    let track = if disabled {
        rgb(213, 216, 222)
    } else if checked {
        rgb(46, 125, 246)
    } else {
        rgb(183, 188, 197)
    };
    let track_brush = unsafe { CreateSolidBrush(track) };
    let knob_brush = unsafe { CreateSolidBrush(rgb(255, 255, 255)) };
    let height = (rect.bottom - rect.top).max(1);
    let diameter = (height - 8).max(1);
    let knob_left = if checked {
        rect.right - diameter - 4
    } else {
        rect.left + 4
    };

    unsafe {
        let old_brush = SelectObject(item.hdc, track_brush as _);
        let old_pen = SelectObject(item.hdc, GetStockObject(NULL_PEN));
        RoundRect(
            item.hdc,
            rect.left + 1,
            rect.top + 2,
            rect.right - 1,
            rect.bottom - 2,
            height,
            height,
        );
        SelectObject(item.hdc, knob_brush as _);
        Ellipse(
            item.hdc,
            knob_left,
            rect.top + 4,
            knob_left + diameter,
            rect.top + 4 + diameter,
        );
        SelectObject(item.hdc, old_pen);
        SelectObject(item.hdc, old_brush);
        DeleteObject(track_brush as _);
        DeleteObject(knob_brush as _);
    }
}

fn draw_toggle_antialiased(hdc: HDC, rect: &RECT, checked: bool, disabled: bool) -> bool {
    let token = *GDIPLUS_TOKEN.get_or_init(|| {
        let input = GdiplusStartupInput {
            version: 1,
            debug_callback: ptr::null(),
            suppress_background_thread: 0,
            suppress_external_codecs: 0,
        };
        let mut token = 0usize;
        let status = unsafe { gdiplus_startup(&mut token, &input, ptr::null_mut()) };
        if status == GDIPLUS_OK { token } else { 0 }
    });
    if token == 0 {
        return false;
    }

    let mut graphics = ptr::null_mut();
    if unsafe { gdip_create_from_hdc(hdc, &mut graphics) } != GDIPLUS_OK {
        return false;
    }
    unsafe { gdip_set_smoothing_mode(graphics, SMOOTHING_MODE_ANTI_ALIAS_8X8) };

    let track_color = if disabled {
        argb(255, 213, 216, 222)
    } else if checked {
        argb(255, 46, 125, 246)
    } else {
        argb(255, 183, 188, 197)
    };
    let mut track_brush = ptr::null_mut();
    let mut knob_brush = ptr::null_mut();
    let brushes_ready = unsafe {
        gdip_create_solid_fill(track_color, &mut track_brush) == GDIPLUS_OK
            && gdip_create_solid_fill(argb(255, 255, 255, 255), &mut knob_brush) == GDIPLUS_OK
    };
    if !brushes_ready {
        unsafe {
            if !track_brush.is_null() {
                gdip_delete_brush(track_brush);
            }
            if !knob_brush.is_null() {
                gdip_delete_brush(knob_brush);
            }
            gdip_delete_graphics(graphics);
        }
        return false;
    }

    let left = rect.left + 1;
    let top = rect.top + 2;
    let height = (rect.bottom - rect.top - 4).max(1);
    let width = (rect.right - rect.left - 2).max(height);
    let radius = height;
    let middle_width = (width - radius).max(1);
    let knob_size = (height - 4).max(1);
    let knob_left = if checked {
        left + width - knob_size - 2
    } else {
        left + 2
    };

    unsafe {
        gdip_fill_ellipse(graphics, track_brush, left, top, radius, radius);
        gdip_fill_rectangle(
            graphics,
            track_brush,
            left + radius / 2,
            top,
            middle_width,
            height,
        );
        gdip_fill_ellipse(
            graphics,
            track_brush,
            left + width - radius,
            top,
            radius,
            radius,
        );
        gdip_fill_ellipse(
            graphics,
            knob_brush,
            knob_left,
            top + 2,
            knob_size,
            knob_size,
        );
        gdip_delete_brush(track_brush);
        gdip_delete_brush(knob_brush);
        gdip_delete_graphics(graphics);
    }
    true
}

fn draw_action_button(item: &NativeDrawItem, primary: bool) {
    let selected = item.item_state & DRAW_ITEM_SELECTED != 0;
    let disabled = item.item_state & DRAW_ITEM_DISABLED != 0;
    let background = if disabled {
        rgb(213, 216, 222)
    } else if primary && selected {
        rgb(29, 92, 190)
    } else if primary {
        rgb(46, 125, 246)
    } else if selected {
        rgb(218, 223, 232)
    } else {
        rgb(239, 242, 247)
    };
    let foreground = if primary && !disabled {
        rgb(255, 255, 255)
    } else if disabled {
        rgb(135, 140, 150)
    } else {
        rgb(35, 39, 47)
    };
    let brush = unsafe { CreateSolidBrush(background) };
    let rect = item.rect;
    let text = window_text_wide(item.hwnd);

    unsafe {
        fill_white(item.hdc, &rect);
        let old_brush = SelectObject(item.hdc, brush as _);
        let old_pen = SelectObject(item.hdc, GetStockObject(NULL_PEN));
        RoundRect(
            item.hdc,
            rect.left + 1,
            rect.top + 1,
            rect.right - 1,
            rect.bottom - 1,
            10,
            10,
        );
        SelectObject(item.hdc, old_pen);
        SelectObject(item.hdc, old_brush);
        DeleteObject(brush as _);

        SetBkMode(item.hdc, TRANSPARENT as i32);
        SetTextColor(item.hdc, foreground);
        let font = SendMessageW(item.hwnd, WM_GETFONT, 0, 0);
        let old_font = (font != 0).then(|| SelectObject(item.hdc, font as _));
        let mut text_rect = RECT {
            left: rect.left + 8,
            top: rect.top + 6,
            right: rect.right - 8,
            bottom: rect.bottom - 6,
        };
        let mut calc_rect = text_rect;
        draw_wrapped_text(
            item.hdc,
            &text,
            &mut calc_rect,
            DRAW_TEXT_CALCRECT | DRAW_TEXT_CENTER,
        );
        let text_height = (calc_rect.bottom - calc_rect.top).max(1);
        let available = (text_rect.bottom - text_rect.top).max(1);
        let text_top = text_rect.top + ((available - text_height).max(0) / 2);
        let mut draw_rect = RECT {
            left: text_rect.left,
            top: text_top,
            right: text_rect.right,
            bottom: (text_top + text_height).min(text_rect.bottom),
        };
        draw_wrapped_text(item.hdc, &text, &mut draw_rect, DRAW_TEXT_CENTER);
        if let Some(old_font) = old_font {
            SelectObject(item.hdc, old_font);
        }
    }
}

fn draw_wrapped_text(hdc: HDC, text: &[u16], rect: &mut RECT, extra_flags: u32) {
    unsafe {
        draw_text_w(
            hdc,
            text.as_ptr(),
            text.len().saturating_sub(1) as i32,
            rect,
            DRAW_TEXT_WORDBREAK | DRAW_TEXT_NOPREFIX | extra_flags,
        );
    }
}

fn rgb(red: u8, green: u8, blue: u8) -> u32 {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

fn argb(alpha: u8, red: u8, green: u8, blue: u8) -> u32 {
    ((alpha as u32) << 24) | ((red as u32) << 16) | ((green as u32) << 8) | blue as u32
}

fn client_rect(hwnd: HWND) -> RECT {
    let mut rect = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(hwnd, &mut rect) };
    rect
}

fn fill_white(hdc: HDC, rect: &RECT) {
    unsafe { FillRect(hdc, rect, GetStockObject(WHITE_BRUSH) as _) };
}

fn dlg_item(parent: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(parent, id) }
}

fn show_window(hwnd: HWND, visible: bool) {
    unsafe { ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE }) };
}

fn set_enabled(hwnd: HWND, enabled: bool) {
    unsafe { enable_window(hwnd, i32::from(enabled)) };
}

fn repaint_control(hwnd: HWND) {
    unsafe {
        invalidate_rect(hwnd, ptr::null(), 1);
    }
}

fn set_text(hwnd: HWND, text: &str) {
    let text = wide(text);
    unsafe {
        SetWindowTextW(hwnd, text.as_ptr());
    }
}

fn window_text_wide(hwnd: HWND) -> Vec<u16> {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    let mut buffer = vec![0u16; length.max(0) as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    buffer.truncate(copied.max(0) as usize + 1);
    buffer
}

fn window_text(hwnd: HWND) -> String {
    let buffer = window_text_wide(hwnd);
    String::from_utf16_lossy(&buffer[..buffer.len() - 1])
}

fn cursor_x() -> Option<i32> {
    let mut point = POINT { x: 0, y: 0 };
    (unsafe { GetCursorPos(&mut point) } != 0).then_some(point.x)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
