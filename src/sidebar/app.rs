use crate::sidebar::{GuiEvent, InitialValues, SettingChange};
use egui::{
    Align, Color32, CornerRadius, CursorIcon, FontId, Layout, RichText, Sense, TextEdit, Ui, Vec2,
    pos2, vec2,
};
use majsoul_max_rs::{LiqiUpdatePhase, LiqiUpdateStatus};
use std::time::{Duration, Instant};
use tao::event_loop::EventLoopProxy;

const TEXT_DEBOUNCE: Duration = Duration::from_millis(450);
const BLUE: Color32 = Color32::from_rgb(46, 125, 246);
const BLUE_PRESSED: Color32 = Color32::from_rgb(29, 92, 190);
const TEXT: Color32 = Color32::from_rgb(35, 39, 47);
const MUTED: Color32 = Color32::from_rgb(135, 140, 150);
const TRACK_OFF: Color32 = Color32::from_rgb(183, 188, 197);
const TRACK_DISABLED: Color32 = Color32::from_rgb(213, 216, 222);
const SECONDARY: Color32 = Color32::from_rgb(239, 242, 247);
const SECONDARY_PRESSED: Color32 = Color32::from_rgb(218, 223, 232);
const DANGER: Color32 = Color32::from_rgb(192, 57, 43);
const OK_GREEN: Color32 = Color32::from_rgb(39, 174, 96);
const WARN: Color32 = Color32::from_rgb(214, 137, 16);
const SECTION_FILL: Color32 = Color32::from_rgb(245, 247, 251);
const ACCENT: Color32 = Color32::from_rgb(74, 132, 232);
const CONTENT_PADDING: f32 = 14.0;
const SECTION_HEIGHT: f32 = 30.0;
const ROW_HEIGHT: f32 = 30.0;
const FIELD_HEIGHT: f32 = 30.0;
const BUTTON_HEIGHT: f32 = 36.0;
const SWITCH_WIDTH: f32 = 40.0;
const SWITCH_HEIGHT: f32 = 22.0;

pub struct SidebarApp {
    pub collapsed: bool,
    update_phase: Option<LiqiUpdatePhase>,
    reloading: bool,
    mod_switch: bool,
    auto_update: bool,
    nickname: String,
    show_server: bool,
    anti_nickname_censorship: bool,
    emoji_switch: bool,
    hint_switch: bool,
    req_proxy: String,
    github_prefix: String,
    liqi_version: String,
    latest_label: String,
    proxy_status: String,
    message: String,
    sent_nickname: String,
    sent_req_proxy: String,
    sent_github_prefix: String,
    nickname_dirty_at: Option<Instant>,
    req_proxy_dirty_at: Option<Instant>,
    github_prefix_dirty_at: Option<Instant>,
}

impl SidebarApp {
    pub fn from_initial(initial: &InitialValues) -> Self {
        Self {
            collapsed: false,
            update_phase: None,
            reloading: false,
            mod_switch: initial.mod_switch,
            auto_update: initial.auto_update,
            nickname: initial.nickname.clone(),
            show_server: initial.show_server,
            anti_nickname_censorship: initial.anti_nickname_censorship,
            emoji_switch: initial.emoji_switch,
            hint_switch: initial.hint_switch,
            req_proxy: initial.req_proxy.clone(),
            github_prefix: initial.github_prefix.clone(),
            liqi_version: initial.liqi_version.clone(),
            latest_label: "最新：尚未检查".to_owned(),
            proxy_status: "● 正在启动…".to_owned(),
            message: String::new(),
            sent_nickname: initial.nickname.clone(),
            sent_req_proxy: initial.req_proxy.clone(),
            sent_github_prefix: initial.github_prefix.clone(),
            nickname_dirty_at: None,
            req_proxy_dirty_at: None,
            github_prefix_dirty_at: None,
        }
    }

    pub fn apply_values(&mut self, values: &InitialValues) {
        self.mod_switch = values.mod_switch;
        self.auto_update = values.auto_update;
        self.nickname.clone_from(&values.nickname);
        self.show_server = values.show_server;
        self.anti_nickname_censorship = values.anti_nickname_censorship;
        self.emoji_switch = values.emoji_switch;
        self.hint_switch = values.hint_switch;
        self.req_proxy.clone_from(&values.req_proxy);
        self.github_prefix.clone_from(&values.github_prefix);
        self.liqi_version.clone_from(&values.liqi_version);
        self.sent_nickname.clone_from(&values.nickname);
        self.sent_req_proxy.clone_from(&values.req_proxy);
        self.sent_github_prefix.clone_from(&values.github_prefix);
        self.nickname_dirty_at = None;
        self.req_proxy_dirty_at = None;
        self.github_prefix_dirty_at = None;
    }

    pub fn take_pending_changes(&mut self) -> Vec<SettingChange> {
        let mut changes = Vec::new();
        flush_text(
            &self.nickname,
            &mut self.sent_nickname,
            &mut self.nickname_dirty_at,
            SettingChange::Nickname,
            &mut changes,
        );
        flush_text(
            &self.req_proxy,
            &mut self.sent_req_proxy,
            &mut self.req_proxy_dirty_at,
            SettingChange::ReqProxy,
            &mut changes,
        );
        flush_text(
            &self.github_prefix,
            &mut self.sent_github_prefix,
            &mut self.github_prefix_dirty_at,
            SettingChange::GithubPrefix,
            &mut changes,
        );
        changes
    }

    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
    }

    pub fn set_update_phase(&mut self, phase: Option<LiqiUpdatePhase>) {
        self.update_phase = phase;
        match phase {
            Some(LiqiUpdatePhase::Checking) => {
                self.latest_label = "最新：检查更新中".to_owned();
            }
            Some(LiqiUpdatePhase::Downloading) => {
                self.latest_label = "最新：正在更新".to_owned();
            }
            None => {}
        }
    }

    pub fn set_reloading(&mut self, reloading: bool) {
        self.reloading = reloading;
    }

    pub fn set_proxy_status(&mut self, message: impl Into<String>) {
        self.proxy_status = message.into();
    }

    pub fn set_latest_version(&mut self, status: &LiqiUpdateStatus) {
        self.update_phase = None;
        match status {
            LiqiUpdateStatus::Latest(version) => {
                self.latest_label = format!("最新：{version} （已是最新）");
                self.message.clear();
            }
            LiqiUpdateStatus::Updated(version) => {
                self.latest_label = format!("最新：{version} （已更新，需重启读取）");
                self.message.clear();
            }
            LiqiUpdateStatus::Failed(error) => {
                self.latest_label = "最新：检查失败".to_owned();
                self.message = format!("检查失败：{error}");
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, proxy: &EventLoopProxy<GuiEvent>) {
        let enabled = !self.reloading;
        let update_busy = self.update_phase.is_some();
        let buttons_enabled = enabled && !update_busy;

        if self.collapsed {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                if icon_button(ui, "›", vec2(32.0, 30.0)) {
                    let _ = proxy.send_event(GuiEvent::ToggleSidebar);
                }
            });
            return;
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_space(CONTENT_PADDING);
            ui.label(
                RichText::new("MajsoulMax")
                    .font(FontId::proportional(19.0))
                    .strong()
                    .color(TEXT),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(CONTENT_PADDING);
                if icon_button(ui, "‹", vec2(32.0, 30.0)) {
                    let _ = proxy.send_event(GuiEvent::ToggleSidebar);
                }
            });
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(CONTENT_PADDING);
            status_line(ui, &self.proxy_status);
        });

        egui::ScrollArea::vertical()
            .id_salt("sidebar-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(12.0);
                section_heading(ui, "常规设置");
                toggle_row(ui, "启用 Mod", &mut self.mod_switch, enabled, |value| {
                    emit(proxy, SettingChange::ModSwitch(value));
                });
                toggle_row(
                    ui,
                    "自动更新协议数据",
                    &mut self.auto_update,
                    enabled,
                    |value| emit(proxy, SettingChange::AutoUpdate(value)),
                );
                labeled_field(
                    ui,
                    "GitHub 代理",
                    &mut self.req_proxy,
                    256,
                    enabled,
                    &mut self.req_proxy_dirty_at,
                    &mut self.sent_req_proxy,
                    proxy,
                    SettingChange::ReqProxy,
                );
                labeled_field(
                    ui,
                    "GitHub 前缀",
                    &mut self.github_prefix,
                    256,
                    enabled,
                    &mut self.github_prefix_dirty_at,
                    &mut self.sent_github_prefix,
                    proxy,
                    SettingChange::GithubPrefix,
                );

                ui.add_space(8.0);
                section_heading(ui, "Mod 设置");
                labeled_field(
                    ui,
                    "本地昵称",
                    &mut self.nickname,
                    64,
                    enabled,
                    &mut self.nickname_dirty_at,
                    &mut self.sent_nickname,
                    proxy,
                    SettingChange::Nickname,
                );
                toggle_row(ui, "显示服务器", &mut self.show_server, enabled, |value| {
                    emit(proxy, SettingChange::ShowServer(value));
                });
                toggle_row(
                    ui,
                    "反昵称审查",
                    &mut self.anti_nickname_censorship,
                    enabled,
                    |value| emit(proxy, SettingChange::AntiNicknameCensorship(value)),
                );
                toggle_row(ui, "额外表情", &mut self.emoji_switch, enabled, |value| {
                    emit(proxy, SettingChange::EmojiSwitch(value));
                });
                toggle_row(
                    ui,
                    "王座便捷提示",
                    &mut self.hint_switch,
                    enabled,
                    |value| emit(proxy, SettingChange::HintSwitch(value)),
                );

                ui.add_space(8.0);
                section_heading(ui, "协议数据版本");
                ui.add_space(2.0);
                padded_label(ui, &format!("当前：{}", self.liqi_version), TEXT);
                padded_label(ui, &self.latest_label, TEXT);

                ui.add_space(10.0);
                let check_label = match self.update_phase {
                    Some(LiqiUpdatePhase::Checking) => "正在检查…",
                    Some(LiqiUpdatePhase::Downloading) => "正在更新…",
                    None => "检查更新",
                };
                if action_button_row(ui, check_label, false, buttons_enabled) {
                    let _ = proxy.send_event(GuiEvent::CheckUpdate);
                }

                ui.add_space(10.0);
                padded_label(
                    ui,
                    "GitHub 代理、前缀和已开启的 Mod 项修改后立即生效。mod 开关与自动更新需重新加载。",
                    MUTED,
                );

                ui.add_space(10.0);
                let restart_label = if self.reloading {
                    "正在重新加载…"
                } else {
                    "重新加载配置并刷新网页"
                };
                if action_button_row(ui, restart_label, true, buttons_enabled) {
                    let _ = proxy.send_event(GuiEvent::Restart);
                }

                if !self.message.is_empty() {
                    ui.add_space(10.0);
                    let color = if self.message.contains("失败") {
                        DANGER
                    } else {
                        TEXT
                    };
                    padded_label(ui, &self.message, color);
                }

                ui.add_space(16.0);
                self.flush_due_text(proxy, ui.ctx());
            });
    }

    fn flush_due_text(&mut self, proxy: &EventLoopProxy<GuiEvent>, ctx: &egui::Context) {
        flush_if_due(
            &self.nickname,
            &mut self.sent_nickname,
            &mut self.nickname_dirty_at,
            SettingChange::Nickname,
            proxy,
            ctx,
        );
        flush_if_due(
            &self.req_proxy,
            &mut self.sent_req_proxy,
            &mut self.req_proxy_dirty_at,
            SettingChange::ReqProxy,
            proxy,
            ctx,
        );
        flush_if_due(
            &self.github_prefix,
            &mut self.sent_github_prefix,
            &mut self.github_prefix_dirty_at,
            SettingChange::GithubPrefix,
            proxy,
            ctx,
        );
    }
}

fn emit(proxy: &EventLoopProxy<GuiEvent>, change: SettingChange) {
    let _ = proxy.send_event(GuiEvent::SettingChanged(change));
}

fn flush_text(
    value: &str,
    sent: &mut String,
    dirty_at: &mut Option<Instant>,
    make: fn(String) -> SettingChange,
    changes: &mut Vec<SettingChange>,
) {
    *dirty_at = None;
    if value != sent {
        sent.clear();
        sent.push_str(value);
        changes.push(make(value.to_owned()));
    }
}

fn flush_if_due(
    value: &str,
    sent: &mut String,
    dirty_at: &mut Option<Instant>,
    make: fn(String) -> SettingChange,
    proxy: &EventLoopProxy<GuiEvent>,
    ctx: &egui::Context,
) {
    let Some(started) = *dirty_at else {
        return;
    };
    if started.elapsed() < TEXT_DEBOUNCE {
        ctx.request_repaint_after(TEXT_DEBOUNCE.saturating_sub(started.elapsed()));
        return;
    }
    *dirty_at = None;
    if value != sent {
        sent.clear();
        sent.push_str(value);
        emit(proxy, make(value.to_owned()));
    }
}

fn section_heading(ui: &mut Ui, text: &str) {
    let width = ui.available_width().max(1.0);
    ui.allocate_ui_with_layout(
        vec2(width, SECTION_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.add_space(CONTENT_PADDING);
            let heading_width = (ui.available_width() - CONTENT_PADDING).max(1.0);
            let (rect, _) = ui.allocate_exact_size(
                vec2(heading_width, SECTION_HEIGHT),
                Sense::hover(),
            );
            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::same(7), SECTION_FILL);
            painter.rect_filled(
                egui::Rect::from_min_max(rect.min, pos2(rect.min.x + 3.0, rect.max.y)),
                CornerRadius::same(2),
                ACCENT,
            );
            painter.text(
                pos2(rect.left() + 14.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                text,
                FontId::proportional(14.0),
                TEXT,
            );
            ui.add_space(CONTENT_PADDING);
        },
    );
    ui.add_space(6.0);
}

fn padded_label(ui: &mut Ui, text: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.add_space(CONTENT_PADDING);
        ui.add(egui::Label::new(RichText::new(text).size(14.0).color(color)).wrap());
        ui.add_space(CONTENT_PADDING);
    });
}

fn status_line(ui: &mut Ui, status: &str) {
    let color = if status.contains("失败") {
        DANGER
    } else if status.contains("运行中") {
        OK_GREEN
    } else {
        WARN
    };
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 14.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
    ui.add_space(6.0);
    let label = status.trim_start_matches(['●', ' ']);
    ui.label(RichText::new(label).size(14.0).color(TEXT));
}

fn toggle_row(
    ui: &mut Ui,
    label: &str,
    value: &mut bool,
    enabled: bool,
    on_change: impl FnOnce(bool),
) {
    let width = ui.available_width().max(1.0);
    ui.allocate_ui_with_layout(
        vec2(width, ROW_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.add_space(CONTENT_PADDING);
            let label_width =
                (ui.available_width() - CONTENT_PADDING - SWITCH_WIDTH).max(1.0);
            ui.add_sized(
                vec2(label_width, ROW_HEIGHT),
                egui::Label::new(RichText::new(label).size(14.0).color(TEXT)),
            );
            let spacer = (ui.available_width() - CONTENT_PADDING - SWITCH_WIDTH).max(0.0);
            ui.add_space(spacer);
            if toggle_switch(ui, value, enabled) {
                on_change(*value);
            }
            ui.add_space(CONTENT_PADDING);
        },
    );
    ui.add_space(4.0);
}

fn action_button_row(ui: &mut Ui, text: &str, primary: bool, enabled: bool) -> bool {
    let width = ui.available_width().max(1.0);
    let mut clicked = false;
    ui.allocate_ui_with_layout(
        vec2(width, BUTTON_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.add_space(CONTENT_PADDING);
            let button_width = (ui.available_width() - CONTENT_PADDING).max(1.0);
            clicked = action_button(ui, text, primary, enabled, button_width);
            ui.add_space(CONTENT_PADDING);
        },
    );
    ui.add_space(4.0);
    clicked
}

fn labeled_field(
    ui: &mut Ui,
    label: &str,
    value: &mut String,
    limit: usize,
    enabled: bool,
    dirty_at: &mut Option<Instant>,
    sent: &mut String,
    proxy: &EventLoopProxy<GuiEvent>,
    make: fn(String) -> SettingChange,
) {
    ui.horizontal(|ui| {
        ui.add_space(CONTENT_PADDING);
        ui.label(RichText::new(label).size(14.0).color(TEXT));
    });
    ui.add_space(4.0);

    let width = ui.available_width().max(1.0);
    ui.allocate_ui_with_layout(
        vec2(width, FIELD_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.add_space(CONTENT_PADDING);
            let field_width = (ui.available_width() - CONTENT_PADDING).max(1.0);
            let response = ui.add_enabled(
                enabled,
                TextEdit::singleline(value)
                    .char_limit(limit)
                    .desired_width(field_width),
            );
            if response.changed() {
                *dirty_at = Some(Instant::now());
                ui.ctx().request_repaint_after(TEXT_DEBOUNCE);
            }
            if response.lost_focus() {
                *dirty_at = None;
                if value.as_str() != sent.as_str() {
                    sent.clone_from(value);
                    emit(proxy, make(value.clone()));
                }
            }
            ui.add_space(CONTENT_PADDING);
        },
    );
    ui.add_space(8.0);
}

fn toggle_switch(ui: &mut Ui, on: &mut bool, enabled: bool) -> bool {
    let desired = vec2(SWITCH_WIDTH, SWITCH_HEIGHT);
    let (rect, mut response) = ui.allocate_exact_size(desired, Sense::click());
    if !enabled {
        response = response.on_hover_cursor(CursorIcon::NotAllowed);
    }
    let mut changed = false;
    if enabled && response.clicked() {
        *on = !*on;
        changed = true;
    }
    let t = ui.ctx().animate_bool(response.id, *on);
    let track = if !enabled {
        TRACK_DISABLED
    } else {
        TRACK_OFF.lerp(BLUE, t)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same((rect.height() / 2.0) as u8), track);
    let knob_d = rect.height() - 6.0;
    let x = rect.left() + 3.0 + t * (rect.width() - 6.0 - knob_d);
    painter.circle_filled(
        pos2(x + knob_d / 2.0, rect.center().y),
        knob_d / 2.0,
        Color32::WHITE,
    );
    changed
}

fn icon_button(ui: &mut Ui, text: &str, size: Vec2) -> bool {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let fill = if response.is_pointer_button_down_on() {
        SECONDARY_PRESSED
    } else if response.hovered() {
        SECONDARY
    } else {
        Color32::from_rgb(248, 249, 251)
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        FontId::proportional(16.0),
        TEXT,
    );
    response.clicked()
}

fn action_button(ui: &mut Ui, text: &str, primary: bool, enabled: bool, width: f32) -> bool {
    let width = width.min(ui.available_width()).max(1.0);
    let (rect, mut response) = ui.allocate_exact_size(vec2(width, BUTTON_HEIGHT), Sense::click());
    if !enabled {
        response = response.on_hover_cursor(CursorIcon::NotAllowed);
    }
    let fill = if !enabled {
        TRACK_DISABLED
    } else if primary && response.is_pointer_button_down_on() {
        BLUE_PRESSED
    } else if primary {
        BLUE
    } else if response.is_pointer_button_down_on() {
        SECONDARY_PRESSED
    } else if response.hovered() {
        SECONDARY
    } else {
        SECONDARY
    };
    let fg = if primary && enabled {
        Color32::WHITE
    } else if enabled {
        TEXT
    } else {
        MUTED
    };
    ui.painter().rect_filled(rect, CornerRadius::same(10), fill);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        FontId::proportional(14.0),
        fg,
    );
    enabled && response.clicked()
}

trait ColorLerp {
    fn lerp(self, other: Color32, t: f32) -> Color32;
}

impl ColorLerp for Color32 {
    fn lerp(self, other: Color32, t: f32) -> Color32 {
        let a = self.to_array();
        let b = other.to_array();
        Color32::from_rgba_premultiplied(
            lerp_u8(a[0], b[0], t),
            lerp_u8(a[1], b[1], t),
            lerp_u8(a[2], b[2], t),
            lerp_u8(a[3], b[3], t),
        )
    }
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)).round() as u8
}
