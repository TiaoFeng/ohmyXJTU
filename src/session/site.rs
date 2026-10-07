//! 站點種類、存取策略與站點擴充點。

use std::fmt;

use url::Url;

use crate::error::{AppError, AppResult};
use crate::http::{HttpClient, HttpRequest, HttpResponse};
use crate::webvpn;

/// 一般桌面瀏覽器的 User-Agent。
pub const DESKTOP_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// WebVPN 站點登入網址。
pub const WEBVPN_LOGIN_URL: &str = "https://webvpn.xjtu.edu.cn/login?cas_login=true";

/// 站點種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SiteKind {
    /// 本科考勤系統。
    Attendance,
    /// 思源學堂。
    Lms,
}

impl SiteKind {
    /// 站點的中文名稱。
    pub fn label(self) -> &'static str {
        match self {
            Self::Attendance => "考勤系统",
            Self::Lms => "思源学堂",
        }
    }
}

impl fmt::Display for SiteKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// 站點的存取策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SitePolicy {
    /// 瀏覽器開啟後會跳轉到統一認證的入口網址。
    pub login_url: &'static str,
    /// 是否支援經 WebVPN 存取。
    pub supports_webvpn: bool,
    /// 自動模式偵測為校外時，是否應改用 WebVPN。
    ///
    /// 考勤系統僅校內可直連，思源學堂為雲端服務、校外可直接連線，
    /// 因此後者維持直連可少一層轉發。
    pub use_webvpn_when_off_campus: bool,
}

/// 站點登入成功後的回傳值。
///
/// 手寫 [`fmt::Debug`]：標頭帶著站點業務憑證（例如考勤的 `X-Business-Token`），
/// 任何 `{:?}` 只輸出標頭**名稱**與「有沒有取到站點識別碼」，不輸出內容。
#[derive(Clone, Default)]
pub struct SiteLogin {
    /// 之後每個請求都要附帶的標頭（例如考勤系統的 `X-Business-Token`）。
    pub headers: Vec<(String, String)>,
    /// 站點內的識別碼（例如思源學堂的使用者 ID）。
    pub user_id: Option<String>,
}

impl fmt::Debug for SiteLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SiteLogin")
            .field("headers", &crate::http::field_names(&self.headers))
            .field("user_id", &self.user_id.is_some())
            .finish()
    }
}

/// 站點登入收尾時可用的請求工具：自動依訪問方式改寫網址。
pub struct PostLogin<'a> {
    client: &'a dyn HttpClient,
    access_mode: AccessMode,
    final_response: Option<&'a HttpResponse>,
}

impl<'a> PostLogin<'a> {
    /// 建立收尾工具。
    pub fn new(
        client: &'a dyn HttpClient,
        access_mode: AccessMode,
        final_response: Option<&'a HttpResponse>,
    ) -> Self {
        Self {
            client,
            access_mode,
            final_response,
        }
    }

    /// 登入成功時的最終回應（供站點取出 ticket 等資訊）。
    pub fn final_response(&self) -> Option<&HttpResponse> {
        self.final_response
    }

    /// 依訪問方式改寫網址後送出 GET。
    pub fn get(&self, url: &str) -> AppResult<HttpResponse> {
        self.client.send(HttpRequest::get(self.url(url)?))
    }

    /// 依訪問方式改寫網址後送出 JSON POST。
    pub fn post_json(&self, url: &str, value: serde_json::Value) -> AppResult<HttpResponse> {
        self.client
            .send(HttpRequest::post_json(self.url(url)?, value))
    }

    fn url(&self, url: &str) -> AppResult<String> {
        rewrite_for_mode(self.access_mode, url)
    }
}

/// 訪問方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AccessMode {
    /// 直接連線。
    #[default]
    Direct,
    /// 經 `webvpn.xjtu.edu.cn` 轉發。
    WebVpn,
}

impl AccessMode {
    /// 簡體中文名稱。
    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "直连",
            Self::WebVpn => "WebVPN",
        }
    }
}

/// 依訪問方式改寫校內網址：WebVPN 模式且適用改寫時改寫，否則原樣回傳。
///
/// 全專案唯一的改址規則入口（站點登入請求、一般請求轉送與外部網頁開啟都經
/// 此函式），避免模式判斷與 `should_rewrite` 條件各自漂移。
pub fn rewrite_for_mode(mode: AccessMode, url: &str) -> AppResult<String> {
    if mode == AccessMode::WebVpn && webvpn::should_rewrite(url) {
        webvpn::to_webvpn_url(url)
    } else {
        Ok(url.to_owned())
    }
}

/// 站點擴充點：站點模組以此掛進會話管理。
pub trait SiteAdapter: Send + Sync {
    /// 站點種類。
    fn kind(&self) -> SiteKind;

    /// 存取策略。
    fn policy(&self) -> SitePolicy;

    /// 登入成功後的收尾（換取業務 token、取得站點內識別碼等）。
    fn post_login(&self, context: &PostLogin<'_>) -> AppResult<SiteLogin>;
}

/// 取出站點必填的標頭，並確保請求自身的同名標頭優先。
pub(crate) fn merge_headers(
    request: HttpRequest,
    site_headers: &[(String, String)],
) -> HttpRequest {
    let mut request = request;
    for (name, value) in site_headers {
        if request.header_value(name).is_none() {
            request = request.header(name.clone(), value.clone());
        }
    }
    request
}

/// 統一身份（Keycloak）主機；部分跳轉會落在這裡而不是統一認證。
pub const IDENTITY_HOST: &str = "identity1.xjtu.edu.cn";

/// 統一認證主機（`login.xjtu.edu.cn`）。
const LOGIN_HOST: &str = "login.xjtu.edu.cn";

/// 判定回應是否代表登入態失效。
///
/// 除了明確的 401 與二次認證頁，也涵蓋各種「被導向登入入口」的情況：統一認證
/// （`login.xjtu.edu.cn`）、統一身份主機、站點自身的 CAS 入口，以及 **WebVPN
/// 閘道本身的登入頁**。閘道登入態過期若漏判，資料查詢只會回報「响应无法解析」
/// 或「待核实」，使用者不會看到自動重新登入。
pub(crate) fn is_auth_failure(response: &HttpResponse) -> bool {
    if response.status == 401 || is_login_endpoint(&response.final_url) {
        return true;
    }
    let is_html = response
        .header("content-type")
        .is_some_and(|value| value.contains("html"));
    if is_html {
        let text = response.text();
        if crate::auth::html::is_safety_verify_page(&text) {
            return true;
        }
    }
    false
}

/// `final_url` 是否為登入入口。
///
/// 只在學校網域內判定（外部網站的 `/login` 與我們無關），並以解析後的主機與
/// 路徑比對，避免以子字串比對整條 URL 而被查詢參數誤導。
fn is_login_endpoint(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if !webvpn::is_school_host(host) {
        return false;
    }

    let path = parsed.path();
    let query = parsed.query();
    if host == webvpn::WEBVPN_HOST {
        // 閘道自身的登入頁。
        if path == "/login" || path.starts_with("/login/") {
            return true;
        }
        // 代理路徑（`/https[-port]/<cipher>/<目標路徑>`）以代理目標判定：
        // 代理的對象是登入入口時，一樣代表登入態已失效。
        let Some(target) = webvpn::proxied_target(path) else {
            return false;
        };
        if target.host.as_deref().is_some_and(is_login_host) {
            return true;
        }
        return is_login_path(&target.path, query);
    }

    is_login_host(host) || is_login_path(path, query)
}

/// 是否為統一認證／統一身份主機（在其上的一切回應都代表仍在登入流程中）。
fn is_login_host(host: &str) -> bool {
    host == IDENTITY_HOST || host == LOGIN_HOST
}

/// 路徑與查詢是否指向登入入口。
fn is_login_path(path: &str, query: Option<&str>) -> bool {
    path.contains("/cas/login")
        || path == "/login"
        || path.starts_with("/login/")
        || query.is_some_and(|query| query.contains("cas_login"))
}

/// 缺少站點設定時的回報。
pub(crate) fn unknown_site(site: SiteKind) -> AppError {
    AppError::protocol(format!("未注册的站点：{site}"))
}

#[cfg(test)]
#[path = "tests/site_test.rs"]
mod site_test;
