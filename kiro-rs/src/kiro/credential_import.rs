//! 从官方 Kiro CLI / Amazon Q CLI 登录库导入凭据

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use rusqlite::Connection;
use serde_json::Value;

use crate::kiro::model::credentials::{CredentialsConfig, KiroCredentials};

const TOKEN_KEYS: &[&str] = &[
    "kirocli:social:token",
    "kirocli:odic:token",
    "codewhisperer:social:token",
    "codewhisperer:odic:token",
];

const DEVICE_REGISTRATION_KEYS: &[&str] = &[
    "kirocli:odic:device-registration",
    "codewhisperer:odic:device-registration",
];

#[derive(Debug, Clone)]
pub struct ImportKiroCliOptions {
    pub db_path: Option<String>,
    pub credentials_path: String,
    pub replace: bool,
    pub priority: Option<u32>,
    pub region: Option<String>,
    pub auth_region: Option<String>,
    pub api_region: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LoadKiroCliCredentialOptions {
    pub db_path: Option<String>,
    pub priority: Option<u32>,
    pub region: Option<String>,
    pub auth_region: Option<String>,
    pub api_region: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImportKiroCliResult {
    pub db_path: PathBuf,
    pub credentials_path: PathBuf,
    pub auth_method: String,
    pub updated_existing: bool,
    pub total_credentials: usize,
}

pub fn import_kiro_cli_credentials(
    options: ImportKiroCliOptions,
) -> anyhow::Result<ImportKiroCliResult> {
    let db_path = resolve_db_path(options.db_path.as_deref())?;
    let credentials_path = expand_tilde(&options.credentials_path);
    let credential = load_credential_from_sqlite_with_overrides(
        &db_path,
        options.priority,
        options.region,
        options.auth_region,
        options.api_region,
    )?;
    let auth_method = credential
        .auth_method
        .clone()
        .unwrap_or_else(|| "social".to_string());

    let (credentials, updated_existing) = if options.replace {
        (vec![credential], false)
    } else {
        merge_credential(&credentials_path, credential)?
    };

    write_credentials(&credentials_path, &credentials)?;

    Ok(ImportKiroCliResult {
        db_path,
        credentials_path,
        auth_method,
        updated_existing,
        total_credentials: credentials.len(),
    })
}

pub fn load_kiro_cli_credential(
    options: LoadKiroCliCredentialOptions,
) -> anyhow::Result<KiroCredentials> {
    let db_path = resolve_db_path(options.db_path.as_deref())?;
    load_credential_from_sqlite_with_overrides(
        &db_path,
        options.priority,
        options.region,
        options.auth_region,
        options.api_region,
    )
}

fn resolve_db_path(input: Option<&str>) -> anyhow::Result<PathBuf> {
    if let Some(path) = input.and_then(non_empty_str) {
        let path = expand_tilde(path);
        if path.exists() {
            return Ok(path);
        }
        bail!("Kiro CLI SQLite 数据库不存在: {}", path.display());
    }

    let candidates = [
        "~/Library/Application Support/kiro-cli/data.sqlite3",
        "~/.local/share/kiro-cli/data.sqlite3",
        "~/.local/share/amazon-q/data.sqlite3",
    ];

    for candidate in candidates {
        let path = expand_tilde(candidate);
        if path.exists() {
            return Ok(path);
        }
    }

    bail!(
        "未找到 Kiro CLI SQLite 数据库。请先运行 `kiro-cli login`，或通过 `--db` 指定 data.sqlite3 路径"
    );
}

fn load_credential_from_sqlite_with_overrides(
    db_path: &Path,
    priority: Option<u32>,
    region: Option<String>,
    auth_region: Option<String>,
    api_region: Option<String>,
) -> anyhow::Result<KiroCredentials> {
    let mut credential = load_credential_from_sqlite(db_path)?;

    if let Some(priority) = priority {
        credential.priority = priority;
    }
    if let Some(region) = non_empty(region) {
        credential.region = Some(region);
    }
    if let Some(auth_region) = non_empty(auth_region) {
        credential.auth_region = Some(auth_region);
    }
    if let Some(api_region) = non_empty(api_region) {
        credential.api_region = Some(api_region);
    }

    credential.canonicalize_auth_method();
    Ok(credential)
}

fn load_credential_from_sqlite(path: &Path) -> anyhow::Result<KiroCredentials> {
    let conn = Connection::open(path)
        .with_context(|| format!("打开 SQLite 数据库失败: {}", path.display()))?;
    ensure_auth_kv_table(&conn)?;

    let (token_key, token_json) = read_first_json_value(&conn, TOKEN_KEYS)?
        .ok_or_else(|| anyhow::anyhow!("auth_kv 中未找到 Kiro CLI token 记录"))?;

    let mut credential = credential_from_token_json(&token_json)
        .with_context(|| format!("解析 token 记录失败: {}", token_key))?;

    if let Some((_, registration_json)) = read_first_json_value(&conn, DEVICE_REGISTRATION_KEYS)? {
        apply_device_registration(&mut credential, &registration_json)?;
    }

    if credential.refresh_token.as_deref().unwrap_or("").is_empty() {
        bail!("Kiro CLI token 记录缺少 refresh_token");
    }

    if credential.client_id.is_some() && credential.client_secret.is_some() {
        credential.auth_method = Some("idc".to_string());
    } else {
        credential.auth_method = Some("social".to_string());
    }

    Ok(credential)
}

fn ensure_auth_kv_table(conn: &Connection) -> anyhow::Result<()> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='auth_kv'",
            [],
            |row| row.get(0),
        )
        .context("检查 auth_kv 表失败")?;

    if exists == 0 {
        bail!("SQLite 数据库中不存在 auth_kv 表");
    }

    Ok(())
}

fn read_first_json_value(
    conn: &Connection,
    keys: &[&str],
) -> anyhow::Result<Option<(String, Value)>> {
    for key in keys {
        let result: rusqlite::Result<String> =
            conn.query_row("SELECT value FROM auth_kv WHERE key = ?1", [key], |row| {
                row.get(0)
            });

        match result {
            Ok(value) => {
                let json = serde_json::from_str(&value)
                    .with_context(|| format!("auth_kv[{}] 不是有效 JSON", key))?;
                return Ok(Some(((*key).to_string(), json)));
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(err) => return Err(err).context("读取 auth_kv 失败"),
        }
    }

    Ok(None)
}

fn credential_from_token_json(value: &Value) -> anyhow::Result<KiroCredentials> {
    let refresh_token = string_field(value, &["refresh_token", "refreshToken"]);
    let access_token = string_field(value, &["access_token", "accessToken"]);
    let expires_at = string_field(value, &["expires_at", "expiresAt", "expires"]);
    let profile_arn = string_field(value, &["profile_arn", "profileArn"]);
    let email = string_field(value, &["email", "email_address", "emailAddress"]);
    let region = string_field(value, &["region", "sso_region", "ssoRegion"]);

    if refresh_token.is_none() {
        bail!("token JSON 缺少 refresh_token");
    }

    Ok(KiroCredentials {
        access_token,
        refresh_token,
        profile_arn,
        expires_at,
        email,
        region,
        ..Default::default()
    })
}

fn apply_device_registration(
    credential: &mut KiroCredentials,
    value: &Value,
) -> anyhow::Result<()> {
    credential.client_id = string_field(value, &["client_id", "clientId"]);
    credential.client_secret = string_field(value, &["client_secret", "clientSecret"]);

    if credential.region.is_none() {
        credential.region = string_field(value, &["region", "sso_region", "ssoRegion"]);
    }

    Ok(())
}

fn merge_credential(
    path: &Path,
    credential: KiroCredentials,
) -> anyhow::Result<(Vec<KiroCredentials>, bool)> {
    let mut credentials = if path.exists() {
        CredentialsConfig::load(path)
            .with_context(|| format!("加载现有凭据失败: {}", path.display()))?
            .into_sorted_credentials()
    } else {
        Vec::new()
    };

    let refresh_token = credential.refresh_token.clone();
    if let Some(refresh_token) = refresh_token {
        if let Some(existing) = credentials
            .iter_mut()
            .find(|item| item.refresh_token.as_deref() == Some(refresh_token.as_str()))
        {
            *existing = credential;
            credentials.sort_by_key(|item| item.priority);
            return Ok((credentials, true));
        }
    }

    credentials.push(credential);
    credentials.sort_by_key(|item| item.priority);
    Ok((credentials, false))
}

fn write_credentials(path: &Path, credentials: &[KiroCredentials]) -> anyhow::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建凭据目录失败: {}", parent.display()))?;
    }

    let json = serde_json::to_string_pretty(credentials).context("序列化凭据失败")?;
    fs::write(path, json).with_context(|| format!("写入凭据文件失败: {}", path.display()))?;
    Ok(())
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = value
            .get(*key)
            .and_then(Value::as_str)
            .and_then(non_empty_str)
        {
            return Some(value.to_string());
        }
    }
    None
}

fn non_empty(value: Option<String>) -> Option<String> {
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

    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use uuid::Uuid;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kiro-rs-{}-{}", name, Uuid::new_v4()))
    }

    fn create_auth_db(path: &Path, token_key: &str, token: Value, registration: Option<Value>) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
            params![token_key, token.to_string()],
        )
        .unwrap();
        if let Some(registration) = registration {
            conn.execute(
                "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
                params!["kirocli:odic:device-registration", registration.to_string()],
            )
            .unwrap();
        }
    }

    #[test]
    fn imports_social_token_from_sqlite() {
        let db = temp_path("social.sqlite3");
        create_auth_db(
            &db,
            "kirocli:social:token",
            serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh",
                "expires_at": "2026-01-01T00:00:00Z",
                "region": "us-east-1",
                "email": "user@example.com"
            }),
            None,
        );

        let credential = load_credential_from_sqlite(&db).unwrap();
        assert_eq!(credential.auth_method.as_deref(), Some("social"));
        assert_eq!(credential.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(credential.region.as_deref(), Some("us-east-1"));
        assert!(credential.client_id.is_none());

        let _ = fs::remove_file(db);
    }

    #[test]
    fn imports_oidc_device_registration_from_sqlite() {
        let db = temp_path("oidc.sqlite3");
        create_auth_db(
            &db,
            "kirocli:odic:token",
            serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh",
                "expires_at": "2026-01-01T00:00:00Z"
            }),
            Some(serde_json::json!({
                "client_id": "client",
                "client_secret": "secret",
                "sso_region": "eu-west-1"
            })),
        );

        let credential = load_credential_from_sqlite(&db).unwrap();
        assert_eq!(credential.auth_method.as_deref(), Some("idc"));
        assert_eq!(credential.client_id.as_deref(), Some("client"));
        assert_eq!(credential.client_secret.as_deref(), Some("secret"));
        assert_eq!(credential.region.as_deref(), Some("eu-west-1"));

        let _ = fs::remove_file(db);
    }

    #[test]
    fn merge_updates_existing_refresh_token() {
        let path = temp_path("credentials.json");
        let existing = vec![KiroCredentials {
            refresh_token: Some("same".to_string()),
            access_token: Some("old".to_string()),
            priority: 7,
            ..Default::default()
        }];
        write_credentials(&path, &existing).unwrap();

        let (merged, updated) = merge_credential(
            &path,
            KiroCredentials {
                refresh_token: Some("same".to_string()),
                access_token: Some("new".to_string()),
                priority: 1,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(updated);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].access_token.as_deref(), Some("new"));
        assert_eq!(merged[0].priority, 1);

        let _ = fs::remove_file(path);
    }
}
