use crate::{
    proto::{base::BaseMessage, lq},
    settings::{LiveModPatch, MaxData, ModSettings},
};
use anyhow::{Context, Result, anyhow, ensure};
use bytes::Bytes;
use const_format::formatcp;
use prost::Message;
use rand::{rng, seq::IndexedRandom};
use std::{collections::HashSet, fmt::Write as _};
use tokio::sync::RwLock;
use tracing::{debug, error, info};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const ANNOUNCEMENT: &str = formatcp!(
    "<color=#f9963b>作者: Xerxes-2        版本: {VERSION}</color>\n
<b>本工具完全免费、开源，如果您为此付费，说明您被骗了！</b>\n
<b>本工具仅供学习交流, 请在下载后24小时内删除, 不得用于商业用途, 否则后果自负！</b>\n
<b>本工具有可能导致账号被封禁，给猫粮充钱才是正道！</b>\n\n
<color=#f9963b>开源地址：</color>\n
<href=https://github.com/Xerxes-2/MajsoulMax-rs>https://github.com/Xerxes-2/MajsoulMax-rs</href>\n\n
<color=#f9963b>再次重申：脚本完全免费使用，没有收费功能！</color>"
);

const REQ_HEADER_LEN: usize = 3;
const NOTIFY_HEADER: [u8; 1] = [0x01];

#[derive(Default)]
struct Safe {
    account_id: u32,
    characters: Vec<lq::Character>,
    main_character_id: u32,
    items: Vec<lq::Item>,
}

#[derive(Default)]
pub struct Modder {
    max_data: MaxData,
    mod_settings: RwLock<ModSettings>,
    safe: RwLock<Safe>,
    contract: RwLock<String>,
}

pub struct ModifyResult {
    pub msg: Option<Bytes>,
    pub inject_msg: Option<Bytes>,
}

impl Modder {
    pub fn new(mod_settings: RwLock<ModSettings>, max_data: MaxData) -> Self {
        Self {
            max_data,
            mod_settings,
            ..Default::default()
        }
    }

    pub async fn apply_live_patch(&self, patch: LiveModPatch) {
        self.mod_settings.write().await.apply_live_patch(&patch);
    }

    pub async fn modify(
        &self,
        buf: Bytes,
        from_client: bool,
        method_name: impl AsRef<str>,
    ) -> ModifyResult {
        let res = match buf.first().copied() {
            Some(0x01) => self.modify_notify(buf.clone()).await,
            Some(0x02) => self.modify_req(buf.clone(), from_client).await,
            Some(0x03) => self.modify_res(buf.clone(), from_client, method_name).await,
            Some(msg_type) => Err(anyhow!("Unimplemented message type: {msg_type}")),
            None => Err(anyhow!("Empty websocket payload")),
        };
        match res {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to modify message: {e}");
                ModifyResult {
                    msg: Some(buf),
                    inject_msg: None,
                }
            }
        }
    }

    async fn edit_mod_settings(&self, edit: impl FnOnce(&mut ModSettings)) {
        let mut mod_settings = self.mod_settings.write().await;
        edit(&mut mod_settings);
        mod_settings.persist();
    }

    async fn modify_res(
        &self,
        buf: Bytes,
        from_client: bool,
        method_name: impl AsRef<str>,
    ) -> Result<ModifyResult> {
        let method_name = method_name.as_ref();
        debug!("Respond method: {method_name}");
        ensure!(!from_client, "Respond message came from the client");
        ensure!(buf.len() >= REQ_HEADER_LEN, "Truncated respond message");
        let mut msg_block = BaseMessage::decode(&buf[REQ_HEADER_LEN..])?;
        ensure!(
            msg_block.method_name.is_empty(),
            "Non-empty respond method name"
        );
        let mut modified_data: Option<Vec<u8>> = None;
        match method_name {
            ".lq.Lobby.fetchAccountInfo" => {
                let mut msg = lq::ResAccountInfo::decode(msg_block.data.as_ref())?;
                let account_id = self.safe.read().await.account_id;
                if let Some(acc) = msg.account.as_mut()
                    && acc.account_id == account_id
                {
                    let mod_settings = self.mod_settings.read().await;
                    acc.avatar_frame = mod_settings.avatar_frame();
                    acc.avatar_id = mod_settings.main_avatar_id()?;
                    acc.verified = mod_settings.verified;
                    drop(mod_settings);
                    modified_data = Some(msg.encode_to_vec());
                }
            }
            ".lq.Lobby.fetchCharacterInfo" => {
                let mut msg = lq::ResCharacterInfo::decode(msg_block.data.as_ref())?;
                self.fill_character_info(&mut msg).await?;
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.Lobby.login" | ".lq.Lobby.oauth2Login" => {
                let mut msg = lq::ResLogin::decode(msg_block.data.as_ref())?;
                self.safe.write().await.account_id = msg.account_id;
                if let Some(account) = msg.account.as_mut() {
                    let mod_settings = self.mod_settings.read().await;
                    account.avatar_id = mod_settings.main_avatar_id()?;
                    if !mod_settings.nickname.is_empty() {
                        account.nickname.clone_from(&mod_settings.nickname);
                    }
                    account.title = mod_settings.title;
                    account.loading_image.clear();
                    account
                        .loading_image
                        .extend_from_slice(&mod_settings.loading_bg);
                    account.verified = mod_settings.verified;
                }
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.Lobby.createRoom" => {
                let mut msg = lq::ResCreateRoom::decode(msg_block.data.as_ref())?;
                if let Some(room) = msg.room.as_mut() {
                    for p in &mut room.persons {
                        self.change_player(p).await?;
                    }
                }
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.FastTest.authGame" => {
                let mut msg = lq::ResAuthGame::decode(msg_block.data.as_ref())?;
                let hint_on = self.mod_settings.read().await.hint_on();
                if hint_on && let Some(c) = msg.game_config.as_mut() {
                    if let Some(r) = c.mode.as_mut().and_then(|m| m.detail_rule.as_mut()) {
                        r.bianjietishi = true;
                    }
                    if let Some(meta) = c.meta.as_mut() {
                        match meta.mode_id {
                            15..=16 => meta.mode_id -= 4,
                            25..=26 => meta.mode_id -= 2,
                            _ => {}
                        }
                    }
                }
                for p in &mut msg.players {
                    self.change_player(p).await?;
                }
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.Lobby.fetchTitleList" => {
                let mut msg = lq::ResTitleList::decode(msg_block.data.as_ref())?;
                msg.title_list.clear();
                msg.title_list.extend_from_slice(&self.max_data.title);
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.Lobby.fetchRoom" => {
                let mut msg = lq::ResSelfRoom::decode(msg_block.data.as_ref())?;
                if let Some(room) = msg.room.as_mut() {
                    for p in &mut room.persons {
                        self.change_player(p).await?;
                    }
                }
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.Lobby.fetchBagInfo" => {
                let mut msg = lq::ResBagInfo::decode(msg_block.data.as_ref())?;
                if let Some(bag) = msg.bag.as_mut() {
                    self.fill_bag(bag).await;
                }
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.Lobby.fetchAllCommonViews" => {
                let mut msg = lq::ResAllcommonViews::decode(msg_block.data.as_ref())?;
                self.fill_common_views(&mut msg).await;
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.Lobby.fetchAnnouncement" => {
                let mut msg = lq::ResAnnouncement::decode(msg_block.data.as_ref())?;
                msg.announcements.insert(
                    0,
                    lq::Announcement {
                        title: "雀魂Max-rs载入成功".to_string(),
                        id: 1145141919,
                        header_image: "internal://2.jpg".to_string(),
                        content: ANNOUNCEMENT.to_string(),
                    },
                );
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.Lobby.fetchInfo" => {
                let mut msg = lq::ResFetchInfo::decode(msg_block.data.as_ref())?;
                if let Some(char_info) = msg.character_info.as_mut() {
                    self.fill_character_info(char_info).await?;
                }
                if let Some(bag_info) = msg.bag_info.as_mut()
                    && let Some(bag) = bag_info.bag.as_mut()
                {
                    self.fill_bag(bag).await;
                }
                if let Some(views) = msg.all_common_views.as_mut() {
                    self.fill_common_views(views).await;
                }
                msg.title_list = Some(lq::ResTitleList {
                    title_list: self.max_data.title.clone(),
                    ..Default::default()
                });
                msg.random_character = Some(self.random_character().await);
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.Lobby.fetchServerSettings" => {
                let mut msg = lq::ResServerSettings::decode(msg_block.data.as_ref())?;
                let anti_censorship = self.mod_settings.read().await.anti_nickname_censorship();
                if anti_censorship
                    && let Some(settings) = msg.settings.as_mut()
                    && let Some(nick_setting) = settings.nickname_setting.as_mut()
                {
                    nick_setting.enable = 0;
                    nick_setting.nicknames.clear();
                    modified_data = Some(msg.encode_to_vec());
                }
            }
            ".lq.Lobby.fetchGameRecord" => {
                let msg = lq::ResGameRecord::decode(msg_block.data.as_ref())?;
                if let Some(head) = msg.head.as_ref() {
                    let uuid = head.uuid.as_str();
                    let anonymous_uuid = encode_uuid(uuid);
                    let self_account_id = self.safe.read().await.account_id;
                    let mut logs = String::new();
                    for acc in &head.accounts {
                        logs.push_str(match acc.seat {
                            0 => "东家：",
                            1 => "南家：",
                            2 => "西家：",
                            3 => "北家：",
                            _ => "",
                        });
                        if acc.account_id == self_account_id {
                            logs.push_str("（自己）");
                        }
                        let anonymous_id = encode_account_id(acc.account_id);
                        let _ = writeln!(
                            logs,
                            "{}\n账号id: {}\t加好友id: {}\n主视角牌谱链接: {uuid}_a{anonymous_id}\n主视角牌谱链接(匿名): {anonymous_uuid}_a{anonymous_id}_2\n",
                            add_zone_id(acc.account_id, &acc.nickname),
                            acc.account_id,
                            encode_account_id2(acc.account_id),
                        );
                    }
                    info!("发现读入牌谱！\n{logs}注意：只有在同一服务器才能添加好友！");
                }
            }
            ".lq.Lobby.fetchRandomCharacter" => {
                let mut msg = lq::ResRandomCharacter::decode(msg_block.data.as_ref())?;
                let current = self.random_character().await;
                msg.enabled = current.enabled;
                msg.pool = current.pool;
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.Lobby.setHiddenCharacter" => {
                let mut msg = lq::ResSetHiddenCharacter::decode(msg_block.data.as_ref())?;
                msg.hidden_characters
                    .clone_from(&self.mod_settings.read().await.hidden_characters);
                modified_data = Some(msg.encode_to_vec());
            }
            _ => {}
        }
        let msg = match modified_data {
            Some(data) => {
                msg_block.data = data;
                envelope(&buf[..REQ_HEADER_LEN], &msg_block)
            }
            None => buf,
        };
        Ok(ModifyResult {
            msg: Some(msg),
            inject_msg: None,
        })
    }

    async fn fill_character_info(&self, info: &mut lq::ResCharacterInfo) -> Result<()> {
        {
            let mut safe = self.safe.write().await;
            safe.main_character_id = info.main_character_id;
            safe.characters.clone_from(&info.characters);
        }
        let mod_settings = self.mod_settings.read().await;
        info.characters.clear();
        info.characters.reserve(self.max_data.character.len());
        for charid in self.max_data.character.iter().copied() {
            info.characters
                .push(self.build_character(&mod_settings, charid)?);
        }
        info.skins.clear();
        info.skins.extend_from_slice(&self.max_data.skin);
        info.main_character_id = mod_settings.main_char;
        info.character_sort.clear();
        info.character_sort
            .extend_from_slice(&mod_settings.star_character);
        info.hidden_characters.clear();
        info.hidden_characters
            .extend_from_slice(&mod_settings.hidden_characters);
        info.finished_endings.clear();
        info.finished_endings
            .extend_from_slice(&self.max_data.endings);
        info.rewarded_endings.clear();
        info.rewarded_endings
            .extend_from_slice(&self.max_data.endings);
        Ok(())
    }

    async fn fill_common_views(&self, views: &mut lq::ResAllcommonViews) {
        let mod_settings = self.mod_settings.read().await;
        views.r#use = mod_settings.preset_index;
        views.views.clear();
        views.views.reserve(mod_settings.views_presets.len());
        for (index, preset) in mod_settings.views_presets.iter().enumerate() {
            views.views.push(lq::res_allcommon_views::Views {
                index: index as u32,
                name: format!("View{index}"),
                values: preset.clone(),
            });
        }
    }

    async fn random_character(&self) -> lq::ResRandomCharacter {
        let mod_settings = self.mod_settings.read().await;
        lq::ResRandomCharacter {
            enabled: mod_settings.random_char_switch,
            pool: mod_settings
                .random_char_pool
                .iter()
                .map(|&(character_id, skin_id)| lq::RandomCharacter {
                    character_id,
                    skin_id,
                })
                .collect(),
            error: None,
        }
    }

    async fn fill_bag(&self, bag: &mut lq::Bag) {
        self.safe.write().await.items.clone_from(&bag.items);
        let mut seen: HashSet<u32> = bag.items.iter().map(|item| item.item_id).collect();
        let unlocked = self
            .max_data
            .item
            .iter()
            .chain(self.max_data.loading_image.iter())
            .copied();
        for item_id in unlocked {
            if seen.insert(item_id) {
                bag.items.push(lq::Item { item_id, stack: 1 });
            }
        }
    }

    async fn change_player(&self, p: &mut lq::PlayerGameView) -> Result<()> {
        let account_id = self.safe.read().await.account_id;
        let mod_settings = self.mod_settings.read().await;
        if let Some(character) = p.character.as_mut() {
            character.is_upgraded = true;
            character.level = 5;
            if p.account_id == account_id {
                if mod_settings.random_char_switch && !mod_settings.random_char_pool.is_empty() {
                    let (charid, skin) = mod_settings
                        .random_char_pool
                        .choose(&mut rng())
                        .context("Failed to choose random character")?;
                    character.charid = *charid;
                    p.avatar_id = *skin;
                    character.skin = *skin;
                } else {
                    character.charid = mod_settings.main_char;
                    p.avatar_id = mod_settings.avatar_id_of(character.charid)?;
                    character.skin = p.avatar_id;
                }
                *character = self.build_character(&mod_settings, character.charid)?;
                if !mod_settings.nickname.is_empty() {
                    p.nickname.clone_from(&mod_settings.nickname);
                }
                p.title = mod_settings.title;
                p.views.clear();
                p.views.extend_from_slice(mod_settings.current_preset());
                p.views.iter_mut().for_each(|v| {
                    if v.r#type == 1 {
                        v.item_id = v.item_id_list.choose(&mut rng()).copied().unwrap_or(0);
                    }
                });
                // avatar_frame id is view.item_id which view.slot is 5
                p.avatar_frame = mod_settings.avatar_frame();
                p.verified = mod_settings.verified;
            }
        }
        if mod_settings.show_server() {
            p.nickname = add_zone_id(p.account_id, &p.nickname);
        }
        Ok(())
    }

    fn build_character(&self, mod_settings: &ModSettings, id: u32) -> Result<lq::Character> {
        let mut character = lq::Character {
            charid: id,
            exp: 0,
            is_upgraded: true,
            level: 5,
            skin: mod_settings.avatar_id_of(id)?,
            ..Default::default()
        };
        character.rewarded_level.extend([1, 2, 3, 4, 5]);
        if mod_settings.emoji_on()
            && let Some(emojis) = self.max_data.emoji.get(&id)
        {
            character.extra_emoji.extend_from_slice(emojis);
        }
        character
            .views
            .extend_from_slice(mod_settings.current_preset());
        Ok(character)
    }

    async fn modify_req(&self, buf: Bytes, from_client: bool) -> Result<ModifyResult> {
        ensure!(from_client, "Request message came from the server");
        ensure!(buf.len() >= REQ_HEADER_LEN, "Truncated request message");
        let mut msg_block = BaseMessage::decode(&buf[REQ_HEADER_LEN..])?;
        let mut fake = false;
        let method_name = &msg_block.method_name;
        debug!("Request method: {method_name}");
        let mut inject_msg: Option<Bytes> = None;
        match method_name.as_str() {
            ".lq.Lobby.changeMainCharacter" => {
                fake = true;
                let msg = lq::ReqChangeMainCharacter::decode(msg_block.data.as_ref())?;
                self.edit_mod_settings(|s| s.main_char = msg.character_id)
                    .await;
            }
            ".lq.Lobby.changeCharacterSkin" => {
                fake = true;
                let msg = lq::ReqChangeCharacterSkin::decode(msg_block.data.as_ref())?;
                let mut mod_settings = self.mod_settings.write().await;
                mod_settings.char_skin.insert(msg.character_id, msg.skin);
                let character = self.build_character(&mod_settings, msg.character_id)?;
                mod_settings.persist();
                drop(mod_settings);
                let update = lq::NotifyAccountUpdate {
                    update: Some(lq::AccountUpdate {
                        character: Some(lq::account_update::CharacterUpdate {
                            characters: vec![character],
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                };
                inject_msg = Some(envelope(
                    &NOTIFY_HEADER,
                    &BaseMessage {
                        method_name: ".lq.NotifyAccountUpdate".to_string(),
                        data: update.encode_to_vec(),
                    },
                ));
            }
            ".lq.Lobby.addFinishedEnding" => {
                // drop
                return Ok(ModifyResult {
                    msg: None,
                    inject_msg: None,
                });
            }
            ".lq.Lobby.updateCharacterSort" => {
                fake = true;
                let msg = lq::ReqUpdateCharacterSort::decode(msg_block.data.as_ref())?;
                self.edit_mod_settings(|s| {
                    s.star_character = msg.sort;
                    s.hidden_characters = msg.hidden_characters;
                })
                .await;
            }
            ".lq.Lobby.useTitle" => {
                fake = true;
                let msg = lq::ReqUseTitle::decode(msg_block.data.as_ref())?;
                self.edit_mod_settings(|s| s.title = msg.title).await;
            }
            ".lq.Lobby.setLoadingImage" => {
                fake = true;
                let msg = lq::ReqSetLoadingImage::decode(msg_block.data.as_ref())?;
                self.edit_mod_settings(|s| s.loading_bg = msg.images).await;
            }
            ".lq.Lobby.saveCommonViews" => {
                fake = true;
                let mut msg = lq::ReqSaveCommonViews::decode(msg_block.data.as_ref())?;
                for view in msg.views.iter_mut() {
                    match view.r#type {
                        0 => view.item_id_list.clear(),
                        1 => view.item_id = 0,
                        _ => {}
                    }
                }
                // save_index 来自客户端，views_presets 是定长数组，不检查会 panic
                let mut mod_settings = self.mod_settings.write().await;
                let count = mod_settings.preset_count();
                ensure!(
                    (msg.save_index as usize) < count,
                    "saveCommonViews 的 save_index {} 越界（共 {count} 个预设）",
                    msg.save_index
                );
                mod_settings.views_presets[msg.save_index as usize] = msg.views;
                if msg.is_use == 1 {
                    mod_settings.preset_index = msg.save_index;
                }
                mod_settings.persist();
            }
            ".lq.Lobby.useCommonView" => {
                let msg = lq::ReqUseCommonView::decode(msg_block.data.as_ref())?;
                // index 来自客户端，越界值会被持久化进 settings.mod.json 造成持续损坏
                let mut mod_settings = self.mod_settings.write().await;
                let count = mod_settings.preset_count();
                ensure!(
                    (msg.index as usize) < count,
                    "useCommonView 的 index {} 越界（共 {count} 个预设）",
                    msg.index
                );
                mod_settings.preset_index = msg.index;
                mod_settings.persist();
            }
            ".lq.Lobby.loginBeat" => {
                let msg = lq::ReqLoginBeat::decode(msg_block.data.as_ref())?;
                *self.contract.write().await = msg.contract;
            }
            ".lq.Lobby.readAnnouncement" => {
                let msg = lq::ReqReadAnnouncement::decode(msg_block.data.as_ref())?;
                if msg.announcement_id == 1145141919 {
                    fake = true;
                }
            }
            ".lq.Lobby.receiveCharacterRewards" => {
                fake = true;
            }
            ".lq.Lobby.setRandomCharacter" => {
                fake = true;
                let msg = lq::ReqRandomCharacter::decode(msg_block.data.as_ref())?;
                self.edit_mod_settings(|s| {
                    s.random_char_switch = msg.enabled;
                    s.random_char_pool = msg
                        .pool
                        .iter()
                        .map(|c| (c.character_id, c.skin_id))
                        .collect();
                })
                .await;
            }
            ".lq.Lobby.setHiddenCharacter" => {
                fake = true;
                let msg = lq::ReqSetHiddenCharacter::decode(msg_block.data.as_ref())?;
                self.edit_mod_settings(|s| s.hidden_characters = msg.chara_list)
                    .await;
            }
            _ => {}
        }
        let msg = if fake {
            msg_block.method_name = ".lq.Lobby.loginBeat".to_string();
            msg_block.data = lq::ReqLoginBeat {
                contract: self.contract.read().await.clone(),
            }
            .encode_to_vec();
            envelope(&buf[..REQ_HEADER_LEN], &msg_block)
        } else {
            buf
        };
        Ok(ModifyResult {
            msg: Some(msg),
            inject_msg,
        })
    }

    async fn modify_notify(&self, buf: Bytes) -> Result<ModifyResult> {
        let mut msg_block = BaseMessage::decode(&buf[NOTIFY_HEADER.len()..])?;
        let method_name = &msg_block.method_name;
        debug!("Notify method: {method_name}");
        let mut modified_data: Option<Vec<u8>> = None;
        match method_name.as_str() {
            ".lq.NotifyAccountUpdate" => {
                let msg = lq::NotifyAccountUpdate::decode(msg_block.data.as_ref())?;
                if msg.update.is_some_and(|update| update.character.is_some()) {
                    // drop message if character is updated
                    return Ok(ModifyResult {
                        msg: None,
                        inject_msg: None,
                    });
                }
            }
            ".lq.NotifyRoomPlayerUpdate" => {
                let mut msg = lq::NotifyRoomPlayerUpdate::decode(msg_block.data.as_ref())?;
                let account_id = self.safe.read().await.account_id;
                let mod_settings = self.mod_settings.read().await;
                let show_server = mod_settings.show_server();
                for player in msg.player_list.iter_mut().chain(msg.robots.iter_mut()) {
                    if player.account_id == account_id {
                        player.avatar_id = mod_settings.main_avatar_id()?;
                        if !mod_settings.nickname.is_empty() {
                            player.nickname.clone_from(&mod_settings.nickname);
                        }
                        player.title = mod_settings.title;
                    }
                    if show_server {
                        player.nickname = add_zone_id(player.account_id, &player.nickname);
                    }
                }
                drop(mod_settings);
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.NotifyGameFinishRewardV2" => {
                let mut msg = Box::new(lq::NotifyGameFinishRewardV2::decode(
                    msg_block.data.as_ref(),
                )?);
                {
                    let mut safe = self.safe.write().await;
                    let main = safe.main_character_id;
                    if let Some(main_char) = msg.main_character.as_ref()
                        && let Some(char) = safe.characters.iter_mut().find(|c| c.charid == main)
                    {
                        char.exp = main_char.exp;
                        char.level = main_char.level;
                    }
                }
                if let Some(main_char) = msg.main_character.as_mut() {
                    main_char.add = 0;
                    main_char.exp = 0;
                    main_char.level = 5;
                }
                modified_data = Some(msg.encode_to_vec());
            }
            ".lq.NotifyCustomContestSystemMsg" => {
                let show_server = self.mod_settings.read().await.show_server();
                if show_server {
                    let mut msg =
                        lq::NotifyCustomContestSystemMsg::decode(msg_block.data.as_ref())?;
                    if let Some(game) = msg.game_start.as_mut() {
                        for p in game.players.iter_mut() {
                            p.nickname = add_zone_id(p.account_id, &p.nickname);
                        }
                        modified_data = Some(msg.encode_to_vec());
                    }
                }
            }
            _ => {}
        }
        let msg = match modified_data {
            Some(data) => {
                msg_block.data = data;
                envelope(&NOTIFY_HEADER, &msg_block)
            }
            None => buf,
        };
        Ok(ModifyResult {
            msg: Some(msg),
            inject_msg: None,
        })
    }
}

fn envelope(header: &[u8], msg_block: &BaseMessage) -> Bytes {
    let mut buf = Vec::with_capacity(header.len() + msg_block.encoded_len());
    buf.extend_from_slice(header);
    let _ = msg_block.encode(&mut buf);
    buf.into()
}

fn add_zone_id(id: u32, name: &str) -> String {
    const CN: &str = "[C\u{feff}N]";
    let zone = match id >> 23 {
        0..=6 => CN,
        7..=12 => "[JP]",
        13..=15 => "[EN]",
        _ => "[??]",
    };
    let mut tagged = String::with_capacity(zone.len() + name.len());
    tagged.push_str(zone);
    tagged.push_str(name);
    tagged
}

fn encode_uuid(uuid: &str) -> String {
    const CODE_0: u32 = '0' as u32;
    const CODE_A: u32 = 'a' as u32;
    let mut buf = String::with_capacity(uuid.len());
    for (i, c) in uuid.chars().enumerate() {
        let code = c as u32;
        let digit = if (CODE_0..CODE_0 + 10).contains(&code) {
            Some(code - CODE_0)
        } else if (CODE_A..CODE_A + 26).contains(&code) {
            Some(code - CODE_A + 10)
        } else {
            None
        };
        match digit {
            Some(digit) => {
                let shifted = (digit + 17 + i as u32) % 36;
                let code = if shifted < 10 {
                    CODE_0 + shifted
                } else {
                    CODE_A + shifted - 10
                };
                buf.push(code as u8 as char);
            }
            None => buf.push(c),
        }
    }
    buf
}

fn encode_account_id(id: u32) -> u32 {
    ((7 * id + 1117113) ^ 86216345) + 1358437
}

fn encode_account_id2(id: u32) -> u32 {
    let p = 6139246 ^ id;
    const H: u32 = 67108863;
    let s = p & !H;
    let mut z = p & H;
    for _ in 0..5 {
        z = ((511 & z) << 17) | (z >> 9);
    }
    z + s + 10_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_modder() -> Modder {
        // 全新安装：settings.mod.json 不存在 -> ModSettings::default()，char_skin 为空
        Modder::new(RwLock::new(ModSettings::default()), MaxData::default())
    }

    fn wrap(msg_type: u8, method_name: &str, data: Vec<u8>) -> Bytes {
        let block = BaseMessage {
            method_name: method_name.into(),
            data,
        };
        let mut buf = vec![msg_type, 0x00, 0x00];
        buf.extend(block.encode_to_vec());
        Bytes::from(buf)
    }

    #[tokio::test]
    async fn fetch_account_info_survives_empty_char_skin() {
        // Regression: 曾经直接 char_skin[&main_char] 索引，全新安装时 panic
        // "no entry found for key"
        let modder = fresh_modder();
        let res = lq::ResAccountInfo {
            account: Some(lq::Account {
                account_id: 0, // 与 Safe::default().account_id 相同，进入修改分支
                ..Default::default()
            }),
            ..Default::default()
        };

        let out = modder
            .modify_res(
                wrap(0x03, "", res.encode_to_vec()),
                false,
                ".lq.Lobby.fetchAccountInfo",
            )
            .await
            .expect("fetchAccountInfo 不应报错");

        let body = out.msg.expect("应返回消息体");
        let account =
            lq::ResAccountInfo::decode(&BaseMessage::decode(&body[3..]).unwrap().data[..])
                .unwrap()
                .account
                .unwrap();
        // 回退到 main_char(200001) 的默认装扮
        assert_eq!(account.avatar_id, 400101);
    }

    #[tokio::test]
    async fn out_of_range_use_common_view_is_rejected() {
        // Regression: preset_index 曾被客户端消息直接写入，越界值会持久化进
        // settings.mod.json，之后每次读取都 panic
        let modder = fresh_modder();
        let msg = lq::ReqUseCommonView { index: 9999 };

        // modify() 内部捕获错误并原样放行，不应 panic
        modder
            .modify(
                wrap(0x02, ".lq.Lobby.useCommonView", msg.encode_to_vec()),
                true,
                "",
            )
            .await;

        assert_eq!(
            modder.mod_settings.read().await.preset_index,
            0,
            "越界的 index 不应被写入"
        );
    }

    #[tokio::test]
    async fn out_of_range_save_common_views_is_rejected() {
        let modder = fresh_modder();
        let msg = lq::ReqSaveCommonViews {
            save_index: 9999,
            views: vec![],
            is_use: 1,
            ..Default::default()
        };

        modder
            .modify(
                wrap(0x02, ".lq.Lobby.saveCommonViews", msg.encode_to_vec()),
                true,
                "",
            )
            .await;

        assert_eq!(modder.mod_settings.read().await.preset_index, 0);
    }

    #[tokio::test]
    async fn truncated_frames_do_not_panic() {
        let modder = fresh_modder();
        for raw in [vec![], vec![0x03], vec![0x02, 0x00]] {
            let buf = Bytes::from(raw);
            let out = modder.modify(buf.clone(), true, "").await;
            assert_eq!(out.msg.as_deref(), Some(&buf[..]), "短帧应原样放行");
        }
    }
}
