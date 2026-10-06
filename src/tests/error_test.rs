//! `AppError` 的失敗分類：哪些錯誤值得自動重試。
//!
//! 自動重試的代價是「使用者多等幾秒才看到失敗」，因此判準要保守：只有
//! 短暫、重送請求就可能成功的失敗（連線層網路錯誤）才重試；伺服器已經
//! 明確回應的失敗（狀態碼、業務錯誤、格式不符）、以及憑證或驗證碼錯誤
//! 一律直接回報，讓使用者立刻採取對應行動。
//!
//! 這裡刻意不提供「兩者合起來」的判定（例如已移除的 `is_retryable`）：重試
//! 手段不同——連線層錯誤只要重送請求，登入態失效必須先重新登入——合併成
//! 一個布林值會讓呼叫端無法決定要做哪一件事。

use super::{AppError, NetworkKind};

/// 連線層錯誤（逾時、連不上、DNS、TLS）可自動重試。
#[test]
fn connection_level_errors_are_retryable() {
    for kind in [
        NetworkKind::Dns,
        NetworkKind::Connect,
        NetworkKind::Tls,
        NetworkKind::Timeout,
    ] {
        let err = AppError::network_kind(kind, "x");
        assert!(err.is_connection_error(), "{kind:?} 屬於連線層");
        assert!(!err.needs_relogin(), "{kind:?} 不是登录态失效");
    }
}

/// 伺服器有回應的失敗不重試：重送同樣的請求只會得到同樣的結果。
#[test]
fn non_connection_errors_are_not_retryable() {
    let cases = [
        AppError::network_kind(NetworkKind::HttpParse, "x"),
        AppError::network_kind(NetworkKind::Redirect, "x"),
        AppError::network_kind(NetworkKind::Other, "x"),
        AppError::Http { status: 500 },
        AppError::Server {
            code: 1,
            message: "x".to_owned(),
        },
        AppError::protocol("x"),
        AppError::Crypto("x".to_owned()),
        AppError::WrongPassphrase,
        AppError::VaultFile("x".to_owned()),
        AppError::ReloginExhausted,
        AppError::VerificationRetry("x".to_owned()),
        AppError::UntrustedHost {
            host: "x".to_owned(),
        },
        AppError::TaskNotFound,
        AppError::config("x"),
    ];
    for err in cases {
        assert!(!err.is_connection_error(), "{err} 不是连线层错误");
        assert!(!err.needs_relogin(), "{err} 不需要重新登入");
    }
}

/// 登入態失效要靠重新登入恢復：不是連線層錯誤，也不該被當成同類。
///
/// 兩者分開判定的原因是重試手段不同——連線錯誤只要重送請求，登入態失效
/// 必須先重新登入；混為一談會讓呼叫端無法決定要做哪一件事。
#[test]
fn session_expiry_is_not_a_connection_error() {
    assert!(AppError::SessionExpired.needs_relogin());
    assert!(!AppError::SessionExpired.is_connection_error());
}
