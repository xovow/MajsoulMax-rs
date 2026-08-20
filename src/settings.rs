use crate::proto::lq::ViewSlot;
use anyhow::{Context, Result, bail, ensure};
use prost::Message;
use prost_reflect::{DescriptorPool, prost_types::FileDescriptorSet};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use tokio::spawn;
use tracing::{error, info};

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
        let content = std::fs::read_to_string(dir.join("max_data.yaml"))
            .context("无法读取max_data.yaml")?;
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
                    "character"
                        | "skin"
                        | "title"
                        | "item"
                        | "loading_image"
                        | "emoji"
                        | "endings"
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

    ensure!(!result.character.is_empty(), "max_data.yaml has no characters");
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

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub send_method: Vec<String>,
    pub send_action: Vec<String>,
    pub proxy_addr: String,
    pub api_url: String,
    helper_switch: bool,
    mod_switch: bool,
    auto_update: bool,
    liqi_version: String,
    github_token: String,
    #[serde(default)]
    req_proxy: Option<url::Url>,
    #[serde(skip)]
    methods_set: HashSet<String>,
    #[serde(skip)]
    actions_set: HashSet<String>,
    #[serde(skip)]
    pub desc: DescriptorPool,
    #[serde(skip)]
    pub proto_json: Value,
    #[serde(skip)]
    dir: PathBuf,
}

const APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

impl Settings {
    fn create_github_client(&self) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder().user_agent(APP_USER_AGENT);
        if let Some(proxy) = &self.req_proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy.clone())?);
        }
        builder.build().context("Failed to build HTTP client")
    }

    pub fn new(arg_dir: &Path) -> Result<Self> {
        let exe = std::env::current_exe().context("无法获取当前可执行文件路径")?;
        let dir = if arg_dir.is_dir() {
            arg_dir.to_path_buf()
        } else {
            exe.parent()
                .context("无法获取可执行文件的父目录")?
                .join("liqi_config")
        };
        let content = std::fs::read_to_string(dir.join("settings.json"))
            .context("无法读取settings.json")?;
        let mut settings: Settings =
            serde_json::from_str(&content).context("无法解析settings.json")?;
        settings.methods_set = settings.send_method.iter().cloned().collect();
        settings.actions_set = settings.send_action.iter().cloned().collect();

        let descriptor_bytes =
            std::fs::read(dir.join("liqi.desc")).context("无法读取liqi.desc")?;
        let descriptor_set = FileDescriptorSet::decode(descriptor_bytes.as_slice())
            .context("无法解析liqi.desc")?;
        settings.desc = DescriptorPool::from_file_descriptor_set(descriptor_set)
            .context("无法构建liqi descriptor pool")?;
        settings.proto_json = serde_json::from_str(
            &std::fs::read_to_string(dir.join("liqi.json"))
                .context("无法读取liqi.json")?,
        )
        .context("无法解析liqi.json")?;
        settings.dir = dir;
        Ok(settings)
    }

    pub fn data_dir(&self) -> &Path {
        &self.dir
    }
    pub fn is_method(&self, method: &str) -> bool {
        self.methods_set.contains(method)
    }
    pub fn is_action(&self, action: &str) -> bool {
        self.actions_set.contains(action)
    }
    pub fn helper_on(&self) -> bool {
        self.helper_switch
    }
    pub fn mod_on(&self) -> bool {
        self.mod_switch
    }
    pub fn auto_update(&self) -> bool {
        self.auto_update
    }

    pub async fn update(&mut self) -> Result<bool> {
        let client = self.create_github_client()?;
        let mut request = client
            .get("https://api.github.com/repos/Avenshy/MajsoulData/releases/latest")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(std::time::Duration::from_secs(10));
        if !self.github_token.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", self.github_token));
        }
        let response = request
            .send()
            .await
            .context("Failed to get MajsoulData latest release")?
            .error_for_status()
            .context("MajsoulData latest release request failed")?;
        if response
            .headers()
            .get("X-RateLimit-Remaining")
            .and_then(|value| value.to_str().ok()) == Some("0")
        {
            bail!("GitHub API rate limit exceeded");
        }
        let release: Value = response.json().await?;
        let version = release["tag_name"]
            .as_str()
            .context("MajsoulData release has no tag_name")?;
        if self.liqi_version == version {
            info!("无需更新协议和资源数据, 当前版本: {version}");
            return Ok(false);
        }

        let assets = release["assets"]
            .as_array()
            .context("MajsoulData release has no assets")?;
        let mut descriptor = None;
        let mut max_data = None;
        for asset in assets {
            match asset["name"].as_str().unwrap_or_default() {
                "liqi.desc" => descriptor = Some(self.download_asset(asset).await?),
                "max_data.yaml" => max_data = Some(self.download_asset(asset).await?),
                _ => {}
            }
        }
        let descriptor = descriptor.context("MajsoulData release lacks liqi.desc")?;
        let max_data = max_data.context("MajsoulData release lacks max_data.yaml")?;
        let rpc_map = generate_liqi_json(&descriptor)?;
        parse_max_data(
            std::str::from_utf8(&max_data).context("max_data.yaml is not valid UTF-8")?,
        )?;

        // Write all related files only after every required asset has downloaded successfully.
        std::fs::write(self.dir.join("liqi.desc"), descriptor)?;
        std::fs::write(self.dir.join("max_data.yaml"), max_data)?;
        std::fs::write(self.dir.join("liqi.json"), rpc_map)?;
        self.liqi_version = version.to_string();
        std::fs::write(
            self.dir.join("settings.json"),
            serde_json::to_string_pretty(self)?,
        )?;
        info!("协议和资源数据更新完成: {version}");
        Ok(true)
    }

    async fn download_asset(&self, asset: &Value) -> Result<Vec<u8>> {
        let name = asset["name"].as_str().context("No asset name")?;
        ensure!(
            matches!(name, "liqi.desc" | "max_data.yaml"),
            "Unsupported asset: {name}"
        );
        let url = asset["browser_download_url"].as_str().context("No asset URL")?;
        let client = self.create_github_client()?;
        let mut request = client
            .get(url)
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(std::time::Duration::from_secs(10));
        if !self.github_token.is_empty() {
            request = request.header("Authorization", format!("Bearer {}", self.github_token));
        }
        let response = request
            .send()
            .await
            .context("Failed to download asset")?
            .error_for_status()
            .context("Asset download failed")?;
        Ok(response.bytes().await?.to_vec())
    }
}

fn generate_liqi_json(bytes: &[u8]) -> Result<String> {
    let descriptors = FileDescriptorSet::decode(bytes).context("无法解析liqi.desc")?;
    let mut rpc_map = serde_json::Map::new();
    for file in descriptors.file {
        let package = file.package.unwrap_or_default();
        for service in file.service {
            let service_name = service.name.context("service without name in liqi.desc")?;
            for method in service.method {
                let method_name = method.name.context("method without name in liqi.desc")?;
                let input_type = method
                    .input_type
                    .context("method without input type in liqi.desc")?;
                let output_type = method
                    .output_type
                    .context("method without output type in liqi.desc")?;
                let full_method = format!(".{package}.{service_name}.{method_name}");
                rpc_map.insert(
                    full_method,
                    serde_json::json!({"req": input_type, "resp": output_type}),
                );
            }
        }
    }
    Ok(serde_json::to_string(&Value::Object(rpc_map))?)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn generates_rpc_map_from_bundled_descriptor() {
        let json = generate_liqi_json(include_bytes!("../liqi_config/liqi.desc")).unwrap();
        let map: Value = serde_json::from_str(&json).unwrap();
        let auth_game = &map[".lq.FastTest.authGame"];

        assert_eq!(auth_game["req"], ".lq.ReqAuthGame");
        assert_eq!(auth_game["resp"], ".lq.ResAuthGame");
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
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
    // Kept for settings-file compatibility; resource auto-update is no longer used.
    auto_update: bool,
    version: String,
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
            auto_update: false,
            version: String::new(),
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
        let mut settings: Self = match std::fs::read_to_string(&dir) {
            Ok(content) => serde_json::from_str(&content).context("无法解析settings.mod.json")?,
            Err(_) => {
                let mut default = Self::default();
                default.dir = general_settings.data_dir().to_path_buf();
                default.write();
                return Ok(default);
            }
        };
        settings.dir = general_settings.data_dir().to_path_buf();
        Ok(settings)
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
    pub fn auto_update(&self) -> bool {
        self.auto_update
    }
    pub fn anti_nickname_censorship(&self) -> bool {
        self.anti_nickname_censorship
    }

    pub fn write(&self) {
        let dir = self.dir.join("settings.mod.json");
        let Ok(content) = serde_json::to_string_pretty(self) else {
            error!("Failed to serialize settings.mod.json");
            return;
        };
        spawn(async move {
            tokio::fs::write(dir, content)
                .await
                .inspect_err(|e| error!("Failed to write settings.mod.json: {e}"))
        });
    }
}
