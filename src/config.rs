//! 應用程式設定檔（`config.json`）。
//!
//! 設定檔不含任何機密資訊；帳號與密碼存放於加密的憑證檔。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::io;
use crate::random;

/// 校內系統訪問策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AccessPolicy {
    /// 自動探測：能直連校內網路時走直連，否則經 WebVPN。
    #[default]
    #[serde(rename = "auto")]
    Auto,
    /// 一律直連。
    #[serde(rename = "direct")]
    Direct,
    /// 一律經 WebVPN。
    #[serde(rename = "webvpn")]
    WebVpn,
}

impl AccessPolicy {
    /// 可選策略的完整清單，供 TUI 顯示與切換。
    pub const ALL: [Self; 3] = [Self::Auto, Self::Direct, Self::WebVpn];

    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::Direct => "直连",
            Self::WebVpn => "WebVPN",
        }
    }
}

/// 裝置標識長度（32 位十六進位，對應統一認證的 `fpVisitorId`）。
pub const VISITOR_ID_LEN: usize = 32;

/// 應用程式設定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 裝置標識：隨機產生一次後固定保存，供統一認證識別可信裝置。
    pub visitor_id: String,
    /// 校內系統訪問策略。
    pub access_policy: AccessPolicy,
    /// 使用者記住的學期（`YYYY-YYYY+1-T`）；無法由考勤系統判定時作為預設。
    #[serde(default)]
    pub homework_term: Option<String>,
    /// 使用者已同意的用户协议版本（與 [`crate::privacy::VERSION`] 比對）。
    #[serde(default)]
    pub privacy_version: Option<String>,
    /// 設定檔路徑覆寫（測試用；正式執行為 `None`，寫入預設位置）。
    #[serde(skip)]
    pub save_path: Option<PathBuf>,
    /// 設定檔損毀後重建的旗標（非持久化）：啟動時用以提示使用者已同意的
    /// 協議版本與記住的學期已一併重設。
    #[serde(skip)]
    pub rebuilt: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            visitor_id: "0".repeat(VISITOR_ID_LEN),
            access_policy: AccessPolicy::default(),
            homework_term: None,
            privacy_version: None,
            save_path: None,
            rebuilt: false,
        }
    }
}

impl Config {
    /// 讀取設定檔；不存在或損毀時以預設值建立。
    pub fn load_or_create() -> AppResult<Self> {
        Self::load_or_create_at(io::config_path()?)
    }

    /// 由指定路徑讀取或建立設定檔（供啟動與測試共用）。
    fn load_or_create_at(path: PathBuf) -> AppResult<Self> {
        let (mut config, mut dirty) = match io::read_private(&path) {
            Ok(bytes) => match serde_json::from_slice::<Self>(&bytes) {
                Ok(config) => (config, false),
                // 設定檔僅含非機密資訊，損毀時直接以預設值重建；標記 `rebuilt`
                // 以便啟動時告知使用者（協議同意與記住的學期會一併重設）。
                Err(_) => {
                    let mut fresh = Self::generate()?;
                    fresh.rebuilt = true;
                    (fresh, true)
                }
            },
            Err(AppError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                (Self::generate()?, true)
            }
            Err(err) => return Err(err),
        };
        // 後續寫入沿用同一個路徑。
        config.save_path = Some(path);

        // 裝置標識必須是 32 位十六進位，否則重新產生。
        let normalized = config.visitor_id.trim().to_ascii_lowercase();
        if normalized.len() == VISITOR_ID_LEN && normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
            if config.visitor_id != normalized {
                config.visitor_id = normalized;
                dirty = true;
            }
        } else {
            config.visitor_id = random::hex(VISITOR_ID_LEN / 2)?;
            dirty = true;
        }

        if dirty {
            config.save()?;
        }
        Ok(config)
    }

    /// 以隨機裝置標識產生預設設定。
    fn generate() -> AppResult<Self> {
        Ok(Self {
            visitor_id: random::hex(VISITOR_ID_LEN / 2)?,
            access_policy: AccessPolicy::default(),
            homework_term: None,
            privacy_version: None,
            save_path: None,
            rebuilt: false,
        })
    }

    /// 覆寫設定檔。
    pub fn save(&self) -> AppResult<()> {
        let path = match &self.save_path {
            Some(path) => path.clone(),
            None => io::config_path()?,
        };
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| AppError::config(format!("配置序列化失败：{err}")))?;
        io::write_private_atomic(&path, &bytes)
    }

    /// 更新訪問策略並寫回檔案。
    pub fn set_access_policy(&mut self, policy: AccessPolicy) -> AppResult<()> {
        if self.access_policy != policy {
            self.access_policy = policy;
            self.save()?;
        }
        Ok(())
    }

    /// 是否已同意指定版本的协议（版本字串需完全一致）。
    pub fn privacy_accepted(&self, version: &str) -> bool {
        self.privacy_version.as_deref() == Some(version)
    }
}

#[cfg(test)]
#[path = "tests/config_test.rs"]
mod config_test;
