//! 記憶體中的敏感字串包裝。
//!
//! 明文的加密口令、密碼在程式各層之間流動時（表單 → 任務 → 保險庫），
//! 一律使用 [`Secret`]：
//!
//! - 離開作用域或 [`Secret::clear`] 時覆寫底層緩衝（`Zeroizing<String>`）。
//! - [`std::fmt::Debug`] 只輸出遮罩，任何 `{:?}` 都不會洩漏內容。
//! - 不實作 `Display`，避免誤用於輸出。

use std::fmt;
use std::ops::Deref;

use zeroize::{Zeroize, Zeroizing};

/// 自動零化、遮蔽 `Debug` 的字串。
#[derive(Clone, Default)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// 以明文檢視內容。
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// 是否為空字串。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 清空並覆寫底層緩衝。
    ///
    /// 目前只有測試呼叫（生產碼靠 [`Drop`] 自動零化）：保留這個方法讓「用完
    /// 立刻丟掉明文」不用依賴作用域結束。
    pub fn clear(&mut self) {
        self.0.zeroize();
    }
}

impl Deref for Secret {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(Zeroizing::new(value))
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(Zeroizing::new(value.to_owned()))
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Secret {}

impl PartialEq<str> for Secret {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Secret {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

#[cfg(test)]
#[path = "tests/secret_test.rs"]
mod secret_test;
