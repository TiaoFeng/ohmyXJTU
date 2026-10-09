//! 堅果雲同步設定與同步記錄的加密存儲（`sync.vault`）。
//!
//! 與憑證保險庫、任務檔共用同一套信封格式與派生參數（見
//! [`crate::credentials::envelope`]），AAD 前綴 `ohmyXJTU-sync`；金鑰自使用者的
//! 加密口令派生，磁碟上不含明文。這裡存兩樣東西：
//!
//! - **連線設定**：伺服器位址、帳號與應用密碼（應用密碼是秘密，不進 `config.json`）。
//! - **同步記錄**：每個遠端檔案上次同步時的遠端版本與本機內容指紋，用來偵測
//!   「本機改了／遠端改了／兩邊都改了」。
//!
//! 檔案無法讀取時的行為與任務檔一致：不覆寫、不清空，標記本會話不可用，之後
//! 的寫入一律拒絕並回報原因。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::credentials::envelope::{self, Sealed};
use crate::error::{AppError, AppResult};
use crate::io;

/// 同步設定檔的 AAD 前綴（與憑證、任務檔區隔）。
const AAD_PREFIX: &str = "ohmyXJTU-sync";

/// 受同步的檔案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncFile {
    /// 憑證檔（帳號密碼）。
    Credentials,
    /// 自訂義任務檔。
    Tasks,
}

impl SyncFile {
    /// 遠端檔名（放在伺服器位址之下）。
    pub fn remote_name(self) -> &'static str {
        match self {
            Self::Credentials => "ohmyXJTU-credentials.vault",
            Self::Tasks => "ohmyXJTU-tasks.vault",
        }
    }
}

/// 堅果雲連線設定。
///
/// 手寫 [`std::fmt::Debug`]（遮罩帳號與應用密碼），並在 `Drop` 時零化字串，
/// 與 [`crate::credentials::vault::Credentials`] 的處理一致。
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncConfig {
    /// 伺服器位址（例如 `https://dav.jianguoyun.com/dav/`）。
    pub url: String,
    /// 堅果雲帳號。
    pub account: String,
    /// 應用密碼。
    pub app_password: String,
    /// 是否啟用自動同步（解鎖後自動下載、變更後自動上傳）。
    #[serde(default)]
    pub auto_sync: bool,
}

impl SyncConfig {
    /// 建立設定。
    pub fn new(
        url: impl Into<String>,
        account: impl Into<String>,
        app_password: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            account: account.into(),
            app_password: app_password.into(),
            auto_sync: false,
        }
    }
}

impl std::fmt::Debug for SyncConfig {
    /// 只輸出遮罩：任何 `{:?}` 都不會洩漏帳號或應用密碼。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SyncConfig")
            .field("url", &self.url)
            .field("account", &"<redacted>")
            .field("app_password", &"<redacted>")
            .field("auto_sync", &self.auto_sync)
            .finish()
    }
}

impl Drop for SyncConfig {
    fn drop(&mut self) {
        self.account.zeroize();
        self.app_password.zeroize();
    }
}

/// 單一遠端檔案上次同步的記錄。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    /// 上次同步時遠端檔案的版本（`ETag`，否則 `Last-Modified`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 上次同步時遠端 `ETag` 的原始值（含引號），供下一次上傳作 `If-Match`
    /// 條件；遠端未提供 ETag 或僅有弱驗證標籤時為 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// 上次同步時本機內容的指紋。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// `sync.vault` 的內容。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SyncFileContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    config: Option<SyncConfig>,
    #[serde(default)]
    credentials: FileRecord,
    #[serde(default)]
    tasks: FileRecord,
}

impl SyncFileContent {
    fn record(&self, file: SyncFile) -> &FileRecord {
        match file {
            SyncFile::Credentials => &self.credentials,
            SyncFile::Tasks => &self.tasks,
        }
    }

    fn record_mut(&mut self, file: SyncFile) -> &mut FileRecord {
        match file {
            SyncFile::Credentials => &mut self.credentials,
            SyncFile::Tasks => &mut self.tasks,
        }
    }
}

/// 同步設定與記錄的加密存儲。
pub(crate) struct SyncStore {
    path: PathBuf,
    content: SyncFileContent,
    sealed: Option<Sealed>,
    /// 本會話無法使用存儲時的原因（例如解鎖時讀檔失敗）。
    unavailable: Option<String>,
}

impl SyncStore {
    /// 使用指定路徑（便於測試）。
    pub(crate) fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            content: SyncFileContent::default(),
            sealed: None,
            unavailable: None,
        }
    }

    /// 以口令載入設定檔並記住金鑰。
    ///
    /// 檔案不存在時只準備新信封（首次保存才落盤）。回傳需要告知使用者的提示；
    /// 檔案層級的讀取失敗（非「不存在」）仍向上傳播。
    pub(crate) fn init(&mut self, passphrase: &str) -> AppResult<Option<String>> {
        let mut notices: Vec<String> = Vec::new();
        self.unavailable = None;
        match io::read_private(&self.path) {
            Ok(bytes) => {
                if let Err(err) = self.open(passphrase, &bytes) {
                    // 無法解開或解析：不覆寫、不清空。同時丟棄先前成功載入的
                    // 內容與金鑰，避免同一實例再次初始化後以舊內容覆寫原檔。
                    self.content = SyncFileContent::default();
                    self.sealed = None;
                    let message = format!(
                        "同步配置文件无法读取（{err}）；原文件（{}）未被修改。如确认不再需要，请删除该文件后重启程序。",
                        self.path.display()
                    );
                    self.unavailable = Some(message.clone());
                    return Ok(Some(message));
                }
            }
            Err(AppError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                self.provision(passphrase)?;
            }
            Err(err) => return Err(err),
        }
        if let Ok(true) = io::ensure_private(&self.path) {
            notices.push(format!(
                "同步配置文件权限过宽（其他用户可读），已收紧为仅本人可读写：{}",
                self.path.display()
            ));
        }
        Ok(if notices.is_empty() {
            None
        } else {
            Some(notices.join("；"))
        })
    }

    /// 目前連線設定。
    pub(crate) fn config(&self) -> Option<&SyncConfig> {
        self.content.config.as_ref()
    }

    /// 指定檔案的同步記錄。
    pub(crate) fn record(&self, file: SyncFile) -> &FileRecord {
        self.content.record(file)
    }

    /// 寫入連線設定。
    pub(crate) fn set_config(&mut self, config: SyncConfig) -> AppResult<()> {
        self.mutate(move |content| {
            content.config = Some(config);
            Ok(())
        })
    }

    /// 切換自動同步（需已設定）。
    pub(crate) fn set_auto_sync(&mut self, enabled: bool) -> AppResult<()> {
        self.mutate(move |content| {
            let config = content
                .config
                .as_mut()
                .ok_or_else(|| AppError::config("尚未配置坚果云同步"))?;
            config.auto_sync = enabled;
            Ok(())
        })
    }

    /// 更新指定檔案的同步記錄。
    pub(crate) fn set_record(&mut self, file: SyncFile, record: FileRecord) -> AppResult<()> {
        self.mutate(move |content| {
            *content.record_mut(file) = record;
            Ok(())
        })
    }

    /// 清除連線設定與同步記錄（刪除檔案）。之後仍可重新設定。
    pub(crate) fn clear(&mut self) -> AppResult<()> {
        self.content = SyncFileContent::default();
        self.unavailable = None;
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(AppError::Io(err)),
        }
    }

    /// 換口令：以新口令重新加密設定檔（新鹽值與金鑰）。
    ///
    /// 尚未設定同步、也沒有檔案時直接略過：沒有東西要換鑰，也不該憑空建立
    /// 一個 `sync.vault`。存儲不可用（原檔讀不到）時同樣略過：不碰這個檔案，
    /// 也不該讓「修改加密口令」被一個已不可用的同步檔連帶拖垮。
    pub(crate) fn rekey(&mut self, passphrase: &str) -> AppResult<()> {
        if self.unavailable.is_some() {
            return Ok(());
        }
        if self.content.config.is_none() && !self.path.is_file() {
            return Ok(());
        }
        let plaintext = self.serialize()?;
        let (bytes, sealed) = envelope::seal(passphrase, AAD_PREFIX, &plaintext)?;
        io::write_private_atomic(&self.path, &bytes)?;
        self.sealed = Some(sealed);
        Ok(())
    }

    /// 標記本會話無法使用存儲；之後的寫入一律拒絕。
    pub(crate) fn mark_unavailable(&mut self, message: String) {
        self.content = SyncFileContent::default();
        self.sealed = None;
        self.unavailable = Some(message);
    }

    /// 丟棄金鑰（工作階段停用時）。
    pub(crate) fn lock(&mut self) {
        self.sealed = None;
    }

    /// 解開並載入設定檔內容。
    fn open(&mut self, passphrase: &str, bytes: &[u8]) -> AppResult<()> {
        let (plaintext, sealed) = envelope::unseal(passphrase, AAD_PREFIX, bytes)?;
        let content: SyncFileContent = serde_json::from_slice(&plaintext).map_err(|err| {
            AppError::VaultFile(format!(
                "同步配置无法解析（{}）",
                crate::error::describe_json_failure(err.classify())
            ))
        })?;
        self.content = content;
        self.sealed = Some(sealed);
        Ok(())
    }

    /// 準備全新信封（不寫檔）。
    fn provision(&mut self, passphrase: &str) -> AppResult<()> {
        let plaintext = self.serialize()?;
        let (_, sealed) = envelope::seal(passphrase, AAD_PREFIX, &plaintext)?;
        self.sealed = Some(sealed);
        Ok(())
    }

    /// 序列化目前內容。
    fn serialize(&self) -> AppResult<Vec<u8>> {
        serde_json::to_vec(&self.content)
            .map_err(|err| AppError::Crypto(format!("同步配置序列化失败：{err}")))
    }

    /// 以目前信封寫入設定檔（沿用同一鹽值，只換 nonce）。
    fn save(&self) -> AppResult<()> {
        if let Some(message) = &self.unavailable {
            return Err(AppError::Crypto(message.clone()));
        }
        let Some(sealed) = &self.sealed else {
            return Err(AppError::Crypto("同步存储尚未解锁".to_owned()));
        };
        let bytes = envelope::reseal(sealed, AAD_PREFIX, &self.serialize()?)?;
        io::write_private_atomic(&self.path, &bytes)
    }

    /// 套用一次異動並寫入；失敗時還原記憶體狀態（磁碟不變）。
    fn mutate<T>(
        &mut self,
        change: impl FnOnce(&mut SyncFileContent) -> AppResult<T>,
    ) -> AppResult<T> {
        let backup = self.content.clone();
        let outcome = change(&mut self.content).and_then(|value| self.save().map(|()| value));
        if outcome.is_err() {
            self.content = backup;
        }
        outcome
    }
}

#[cfg(test)]
#[path = "tests/config_test.rs"]
mod config_test;
