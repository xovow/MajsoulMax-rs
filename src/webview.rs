use crate::sidebar::{
    GuiEvent, InitialValues, ReloadedSettings, SettingChange, Sidebar,
};
use anyhow::{Context, Result};
use majsoul_max_rs::{LiqiUpdatePhase, LiveModPatch, Settings};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, path::Path, sync::Arc};
use tao::{
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::{run_return::EventLoopExtRunReturn, windows::WindowExtWindows},
    window::{Window, WindowBuilder},
};
use windows_sys::Win32::Foundation::HWND;
use wry::{Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows};

pub enum ProxyCommand {
    Reload {
        response: tokio::sync::oneshot::Sender<std::result::Result<ReloadedSettings, String>>,
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
}

pub fn run(
    proxy_addr: &str,
    settings: Arc<Settings>,
    proxy_commands: tokio::sync::mpsc::UnboundedSender<ProxyCommand>,
) -> Result<()> {
    let proxy_addr = proxy_addr.to_owned();
    let config_dir = settings.data_dir().to_path_buf();
    let state_path = config_dir.join(GUI_STATE_FILE);
    let mut gui_state = load_gui_state(&state_path);
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
    let sidebar = Sidebar::new(
        window.hwnd() as HWND,
        event_proxy.clone(),
        &initial,
        window.scale_factor(),
    )?;
    let mut game_webview: Option<WebView> = None;
    apply_layout(&window, &sidebar, None, &gui_state)?;
    window.set_visible(true);

    let runtime = tokio::runtime::Handle::current();
    let mut current_liqi_version = initial.liqi_version.clone();
    let mut current_settings = settings;
    let mut checking_update = false;
    let mut reloading_proxy = false;
    let mut drag_origin: Option<(i32, f64)> = None;
    let mut last_update_status = None;
    let auto_update = current_settings.auto_update();
    if auto_update {
        checking_update = true;
        sidebar.set_update_phase(Some(LiqiUpdatePhase::Checking));
        let settings = Arc::clone(&current_settings);
        let progress_proxy = event_proxy.clone();
        let done_proxy = event_proxy.clone();
        runtime.spawn(async move {
            let status = settings
                .check_and_download_with_progress(|phase| {
                    let _ = progress_proxy.send_event(GuiEvent::UpdateProgress(phase));
                })
                .await;
            let _ = done_proxy.send_event(GuiEvent::StartupUpdateFinished(status));
        });
    } else if begin_proxy_reload(&sidebar, &proxy_commands, &event_proxy, &runtime, true) {
        reloading_proxy = true;
    }

    event_loop.run_return(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                window_id, event, ..
            } if window_id == window.id() => match event {
                WindowEvent::CloseRequested => {
                    let _ = persist_pending_changes(
                        &sidebar,
                        &config_dir,
                        &mut current_settings,
                        &proxy_commands,
                    );
                    capture_window_state(&window, &mut gui_state);
                    if let Err(error) = save_gui_state(&state_path, &gui_state) {
                        tracing::warn!("Failed to save GUI state: {error}");
                    }
                    let _ = proxy_commands.send(ProxyCommand::Shutdown);
                    *control_flow = ControlFlow::Exit;
                }
                WindowEvent::Resized(_) => {
                    capture_window_state(&window, &mut gui_state);
                    if let Err(error) =
                        apply_layout(&window, &sidebar, game_webview.as_ref(), &gui_state)
                    {
                        tracing::warn!("Failed to resize GUI: {error}");
                    }
                }
                WindowEvent::Moved(_) => capture_window_state(&window, &mut gui_state),
                _ => {}
            },
            Event::UserEvent(GuiEvent::SettingChanged(change)) => {
                match persist_setting_change(
                    &config_dir,
                    &mut current_settings,
                    &proxy_commands,
                    change,
                ) {
                    Ok(immediate) => sidebar.set_message(if immediate {
                        "已保存，已立即生效。"
                    } else {
                        "已保存；点击重新加载以应用。"
                    }),
                    Err(error) => sidebar.set_message(&format!("保存失败：{error}")),
                }
            }
            Event::UserEvent(GuiEvent::CheckUpdate) if !checking_update && !reloading_proxy => {
                if let Err(error) = persist_pending_changes(
                    &sidebar,
                    &config_dir,
                    &mut current_settings,
                    &proxy_commands,
                ) {
                    sidebar.set_message(&format!("无法保存设置：{error}"));
                    return;
                }
                checking_update = true;
                sidebar.set_checking(true);
                let settings = Arc::clone(&current_settings);
                let progress_proxy = event_proxy.clone();
                let done_proxy = event_proxy.clone();
                runtime.spawn(async move {
                    let status = settings
                        .check_and_download_with_progress(|phase| {
                            let _ = progress_proxy.send_event(GuiEvent::UpdateProgress(phase));
                        })
                        .await;
                    let _ = done_proxy.send_event(GuiEvent::LatestVersion(status));
                });
            }
            Event::UserEvent(GuiEvent::UpdateProgress(phase)) => {
                sidebar.set_update_phase(Some(phase));
            }
            Event::UserEvent(GuiEvent::LatestVersion(status)) => {
                checking_update = false;
                last_update_status = Some(status.clone());
                sidebar.set_latest_version(&status);
            }
            Event::UserEvent(GuiEvent::StartupUpdateFinished(status)) => {
                checking_update = false;
                last_update_status = Some(status);
                let _ = persist_pending_changes(
                    &sidebar,
                    &config_dir,
                    &mut current_settings,
                    &proxy_commands,
                );
                if begin_proxy_reload(&sidebar, &proxy_commands, &event_proxy, &runtime, true) {
                    reloading_proxy = true;
                }
            }
            Event::UserEvent(GuiEvent::Restart) if !reloading_proxy => {
                if let Err(error) = persist_pending_changes(
                    &sidebar,
                    &config_dir,
                    &mut current_settings,
                    &proxy_commands,
                ) {
                    sidebar.set_message(&format!("无法保存设置：{error}"));
                    return;
                }
                capture_window_state(&window, &mut gui_state);
                if let Err(error) = save_gui_state(&state_path, &gui_state) {
                    sidebar.set_message(&format!("无法保存窗口状态：{error}"));
                    return;
                }
                if begin_proxy_reload(&sidebar, &proxy_commands, &event_proxy, &runtime, false) {
                    reloading_proxy = true;
                }
            }
            Event::UserEvent(GuiEvent::ProxyReloaded(result)) => {
                reloading_proxy = false;
                sidebar.set_reloading(false);
                match result {
                    Ok(reloaded) => {
                        let values = load_initial_values(reloaded.settings.as_ref());
                        current_liqi_version.clone_from(&values.liqi_version);
                        sidebar.apply_values(&values);
                        if let Some(status) = last_update_status.take() {
                            let status = status.resolved_after_reload(&current_liqi_version);
                            sidebar.set_latest_version(&status);
                            last_update_status = Some(status);
                        }
                        current_settings = reloaded.settings;
                        sidebar.set_proxy_status("● 本地代理运行中");
                        let first_start = game_webview.is_none();
                        if first_start {
                            match create_game_webview(&window, &proxy_addr) {
                                Ok(webview) => {
                                    game_webview = Some(webview);
                                    sidebar.set_message("");
                                }
                                Err(error) => {
                                    sidebar.set_message(&format!("网页启动失败：{error}"));
                                }
                            }
                        }
                        if let Err(error) =
                            apply_layout(&window, &sidebar, game_webview.as_ref(), &gui_state)
                        {
                            tracing::warn!("Failed to layout GUI: {error}");
                        }
                        if !first_start {
                            if let Some(webview) = game_webview.as_ref() {
                                match webview.evaluate_script("window.location.reload()") {
                                    Ok(()) => sidebar.set_message("配置已重新加载。"),
                                    Err(error) => sidebar.set_message(&format!(
                                        "代理已重载，但网页刷新失败：{error}"
                                    )),
                                }
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(status) = last_update_status.as_ref() {
                            sidebar.set_latest_version(status);
                        }
                        sidebar.set_proxy_reload_failed();
                        sidebar.set_message(&format!("重新加载失败：{error}"));
                    }
                }
            }
            Event::UserEvent(GuiEvent::ToggleSidebar) => {
                gui_state.sidebar_collapsed = !gui_state.sidebar_collapsed;
                if let Err(error) =
                    apply_layout(&window, &sidebar, game_webview.as_ref(), &gui_state)
                {
                    sidebar.set_message(&format!("调整侧栏失败：{error}"));
                }
                if let Err(error) = save_gui_state(&state_path, &gui_state) {
                    sidebar.set_message(&format!("无法保存侧栏状态：{error}"));
                }
            }
            Event::UserEvent(GuiEvent::SidebarDragStart(screen_x)) => {
                if !gui_state.sidebar_collapsed {
                    drag_origin = Some((screen_x, gui_state.sidebar_width));
                }
            }
            Event::UserEvent(GuiEvent::SidebarDrag(screen_x)) => {
                if let Some((origin_x, origin_width)) = drag_origin {
                    let logical_delta = (screen_x - origin_x) as f64 / window.scale_factor();
                    let window_width = window
                        .inner_size()
                        .to_logical::<f64>(window.scale_factor())
                        .width;
                    gui_state.sidebar_width = (origin_width + logical_delta)
                        .clamp(MIN_SIDEBAR_WIDTH, max_sidebar_width(window_width));
                    let _ = apply_layout(&window, &sidebar, game_webview.as_ref(), &gui_state);
                }
            }
            Event::UserEvent(GuiEvent::SidebarDragEnd) => {
                if drag_origin.take().is_some()
                    && let Err(error) = save_gui_state(&state_path, &gui_state)
                {
                    sidebar.set_message(&format!("无法保存侧栏宽度：{error}"));
                }
            }
            _ => {}
        }
    });
    Ok(())
}

fn apply_layout(
    window: &Window,
    sidebar: &Sidebar,
    game_webview: Option<&WebView>,
    state: &GuiState,
) -> Result<()> {
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
    sidebar.layout(
        sidebar_width as i32,
        inner_size.height as i32,
        scale_factor,
        state.sidebar_collapsed,
    );
    Ok(())
}

fn create_game_webview(window: &Window, proxy_addr: &str) -> Result<WebView> {
    let browser_args = format!("--proxy-server=http://{proxy_addr} --disable-quic");
    WebViewBuilder::new(window)
        .with_additional_browser_args(&browser_args)
        .with_url(GAME_URL)
        .build()
        .context("Failed to build game WebView2")
}

fn begin_proxy_reload(
    sidebar: &Sidebar,
    proxy_commands: &tokio::sync::mpsc::UnboundedSender<ProxyCommand>,
    event_proxy: &EventLoopProxy<GuiEvent>,
    runtime: &tokio::runtime::Handle,
    starting: bool,
) -> bool {
    let (response, receiver) = tokio::sync::oneshot::channel();
    if proxy_commands
        .send(ProxyCommand::Reload { response })
        .is_err()
    {
        sidebar.set_message("代理管理器已停止，无法重新加载。");
        return false;
    }
    sidebar.set_reloading(true);
    sidebar.set_proxy_status(if starting {
        "● 正在启动代理…"
    } else {
        "● 正在重新加载代理…"
    });
    let event_proxy = event_proxy.clone();
    runtime.spawn(async move {
        let result = receiver
            .await
            .unwrap_or_else(|_| Err("代理管理器未返回重新加载结果".to_owned()));
        let _ = event_proxy.send_event(GuiEvent::ProxyReloaded(result));
    });
    true
}

fn max_sidebar_width(window_width: f64) -> f64 {
    MAX_SIDEBAR_WIDTH.min((window_width - MIN_GAME_WIDTH).max(MIN_SIDEBAR_WIDTH))
}

fn load_initial_values(settings: &Settings) -> InitialValues {
    let mod_json = fs::read_to_string(settings.data_dir().join("settings.mod.json"))
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
        .unwrap_or(Value::Null);
    InitialValues {
        mod_switch: settings.mod_on(),
        auto_update: settings.auto_update(),
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

fn persist_pending_changes(
    sidebar: &Sidebar,
    config_dir: &Path,
    current_settings: &mut Arc<Settings>,
    proxy_commands: &tokio::sync::mpsc::UnboundedSender<ProxyCommand>,
) -> Result<()> {
    for change in sidebar.take_pending_changes() {
        persist_setting_change(config_dir, current_settings, proxy_commands, change)?;
    }
    Ok(())
}

fn persist_setting_change(
    config_dir: &Path,
    current_settings: &mut Arc<Settings>,
    proxy_commands: &tokio::sync::mpsc::UnboundedSender<ProxyCommand>,
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
        SettingChange::ReqProxy(_) | SettingChange::GithubPrefix(_) => true,
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
        SettingChange::AutoUpdate(value) => ("settings.json", "autoUpdate", Value::Bool(value)),
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
    let path = config_dir.join(file_name);
    let content =
        fs::read_to_string(&path).with_context(|| format!("无法读取 {}", path.display()))?;
    let mut document: Value =
        serde_json::from_str(&content).with_context(|| format!("无法解析 {}", path.display()))?;
    let object = document
        .as_object_mut()
        .with_context(|| format!("{} 的根节点不是 JSON 对象", path.display()))?;
    if object.get(key) == Some(&value) {
        return Ok(());
    }
    object.insert(key.to_owned(), value);
    let content = serde_json::to_string_pretty(&document)?;
    fs::write(&path, format!("{content}\n")).with_context(|| format!("无法写入 {}", path.display()))
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
