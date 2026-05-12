//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::kiro::credential_import::{LoadKiroCliCredentialOptions, load_kiro_cli_credential};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;

use super::error::AdminServiceError;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse, CredentialStatusItem,
    CredentialsStatusResponse, KiroCliOAuthCallbackRequest, KiroCliOAuthLoginRequest,
    KiroCliOAuthLoginResponse, KiroCliOAuthLoginStatus, LoadBalancingModeResponse,
    SetLoadBalancingModeRequest, SuccessResponse,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
    kiro_cli_oauth: Arc<Mutex<KiroCliOAuthLoginStatus>>,
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
    ) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));

        let balance_cache = Self::load_balance_cache_from(&cache_path);

        Self {
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            kiro_cli_oauth: Arc::new(Mutex::new(KiroCliOAuthLoginStatus::default())),
        }
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let default_endpoint = self.token_manager.config().default_endpoint.clone();

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| CredentialStatusItem {
                id: entry.id,
                priority: entry.priority,
                disabled: entry.disabled,
                failure_count: entry.failure_count,
                is_current: entry.id == snapshot.current_id,
                expires_at: entry.expires_at,
                auth_method: entry.auth_method,
                has_profile_arn: entry.has_profile_arn,
                refresh_token_hash: entry.refresh_token_hash,
                api_key_hash: entry.api_key_hash,
                masked_api_key: entry.masked_api_key,
                email: entry.email,
                account_id_hash: entry.account_id_hash,
                success_count: entry.success_count,
                last_used_at: entry.last_used_at.clone(),
                has_proxy: entry.has_proxy,
                proxy_url: entry.proxy_url,
                refresh_failure_count: entry.refresh_failure_count,
                disabled_reason: entry.disabled_reason,
                endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials,
        }
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        let remaining = (usage_limit - current_usage).max(0.0);
        let usage_percentage = if usage_limit > 0.0 {
            (current_usage / usage_limit * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
        })
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 校验端点名：未指定则默认合法，指定则必须已注册
        if let Some(ref name) = req.endpoint {
            if !self.known_endpoints.contains(name) {
                let mut known: Vec<&str> =
                    self.known_endpoints.iter().map(|s| s.as_str()).collect();
                known.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知端点 \"{}\"，已注册端点: {:?}",
                    name, known
                )));
            }
        }

        // 构建凭据对象
        let email = req.email.clone();
        let new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: req.refresh_token,
            profile_arn: None,
            expires_at: None,
            auth_method: Some(req.auth_method),
            client_id: req.client_id,
            client_secret: req.client_secret,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            account_id_hash: None,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 启动官方 Kiro CLI OAuth 登录，并在成功后导入到当前 token manager。
    pub fn start_kiro_cli_oauth_login(
        &self,
        req: KiroCliOAuthLoginRequest,
    ) -> Result<KiroCliOAuthLoginResponse, AdminServiceError> {
        {
            let status = self.kiro_cli_oauth.lock();
            if status.running {
                return Err(AdminServiceError::InvalidCredential(
                    "已有 Kiro CLI OAuth 登录任务正在运行".to_string(),
                ));
            }
        }

        let cli_path = resolve_kiro_cli_path(req.cli_path.as_deref()).map_err(|e| {
            AdminServiceError::InvalidCredential(format!("未找到官方 Kiro CLI: {}", e))
        })?;

        let token_manager = self.token_manager.clone();
        let status_state = self.kiro_cli_oauth.clone();
        let initial_status = KiroCliOAuthLoginStatus {
            running: true,
            phase: "starting".to_string(),
            started_at: Some(Utc::now().to_rfc3339()),
            ..Default::default()
        };
        *status_state.lock() = initial_status.clone();

        let runtime_handle = tokio::runtime::Handle::current();
        thread::spawn(move || {
            run_kiro_cli_oauth_task(status_state, token_manager, runtime_handle, cli_path, req);
        });

        Ok(KiroCliOAuthLoginResponse {
            success: true,
            message: "Kiro CLI OAuth 登录已启动".to_string(),
            status: initial_status,
        })
    }

    /// 获取官方 Kiro CLI OAuth 登录状态。
    pub fn get_kiro_cli_oauth_status(&self) -> KiroCliOAuthLoginStatus {
        self.kiro_cli_oauth.lock().clone()
    }

    /// 手动提交官方 Kiro CLI OAuth callback。
    pub async fn submit_kiro_cli_oauth_callback(
        &self,
        req: KiroCliOAuthCallbackRequest,
    ) -> Result<SuccessResponse, AdminServiceError> {
        let callback_url = normalize_kiro_callback_url(&req.callback_url)
            .map_err(|err| AdminServiceError::InvalidCredential(err.to_string()))?;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|err| AdminServiceError::InternalError(err.to_string()))?;

        let response = client
            .get(callback_url.as_str())
            .send()
            .await
            .map_err(|err| {
                AdminServiceError::InvalidCredential(format!(
                    "提交 OAuth callback 到本机 Kiro CLI 失败: {}",
                    err
                ))
            })?;

        let status_code = response.status();
        if !status_code.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(AdminServiceError::InvalidCredential(format!(
                "Kiro CLI callback 返回失败: {} {}",
                status_code, body
            )));
        }

        update_oauth_status(&self.kiro_cli_oauth, |status| {
            push_oauth_output(status, "已提交 OAuth callback 给本机 Kiro CLI".to_string());
        });

        Ok(SuccessResponse::new("OAuth callback 已提交"))
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 丢弃超过 TTL 的条目
                if (now - v.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. API Key 凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API Key 凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 3. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error =
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误）
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 4. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据")
        {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }
}

fn resolve_kiro_cli_path(input: Option<&str>) -> anyhow::Result<PathBuf> {
    if let Some(path) = input.and_then(non_empty_str) {
        let path = expand_tilde(path);
        if path.exists() {
            return Ok(path);
        }
        anyhow::bail!("路径不存在: {}", path.display());
    }

    for name in ["kiro-cli", "kiro"] {
        if let Some(path) = find_executable_in_path(name) {
            return Ok(path);
        }
    }

    let candidates = [
        "/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli",
        "/Applications/Kiro.app/Contents/MacOS/kiro-cli",
        "/usr/bin/kiro-cli",
        "/usr/local/bin/kiro-cli",
        "/usr/bin/kiro",
        "/usr/local/bin/kiro",
        "/opt/kiro/kiro-cli",
        "/opt/Kiro/kiro-cli",
        "/opt/Kiro/resources/app/bin/kiro-cli",
        "/opt/homebrew/bin/kiro-cli",
    ];

    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Ok(path);
        }
    }

    anyhow::bail!("请安装 Kiro CLI，或指定 cliPath")
}

fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

fn run_kiro_cli_oauth_task(
    status_state: Arc<Mutex<KiroCliOAuthLoginStatus>>,
    token_manager: Arc<MultiTokenManager>,
    runtime_handle: tokio::runtime::Handle,
    cli_path: PathBuf,
    req: KiroCliOAuthLoginRequest,
) {
    let force_logout = req.force_logout.unwrap_or(false);
    let use_device_flow = req.use_device_flow.unwrap_or(false);

    if force_logout {
        update_oauth_status(&status_state, |status| {
            status.phase = "logging-out".to_string();
            push_oauth_output(status, "正在退出官方 Kiro CLI 当前账号".to_string());
        });

        if let Err(err) = run_kiro_cli_logout(&cli_path, &status_state) {
            finish_oauth_status(
                &status_state,
                "failed",
                None,
                Some(format!("退出官方 Kiro CLI 当前账号失败: {}", err)),
                None,
            );
            return;
        }
    }

    update_oauth_status(&status_state, |status| {
        status.phase = "running-cli".to_string();
        push_oauth_output(status, format!("启动官方 Kiro CLI: {}", cli_path.display()));
        if use_device_flow {
            push_oauth_output(
                status,
                "使用 AWS device flow 登录；这通常对应 Builder ID 免费额度".to_string(),
            );
        } else {
            push_oauth_output(
                status,
                "使用 Kiro 官方浏览器 OAuth 登录；请在打开的页面选择 GitHub/Google/Kiro 账号"
                    .to_string(),
            );
        }
    });

    if !use_device_flow {
        close_stale_oauth_callback_tabs(&status_state);
        spawn_safari_oauth_watcher(status_state.clone());
    }

    let mut command = Command::new(&cli_path);
    sanitize_kiro_cli_env(&mut command);
    command.arg("login");
    if use_device_flow {
        command.arg("--use-device-flow");
    }
    command
        .arg("--license")
        .arg(non_empty_owned(req.license.clone()).unwrap_or_else(|| "free".to_string()))
        .arg("-v")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if use_device_flow {
        command.env("Q_FAKE_IS_REMOTE", "1");
    }

    if let Some(identity_provider) = non_empty_owned(req.identity_provider.clone()) {
        command.arg("--identity-provider").arg(identity_provider);
    }
    if let Some(region) = non_empty_owned(req.region.clone()) {
        command.arg("--region").arg(region);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            finish_oauth_status(
                &status_state,
                "failed",
                None,
                Some(format!("启动 Kiro CLI 失败: {}", err)),
                None,
            );
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut readers = Vec::new();

    if let Some(stdout) = stdout {
        readers.push(spawn_oauth_reader(
            status_state.clone(),
            stdout,
            !use_device_flow,
        ));
    }
    if let Some(stderr) = stderr {
        readers.push(spawn_oauth_reader(
            status_state.clone(),
            stderr,
            !use_device_flow,
        ));
    }

    let exit_status = match child.wait() {
        Ok(status) => status,
        Err(err) => {
            finish_oauth_status(
                &status_state,
                "failed",
                None,
                Some(format!("等待 Kiro CLI 登录结束失败: {}", err)),
                None,
            );
            return;
        }
    };

    for reader in readers {
        let _ = reader.join();
    }

    let exit_code = exit_status.code();
    let already_logged_in = oauth_output_contains(&status_state, "Already logged in");
    if !exit_status.success() && (!already_logged_in || force_logout) {
        finish_oauth_status(
            &status_state,
            "failed",
            exit_code,
            Some(format!(
                "Kiro CLI 登录失败，退出码: {}",
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            )),
            None,
        );
        return;
    }

    update_oauth_status(&status_state, |status| {
        status.phase = "importing".to_string();
        if exit_status.success() {
            push_oauth_output(status, "Kiro CLI 登录完成，正在导入凭据".to_string());
        } else {
            push_oauth_output(status, "官方 Kiro CLI 已登录，直接导入现有凭据".to_string());
        }
    });

    let credential = match load_kiro_cli_credential(LoadKiroCliCredentialOptions {
        db_path: req.db_path,
        priority: req.priority,
        region: req.region,
        auth_region: req.auth_region,
        api_region: req.api_region,
    }) {
        Ok(credential) => credential,
        Err(err) => {
            finish_oauth_status(
                &status_state,
                "failed",
                exit_code,
                Some(format!("导入 Kiro CLI 凭据失败: {}", err)),
                None,
            );
            return;
        }
    };

    match add_imported_credential(&runtime_handle, &token_manager, credential) {
        Ok(id) => finish_oauth_status(&status_state, "completed", exit_code, None, Some(id)),
        Err(err) => finish_oauth_status(
            &status_state,
            "failed",
            exit_code,
            Some(format!("添加凭据到当前服务失败: {}", err)),
            None,
        ),
    }
}

fn run_kiro_cli_logout(
    cli_path: &Path,
    status_state: &Arc<Mutex<KiroCliOAuthLoginStatus>>,
) -> anyhow::Result<()> {
    let mut command = Command::new(cli_path);
    sanitize_kiro_cli_env(&mut command);
    let output = command
        .arg("logout")
        .arg("-v")
        .output()
        .map_err(|err| anyhow::anyhow!("启动 logout 失败: {}", err))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}\n{}", stdout, stderr);

    update_oauth_status(status_state, |status| {
        for line in stdout.lines().chain(stderr.lines()) {
            let line = line.trim();
            if !line.is_empty() {
                push_oauth_output(status, line.to_string());
            }
        }
    });

    if output.status.success() || logout_failure_is_safe(&combined) {
        update_oauth_status(status_state, |status| {
            push_oauth_output(status, "已准备好登录新的官方 Kiro CLI 账号".to_string());
        });
        return Ok(());
    }

    anyhow::bail!(
        "logout 退出码: {}",
        output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string())
    )
}

fn logout_failure_is_safe(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("not logged in")
        || lower.contains("not currently logged")
        || lower.contains("no valid token")
}

fn sanitize_kiro_cli_env(command: &mut Command) {
    for key in [
        "STY",
        "WINDOW",
        "TERMCAP",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "Q_TERM",
        "Q_TERM_TMUX",
        "Q_PARENT",
        "Q_SET_PARENT",
        "Q_SET_PARENT_CHECK",
        "Q_NEW_SESSION",
    ] {
        command.env_remove(key);
    }

    command.env("TERM", "dumb");
}

fn close_stale_oauth_callback_tabs(status_state: &Arc<Mutex<KiroCliOAuthLoginStatus>>) {
    #[cfg(target_os = "macos")]
    {
        let script = r#"
if application "Safari" is running then
  tell application "Safari"
    repeat with w in windows
      repeat with i from (count of tabs of w) to 1 by -1
        set u to URL of tab i of w
        if u starts with "https://localhost:3128/oauth/callback" or u starts with "http://localhost:3128/oauth/callback" or u starts with "https://127.0.0.1:3128/oauth/callback" or u starts with "http://127.0.0.1:3128/oauth/callback" or (u contains "app.kiro.dev/signin" and u contains "redirect_from=kirocli") then
          close tab i of w
        end if
      end repeat
    end repeat
  end tell
end if
"#;

        match Command::new("osascript").arg("-e").arg(script).output() {
            Ok(output) if output.status.success() => {
                update_oauth_status(status_state, |status| {
                    push_oauth_output(
                        status,
                        "已清理 Safari 中旧的 Kiro callback 标签".to_string(),
                    );
                });
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                update_oauth_status(status_state, |status| {
                    push_oauth_output(
                        status,
                        format!(
                            "清理旧 callback 标签失败，可手动关闭旧 localhost:3128 页面: {}",
                            stderr.trim()
                        ),
                    );
                });
            }
            Err(err) => {
                update_oauth_status(status_state, |status| {
                    push_oauth_output(
                        status,
                        format!(
                            "清理旧 callback 标签失败，可手动关闭旧 localhost:3128 页面: {}",
                            err
                        ),
                    );
                });
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = status_state;
    }
}

fn spawn_oauth_reader<R>(
    status_state: Arc<Mutex<KiroCliOAuthLoginStatus>>,
    reader: R,
    open_isolated_browser: bool,
) -> thread::JoinHandle<()>
where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            update_oauth_status(&status_state, |status| {
                if status.phase == "running-cli" {
                    status.phase = "waiting-for-user".to_string();
                }
                if status.login_url.is_none() {
                    if let Some(url) = extract_url(&line) {
                        let open_result = open_browser_url(&url, open_isolated_browser);
                        status.login_url = Some(url);
                        match open_result {
                            BrowserOpenResult::Isolated => push_oauth_output(
                                status,
                                "已用独立浏览器配置打开官方登录页，避免复用当前浏览器账号"
                                    .to_string(),
                            ),
                            BrowserOpenResult::Default => {
                                if open_isolated_browser {
                                    push_oauth_output(
                                        status,
                                    "未找到可用的独立 Chromium/Chrome/Edge/Brave，已用默认浏览器打开；如果仍自动登录，请把登录链接复制到隐身窗口"
                                            .to_string(),
                                    );
                                }
                            }
                            BrowserOpenResult::Failed => push_oauth_output(
                                status,
                                "自动打开浏览器失败，请手动打开登录链接".to_string(),
                            ),
                        }
                    }
                }
                push_oauth_output(status, line);
            });
        }
    })
}

fn spawn_safari_oauth_watcher(status_state: Arc<Mutex<KiroCliOAuthLoginStatus>>) {
    #[cfg(target_os = "macos")]
    {
        thread::spawn(move || {
            let mut seen = HashSet::new();
            let started_at = std::time::Instant::now();
            while started_at.elapsed() < std::time::Duration::from_secs(300) {
                if !status_state.lock().running {
                    break;
                }

                thread::sleep(std::time::Duration::from_millis(500));

                let output = Command::new("osascript")
                    .arg("-e")
                    .arg(
	                        r#"
	if application "Safari" is running then
	  tell application "Safari"
	    set out to ""
	    repeat with w in windows
	      repeat with i from (count of tabs of w) to 1 by -1
	        set u to URL of tab i of w
	        if u starts with "https://localhost:3128/oauth/callback" or u starts with "http://localhost:3128/oauth/callback" or u starts with "https://127.0.0.1:3128/oauth/callback" or u starts with "http://127.0.0.1:3128/oauth/callback" then
	          set out to out & "callback " & u & linefeed
	          close tab i of w
	        else if u contains "app.kiro.dev/signin" and u contains "redirect_from=kirocli" then
	          set out to out & "signin " & u & linefeed
	          close tab i of w
	        end if
	      end repeat
	    end repeat
	    return out
  end tell
end if
"#,
                    )
                    .output();

                let Ok(output) = output else {
                    continue;
                };
                if !output.status.success() {
                    continue;
                }

                let urls = String::from_utf8_lossy(&output.stdout);
                for line in urls.lines().map(str::trim).filter(|line| !line.is_empty()) {
                    if !seen.insert(line.to_string()) {
                        continue;
                    }

                    let Some((kind, url)) = line.split_once(' ') else {
                        continue;
                    };

                    if kind == "signin" {
                        let should_open = {
                            let mut status = status_state.lock();
                            let is_same_url = status.login_url.as_deref() == Some(url);
                            if status.login_url.is_none() {
                                status.login_url = Some(url.to_string());
                            }
                            push_oauth_output(
                                &mut status,
                                "已接管 Safari 打开的 Kiro 登录页，改用独立 Chromium 打开"
                                    .to_string(),
                            );
                            !is_same_url
                        };

                        if should_open {
                            let opened = open_isolated_browser_url(url);
                            update_oauth_status(&status_state, |status| {
                                if opened {
                                    push_oauth_output(
                                        status,
                                        "已用独立 Chromium 打开官方登录页".to_string(),
                                    );
                                } else {
                                    push_oauth_output(
	                                        status,
	                                        "未找到可用的独立 Chromium，可能仍需要手动把登录链接复制到 Chrome 隐身窗口"
	                                            .to_string(),
	                                    );
                                }
                            });
                        }
                        continue;
                    }

                    if kind != "callback" {
                        continue;
                    }

                    let Ok(callback_url) = normalize_kiro_callback_url(url) else {
                        continue;
                    };

                    let result = Command::new("curl")
                        .arg("-sS")
                        .arg(callback_url.as_str())
                        .output();

                    update_oauth_status(&status_state, |status| match result {
                        Ok(output) if output.status.success() => push_oauth_output(
                            status,
                            "已自动捕获并提交 Safari OAuth callback".to_string(),
                        ),
                        Ok(output) => {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            push_oauth_output(
                                status,
                                format!("自动提交 OAuth callback 失败: {}", stderr.trim()),
                            );
                        }
                        Err(err) => push_oauth_output(
                            status,
                            format!("自动提交 OAuth callback 失败: {}", err),
                        ),
                    });
                }
            }
        });
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = status_state;
    }
}

fn add_imported_credential(
    runtime_handle: &tokio::runtime::Handle,
    token_manager: &Arc<MultiTokenManager>,
    credential: KiroCredentials,
) -> anyhow::Result<u64> {
    if let Some(existing_id) = find_existing_credential_id(token_manager, &credential) {
        return Ok(existing_id);
    }

    runtime_handle.block_on(token_manager.add_credential(credential))
}

fn find_existing_credential_id(
    token_manager: &MultiTokenManager,
    credential: &KiroCredentials,
) -> Option<u64> {
    let snapshot = token_manager.snapshot();

    if credential.is_api_key_credential() {
        let hash = credential.kiro_api_key.as_deref().map(sha256_hex)?;
        return snapshot
            .entries
            .into_iter()
            .find(|entry| entry.api_key_hash.as_deref() == Some(hash.as_str()))
            .map(|entry| entry.id);
    }

    let hash = credential.refresh_token.as_deref().map(sha256_hex)?;
    snapshot
        .entries
        .into_iter()
        .find(|entry| entry.refresh_token_hash.as_deref() == Some(hash.as_str()))
        .map(|entry| entry.id)
}

fn update_oauth_status(
    status_state: &Arc<Mutex<KiroCliOAuthLoginStatus>>,
    f: impl FnOnce(&mut KiroCliOAuthLoginStatus),
) {
    let mut status = status_state.lock();
    f(&mut status);
}

fn oauth_output_contains(status_state: &Arc<Mutex<KiroCliOAuthLoginStatus>>, needle: &str) -> bool {
    status_state
        .lock()
        .output
        .iter()
        .any(|line| line.contains(needle))
}

fn finish_oauth_status(
    status_state: &Arc<Mutex<KiroCliOAuthLoginStatus>>,
    phase: &str,
    exit_code: Option<i32>,
    error: Option<String>,
    imported_credential_id: Option<u64>,
) {
    update_oauth_status(status_state, |status| {
        status.running = false;
        status.phase = phase.to_string();
        status.finished_at = Some(Utc::now().to_rfc3339());
        status.exit_code = exit_code;
        status.error = error;
        status.imported_credential_id = imported_credential_id;
        if let Some(id) = imported_credential_id {
            push_oauth_output(status, format!("凭据已导入，ID: {}", id));
        }
    });
}

fn push_oauth_output(status: &mut KiroCliOAuthLoginStatus, line: String) {
    const MAX_OUTPUT_LINES: usize = 200;
    status.output.push(redact_oauth_output(&line));
    if status.output.len() > MAX_OUTPUT_LINES {
        let overflow = status.output.len() - MAX_OUTPUT_LINES;
        status.output.drain(0..overflow);
    }
}

fn extract_url(line: &str) -> Option<String> {
    line.split_whitespace()
        .find(|part| part.starts_with("http://") || part.starts_with("https://"))
        .map(|part| {
            part.trim_matches(|c: char| matches!(c, ',' | ';' | ')' | ']' | '"' | '\''))
                .to_string()
        })
}

#[derive(Debug, Clone, Copy)]
enum BrowserOpenResult {
    Isolated,
    Default,
    Failed,
}

fn open_browser_url(url: &str, prefer_isolated: bool) -> BrowserOpenResult {
    if prefer_isolated && open_isolated_browser_url(url) {
        return BrowserOpenResult::Isolated;
    }

    if open_default_browser_url(url) {
        BrowserOpenResult::Default
    } else {
        BrowserOpenResult::Failed
    }
}

fn open_isolated_browser_url(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        let mut candidates = Vec::new();
        if let Some(path) = std::env::var_os("KIRO_RS_OAUTH_BROWSER") {
            candidates.push(PathBuf::from(path));
        }

        candidates.extend([
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
            PathBuf::from("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"),
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        ]);

        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            candidates.extend([
                home.join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
                home.join("Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
                home.join("Applications/Brave Browser.app/Contents/MacOS/Brave Browser"),
                home.join("Applications/Chromium.app/Contents/MacOS/Chromium"),
            ]);
            collect_browser_executable_candidates(
                &home.join("Library/Caches/ms-playwright"),
                8,
                &mut candidates,
            );
            collect_browser_executable_candidates(
                &home.join(".cache/puppeteer/chrome"),
                8,
                &mut candidates,
            );
        }

        let mut seen = HashSet::new();
        candidates.retain(|path| seen.insert(path.clone()));

        for executable in candidates {
            if !executable.exists() {
                continue;
            }

            let profile_dir = std::env::temp_dir().join(format!(
                "kiro-rs-oauth-profile-{}-{}",
                std::process::id(),
                Utc::now().timestamp_millis()
            ));

            if Command::new(&executable)
                .arg(format!("--user-data-dir={}", profile_dir.display()))
                .arg("--no-first-run")
                .arg("--no-default-browser-check")
                .arg("--disable-features=HttpsUpgrades,HttpsFirstBalancedModeAutoEnable")
                .arg("--new-window")
                .arg(url)
                .spawn()
                .is_ok()
            {
                return true;
            }
        }
    }

    false
}

#[cfg(target_os = "macos")]
fn collect_browser_executable_candidates(root: &Path, depth: usize, candidates: &mut Vec<PathBuf>) {
    if depth == 0 || !root.is_dir() {
        return;
    }

    let Ok(entries) = fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let file_name = path.file_name().and_then(|name| name.to_str());
            if matches!(file_name, Some("Google Chrome for Testing" | "Chromium")) {
                candidates.push(path);
            }
        } else if path.is_dir() {
            collect_browser_executable_candidates(&path, depth - 1, candidates);
        }
    }
}

fn open_default_browser_url(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        return Command::new("open").arg(url).spawn().is_ok();
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return Command::new("xdg-open").arg(url).spawn().is_ok();
    }

    #[cfg(target_os = "windows")]
    {
        return Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .is_ok();
    }

    #[allow(unreachable_code)]
    false
}

fn normalize_kiro_callback_url(input: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(input.trim())?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("callback URL 必须是 http/https");
    }

    let host = url.host_str().unwrap_or_default();
    if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        anyhow::bail!("callback URL 必须指向 localhost");
    }
    if url.port_or_known_default() != Some(3128) {
        anyhow::bail!("callback URL 端口必须是 3128");
    }
    if url.path() != "/oauth/callback" {
        anyhow::bail!("callback URL 路径必须是 /oauth/callback");
    }

    url.set_scheme("http")
        .map_err(|_| anyhow::anyhow!("无法转换 callback URL scheme"))?;
    url.set_host(Some("127.0.0.1"))
        .map_err(|_| anyhow::anyhow!("无法转换 callback URL host"))?;
    url.set_port(Some(3128))
        .map_err(|_| anyhow::anyhow!("无法转换 callback URL port"))?;

    Ok(url.to_string())
}

fn redact_oauth_output(line: &str) -> String {
    let mut out = Vec::new();
    for part in line.split_whitespace() {
        if part.starts_with("http://") || part.starts_with("https://") {
            out.push(part.to_string());
        } else if part.len() >= 24
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '='))
        {
            out.push("[redacted]".to_string());
        } else {
            out.push(part.to_string());
        }
    }
    out.join(" ")
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

fn non_empty_owned(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn non_empty_str(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }

    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }

    Path::new(path).to_path_buf()
}
