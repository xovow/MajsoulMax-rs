use crate::proto::lq::ViewSlot;
use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use prost::Message;
use prost_types::FileDescriptorSet;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

// The sidebar, protocol updater and Mod worker share these files. Serialize
// reads and complete read-modify-write operations to avoid lost or partial data.
static SETTINGS_FILE_LOCK: Mutex<()> = Mutex::new(());

pub fn read_settings_file(path: &Path) -> Result<String> {
    let _guard = SETTINGS_FILE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    std::fs::read_to_string(path).with_context(|| format!("无法读取 {}", path.display()))
}

pub fn write_json_setting(path: &Path, key: &str, value: Value) -> Result<()> {
    let _guard = SETTINGS_FILE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let content =
        std::fs::read_to_string(path).with_context(|| format!("无法读取 {}", path.display()))?;
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
    std::fs::write(path, format!("{content}\n"))
        .with_context(|| format!("无法写入 {}", path.display()))
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct MaxData {
    pub character: Vec<u32>,
    pub skin: Vec<u32>,
    pub title: Vec<u32>,
    pub item: Vec<u32>,
    pub loading_image: Vec<u32>,
    pub emoji: HashMap<u32, Vec<u32>>,
    pub endings: Vec<u32>,
}

impl MaxData {
    pub fn load(dir: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(dir.join("max_data.yaml")).context("无法读取max_data.yaml")?;
        parse_max_data(&content)
    }
}

fn parse_max_data(content: &str) -> Result<MaxData> {
    let mut result = MaxData::default();
    let mut section = String::new();
    let mut emoji_character = None;

    for (line_number, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') && !trimmed.starts_with("- ") {
            ensure!(
                trimmed.ends_with(':'),
                "invalid max_data.yaml line {}",
                line_number + 1
            );
            section = trimmed.trim_end_matches(':').to_string();
            emoji_character = None;
            ensure!(
                matches!(
                    section.as_str(),
                    "character" | "skin" | "title" | "item" | "loading_image" | "emoji" | "endings"
                ),
                "unknown max_data.yaml section on line {}",
                line_number + 1
            );
            continue;
        }

        if section == "emoji" && line.starts_with("  ") && trimmed.ends_with(':') {
            let id = trimmed
                .trim_end_matches(':')
                .parse::<u32>()
                .with_context(|| format!("invalid emoji character on line {}", line_number + 1))?;
            result.emoji.entry(id).or_default();
            emoji_character = Some(id);
            continue;
        }

        ensure!(
            trimmed.starts_with("- "),
            "invalid max_data.yaml line {}",
            line_number + 1
        );
        let id = trimmed[2..]
            .parse::<u32>()
            .with_context(|| format!("invalid data ID on line {}", line_number + 1))?;
        match section.as_str() {
            "character" => result.character.push(id),
            "skin" => result.skin.push(id),
            "title" => result.title.push(id),
            "item" => result.item.push(id),
            "loading_image" => result.loading_image.push(id),
            "endings" => result.endings.push(id),
            "emoji" => result
                .emoji
                .get_mut(&emoji_character.context("emoji item without character")?)
                .expect("emoji character was inserted above")
                .push(id),
            _ => bail!(
                "list item outside a max_data.yaml section on line {}",
                line_number + 1
            ),
        }
    }

    ensure!(
        !result.character.is_empty(),
        "max_data.yaml has no characters"
    );
    ensure!(!result.skin.is_empty(), "max_data.yaml has no skins");
    ensure!(!result.title.is_empty(), "max_data.yaml has no titles");
    ensure!(!result.item.is_empty(), "max_data.yaml has no items");
    ensure!(
        !result.loading_image.is_empty(),
        "max_data.yaml has no loading images"
    );
    ensure!(!result.emoji.is_empty(), "max_data.yaml has no emoji data");
    ensure!(
        result.emoji.values().all(|items| !items.is_empty()),
        "max_data.yaml has an empty emoji list"
    );
    ensure!(!result.endings.is_empty(), "max_data.yaml has no endings");
    Ok(result)
}

#[derive(Debug, Clone)]
pub enum LiveModPatch {
    Nickname(String),
    ShowServer(bool),
    AntiNicknameCensorship(bool),
    EmojiSwitch(bool),
    HintSwitch(bool),
}

#[derive(Debug, Clone)]
pub enum LiqiUpdateStatus {
    Latest(String),
    Updated(String),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiqiUpdatePhase {
    Checking,
    Downloading,
}

impl LiqiUpdateStatus {
    pub fn resolved_after_reload(self, current_version: &str) -> Self {
        match self {
            Self::Updated(version) if version == current_version => Self::Latest(version),
            other => other,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum UpdateCheckMode {
    Startup,
    Periodic,
    #[default]
    Disabled,
}

const DEFAULT_UPDATE_INTERVAL_MINUTES: u16 = 60;

fn default_update_interval_minutes() -> u16 {
    DEFAULT_UPDATE_INTERVAL_MINUTES
}

fn deserialize_update_check_mode<'de, D>(deserializer: D) -> Result<UpdateCheckMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredMode {
        Mode(UpdateCheckMode),
        Legacy(bool),
    }

    Ok(match StoredMode::deserialize(deserializer)? {
        StoredMode::Mode(mode) => mode,
        StoredMode::Legacy(true) => UpdateCheckMode::Startup,
        StoredMode::Legacy(false) => UpdateCheckMode::Disabled,
    })
}

fn deserialize_update_interval<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    std::num::NonZeroU16::deserialize(deserializer).map(std::num::NonZeroU16::get)
}

pub fn parse_update_interval_minutes(value: &str) -> Result<u16> {
    value
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|&minutes| minutes > 0)
        .context("检查间隔必须为 1–65535 分钟的整数")
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub proxy_addr: String,
    mod_switch: bool,
    #[serde(default, deserialize_with = "deserialize_update_check_mode")]
    auto_update: UpdateCheckMode,
    #[serde(
        default = "default_update_interval_minutes",
        deserialize_with = "deserialize_update_interval"
    )]
    auto_update_interval_minutes: u16,
    liqi_version: String,
    github_token: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    req_proxy: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    github_prefix: String,
    #[serde(default)]
    debug_log: bool,
    #[serde(skip)]
    dir: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            proxy_addr: String::new(),
            mod_switch: false,
            auto_update: UpdateCheckMode::Disabled,
            auto_update_interval_minutes: DEFAULT_UPDATE_INTERVAL_MINUTES,
            liqi_version: String::new(),
            github_token: String::new(),
            req_proxy: String::new(),
            github_prefix: String::new(),
            debug_log: false,
            dir: PathBuf::new(),
        }
    }
}

const APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

impl Settings {
    fn create_github_client(&self) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder().user_agent(APP_USER_AGENT);
        let proxy = self.req_proxy.trim();
        if !proxy.is_empty() {
            builder = builder
                .proxy(reqwest::Proxy::all(proxy).context("Failed to create proxy from reqProxy")?);
        }
        builder.build().context("Failed to build HTTP client")
    }

    pub fn load_config(arg_dir: &Path) -> Result<Self> {
        let exe = std::env::current_exe().context("无法获取当前可执行文件路径")?;
        let dir = if arg_dir.is_dir() {
            arg_dir.to_path_buf()
        } else {
            exe.parent()
                .context("无法获取可执行文件的父目录")?
                .join("liqi_config")
        };
        let content = read_settings_file(&dir.join("settings.json"))?;
        let mut settings: Settings =
            serde_json::from_str(&content).context("无法解析settings.json")?;
        settings.dir = dir;
        Ok(settings)
    }

    pub fn data_dir(&self) -> &Path {
        &self.dir
    }
    pub fn mod_on(&self) -> bool {
        self.mod_switch
    }
    pub fn debug_log_on(&self) -> bool {
        self.debug_log
    }
    pub fn set_debug_log(&mut self, enabled: bool) {
        self.debug_log = enabled;
    }
    /// 调试日志所在目录，开启 `debugLog` 后由代理写入。
    pub fn debug_log_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }
    pub fn update_check_mode(&self) -> UpdateCheckMode {
        self.auto_update
    }

    pub fn update_interval_minutes(&self) -> u16 {
        self.auto_update_interval_minutes
    }

    pub fn set_update_check_mode(&mut self, mode: UpdateCheckMode) {
        self.auto_update = mode;
    }

    pub fn set_update_interval_minutes(&mut self, minutes: u16) {
        self.auto_update_interval_minutes = minutes.max(1);
    }

    pub fn liqi_version(&self) -> &str {
        &self.liqi_version
    }

    pub fn req_proxy(&self) -> &str {
        &self.req_proxy
    }

    pub fn github_prefix(&self) -> &str {
        &self.github_prefix
    }

    pub fn set_req_proxy(&mut self, value: impl Into<String>) {
        self.req_proxy = value.into();
    }

    pub fn set_github_prefix(&mut self, value: impl Into<String>) {
        self.github_prefix = value.into();
    }

    fn github_url(&self, url: &str) -> String {
        apply_github_prefix(&self.github_prefix, url)
    }

    fn github_request(&self, client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
        let mut request = client
            .get(self.github_url(url))
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(std::time::Duration::from_secs(10));
        if !self.github_token.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", self.github_token));
        }
        request
    }

    async fn github_latest_release(&self, client: &reqwest::Client) -> Result<Value> {
        let response = self
            .github_request(
                client,
                "https://api.github.com/repos/Avenshy/MajsoulData/releases/latest",
            )
            .send()
            .await
            .context("Failed to get MajsoulData latest release")?;
        ensure_github_success(&response, "MajsoulData latest release request failed")?;
        response
            .json()
            .await
            .context("Failed to parse MajsoulData latest release")
    }

    pub async fn check_and_download_with_progress(
        &self,
        mut on_progress: impl FnMut(LiqiUpdatePhase),
    ) -> LiqiUpdateStatus {
        match self.update_with_progress(&mut on_progress).await {
            Ok(Some(version)) => LiqiUpdateStatus::Updated(version),
            Ok(None) => LiqiUpdateStatus::Latest(self.liqi_version.clone()),
            Err(error) => LiqiUpdateStatus::Failed(error.to_string()),
        }
    }

    async fn update_with_progress(
        &self,
        on_progress: &mut impl FnMut(LiqiUpdatePhase),
    ) -> Result<Option<String>> {
        on_progress(LiqiUpdatePhase::Checking);
        let client = self.create_github_client()?;
        let release = self.github_latest_release(&client).await?;
        let version = release["tag_name"]
            .as_str()
            .context("MajsoulData release has no tag_name")?;
        if self.liqi_version == version {
            return Ok(None);
        }
        if self.stored_liqi_version()?.as_deref() == Some(version) {
            return Ok(Some(version.to_owned()));
        }
        on_progress(LiqiUpdatePhase::Downloading);

        let assets = release["assets"]
            .as_array()
            .context("MajsoulData release has no assets")?;
        let mut descriptor = None;
        let mut max_data = None;
        for asset in assets {
            match asset["name"].as_str().unwrap_or_default() {
                "liqi.desc" => descriptor = Some(self.download_asset(&client, asset).await?),
                "max_data.yaml" => max_data = Some(self.download_asset(&client, asset).await?),
                _ => {}
            }
        }
        let descriptor = descriptor.context("MajsoulData release lacks liqi.desc")?;
        let max_data = max_data.context("MajsoulData release lacks max_data.yaml")?;
        validate_descriptor(&descriptor)?;
        parse_max_data(
            std::str::from_utf8(&max_data).context("max_data.yaml is not valid UTF-8")?,
        )?;

        // Write all related files only after every required asset has downloaded successfully.
        std::fs::write(self.dir.join("liqi.desc"), descriptor)?;
        std::fs::write(self.dir.join("max_data.yaml"), max_data)?;
        self.write_liqi_version(version)?;
        Ok(Some(version.to_owned()))
    }

    fn stored_liqi_version(&self) -> Result<Option<String>> {
        let document = self.settings_document()?;
        Ok(document
            .get("liqiVersion")
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    fn write_liqi_version(&self, version: &str) -> Result<()> {
        write_json_setting(
            &self.dir.join("settings.json"),
            "liqiVersion",
            Value::String(version.to_owned()),
        )
    }

    fn settings_document(&self) -> Result<Value> {
        let path = self.dir.join("settings.json");
        let content = read_settings_file(&path)?;
        serde_json::from_str(&content).context("无法解析settings.json")
    }

    async fn download_asset(&self, client: &reqwest::Client, asset: &Value) -> Result<Bytes> {
        let name = asset["name"].as_str().context("No asset name")?;
        ensure!(
            matches!(name, "liqi.desc" | "max_data.yaml"),
            "Unsupported asset: {name}"
        );
        let url = asset["browser_download_url"]
            .as_str()
            .context("No asset URL")?;
        let response = self
            .github_request(client, url)
            .send()
            .await
            .context("Failed to download asset")?;
        ensure_github_success(&response, "Asset download failed")?;
        Ok(response.bytes().await?)
    }
}

/// `build.rs` only generates the `lq` package, so a descriptor without it
/// would break the next build as well as the running protocol types.
fn validate_descriptor(bytes: &[u8]) -> Result<()> {
    let descriptors = FileDescriptorSet::decode(bytes).context("无法解析liqi.desc")?;
    ensure!(
        descriptors
            .file
            .iter()
            .any(|file| file.package.as_deref() == Some("lq")),
        "liqi.desc 缺少 lq 协议包"
    );
    Ok(())
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn apply_github_prefix(prefix: &str, url: &str) -> String {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return url.to_owned();
    }
    format!("{}/{url}", prefix.trim_end_matches('/'))
}

fn ensure_github_success(response: &reqwest::Response, failed_message: &str) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        // The last allowed request still succeeds with zero remaining quota.
        return Ok(());
    }
    let rate_limited = response
        .headers()
        .get("X-RateLimit-Remaining")
        .and_then(|value| value.to_str().ok())
        == Some("0");
    if rate_limited {
        bail!("GitHub API rate limit exceeded");
    }
    bail!("{failed_message}: {status}")
}

#[derive(Serialize, Deserialize, Debug, Clone)]
// serde(default) 不可省略：缺字段会让反序列化整体失败，
// 而失败分支会用默认值覆写用户的 settings.mod.json，等于静默清空全部配置。
// 有了它，新增/删除字段才不会波及既有用户。
#[serde(rename_all = "camelCase", default)]
pub struct ModSettings {
    pub main_char: u32,
    pub char_skin: HashMap<u32, u32>,
    pub nickname: String,
    pub star_character: Vec<u32>,
    pub hidden_characters: Vec<u32>,
    hint_switch: bool,
    pub title: u32,
    pub loading_bg: Vec<u32>,
    emoji_switch: bool,
    pub views_presets: [Vec<ViewSlot>; 10],
    pub preset_index: u32,
    show_server: bool,
    anti_nickname_censorship: bool,
    pub random_char_switch: bool,
    pub random_char_pool: Vec<(u32, u32)>,
    pub verified: u32,
    #[serde(skip)]
    dir: PathBuf,
}

impl Default for ModSettings {
    fn default() -> Self {
        Self {
            main_char: 200001,
            char_skin: Default::default(),
            nickname: String::new(),
            star_character: Vec::new(),
            hidden_characters: Vec::new(),
            hint_switch: true,
            title: 0,
            loading_bg: Vec::new(),
            emoji_switch: false,
            views_presets: Default::default(),
            preset_index: 0,
            show_server: true,
            anti_nickname_censorship: true,
            random_char_switch: false,
            random_char_pool: Vec::new(),
            verified: 0,
            dir: PathBuf::new(),
        }
    }
}

impl ModSettings {
    pub fn new(general_settings: &Settings) -> Result<Self> {
        let dir = general_settings.data_dir().join("settings.mod.json");
        let mut settings: Self = match read_settings_file(&dir) {
            Ok(content) => serde_json::from_str(&content).context("无法解析settings.mod.json")?,
            Err(_) => {
                let default = Self {
                    dir: general_settings.data_dir().to_path_buf(),
                    ..Default::default()
                };
                default.persist()?;
                return Ok(default);
            }
        };
        settings.dir = general_settings.data_dir().to_path_buf();
        Ok(settings)
    }

    /// 角色的默认装扮 ID，规则为 `40{角色号第 5 位起}01`（如 200001 -> 400101，
    /// 20000125 -> 40012501）。
    ///
    /// 必须与 `Modder::build_character` 读取装扮时的回退规则保持一致。
    pub fn default_avatar_id(char_id: u32) -> Result<u32> {
        let id_str = char_id.to_string();
        let slice = id_str
            .get(4..)
            .with_context(|| format!("角色 ID {char_id} 过短，无法推导默认装扮"))?;
        format!("40{slice}01")
            .parse()
            .with_context(|| format!("无法解析角色 {char_id} 的默认装扮 ID"))
    }

    /// 角色当前的装扮 ID。
    ///
    /// `char_skin` 只在 `changeCharacterSkin` 时写入，所以任何时候都可能缺少某个
    /// 角色的条目 —— 典型场景是全新安装尚未换过装扮。缺失时退回默认装扮，绝不 panic。
    pub fn avatar_id_of(&self, char_id: u32) -> Result<u32> {
        match self.char_skin.get(&char_id) {
            Some(skin) => Ok(*skin),
            None => Self::default_avatar_id(char_id),
        }
    }

    /// 主角色当前的装扮 ID。
    pub fn main_avatar_id(&self) -> Result<u32> {
        self.avatar_id_of(self.main_char)
    }

    /// 当前生效的装扮预设。
    ///
    /// `preset_index` 直接来自客户端的 `useCommonView` / `saveCommonViews` 消息，
    /// 越界时退回 0 号预设，避免索引定长数组导致 panic。
    pub fn current_preset(&self) -> &[ViewSlot] {
        self.views_presets
            .get(self.preset_index as usize)
            .unwrap_or(&self.views_presets[0])
    }

    /// 当前生效预设里的头像框（`slot == 5`）道具 ID。
    pub fn avatar_frame(&self) -> u32 {
        self.current_preset()
            .iter()
            .find(|v| v.slot == 5)
            .map(|v| v.item_id)
            .unwrap_or_default()
    }

    /// 装扮预设槽位数量。
    pub fn preset_count(&self) -> usize {
        self.views_presets.len()
    }

    pub fn hint_on(&self) -> bool {
        self.hint_switch
    }
    pub fn emoji_on(&self) -> bool {
        self.emoji_switch
    }
    pub fn show_server(&self) -> bool {
        self.show_server
    }
    pub fn anti_nickname_censorship(&self) -> bool {
        self.anti_nickname_censorship
    }

    pub fn apply_live_patch(&mut self, patch: &LiveModPatch) {
        match patch {
            LiveModPatch::Nickname(value) => self.nickname.clone_from(value),
            LiveModPatch::ShowServer(value) => self.show_server = *value,
            LiveModPatch::AntiNicknameCensorship(value) => self.anti_nickname_censorship = *value,
            LiveModPatch::EmojiSwitch(value) => self.emoji_switch = *value,
            LiveModPatch::HintSwitch(value) => self.hint_switch = *value,
        }
    }

    pub fn persist(&self) -> Result<()> {
        let _guard = SETTINGS_FILE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = self.dir.join("settings.mod.json");
        let content = serde_json::to_string_pretty(self).context("无法序列化 settings.mod.json")?;
        // Finish while the caller still holds the ModSettings lock. Detached
        // writes can finish out of order or be cancelled when the runtime exits.
        std::fs::write(&path, content).with_context(|| format!("无法写入 {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestConfigDir(PathBuf);

    impl TestConfigDir {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "majsoul-max-settings-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestConfigDir {
        fn drop(&mut self) {
            for name in ["settings.json", "settings.mod.json"] {
                let _ = std::fs::remove_file(self.0.join(name));
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn legacy_auto_update_flags_keep_their_behavior_and_default_interval() {
        for (enabled, mode) in [
            (true, UpdateCheckMode::Startup),
            (false, UpdateCheckMode::Disabled),
        ] {
            let mut document = serde_json::to_value(Settings::default()).unwrap();
            document["autoUpdate"] = Value::Bool(enabled);
            document
                .as_object_mut()
                .unwrap()
                .remove("autoUpdateIntervalMinutes");
            let settings: Settings = serde_json::from_value(document).unwrap();
            assert_eq!(settings.update_check_mode(), mode);
            assert_eq!(settings.update_interval_minutes(), 60);
            assert!(!settings.debug_log_on());
        }
    }

    #[test]
    fn update_modes_and_interval_round_trip_through_the_settings_file() {
        let dir = TestConfigDir::new();
        let path = dir.0.join("settings.json");
        std::fs::write(&path, serde_json::to_string(&Settings::default()).unwrap()).unwrap();
        for (mode, name) in [
            (UpdateCheckMode::Startup, "startup"),
            (UpdateCheckMode::Periodic, "periodic"),
            (UpdateCheckMode::Disabled, "disabled"),
        ] {
            write_json_setting(&path, "autoUpdate", serde_json::to_value(mode).unwrap()).unwrap();
            write_json_setting(&path, "autoUpdateIntervalMinutes", Value::from(5)).unwrap();
            let settings = Settings::load_config(&dir.0).unwrap();
            assert_eq!(settings.update_check_mode(), mode);
            assert_eq!(settings.update_interval_minutes(), 5);
            let saved: Value = serde_json::from_str(&read_settings_file(&path).unwrap()).unwrap();
            assert_eq!(saved["autoUpdate"], name);
        }
    }

    #[test]
    fn update_intervals_reject_empty_zero_fractional_and_out_of_range_values() {
        for text in ["", "0", "-1", "1.5", "65536", "abc"] {
            assert!(parse_update_interval_minutes(text).is_err(), "{text:?}");
        }
        assert_eq!(parse_update_interval_minutes(" 1 ").unwrap(), 1);
        assert_eq!(parse_update_interval_minutes("65535").unwrap(), 65535);

        for value in [
            Value::from(0),
            Value::from(-1),
            Value::from(1.5),
            Value::from(65536),
            Value::from("5"),
        ] {
            let mut document = serde_json::to_value(Settings::default()).unwrap();
            document["autoUpdateIntervalMinutes"] = value;
            assert!(serde_json::from_value::<Settings>(document).is_err());
        }
    }

    #[test]
    fn mod_settings_writes_finish_without_a_runtime() {
        let dir = TestConfigDir::new();
        let mut settings = ModSettings {
            dir: dir.0.clone(),
            nickname: "第一次修改".to_owned(),
            ..Default::default()
        };
        settings.persist().unwrap();
        settings.nickname = "最后一次修改".to_owned();
        settings.persist().unwrap();

        let content = read_settings_file(&dir.0.join("settings.mod.json")).unwrap();
        let saved: ModSettings = serde_json::from_str(&content).unwrap();
        assert_eq!(saved.nickname, "最后一次修改");
    }

    #[test]
    fn mod_settings_reports_write_errors_including_initial_creation() {
        let dir = TestConfigDir::new();
        let settings = ModSettings {
            dir: dir.0.join("missing"),
            ..Default::default()
        };
        let error = settings.persist().unwrap_err();
        assert!(error.to_string().contains("settings.mod.json"));

        let general_settings = Settings {
            dir: dir.0.join("missing"),
            ..Default::default()
        };
        assert!(ModSettings::new(&general_settings).is_err());
    }

    #[test]
    fn failed_game_save_reports_error_and_keeps_request_substitution() {
        let dir = TestConfigDir::new();
        let settings = ModSettings {
            dir: dir.0.join("missing"),
            ..Default::default()
        };
        let messages = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = std::sync::Arc::clone(&messages);
        let modder = crate::Modder::new(tokio::sync::RwLock::new(settings), MaxData::default())
            .with_save_error_handler(Box::new(move |message| {
                captured.lock().unwrap().push(message);
            }));
        let request = crate::proto::lq::ReqUseTitle { title: 100001 };
        let envelope = crate::proto::base::BaseMessage {
            method_name: ".lq.Lobby.useTitle".to_owned(),
            data: request.encode_to_vec().into(),
        };
        let mut wire = vec![2, 7, 0];
        envelope.encode(&mut wire).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = runtime.block_on(modder.modify(wire.into(), true, ""));
        let wire = result.msg.unwrap();
        let forwarded = crate::proto::base::BaseMessage::decode(wire.slice(3..)).unwrap();
        assert_eq!(forwarded.method_name, ".lq.Lobby.loginBeat");

        let messages = messages.lock().unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("settings.mod.json"));
    }

    #[test]
    fn live_patch_restores_sidebar_value_after_an_earlier_game_save() {
        let dir = TestConfigDir::new();
        let settings = ModSettings {
            dir: dir.0.clone(),
            title: 100001,
            ..Default::default()
        };
        settings.persist().unwrap();
        let path = dir.0.join("settings.mod.json");
        write_json_setting(&path, "nickname", Value::String("新昵称".to_owned())).unwrap();
        // A game edit can own the ModSettings lock before the live patch arrives.
        settings.persist().unwrap();

        let modder = crate::Modder::new(tokio::sync::RwLock::new(settings), MaxData::default());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(modder.apply_live_patch(LiveModPatch::Nickname("新昵称".to_owned())));
        drop(runtime);

        let saved: ModSettings = serde_json::from_str(&read_settings_file(&path).unwrap()).unwrap();
        assert_eq!(saved.nickname, "新昵称");
        assert_eq!(saved.title, 100001);
    }

    #[test]
    fn concurrent_json_updates_preserve_each_other_and_unknown_fields() {
        let dir = TestConfigDir::new();
        let path = dir.0.join("settings.json");
        std::fs::write(&path, r#"{"custom":{"keep":true}}"#).unwrap();
        let ready = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for (key, prefix) in [("reqProxy", "http://127.0.0.1:"), ("liqiVersion", "v")] {
                let path = &path;
                let ready = &ready;
                scope.spawn(move || {
                    ready.wait();
                    for value in 0..32 {
                        write_json_setting(path, key, Value::String(format!("{prefix}{value}")))
                            .unwrap();
                    }
                });
            }
        });

        let saved: Value = serde_json::from_str(&read_settings_file(&path).unwrap()).unwrap();
        assert_eq!(saved["reqProxy"], "http://127.0.0.1:31");
        assert_eq!(saved["liqiVersion"], "v31");
        assert_eq!(saved["custom"]["keep"], true);
    }

    #[test]
    fn parses_bundled_max_data() {
        let data = parse_max_data(include_str!("../liqi_config/max_data.yaml")).unwrap();

        assert!(!data.character.is_empty());
        assert!(!data.skin.is_empty());
        assert!(!data.title.is_empty());
        assert!(!data.item.is_empty());
        assert!(!data.loading_image.is_empty());
        assert!(!data.endings.is_empty());
        assert!(data.emoji.values().all(|items| !items.is_empty()));
    }

    #[test]
    fn rejects_malformed_list_items_without_panicking() {
        // Regression: malformed list-item lines must return an error, never panic.
        for content in [
            "character:\n- \n",
            "character:\n- abc\n",
            "character:\n-\n",
            "character:\n-\n- 200001\n",
        ] {
            assert!(parse_max_data(content).is_err(), "content: {content:?}");
        }
    }

    #[test]
    fn bundled_mod_settings_deserializes() {
        let bundled: ModSettings =
            serde_json::from_str(include_str!("../liqi_config/settings.mod.json")).unwrap();
        assert_eq!(bundled.main_char, 20000101);
        assert!(!bundled.char_skin.is_empty());
    }

    #[test]
    fn partial_mod_settings_keeps_known_fields_and_defaults_the_rest() {
        // Regression: 无 serde(default) 时缺任一字段都会整体解析失败，
        // 进而被默认值覆写 —— 用户配置被静默清空。
        // 顺带覆盖：旧版遗留的 version / autoUpdate 键应被忽略而非报错。
        let json = r#"{
            "mainChar": 200042,
            "nickname": "雀魂",
            "version": "v0.11.252.w",
            "autoUpdate": true
        }"#;

        let settings: ModSettings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.main_char, 200042);
        assert_eq!(settings.nickname, "雀魂");
        // 未提供的字段回退到默认值，而不是整个配置被重置
        assert!(settings.hint_on());
        assert!(settings.show_server());
        assert_eq!(settings.preset_index, 0);
    }

    #[test]
    fn derives_default_avatar_id_for_both_id_widths() {
        // 6 位和 8 位角色 ID 都存在于 max_data.yaml，规则是取第 5 位起的后缀
        assert_eq!(ModSettings::default_avatar_id(200001).unwrap(), 400101);
        assert_eq!(ModSettings::default_avatar_id(200042).unwrap(), 404201);
        assert_eq!(ModSettings::default_avatar_id(20000125).unwrap(), 40012501);
    }

    #[test]
    fn every_bundled_character_has_a_default_avatar_id() {
        let data = parse_max_data(include_str!("../liqi_config/max_data.yaml")).unwrap();
        for char_id in data.character {
            ModSettings::default_avatar_id(char_id)
                .unwrap_or_else(|e| panic!("角色 {char_id} 推导默认装扮失败: {e}"));
        }
    }

    #[test]
    fn avatar_id_falls_back_when_char_skin_missing() {
        // Regression: char_skin 是惰性填充的，缺失时不得 panic
        let mut settings = ModSettings::default();
        assert!(settings.char_skin.is_empty());
        assert_eq!(settings.main_avatar_id().unwrap(), 400101);

        settings.char_skin.insert(200001, 400199);
        assert_eq!(settings.main_avatar_id().unwrap(), 400199);
    }

    #[test]
    fn out_of_range_preset_index_falls_back_to_first() {
        // Regression: preset_index 来自客户端，越界时不得索引定长数组 panic
        let mut settings = ModSettings::default();
        settings.views_presets[0] = vec![ViewSlot {
            slot: 5,
            r#type: 0,
            item_id: 305519,
            item_id_list: vec![],
        }];

        settings.preset_index = 0;
        assert_eq!(settings.avatar_frame(), 305519);

        settings.preset_index = 9999;
        assert_eq!(settings.current_preset().len(), 1);
        assert_eq!(settings.avatar_frame(), 305519);
    }

    #[test]
    fn validates_protocol_descriptors() {
        validate_descriptor(include_bytes!("../liqi_config/liqi.desc")).unwrap();
        // An empty set decodes successfully but has no lq package.
        assert!(validate_descriptor(&[]).is_err());
        assert!(validate_descriptor(&[0xff]).is_err());
    }

    #[test]
    fn prefixes_github_urls() {
        let api = "https://api.github.com/repos/Avenshy/MajsoulData/releases/latest";
        assert_eq!(apply_github_prefix("", api), api);
        assert_eq!(apply_github_prefix("   ", api), api);
        assert_eq!(
            apply_github_prefix("https://gh-proxy.org/", api),
            format!("https://gh-proxy.org/{api}")
        );
        assert_eq!(
            apply_github_prefix("https://gh-proxy.org", api),
            format!("https://gh-proxy.org/{api}")
        );
    }

    #[test]
    fn applies_live_mod_patches() {
        let mut settings = ModSettings::default();
        settings.apply_live_patch(&LiveModPatch::Nickname("测试".to_owned()));
        settings.apply_live_patch(&LiveModPatch::ShowServer(false));
        settings.apply_live_patch(&LiveModPatch::AntiNicknameCensorship(false));
        settings.apply_live_patch(&LiveModPatch::EmojiSwitch(true));
        settings.apply_live_patch(&LiveModPatch::HintSwitch(false));

        assert_eq!(settings.nickname, "测试");
        assert!(!settings.show_server());
        assert!(!settings.anti_nickname_censorship());
        assert!(settings.emoji_on());
        assert!(!settings.hint_on());
    }
}
