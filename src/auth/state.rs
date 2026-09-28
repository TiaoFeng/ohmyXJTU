//! 登入流程的狀態定義。

/// 帳號身份類型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountType {
    /// 本科生。
    Undergraduate,
    /// 研究生。
    Postgraduate,
}

impl AccountType {
    /// 身份選項名稱中的關鍵字。
    fn keyword(self) -> &'static str {
        match self {
            Self::Undergraduate => "本科",
            Self::Postgraduate => "研究",
        }
    }

    /// 簡體中文名稱。
    pub fn label(self) -> &'static str {
        match self {
            Self::Undergraduate => "本科",
            Self::Postgraduate => "研究生",
        }
    }

    /// 在身份選項中挑出對應的 `label`。
    pub fn select(self, choices: &[super::html::AccountChoice]) -> Option<&str> {
        choices
            .iter()
            .find(|choice| choice.name.contains(self.keyword()))
            .map(|choice| choice.label.as_str())
    }
}

/// MFA（兩步驗證）流程類型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfaFlow {
    /// 登入前的 MFA 偵測（`/cas/mfa`）。
    Detect,
    /// 登入後的二次認證（`/cas/sec`）。
    SafetyVerify,
}

impl MfaFlow {
    /// 對應的網址片段。
    pub fn path_segment(self) -> &'static str {
        match self {
            Self::Detect => "mfa",
            Self::SafetyVerify => "sec",
        }
    }
}

/// 一次 `login` 呼叫的結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginReply {
    /// 登入成功。
    Success,
    /// 登入失敗，附帶可直接顯示的錯誤訊息。
    Fail {
        /// 錯誤訊息。
        message: String,
    },
    /// 需要輸入圖片驗證碼。
    NeedCaptcha,
    /// 需要簡訊驗證碼。
    NeedMfa,
    /// 需要選擇帳號身份。
    NeedAccountChoice(Vec<AccountChoice>),
}

pub use super::html::AccountChoice;
