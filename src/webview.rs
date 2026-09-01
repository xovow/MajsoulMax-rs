use crate::native_sidebar::{
    GuiEvent, InitialValues, NativeSidebar, ReloadedSettings, SettingChange,
};
use anyhow::{Context, Result};
use majsoul_max_rs::{
    LiqiUpdateStatus, LiveModPatch, SaveErrorHandler, Settings, UpdateCheckMode,
    UpdateCheckSchedule, read_settings_file, write_json_setting,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tao::{
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::{run_return::EventLoopExtRunReturn, windows::WindowExtWindows},
    window::{Window, WindowBuilder},
};
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::HWND;
use wry::{Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows};

pub enum ProxyCommand {
    Reload {
        response: tokio::sync::oneshot::Sender<std::result::Result<ReloadedSettings, String>>,
        save_error_handler: SaveErrorHandler,
    },
    ApplyModPatch(LiveModPatch),
    Shutdown,
}

const GAME_URL: &str = "https://game.maj-soul.com/1/";
const GUI_STATE_FILE: &str = "gui-state.json";
const COLLAPSED_SIDEBAR_WIDTH: f64 = 48.0;
const MIN_SIDEBAR_WIDTH: f64 = 240.0;
const MAX_SIDEBAR_WIDTH: f64 = 520.0;
const MIN_GAME_WIDTH: f64 = 360.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GuiState {
    x: Option<i32>,
    y: Option<i32>,
    width: f64,
    height: f64,
    maximized: bool,
    sidebar_width: f64,
    sidebar_collapsed: bool,
    /// Unix seconds of the last update check, shared by every check mode.
    last_update_check: Option<u64>,
}

impl Default for GuiState {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            width: 1280.0,
            height: 800.0,
            maximized: false,
            sidebar_width: 300.0,
            sidebar_collapsed: false,
            last_update_check: None,
        }
    }
}

impl GuiState {
    fn normalize(mut self) -> Self {
        let defaults = Self::default();
        if !self.width.is_finite() {
            self.width = defaults.width;
        }
        if !self.height.is_finite() {
            self.height = defaults.height;
        }
        if !self.sidebar_width.is_finite() {
            self.sidebar_width = defaults.sidebar_width;
        }
        self.width = self.width.clamp(720.0, 8192.0);
        self.height = self.height.clamp(480.0, 8192.0);
        self.sidebar_width = self
            .sidebar_width
            .clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
        self
    }

    fn last_update_check_time(&self) -> Option<SystemTime> {
        self.last_update_check
            .and_then(|seconds| UNIX_EPOCH.checked_add(std::time::Duration::from_secs(seconds)))
    }
}

pub fn run(
    proxy_addr: &str,
    settings: Arc<Settings>,
    proxy_commands: UnboundedSender<ProxyCommand>,
) -> Result<()> {
    let config_dir = settings.data_dir().to_path_buf();
    let state_path = config_dir.join(GUI_STATE_FILE);
    let gui_state = load_gui_state(&state_path);
    let initial = load_initial_values(settings.as_ref());

    let mut event_loop = EventLoopBuilder::<GuiEvent>::with_user_event().build();
    let mut builder = WindowBuilder::new()
        .with_title("MajsoulMax")
        .with_visible(false)
        .with_inner_size(LogicalSize::new(gui_state.width, gui_state.height))
        .with_min_inner_size(LogicalSize::new(720.0, 480.0))
        .with_maximized(gui_state.maximized);
    if let (Some(x), Some(y)) = (gui_state.x, gui_state.y) {
        builder = builder.with_position(PhysicalPosition::new(x, y));
    }
    let window = builder
        .build(&event_loop)
        .context("Failed to create application window")?;

    let event_proxy = event_loop.create_proxy();
    let sidebar = NativeSidebar::new(
        window.hwnd() as HWND,
        event_proxy.clone(),
        &initial,
        window.scale_factor(),
    )?;
    let scroll_position = apply_layout(&window, &sidebar, None, &gui_state, 0)?;
    window.set_visible(true);

    let update_schedule = UpdateCheckSchedule::new(
        settings.update_check_mode(),
        settings.update_interval_minutes(),
        gui_state.last_update_check_time(),
        Instant::now(),
    );
    let mut app = App {
        game_webview: None,
        sidebar,
        window,
        event_proxy,
        runtime: tokio::runtime::Handle::current(),
        proxy_commands,
        proxy_addr: proxy_addr.to_owned(),
        config_dir,
        state_path,
        gui_state,
        settings,
        scroll_position,
        checking_update: false,
        reloading_proxy: false,
        drag_origin: None,
        last_update_status: None,
        update_schedule,
    };
    app.start();
    event_loop.run_return(move |event, _, control_flow| app.handle_event(event, control_flow));
    Ok(())
}

/// Everything the event loop owns. Handlers run on the UI thread, one event at
/// a time, so plain fields are enough.
struct App {
    // Fields drop in declaration order: release the WebView2 child and the
    // sidebar controls before the window that hosts them.
    game_webview: Option<WebView>,
    sidebar: NativeSidebar,
    window: Window,
    event_proxy: EventLoopProxy<GuiEvent>,
    runtime: tokio::runtime::Handle,
    proxy_commands: UnboundedSender<ProxyCommand>,
    proxy_addr: String,
    config_dir: PathBuf,
    state_path: PathBuf,
    gui_state: GuiState,
    settings: Arc<Settings>,
    scroll_position: i32,
    checking_update: bool,
    reloading_proxy: bool,
    drag_origin: Option<(i32, f64)>,
    last_update_status: Option<LiqiUpdateStatus>,
    update_schedule: UpdateCheckSchedule,
}

impl App {
    /// Startup always checks once; periodic checks when the recorded last check
    /// is already at least the configured interval old, which also covers time
    /// the program spent closed. The proxy starts after the check finishes.
    fn start(&mut self) {
        let check_on_launch = self.settings.update_check_mode() == UpdateCheckMode::Startup
            || self.update_schedule.is_due(SystemTime::now());
        if check_on_launch {
            self.start_update_check(true);
        } else {
            self.start_proxy_reload(true);
        }
    }

    fn busy(&self) -> bool {
        self.checking_update || self.reloading_proxy
    }

    fn handle_event(&mut self, event: Event<'_, GuiEvent>, control_flow: &mut ControlFlow) {
        match event {
            Event::WindowEvent {
                window_id, event, ..
            } if window_id == self.window.id() => self.on_window_event(event, control_flow),
            Event::UserEvent(event) => self.on_user_event(event),
            Event::MainEventsCleared => self.run_scheduled_check(),
            _ => {}
        }
        if !matches!(*control_flow, ControlFlow::Exit) {
            self.reconfigure_schedule();
            *control_flow = match self.update_schedule.next_check() {
                // While busy, the completion event wakes the loop. Waiting on an
                // already-passed deadline instead would spin the CPU.
                Some(deadline) if !self.busy() => ControlFlow::WaitUntil(deadline),
                _ => ControlFlow::Wait,
            };
        }
    }

    fn on_window_event(&mut self, event: WindowEvent<'_>, control_flow: &mut ControlFlow) {
        match event {
            WindowEvent::CloseRequested => self.close(control_flow),
            WindowEvent::Resized(_) => {
                capture_window_state(&self.window, &mut self.gui_state);
                if let Err(error) = self.relayout() {
                    self.sidebar.set_message(&format!("界面布局失败：{error}"));
                }
            }
            WindowEvent::Moved(_) => capture_window_state(&self.window, &mut self.gui_state),
            _ => {}
        }
    }

    fn on_user_event(&mut self, event: GuiEvent) {
        match event {
            GuiEvent::SettingsChanged => match self.persist_pending_changes() {
                Ok(Some(immediate)) => self.sidebar.set_message(if immediate {
                    "已保存，已立即生效。"
                } else {
                    "已保存；点击重新加载以应用。"
                }),
                Ok(None) => {}
                Err(error) => self.sidebar.set_message(&format!("保存失败：{error}")),
            },
            GuiEvent::SaveFailed(message) => self.sidebar.set_message(&message),
            GuiEvent::CheckUpdate => {
                if !self.busy() && self.save_before_action() {
                    self.start_update_check(false);
                }
            }
            GuiEvent::UpdateProgress(phase) => self.sidebar.set_update_phase(Some(phase)),
            GuiEvent::LatestVersion(status) => {
                self.checking_update = false;
                self.sidebar.set_latest_version(&status);
                self.last_update_status = Some(status);
            }
            GuiEvent::StartupUpdateFinished(status) => {
                self.checking_update = false;
                self.last_update_status = Some(status);
                if self.save_before_action() {
                    self.start_proxy_reload(true);
                } else {
                    self.sidebar.set_checking(false);
                }
            }
            GuiEvent::Restart => {
                if !self.busy() && self.save_before_action() {
                    self.restart();
                }
            }
            GuiEvent::ProxyReloaded(result) => self.on_proxy_reloaded(result),
            GuiEvent::ToggleSidebar => self.toggle_sidebar(),
            GuiEvent::SidebarDragStart(screen_x) => {
                if !self.gui_state.sidebar_collapsed {
                    self.sidebar.set_dragging(true);
                    self.drag_origin = Some((screen_x, self.gui_state.sidebar_width));
                }
            }
            GuiEvent::SidebarDrag(screen_x) => self.drag_sidebar(screen_x),
            GuiEvent::SidebarDragEnd => {
                self.sidebar.set_dragging(false);
                if self.drag_origin.take().is_some()
                    && let Err(error) = save_gui_state(&self.state_path, &self.gui_state)
                {
                    self.sidebar
                        .set_message(&format!("无法保存侧栏宽度：{error}"));
                }
            }
            GuiEvent::ScrollTo(position) => self.scroll_position = self.sidebar.scroll_to(position),
        }
    }

    fn close(&mut self, control_flow: &mut ControlFlow) {
        if let Err(error) = self.persist_pending_changes() {
            let message = format!("设置保存失败，已取消关闭：{error}");
            self.sidebar.show_save_error(&message);
            return;
        }
        if let Err(error) = self.save_window_state() {
            let message = format!("窗口状态保存失败：{error}");
            self.sidebar.show_save_error(&message);
        }
        let _ = self.proxy_commands.send(ProxyCommand::Shutdown);
        *control_flow = ControlFlow::Exit;
    }

    fn restart(&mut self) {
        if let Err(error) = self.save_window_state() {
            self.sidebar
                .set_message(&format!("无法保存窗口状态：{error}"));
            return;
        }
        self.start_proxy_reload(false);
    }

    fn run_scheduled_check(&mut self) {
        let now = Instant::now();
        if self.busy() || !self.update_schedule.take_due(now, SystemTime::now()) {
            return;
        }
        // A pending edit may have disabled or rescheduled this check before its
        // debounce event was delivered.
        if self.save_before_action() && !self.reconfigure_schedule() {
            self.start_update_check(false);
        }
    }

    /// Reschedule from the current settings; returns whether anything changed.
    fn reconfigure_schedule(&mut self) -> bool {
        self.update_schedule.configure(
            self.settings.update_check_mode(),
            self.settings.update_interval_minutes(),
            self.gui_state.last_update_check_time(),
            Instant::now(),
        )
    }

    fn start_update_check(&mut self, startup: bool) {
        self.checking_update = true;
        self.sidebar.set_checking(true);
        let settings = Arc::clone(&self.settings);
        let progress_proxy = self.event_proxy.clone();
        let done_proxy = self.event_proxy.clone();
        self.runtime.spawn(async move {
            let status = settings
                .check_and_download_with_progress(|phase| {
                    let _ = progress_proxy.send_event(GuiEvent::UpdateProgress(phase));
                })
                .await;
            let event = if startup {
                GuiEvent::StartupUpdateFinished(status)
            } else {
                GuiEvent::LatestVersion(status)
            };
            let _ = done_proxy.send_event(event);
        });
        // Without a record, an overdue periodic schedule stays due and runs
        // another check as soon as this one finishes.
        record_update_check(&self.state_path, &mut self.gui_state);
    }

    fn start_proxy_reload(&mut self, starting: bool) {
        let (response, receiver) = tokio::sync::oneshot::channel();
        let save_error_proxy = std::sync::Mutex::new(self.event_proxy.clone());
        let save_error_handler = Box::new(move |message| {
            let _ = save_error_proxy
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .send_event(GuiEvent::SaveFailed(message));
        });
        let command = ProxyCommand::Reload {
            response,
            save_error_handler,
        };
        if self.proxy_commands.send(command).is_err() {
            self.sidebar.set_message("代理管理器已停止，无法重新加载。");
            return;
        }
        self.reloading_proxy = true;
        self.sidebar.set_reloading(true);
        self.sidebar.set_proxy_status(if starting {
            "● 正在启动代理…"
        } else {
            "● 正在重新加载代理…"
        });
        let event_proxy = self.event_proxy.clone();
        self.runtime.spawn(async move {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err("代理管理器未返回重新加载结果".to_owned()));
            let _ = event_proxy.send_event(GuiEvent::ProxyReloaded(result));
        });
    }

    fn on_proxy_reloaded(&mut self, result: std::result::Result<ReloadedSettings, String>) {
        self.reloading_proxy = false;
        self.sidebar.set_reloading(false);
        let reloaded = match result {
            Ok(reloaded) => reloaded,
            Err(error) => {
                if let Some(status) = self.last_update_status.as_ref() {
                    self.sidebar.set_latest_version(status);
                }
                self.sidebar.set_proxy_reload_failed();
                self.sidebar.set_message(&format!("重新加载失败：{error}"));
                return;
            }
        };

        let values = load_initial_values(reloaded.settings.as_ref());
        self.sidebar.apply_values(&values);
        if let Some(status) = self.last_update_status.take() {
            let status = status.resolved_after_reload(&values.liqi_version);
            self.sidebar.set_latest_version(&status);
            self.last_update_status = Some(status);
        }
        self.settings = reloaded.settings;
        self.sidebar.set_proxy_status("● 本地代理运行中");

        let first_start = self.game_webview.is_none();
        if first_start {
            match create_game_webview(&self.window, &self.proxy_addr) {
                Ok(webview) => {
                    self.game_webview = Some(webview);
                    self.sidebar.set_message("");
                }
                Err(error) => self.sidebar.set_message(&format!("网页启动失败：{error}")),
            }
        }
        let layout = self.relayout();
        if !first_start && let Some(webview) = self.game_webview.as_ref() {
            match webview.evaluate_script("window.location.reload()") {
                Ok(()) => self.sidebar.set_message("配置已重新加载。"),
                Err(error) => {
                    let message = format!("代理已重载，但网页刷新失败：{error}");
                    self.sidebar.set_message(&message);
                }
            }
        }
        if let Err(error) = layout {
            self.sidebar.set_message(&format!("界面布局失败：{error}"));
        }
    }

    fn toggle_sidebar(&mut self) {
        self.gui_state.sidebar_collapsed = !self.gui_state.sidebar_collapsed;
        self.scroll_position = 0;
        if let Err(error) = self.relayout() {
            self.sidebar.set_message(&format!("调整侧栏失败：{error}"));
        }
        if let Err(error) = save_gui_state(&self.state_path, &self.gui_state) {
            self.sidebar
                .set_message(&format!("无法保存侧栏状态：{error}"));
        }
    }

    fn drag_sidebar(&mut self, screen_x: i32) {
        let Some((origin_x, origin_width)) = self.drag_origin else {
            return;
        };
        let scale_factor = self.window.scale_factor();
        let logical_delta = (screen_x - origin_x) as f64 / scale_factor;
        let window_width = self
            .window
            .inner_size()
            .to_logical::<f64>(scale_factor)
            .width;
        self.gui_state.sidebar_width = (origin_width + logical_delta)
            .clamp(MIN_SIDEBAR_WIDTH, max_sidebar_width(window_width));
        // Dragging produces a stream of updates; a failed frame is simply
        // replaced by the next one.
        let _ = self.relayout();
    }

    fn relayout(&mut self) -> Result<()> {
        self.scroll_position = apply_layout(
            &self.window,
            &self.sidebar,
            self.game_webview.as_ref(),
            &self.gui_state,
            self.scroll_position,
        )?;
        Ok(())
    }

    fn save_window_state(&mut self) -> Result<()> {
        capture_window_state(&self.window, &mut self.gui_state);
        save_gui_state(&self.state_path, &self.gui_state)
    }

    /// Save sidebar edits before an action that reads the settings files.
    fn save_before_action(&mut self) -> bool {
        match self.persist_pending_changes() {
            Ok(_) => true,
            Err(error) => {
                self.sidebar.set_message(&format!("无法保存设置：{error}"));
                false
            }
        }
    }

    /// Returns `Some(true)` when every saved change took effect without a reload.
    fn persist_pending_changes(&mut self) -> Result<Option<bool>> {
        let mut immediate = None;
        for change in self.sidebar.pending_changes()? {
            let applied = persist_setting_change(
                &self.config_dir,
                &mut self.settings,
                &self.proxy_commands,
                change.clone(),
            )?;
            self.sidebar.mark_saved(&change);
            immediate = Some(immediate.unwrap_or(true) && applied);
        }
        Ok(immediate)
    }
}

fn apply_layout(
    window: &Window,
    sidebar: &NativeSidebar,
    game_webview: Option<&WebView>,
    state: &GuiState,
    scroll_position: i32,
) -> Result<i32> {
    let inner_size = window.inner_size();
    let scale_factor = window.scale_factor();
    let logical_width = inner_size.to_logical::<f64>(scale_factor).width;
    let sidebar_logical_width = if state.sidebar_collapsed {
        COLLAPSED_SIDEBAR_WIDTH
    } else {
        state
            .sidebar_width
            .clamp(MIN_SIDEBAR_WIDTH, max_sidebar_width(logical_width))
    };
    let sidebar_width = ((sidebar_logical_width * scale_factor).round() as u32)
        .min(inner_size.width.saturating_sub(1));
    if let Some(game_webview) = game_webview {
        let overlap = sidebar_width.min(2);
        let game_x = sidebar_width.saturating_sub(overlap);
        game_webview
            .set_bounds(Rect {
                position: PhysicalPosition::new(game_x as i32, 0).into(),
                size: PhysicalSize::new(
                    inner_size.width.saturating_sub(game_x).max(1),
                    inner_size.height.max(1),
                )
                .into(),
            })
            .context("Failed to resize game WebView2")?;
    }
    Ok(sidebar.layout(
        sidebar_width as i32,
        inner_size.height as i32,
        scale_factor,
        state.sidebar_collapsed,
        scroll_position,
    ))
}

fn create_game_webview(window: &Window, proxy_addr: &str) -> Result<WebView> {
    let browser_args = format!("--proxy-server=http://{proxy_addr} --disable-quic");
    WebViewBuilder::new(window)
        .with_additional_browser_args(&browser_args)
        .with_url(GAME_URL)
        .build()
        .context("Failed to build game WebView2")
}

fn max_sidebar_width(window_width: f64) -> f64 {
    MAX_SIDEBAR_WIDTH.min((window_width - MIN_GAME_WIDTH).max(MIN_SIDEBAR_WIDTH))
}

fn load_initial_values(settings: &Settings) -> InitialValues {
    let mod_json = read_settings_file(&settings.data_dir().join("settings.mod.json"))
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
        .unwrap_or(Value::Null);
    InitialValues {
        mod_switch: settings.mod_on(),
        update_check_mode: settings.update_check_mode(),
        update_interval_minutes: settings.update_interval_minutes(),
        nickname: mod_json["nickname"].as_str().unwrap_or_default().to_owned(),
        show_server: mod_json["showServer"].as_bool().unwrap_or(true),
        anti_nickname_censorship: mod_json["antiNicknameCensorship"].as_bool().unwrap_or(true),
        emoji_switch: mod_json["emojiSwitch"].as_bool().unwrap_or(false),
        hint_switch: mod_json["hintSwitch"].as_bool().unwrap_or(true),
        req_proxy: settings.req_proxy().to_owned(),
        github_prefix: settings.github_prefix().to_owned(),
        liqi_version: settings.liqi_version().to_owned(),
    }
}

fn persist_setting_change(
    config_dir: &Path,
    current_settings: &mut Arc<Settings>,
    proxy_commands: &UnboundedSender<ProxyCommand>,
    change: SettingChange,
) -> Result<bool> {
    write_setting(config_dir, change.clone())?;
    apply_live_setting(current_settings, &change);
    let immediate = setting_applies_immediately(&change, current_settings.mod_on());
    if let Some(patch) = live_mod_patch(change) {
        let _ = proxy_commands.send(ProxyCommand::ApplyModPatch(patch));
    }
    Ok(immediate)
}

fn live_mod_patch(change: SettingChange) -> Option<LiveModPatch> {
    match change {
        SettingChange::Nickname(value) => Some(LiveModPatch::Nickname(value)),
        SettingChange::ShowServer(value) => Some(LiveModPatch::ShowServer(value)),
        SettingChange::AntiNicknameCensorship(value) => {
            Some(LiveModPatch::AntiNicknameCensorship(value))
        }
        SettingChange::EmojiSwitch(value) => Some(LiveModPatch::EmojiSwitch(value)),
        SettingChange::HintSwitch(value) => Some(LiveModPatch::HintSwitch(value)),
        _ => None,
    }
}

fn setting_applies_immediately(change: &SettingChange, mod_on: bool) -> bool {
    match change {
        SettingChange::ReqProxy(_)
        | SettingChange::GithubPrefix(_)
        | SettingChange::UpdateCheckMode(_)
        | SettingChange::UpdateIntervalMinutes(_) => true,
        SettingChange::Nickname(_)
        | SettingChange::ShowServer(_)
        | SettingChange::AntiNicknameCensorship(_)
        | SettingChange::EmojiSwitch(_)
        | SettingChange::HintSwitch(_) => mod_on,
        _ => false,
    }
}

fn apply_live_setting(settings: &mut Arc<Settings>, change: &SettingChange) {
    match change {
        SettingChange::UpdateCheckMode(value) => {
            Arc::make_mut(settings).set_update_check_mode(*value)
        }
        SettingChange::UpdateIntervalMinutes(value) => {
            Arc::make_mut(settings).set_update_interval_minutes(*value)
        }
        SettingChange::ReqProxy(value) => Arc::make_mut(settings).set_req_proxy(value.clone()),
        SettingChange::GithubPrefix(value) => {
            Arc::make_mut(settings).set_github_prefix(value.clone())
        }
        _ => {}
    }
}

fn write_setting(config_dir: &Path, change: SettingChange) -> Result<()> {
    let (file_name, key, value) = match change {
        SettingChange::ModSwitch(value) => ("settings.json", "modSwitch", Value::Bool(value)),
        SettingChange::UpdateCheckMode(value) => {
            ("settings.json", "autoUpdate", serde_json::to_value(value)?)
        }
        SettingChange::UpdateIntervalMinutes(value) => (
            "settings.json",
            "autoUpdateIntervalMinutes",
            Value::from(value),
        ),
        SettingChange::Nickname(value) => ("settings.mod.json", "nickname", Value::String(value)),
        SettingChange::ShowServer(value) => ("settings.mod.json", "showServer", Value::Bool(value)),
        SettingChange::AntiNicknameCensorship(value) => (
            "settings.mod.json",
            "antiNicknameCensorship",
            Value::Bool(value),
        ),
        SettingChange::EmojiSwitch(value) => {
            ("settings.mod.json", "emojiSwitch", Value::Bool(value))
        }
        SettingChange::HintSwitch(value) => ("settings.mod.json", "hintSwitch", Value::Bool(value)),
        SettingChange::ReqProxy(value) => ("settings.json", "reqProxy", Value::String(value)),
        SettingChange::GithubPrefix(value) => {
            ("settings.json", "githubPrefix", Value::String(value))
        }
    };
    write_json_setting(&config_dir.join(file_name), key, value)
}

fn load_gui_state(path: &Path) -> GuiState {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<GuiState>(&content).ok())
        .unwrap_or_default()
        .normalize()
}

fn save_gui_state(path: &Path, state: &GuiState) -> Result<()> {
    let content = serde_json::to_string_pretty(state)?;
    fs::write(path, format!("{content}\n"))
        .with_context(|| format!("无法写入窗口状态 {}", path.display()))
}

/// Remember that an update check started, so later launches and the hourly
/// reconsideration can tell whether the configured interval has elapsed.
fn record_update_check(path: &Path, state: &mut GuiState) {
    state.last_update_check = Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0),
    );
    if let Err(error) = save_gui_state(path, state) {
        eprintln!("无法记录更新检查时间：{error:#}");
    }
}

fn capture_window_state(window: &Window, state: &mut GuiState) {
    state.maximized = window.is_maximized();
    if state.maximized {
        return;
    }
    let logical_size = window.inner_size().to_logical::<f64>(window.scale_factor());
    if logical_size.width >= 720.0 && logical_size.height >= 480.0 {
        state.width = logical_size.width;
        state.height = logical_size.height;
    }
    if let Ok(position) = window.outer_position()
        && position.x > -30_000
        && position.y > -30_000
    {
        state.x = Some(position.x);
        state.y = Some(position.y);
    }
}
