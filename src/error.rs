//! 應用程式錯誤型別。
//!
//! `Display` 的輸出即為面向使用者的簡體中文訊息，可直接顯示於 TUI 狀態列。

use thiserror::Error;

/// 應用程式統一的結果型別。
pub type AppResult<T> = Result<T, AppError>;

/// `serde_json` 解析失敗的類別描述。
///
/// 錯誤訊息一律只描述類別，不直接使用 `serde_json::Error` 的 `Display`：
/// 型別不符時它會引用出問題的欄位值（例如 `invalid type: string "…"`），
/// 不該出現在使用者可見的訊息或紀錄裡。
pub fn describe_json_failure(category: serde_json::error::Category) -> &'static str {
    match category {
        serde_json::error::Category::Io => "读取失败",
        serde_json::error::Category::Syntax => "语法错误",
        serde_json::error::Category::Data => "数据类型不符",
        serde_json::error::Category::Eof => "内容不完整",
    }
}

/// 網路錯誤的類別。
///
/// 由 HTTP 層依錯誤鏈分類，供介面顯示與路由決策使用：只有
/// [`NetworkKind::is_connection_level`] 為真（例如逾時、DNS 或連線失敗）
/// 才代表「這條路由實際連不上」，可以有限回退到另一條路徑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkKind {
    /// 網域名稱解析失敗。
    Dns,
    /// 無法建立連線（連線被拒、網路不可達等）。
    Connect,
    /// TLS 握手或憑證驗證失敗。
    Tls,
    /// 請求或回應逾時。
    Timeout,
    /// HTTP 回應標頭解析失敗。
    HttpParse,
    /// 重定向相關錯誤。
    Redirect,
    /// 其他網路錯誤。
    Other,
}

impl NetworkKind {
    /// 簡體中文標籤（用於錯誤訊息）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Dns => "域名解析失败",
            Self::Connect => "连接失败",
            Self::Tls => "TLS 握手失败",
            Self::Timeout => "请求超时",
            Self::HttpParse => "响应头解析失败",
            Self::Redirect => "重定向失败",
            Self::Other => "其他错误",
        }
    }

    /// 是否屬於「連線層」錯誤。
    ///
    /// 協定格式錯誤（如 [`Self::HttpParse`]）不算：它代表伺服器有回應，
    /// 只是本地解析器無法接受，換一條路由未必有幫助。
    pub fn is_connection_level(self) -> bool {
        matches!(self, Self::Dns | Self::Connect | Self::Tls | Self::Timeout)
    }
}

impl std::fmt::Display for NetworkKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// 所有可預期錯誤的集合。
#[derive(Debug, Error)]
pub enum AppError {
    /// 網路連線失敗、逾時或無法讀取回應。
    #[error("网络连接失败（{kind}）：{detail}")]
    Network {
        /// 錯誤類別。
        kind: NetworkKind,
        /// 去識別化的錯誤鏈摘要（不含 URL 與查詢參數）。
        detail: String,
    },

    /// 伺服器回應了非預期的 HTTP 狀態碼。
    #[error("服务器返回异常状态码：{status}")]
    Http { status: u16 },

    /// 回應本文超過允許的上限（避免超大回應耗盡記憶體或磁碟）。
    ///
    /// 這不是連線層失敗：伺服器正常回應，只是內容太大，因此不自動重試，
    /// 訊息也不該冠上「网络连接失败」。
    #[error("服务器响应过大（{size} 字节，上限 {limit} 字节）")]
    ResponseTooLarge {
        /// 實際（或宣稱的）本文長度。
        size: u64,
        /// 允許的上限。
        limit: u64,
    },

    /// 伺服器回應了業務錯誤（形如 `{"code": .., "message": ..}`）。
    #[error("服务返回错误（{code}）：{message}")]
    Server { code: i64, message: String },

    /// 登入流程重定向到學校網域之外的主機，已中止提交。
    #[error("登录重定向到学校网域之外的主机（已中止）：{host}")]
    UntrustedHost {
        /// 目標主機名（不含路徑與查詢參數）。
        host: String,
    },

    /// 準備以系統瀏覽器開啟的網址不在允許範圍（非校內主機，或未使用 https）。
    ///
    /// 訊息只含主機名：網址本身可能帶有存取 token（例如思源學堂的播放地址），
    /// 不得寫進任何使用者可見的文字或紀錄。
    #[error("已阻止打开该网址（{reason}）：{host}")]
    UntrustedUrl {
        /// 目標主機名（不含路徑與查詢參數）。
        host: String,
        /// 拒絕原因（不含網址本身）。
        reason: &'static str,
    },

    /// 登入態已失效，需要重新登入。
    #[error("登录状态已失效，请重新登录")]
    SessionExpired,

    /// 自動重新登入後站點仍回報登入態失效（已達自動重試上限）。
    #[error("自动重新登录后仍然失败，请按 r 重试")]
    ReloginExhausted,

    /// 互動驗證（圖片驗證碼或簡訊驗證碼）未通過：同一次登入可以重試。
    ///
    /// 與其他錯誤不同，這種失敗不代表本次登入結束——使用者重輸驗證碼
    /// 即可繼續；呼叫端不得據此作廢整個帳號切換。
    #[error("{0}")]
    VerificationRetry(String),

    /// 回應內容與預期格式不符。
    #[error("服务器响应无法解析：{0}")]
    Protocol(String),

    /// 隨機數或加解密操作失敗。
    #[error("本地加密操作失败：{0}")]
    Crypto(String),

    /// 憑證檔解密驗證失敗，通常代表口令錯誤。
    #[error("口令错误或凭证文件已损坏")]
    WrongPassphrase,

    /// 憑證檔格式不合法。
    #[error("凭证文件格式不正确：{0}")]
    VaultFile(String),

    /// 修改加密口令失敗，且已換鑰的檔案未能還原成舊口令。
    ///
    /// 保險庫、任務檔與同步設定檔是獨立檔案，無法一起原子寫入：後續步驟失敗時
    /// 會把已換鑰的檔案換回舊口令；換不回去時必須明確告訴使用者「哪些檔案可能
    /// 得以舊口令解鎖」。不能改用另外發一則通知的做法——該通知會被緊接著發出的
    /// 失敗訊息蓋掉，使用者永遠看不到。
    #[error("{reason}；且以下文件未能还原，请以原口令重新解锁：{rollback}")]
    PassphraseRollback {
        /// 保險庫寫入失敗的原因。
        reason: String,
        /// 未能還原的檔案與各自的失敗原因。
        rollback: String,
    },

    /// 找不到指定的自訂義任務（可能已被刪除）。
    #[error("任务不存在（可能已被删除）")]
    TaskNotFound,

    /// 堅果雲（WebDAV）認證失敗。
    #[error("坚果云认证失败，请检查账号与应用密码")]
    WebDavAuth,

    /// 遠端同步文檔已被其他裝置修改（條件請求未通過）。
    #[error("远端文档已被其他设备修改，请先同步")]
    WebDavConflict,

    /// 其他 WebDAV（坚果云同步）错误（訊息自足，不再重複「坚果云同步失败」前缀）。
    #[error("{0}")]
    WebDav(String),

    /// 設定檔或路徑相關錯誤。
    #[error("配置错误：{0}")]
    Config(String),

    /// 檔案存取失敗。
    #[error("文件读写失败：{0}")]
    Io(#[from] std::io::Error),

    /// 終端初始化或復原失敗。
    #[error("终端错误：{0}")]
    Tui(String),
}

impl AppError {
    /// 建立網路錯誤（未分類）。
    pub fn network(message: impl Into<String>) -> Self {
        Self::Network {
            kind: NetworkKind::Other,
            detail: message.into(),
        }
    }

    /// 建立帶類別的網路錯誤。
    pub fn network_kind(kind: NetworkKind, detail: impl Into<String>) -> Self {
        Self::Network {
            kind,
            detail: detail.into(),
        }
    }

    /// 建立回應格式錯誤。
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    /// 建立設定錯誤。
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// 建立「網址被拒絕開啟」錯誤（主機名為空時以佔位字串呈現）。
    ///
    /// 只接受主機名與原因，不接受完整網址——避免呼叫端失手把帶 token 的
    /// 網址寫進使用者可見的訊息。
    pub fn untrusted_url(host: &str, reason: &'static str) -> Self {
        let host = if host.is_empty() {
            "（无主机名）".to_owned()
        } else {
            host.to_owned()
        };
        Self::UntrustedUrl { host, reason }
    }

    /// 建立 WebDAV 錯誤。
    pub fn webdav(message: impl Into<String>) -> Self {
        Self::WebDav(message.into())
    }

    /// 是否屬於「需要重新登入」類錯誤。
    pub fn needs_relogin(&self) -> bool {
        matches!(self, Self::SessionExpired)
    }

    /// 是否屬於「連線層」網路錯誤（逾時、連不上、DNS、TLS）。
    ///
    /// 這類失敗代表這條路由當下實際連不上，多半是短暫的網路抖動：自動
    /// 重試同一個請求有機會成功。與 [`Self::needs_relogin`] 語意互斥——
    /// 登入態失效要靠重新登入處理，不能只重送請求。
    pub fn is_connection_error(&self) -> bool {
        matches!(self, Self::Network { kind, .. } if kind.is_connection_level())
    }
}

#[cfg(test)]
#[path = "tests/error_test.rs"]
mod error_test;
