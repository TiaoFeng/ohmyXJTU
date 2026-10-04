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
    WEBVPN_LOGIN_URL, is_auth_failure, merge_headers, rewrite_for_mode, unknown_site,
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

/// 建立 HTTP 後端的工廠。
///
/// 換帳號時必須取得**全新**的連線池與 cookie jar：只清狀態表不足以丟棄舊帳號
/// 留下的 SSO cookie，殘留的登入態會讓新帳號的登入被判定為「已登入」而略過
/// 帳密提交，之後卻把未經驗證的憑證寫回保險庫。
type ClientFactory = Arc<dyn Fn() -> AppResult<Arc<dyn HttpClient>> + Send + Sync>;

/// 正式執行使用的後端工廠：每次呼叫都建立一個新的 reqwest 客戶端。
fn reqwest_factory() -> ClientFactory {
    Arc::new(|| Ok(Arc::new(ReqwestClient::new(DESKTOP_USER_AGENT)?) as Arc<dyn HttpClient>))
}

/// 站點狀態。
struct SiteState {
    access_mode: AccessMode,
    headers: Vec<(String, String)>,
    user_id: Option<String>,
    /// 使用者資訊解析失敗的原因（負快取，避免逐項重取）。
    user_id_error: Option<String>,
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
    resolved: HashMap<SiteKind, AccessMode>,
    pending: Option<(SiteKind, PendingStage)>,
    sites: HashMap<SiteKind, SiteState>,
    credentials: Option<Credentials>,
    /// 已送出的站點請求數（診斷用；不含校園網探測）。
    requests: usize,
    /// 直連後端工廠（[`Self::reset_session`] 據此重建直連後端）。
    direct_factory: ClientFactory,
    /// WebVPN 後端工廠。
    webvpn_factory: ClientFactory,
}

impl SessionManager {
    /// 建立會話管理器。
    pub fn new(config: &Config) -> AppResult<Self> {
        Self::from_factories(config, reqwest_factory(), reqwest_factory())
    }

    /// 以指定的後端建立管理器（測試用，避免真實網路）。
    ///
    /// 工廠固定回傳同一個注入的後端，因此 [`Self::reset_session`] 不會換掉它；
    /// 需要驗證「換帳號會丟棄舊 cookie jar」時改用 [`Self::with_client_factories`]。
    #[cfg(test)]
    pub fn with_clients(
        config: &Config,
        direct: Arc<dyn HttpClient>,
        webvpn: Arc<dyn HttpClient>,
    ) -> Self {
        let direct_factory: ClientFactory = Arc::new(move || Ok(Arc::clone(&direct)));
        let webvpn_factory: ClientFactory = Arc::new(move || Ok(Arc::clone(&webvpn)));
        Self::from_factories(config, direct_factory, webvpn_factory).expect("建立测试用会话管理器")
    }

    /// 以指定的後端工廠建立管理器（測試用）：每次重建後端都會呼叫工廠。
    #[cfg(test)]
    pub fn with_client_factories(
        config: &Config,
        direct_factory: ClientFactory,
        webvpn_factory: ClientFactory,
    ) -> AppResult<Self> {
        Self::from_factories(config, direct_factory, webvpn_factory)
    }

    /// 直接標記站點已登入（跳過登入流程，測試用）。
    #[cfg(test)]
    pub fn mark_logged_in(
        &mut self,
        site: SiteKind,
        mode: AccessMode,
        headers: Vec<(String, String)>,
    ) {
        self.resolved.insert(site, mode);
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

    /// 依後端工廠建立管理器。
    fn from_factories(
        config: &Config,
        direct_factory: ClientFactory,
        webvpn_factory: ClientFactory,
    ) -> AppResult<Self> {
        let direct = Backend {
            client: direct_factory()?,
            logged_in: false,
        };
        let webvpn = Backend {
            client: webvpn_factory()?,
            logged_in: false,
        };
        Ok(Self {
            policy: config.access_policy,
            visitor_id: config.visitor_id.clone(),
            adapters: Vec::new(),
            direct_factory,
            webvpn_factory,
            direct,
            webvpn,
            probe: None,
            requests: 0,
            resolved: HashMap::new(),
            pending: None,
            sites: HashMap::new(),
            credentials: None,
        })
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
            self.resolved.clear();
            self.webvpn.logged_in = false;
            self.sites.clear();
        }
    }

    /// 站點是否已完成登入。
    pub fn is_logged_in(&self, site: SiteKind) -> bool {
        self.sites.contains_key(&site)
    }

    /// 重建兩個 HTTP 後端並清除所有站點狀態（換帳號時使用）。
    ///
    /// 與 [`Self::set_access_policy`] 不同，不動訪問策略。舊帳號在服務端留下的
    /// SSO cookie 會連同舊後端一起被丟棄（只清狀態表不會清 cookie jar），
    /// 因此下一次 [`Self::next_login_step`] 一定會重新提交憑證。
    ///
    /// 目前已解析的路由、探測快取與站點登入態一併失效。
    pub fn reset_session(&mut self) -> AppResult<()> {
        // 兩個後端都先建好才指派：任一失敗時現有會話維持原狀。
        let direct = (self.direct_factory)()?;
        let webvpn = (self.webvpn_factory)()?;
        self.direct = Backend {
            client: direct,
            logged_in: false,
        };
        self.webvpn = Backend {
            client: webvpn,
            logged_in: false,
        };
        self.reset_state();
        Ok(())
    }

    /// 清除站點登入狀態、已解析的路由與校園網探測快取。
    fn reset_state(&mut self) {
        self.probe = None;
        self.resolved.clear();
        self.pending = None;
        self.sites.clear();
    }

    /// 站點目前使用的訪問方式（尚未解析時為 `None`）。
    pub fn access_mode(&self, site: SiteKind) -> Option<AccessMode> {
        self.sites.get(&site).map(|state| state.access_mode)
    }

    /// 站點已解析的訪問方式（尚未完成登入也會回傳；用於區分登入失敗的後端）。
    pub fn resolved_access_mode(&self, site: SiteKind) -> Option<AccessMode> {
        self.resolved.get(&site).copied()
    }

    /// 已送出的站點請求數（診斷用；不含校園網探測）。
    pub fn request_count(&self) -> usize {
        self.requests
    }

    /// 站點內的識別碼（例如思源學堂的使用者 ID）。
    pub fn site_user_id(&self, site: SiteKind) -> Option<&str> {
        self.sites
            .get(&site)
            .and_then(|state| state.user_id.as_deref())
    }

    /// 記錄站點內的識別碼（站點層稍後才取得時回寫）；同時清除解析失敗的負快取。
    pub fn set_site_user_id(&mut self, site: SiteKind, user_id: String) {
        if let Some(state) = self.sites.get_mut(&site) {
            state.user_id = Some(user_id);
            state.user_id_error = None;
        }
    }

    /// 站點使用者資訊的解析失敗原因（負快取；成功解析或重新登入時清除）。
    pub fn site_user_id_error(&self, site: SiteKind) -> Option<&str> {
        self.sites
            .get(&site)
            .and_then(|state| state.user_id_error.as_deref())
    }

    /// 記錄站點使用者資訊的解析失敗原因，避免對同一站點重複請求。
    pub fn set_site_user_id_error(&mut self, site: SiteKind, message: String) {
        if let Some(state) = self.sites.get_mut(&site) {
            state.user_id_error = Some(message);
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
        if let Some(mode) = self.resolved.get(&site) {
            return Ok(*mode);
        }

        let webvpn_when_off_campus = policy.supports_webvpn && policy.use_webvpn_when_off_campus;
        let mode = match self.policy {
            AccessPolicy::Direct => AccessMode::Direct,
            AccessPolicy::WebVpn if policy.supports_webvpn => AccessMode::WebVpn,
            AccessPolicy::WebVpn => AccessMode::Direct,
            // 只有探測結果真的會改變路由時才做校園網探測（目前僅考勤系統）；
            // 思源學堂校外一律直連，探測沒有意義、只會白白多一次請求。
            AccessPolicy::Auto => {
                if webvpn_when_off_campus && !self.probe_campus_network() {
                    AccessMode::WebVpn
                } else {
                    AccessMode::Direct
                }
            }
        };
        self.resolved.insert(site, mode);
        Ok(mode)
    }

    /// 直連失敗後的有限回退：改用 WebVPN 並要求重新登入該站。
    ///
    /// 僅在使用者策略為 [`AccessPolicy::Auto`]、站點支援 WebVPN、允許校外走
    /// WebVPN，且該站目前解析為直連時生效（回傳 `true`，呼叫方據此重試一次
    /// 並重新登入）；強制模式與已在 WebVPN 的情況一律回傳 `false`，因此同一
    /// 條任務最多只會回退一次。
    pub fn fallback_to_webvpn(&mut self, site: SiteKind) -> bool {
        if self.policy != AccessPolicy::Auto {
            return false;
        }
        let Ok(policy) = self.policy_for(site) else {
            return false;
        };
        if !(policy.supports_webvpn && policy.use_webvpn_when_off_campus) {
            return false;
        }
        if self.resolved.get(&site) != Some(&AccessMode::Direct) {
            return false;
        }

        // 直連已證明不可用：校正探測快取並記下該站改走 WebVPN。
        self.probe = Some((false, Instant::now()));
        self.resolved.insert(site, AccessMode::WebVpn);
        self.invalidate(site);
        true
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
        let login_url = rewrite_for_mode(mode, policy.login_url)?;
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
        let (mode, headers) = self.site_transport(site)?;
        let request = prepare_request(mode, &headers, request)?;

        let client = self.backend(mode).client.clone();
        self.requests += 1;
        let response = client
            .send(request)
            .map_err(|err| with_site_context(err, site, mode))?;
        if is_auth_failure(&response) {
            self.invalidate(site);
            return Err(AppError::SessionExpired);
        }
        Ok(response)
    }

    /// 併發送出同一站點的多個請求，回傳與輸入**同序**的結果。
    ///
    /// 只負責傳輸：標頭注入、WebVPN 改址與請求計數都在這裡處理，解析留給
    /// 呼叫端。與 [`Self::send`] 的差異有兩點：
    ///
    /// - 單一請求的網路錯誤留在各自的結果中（呼叫端可只略過那一項）。
    /// - 只要任一筆回應被判定為登入態失效，整批結果一併丟棄並回傳
    ///   [`AppError::SessionExpired`]，交由呼叫端走既有的統一重新登入流程
    ///   （不讓部分回應混著舊會話的結果回填畫面）。
    pub fn send_batch(
        &mut self,
        site: SiteKind,
        requests: Vec<HttpRequest>,
    ) -> AppResult<Vec<AppResult<HttpResponse>>> {
        let (mode, headers) = self.site_transport(site)?;
        let mut prepared = Vec::with_capacity(requests.len());
        for request in requests {
            prepared.push(prepare_request(mode, &headers, request)?);
        }
        self.requests += prepared.len();

        let client = self.backend(mode).client.clone();
        let responses: Vec<AppResult<HttpResponse>> =
            crate::http::batch::send_concurrently(client, prepared)
                .into_iter()
                .map(|result| result.map_err(|err| with_site_context(err, site, mode)))
                .collect();
        if responses
            .iter()
            .any(|response| response.as_ref().is_ok_and(is_auth_failure))
        {
            self.invalidate(site);
            return Err(AppError::SessionExpired);
        }
        Ok(responses)
    }

    /// 站點目前的訪問方式與要注入的標頭。
    fn site_transport(&self, site: SiteKind) -> AppResult<(AccessMode, Vec<(String, String)>)> {
        let state = self.sites.get(&site).ok_or(AppError::SessionExpired)?;
        Ok((state.access_mode, state.headers.clone()))
    }

    /// 站點登入成功後寫入狀態。
    fn store_site(&mut self, site: SiteKind, mode: AccessMode, login: SiteLogin) {
        self.sites.insert(
            site,
            SiteState {
                access_mode: mode,
                headers: login.headers,
                user_id: login.user_id,
                user_id_error: None,
            },
        );
    }

    /// 依站點目前的訪問方式改寫網址（供開啟外部網頁等使用）。
    ///
    /// 站點尚未登入、訪問方式未知時原樣回傳（瀏覽器開啟後由使用者自行登入）。
    pub fn rewrite_url(&self, site: SiteKind, url: &str) -> AppResult<String> {
        match self.access_mode(site) {
            Some(mode) => rewrite_for_mode(mode, url),
            None => Ok(url.to_owned()),
        }
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

/// 依訪問方式準備要送出的請求：注入站點標頭，必要時改寫為 WebVPN 網址。
///
/// 抽成自由函式（而非 `&self` 方法）是為了讓併發送出時能先**序列地**完成
/// 準備工作，再交給 [`crate::http::batch::send_concurrently`] 平行傳輸。
fn prepare_request(
    mode: AccessMode,
    headers: &[(String, String)],
    request: HttpRequest,
) -> AppResult<HttpRequest> {
    let request = merge_headers(request, headers);
    if mode != AccessMode::WebVpn {
        return Ok(request);
    }

    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("Referer"))
        .cloned()
        .collect();
    if let Some(referer) = request.header_value("Referer").map(str::to_owned)
        && webvpn::should_rewrite(&referer)
    {
        headers.push((
            "Referer".to_owned(),
            rewrite_for_mode(AccessMode::WebVpn, &referer)?,
        ));
    }

    Ok(HttpRequest {
        method: request.method,
        url: rewrite_for_mode(AccessMode::WebVpn, &request.url)?,
        headers,
        body: request.body,
        timeout: request.timeout,
        follow_redirects: request.follow_redirects,
    })
}

/// 為網路錯誤補上「站點（訪問方式）」上下文，方便使用者定位失敗發生在哪裡。
fn with_site_context(err: AppError, site: SiteKind, mode: AccessMode) -> AppError {
    match err {
        AppError::Network { kind, detail } => AppError::network_kind(
            kind,
            format!("{}（{}）：{detail}", site.label(), mode.label()),
        ),
        other => other,
    }
}

#[cfg(test)]
#[path = "tests/manager_test.rs"]
mod manager_test;
