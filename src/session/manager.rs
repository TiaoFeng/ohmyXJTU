//! 會話管理：解析訪問方式、編排登入流程、轉送站點請求。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::auth::{LoginDriver, webvpn};
use crate::config::{AccessPolicy, Config};
use crate::credentials::Credentials;
use crate::error::{AppError, AppResult};
use crate::http::{HttpClient, HttpRequest, HttpResponse, ReqwestClient};

use super::site::{
    AccessMode, DESKTOP_USER_AGENT, PostLogin, SiteAdapter, SiteKind, SiteLogin, SitePolicy,
    WEBVPN_LOGIN_URL, is_auth_failure, merge_headers, unknown_site,
};

/// 校內網路探測網址（考勤系統登入入口；校外無法直連）。
const CAMPUS_PROBE_URL: &str = "https://bk-kq.xjtu.edu.cn/sa/auth/cas/login/student-pc";
/// 校內網路探測逾時。
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// 校內網路探測結果快取時間。
const PROBE_TTL: Duration = Duration::from_secs(5 * 60);

/// 登入流程的下一步。
pub enum LoginStage {
    /// 需要驅動這個登入器（可能是 WebVPN 後端，也可能是站點本身）。
    Drive(Box<LoginDriver>),
    /// 站點登入完成。
    Done,
}

/// 目前正在進行的登入階段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingStage {
    WebVpnBackend,
    Site,
}

/// 一個會話後端：一組 cookie 與連線池。
struct Backend {
    client: Arc<dyn HttpClient>,
    logged_in: bool,
}

/// 站點狀態。
struct SiteState {
    access_mode: AccessMode,
    headers: Vec<(String, String)>,
    user_id: Option<String>,
}

/// 會話管理器。
///
/// 管理員負責三件事：
///
/// 1. 依設定與網路探測決定站點要走直連還是 WebVPN。
/// 2. 編排「WebVPN 後端登入 → 站點登入 → 站點收尾」的流程。
/// 3. 轉送站點請求，必要時改寫網址，並在登入態失效時回報 [`AppError::SessionExpired`]。
pub struct SessionManager {
    policy: AccessPolicy,
    visitor_id: String,
    adapters: Vec<Box<dyn SiteAdapter>>,
    direct: Backend,
    webvpn: Backend,
    probe: Option<(bool, Instant)>,
    resolved: Option<(SiteKind, AccessMode)>,
    pending: Option<(SiteKind, PendingStage)>,
    sites: HashMap<SiteKind, SiteState>,
    credentials: Option<Credentials>,
}

impl SessionManager {
    /// 建立會話管理器。
    pub fn new(config: &Config) -> AppResult<Self> {
        let direct = Backend {
            client: Arc::new(ReqwestClient::new(DESKTOP_USER_AGENT)?),
            logged_in: false,
        };
        let webvpn = Backend {
            client: Arc::new(ReqwestClient::new(DESKTOP_USER_AGENT)?),
            logged_in: false,
        };
        Ok(Self::from_backends(config, direct, webvpn))
    }

    /// 以指定的後端建立管理器（測試用，避免真實網路）。
    #[cfg(test)]
    pub fn with_clients(
        config: &Config,
        direct: Arc<dyn HttpClient>,
        webvpn: Arc<dyn HttpClient>,
    ) -> Self {
        Self::from_backends(
            config,
            Backend {
                client: direct,
                logged_in: false,
            },
            Backend {
                client: webvpn,
                logged_in: false,
            },
        )
    }

    /// 直接標記站點已登入（跳過登入流程，測試用）。
    #[cfg(test)]
    pub fn mark_logged_in(
        &mut self,
        site: SiteKind,
        mode: AccessMode,
        headers: Vec<(String, String)>,
    ) {
        self.resolved = Some((site, mode));
        self.store_site(
            site,
            mode,
            SiteLogin {
                headers,
                user_id: None,
            },
        );
        if mode == AccessMode::WebVpn {
            self.webvpn.logged_in = true;
        }
    }

    fn from_backends(config: &Config, direct: Backend, webvpn: Backend) -> Self {
        Self {
            policy: config.access_policy,
            visitor_id: config.visitor_id.clone(),
            adapters: Vec::new(),
            direct,
            webvpn,
            probe: None,
            resolved: None,
            pending: None,
            sites: HashMap::new(),
            credentials: None,
        }
    }

    /// 註冊站點擴充點。
    pub fn register(&mut self, adapter: Box<dyn SiteAdapter>) {
        self.adapters.push(adapter);
    }

    /// 保存登入憑證（供登入態失效後自動重登使用）。
    pub fn set_credentials(&mut self, credentials: Credentials) {
        self.credentials = Some(credentials);
    }

    /// 目前的登入憑證。
    pub fn credentials(&self) -> Option<&Credentials> {
        self.credentials.as_ref()
    }

    /// 目前的訪問策略。
    pub fn access_policy(&self) -> AccessPolicy {
        self.policy
    }

    /// 更新訪問策略並清除已解析的結果與探測快取。
    pub fn set_access_policy(&mut self, policy: AccessPolicy) {
        if self.policy != policy {
            self.policy = policy;
            self.probe = None;
            self.resolved = None;
            self.webvpn.logged_in = false;
            self.sites.clear();
        }
    }

    /// 站點是否已完成登入。
    pub fn is_logged_in(&self, site: SiteKind) -> bool {
        self.sites.contains_key(&site)
    }

    /// 站點目前使用的訪問方式（尚未解析時為 `None`）。
    pub fn access_mode(&self, site: SiteKind) -> Option<AccessMode> {
        self.sites.get(&site).map(|state| state.access_mode)
    }

    /// 站點內的識別碼（例如思源學堂的使用者 ID）。
    pub fn site_user_id(&self, site: SiteKind) -> Option<&str> {
        self.sites
            .get(&site)
            .and_then(|state| state.user_id.as_deref())
    }

    /// 記錄站點內的識別碼（站點層稍後才取得時回寫）。
    pub fn set_site_user_id(&mut self, site: SiteKind, user_id: String) {
        if let Some(state) = self.sites.get_mut(&site) {
            state.user_id = Some(user_id);
        }
    }

    /// 使站點登入態失效（下一次 [`Self::next_login_step`] 會重新登入）。
    pub fn invalidate(&mut self, site: SiteKind) {
        if let Some(state) = self.sites.remove(&site)
            && state.access_mode == AccessMode::WebVpn
        {
            // WebVPN 代理的登入態通常與站點同時失效，需一併重新確認。
            self.webvpn.logged_in = false;
        }
    }

    /// 解析站點的訪問方式。
    pub fn resolve_access_mode(&mut self, site: SiteKind) -> AppResult<AccessMode> {
        let policy = self.policy_for(site)?;
        if let Some((resolved_site, mode)) = self.resolved
            && resolved_site == site
        {
            return Ok(mode);
        }

        let mode = match self.policy {
            AccessPolicy::Direct => AccessMode::Direct,
            AccessPolicy::WebVpn if policy.supports_webvpn => AccessMode::WebVpn,
            AccessPolicy::WebVpn => AccessMode::Direct,
            AccessPolicy::Auto => {
                if self.probe_campus_network() {
                    AccessMode::Direct
                } else if policy.supports_webvpn && policy.use_webvpn_when_off_campus {
                    AccessMode::WebVpn
                } else {
                    AccessMode::Direct
                }
            }
        };
        self.resolved = Some((site, mode));
        Ok(mode)
    }

    /// 取得登入流程的下一步。
    pub fn next_login_step(&mut self, site: SiteKind) -> AppResult<LoginStage> {
        if self.is_logged_in(site) {
            return Ok(LoginStage::Done);
        }

        let mode = self.resolve_access_mode(site)?;
        if mode == AccessMode::WebVpn && !self.webvpn.logged_in {
            let client = Arc::clone(&self.webvpn.client);
            let driver = LoginDriver::new(client, WEBVPN_LOGIN_URL, &self.visitor_id)?;
            self.pending = Some((site, PendingStage::WebVpnBackend));
            return Ok(LoginStage::Drive(Box::new(driver)));
        }

        let policy = self.policy_for(site)?;
        let login_url = if mode == AccessMode::WebVpn && webvpn::should_rewrite(policy.login_url) {
            webvpn::to_webvpn_url(policy.login_url)?
        } else {
            policy.login_url.to_owned()
        };
        let client = self.backend(mode).client.clone();
        let driver = LoginDriver::new(client, &login_url, &self.visitor_id)?;
        self.pending = Some((site, PendingStage::Site));
        Ok(LoginStage::Drive(Box::new(driver)))
    }

    /// 完成目前登入階段，回傳下一步。
    pub fn complete_login_step(
        &mut self,
        site: SiteKind,
        driver: &LoginDriver,
    ) -> AppResult<LoginStage> {
        let (pending_site, stage) = self
            .pending
            .take()
            .ok_or_else(|| AppError::protocol("当前没有进行中的登录流程"))?;
        if pending_site != site {
            return Err(AppError::protocol("登录流程与站点不匹配"));
        }

        match stage {
            PendingStage::WebVpnBackend => {
                self.webvpn.logged_in = true;
                self.next_login_step(site)
            }
            PendingStage::Site => {
                let mode = self.resolve_access_mode(site)?;
                let client = driver.client();
                let context = PostLogin::new(client.as_ref(), mode, driver.final_response());
                let login = {
                    let adapter = self
                        .adapters
                        .iter()
                        .find(|adapter| adapter.kind() == site)
                        .ok_or_else(|| unknown_site(site))?;
                    adapter.post_login(&context)?
                };
                self.store_site(site, mode, login);
                Ok(LoginStage::Done)
            }
        }
    }

    /// 轉送站點請求。
    ///
    /// 站點尚未登入或登入態已失效時回報 [`AppError::SessionExpired`]，
    /// 由呼叫端重新走一次登入流程後重試。
    pub fn send(&mut self, site: SiteKind, request: HttpRequest) -> AppResult<HttpResponse> {
        let (mode, headers) = {
            let state = self.sites.get(&site).ok_or(AppError::SessionExpired)?;
            (state.access_mode, state.headers.clone())
        };

        let mut request = merge_headers(request, &headers);
        if mode == AccessMode::WebVpn {
            request = self.rewrite_for_webvpn(request)?;
        }

        let client = self.backend(mode).client.clone();
        let response = client.send(request)?;
        if is_auth_failure(&response) {
            self.invalidate(site);
            return Err(AppError::SessionExpired);
        }
        Ok(response)
    }

    /// 站點登入成功後寫入狀態。
    fn store_site(&mut self, site: SiteKind, mode: AccessMode, login: SiteLogin) {
        self.sites.insert(
            site,
            SiteState {
                access_mode: mode,
                headers: login.headers,
                user_id: login.user_id,
            },
        );
    }

    /// 將請求網址與 `Referer` 改寫為 WebVPN 網址。
    fn rewrite_for_webvpn(&self, request: HttpRequest) -> AppResult<HttpRequest> {
        let mut headers: Vec<(String, String)> = request
            .headers
            .iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("Referer"))
            .cloned()
            .collect();
        if let Some(referer) = request.header_value("Referer").map(str::to_owned)
            && webvpn::should_rewrite(&referer)
        {
            headers.push(("Referer".to_owned(), webvpn::to_webvpn_url(&referer)?));
        }

        Ok(HttpRequest {
            method: request.method,
            url: webvpn::to_webvpn_url(&request.url)?,
            headers,
            body: request.body,
            timeout: request.timeout,
            follow_redirects: request.follow_redirects,
        })
    }

    /// 探測校內網路是否可直連（結果快取五分鐘）。
    fn probe_campus_network(&mut self) -> bool {
        if let Some((value, at)) = self.probe
            && at.elapsed() < PROBE_TTL
        {
            return value;
        }

        let request = HttpRequest::get(CAMPUS_PROBE_URL)
            .no_redirect()
            .timeout(PROBE_TIMEOUT);
        let reachable = self
            .direct
            .client
            .send(request)
            .is_ok_and(|response| response.status < 500);

        self.probe = Some((reachable, Instant::now()));
        reachable
    }

    fn backend(&self, mode: AccessMode) -> &Backend {
        match mode {
            AccessMode::Direct => &self.direct,
            AccessMode::WebVpn => &self.webvpn,
        }
    }

    fn policy_for(&self, site: SiteKind) -> AppResult<SitePolicy> {
        self.adapters
            .iter()
            .find(|adapter| adapter.kind() == site)
            .map(|adapter| adapter.policy())
            .ok_or_else(|| unknown_site(site))
    }
}

#[cfg(test)]
#[path = "tests/manager_test.rs"]
mod manager_test;
