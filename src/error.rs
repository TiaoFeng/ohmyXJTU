//! 應用程式錯誤型別。
//!
//! `Display` 的輸出即為面向使用者的簡體中文訊息，可直接顯示於 TUI 狀態列。

use thiserror::Error;

/// 應用程式統一的結果型別。
pub type AppResult<T> = Result<T, AppError>;

/// 所有可預期錯誤的集合。
#[derive(Debug, Error)]
pub enum AppError {
    /// 網路連線失敗、逾時或無法讀取回應。
    #[error("网络连接失败：{0}")]
    Network(String),

    /// 伺服器回應了非預期的 HTTP 狀態碼。
    #[error("服务器返回异常状态码：{status}")]
    Http { status: u16 },

    /// 伺服器回應了業務錯誤（形如 `{"code": .., "message": ..}`）。
    #[error("服务返回错误（{code}）：{message}")]
    Server { code: i64, message: String },

    /// 登入態已失效，需要重新登入。
    #[error("登录状态已失效，请重新登录")]
    SessionExpired,

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
    /// 建立網路錯誤。
    pub fn network(message: impl Into<String>) -> Self {
        Self::Network(message.into())
    }

    /// 建立回應格式錯誤。
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    /// 建立設定錯誤。
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// 是否屬於「需要重新登入」類錯誤。
    pub fn needs_relogin(&self) -> bool {
        matches!(self, Self::SessionExpired)
    }
}
