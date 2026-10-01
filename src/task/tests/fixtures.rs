//! 測試共用 fixture：登入流程的假頁面、站點位址常數與測試用公鑰。
//!
//! 由 `worker_test` 與 `scheduler_test` 共用（以 `#[path]` 由 `worker` 模組
//! 引入）；內容全部離線構造，不含任何真實憑證或回應。

use std::sync::OnceLock;

use ::rsa::pkcs8::EncodePublicKey as _;
use ::rsa::{RsaPrivateKey, RsaPublicKey};

/// 考勤站點的登入頁位址（同時是帳密表單的提交位址）。
pub(super) const ATTENDANCE_POST: &str = "https://login.xjtu.edu.cn/cas/login?service=attendance";
/// 考勤站點的登入回跳位址（帶 `loginRequestId` 與 `ticket`）。
pub(super) const ATTENDANCE_TARGET: &str =
    "https://bk-kq.xjtu.edu.cn/sa/auth/cas/student-pc?loginRequestId=req-1&ticket=ticket-1";
/// 考勤站點的業務 token 交換端點。
pub(super) const ATTENDANCE_EXCHANGE: &str = "https://bk-kq.xjtu.edu.cn/sa/auth/cas/exchange";
/// 思源學堂的登入頁位址（同時是帳密表單的提交位址）。
pub(super) const LMS_POST: &str = "https://login.xjtu.edu.cn/cas/login?service=lms";
/// 思源學堂首頁位址。
pub(super) const LMS_HOME: &str = "https://lms.xjtu.edu.cn/user/index";
/// 思源學堂課程清單端點。
pub(super) const LMS_COURSES: &str = "https://lms.xjtu.edu.cn/api/my-courses";
/// 登入成功後回傳的目標網頁。
pub(super) const TARGET_BODY: &str =
    "<html><head><title>思源学堂</title></head><body>globalData</body></html>";

/// 測試用公鑰 PEM（2048 位元金鑰產生較慢，整個測試二進位檔共用一份）。
pub(super) fn public_key_pem() -> &'static str {
    static PEM: OnceLock<String> = OnceLock::new();
    PEM.get_or_init(|| {
        let mut rng = chacha20poly1305::aead::OsRng;
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("生成测试密钥");
        RsaPublicKey::from(&private)
            .to_public_key_pem(::rsa::pkcs8::LineEnding::LF)
            .expect("导出公钥 PEM")
    })
}

/// 統一認證登入頁（含 `execution`）；`mfa_enabled` 控制是否要求簡訊驗證。
///
/// 注意：原始碼中的 `\"` 會原樣出現在頁面文字裡，測試裡要改 `mfaEnabled`
/// 必須比對整段（見下方兩個包裝函式）。
pub(super) fn login_page_with(mfa_enabled: bool) -> String {
    format!(
        r#"<html><head><script>
    var globalConfig = eval('(' + "{{\"mfaEnabled\":{mfa_enabled}}}" + ')');
    </script></head><body>
    <input type="hidden" name="execution" value="e1s1" />
    </body></html>"#
    )
}

/// 統一認證登入頁（不需要簡訊驗證）。
pub(super) fn login_page() -> String {
    login_page_with(false)
}

/// 統一認證登入頁（需要簡訊驗證）。
pub(super) fn login_page_with_mfa() -> String {
    login_page_with(true)
}
