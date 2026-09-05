mod app;
mod host;
mod raster;

use anyhow::Result;
use majsoul_max_rs::{LiqiUpdatePhase, LiqiUpdateStatus, Settings};
use std::sync::Arc;
use tao::event_loop::EventLoopProxy;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

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

pub struct Sidebar {
    hwnd: HWND,
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

impl Sidebar {
    pub fn new(
        parent: HWND,
        proxy: EventLoopProxy<GuiEvent>,
        initial: &InitialValues,
        scale_factor: f64,
    ) -> Result<Self> {
        let hwnd = host::attach(parent, proxy, initial, scale_factor)?;
        Ok(Self { hwnd })
    }

    pub fn take_pending_changes(&self) -> Vec<SettingChange> {
        host::with_host(self.hwnd, |host| host.app.take_pending_changes()).unwrap_or_default()
    }

    pub fn apply_values(&self, values: &InitialValues) {
        host::with_host(self.hwnd, |host| host.app.apply_values(values));
        host::request_repaint(self.hwnd);
    }

    pub fn layout(&self, width: i32, height: i32, scale_factor: f64, collapsed: bool) {
        host::with_host(self.hwnd, |host| {
            host.app.collapsed = collapsed;
            host.pixels_per_point = scale_factor.max(0.5) as f32;
        });
        host::resize(self.hwnd, width, height);
        host::request_repaint(self.hwnd);
    }

    pub fn set_message(&self, message: &str) {
        host::with_host(self.hwnd, |host| host.app.set_message(message));
        host::request_repaint(self.hwnd);
    }

    pub fn set_checking(&self, checking: bool) {
        self.set_update_phase(checking.then_some(LiqiUpdatePhase::Checking));
    }

    pub fn set_update_phase(&self, phase: Option<LiqiUpdatePhase>) {
        host::with_host(self.hwnd, |host| host.app.set_update_phase(phase));
        host::request_repaint(self.hwnd);
    }

    pub fn set_reloading(&self, reloading: bool) {
        host::with_host(self.hwnd, |host| host.app.set_reloading(reloading));
        host::request_repaint(self.hwnd);
    }

    pub fn set_proxy_status(&self, message: &str) {
        host::with_host(self.hwnd, |host| host.app.set_proxy_status(message));
        host::request_repaint(self.hwnd);
    }

    pub fn set_proxy_reload_failed(&self) {
        self.set_proxy_status("● 代理重新加载失败");
    }

    pub fn set_latest_version(&self, status: &LiqiUpdateStatus) {
        host::with_host(self.hwnd, |host| host.app.set_latest_version(status));
        host::request_repaint(self.hwnd);
    }
}

impl Drop for Sidebar {
    fn drop(&mut self) {
        host::destroy(self.hwnd);
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
