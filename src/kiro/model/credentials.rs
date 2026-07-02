//! Kiro OAuth 凭证数据模型
//!
//! 支持从 Kiro IDE 的凭证文件加载，使用 Social 认证方式
//! 支持单凭据和多凭据配置格式

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::http_client::ProxyConfig;
use crate::model::config::Config;

pub const BUILDER_ID_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";
pub const SOCIAL_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";
const ENTERPRISE_FALLBACK_ACCOUNT_ID: &str = "610548660232";
const ENTERPRISE_FALLBACK_PROFILE_ID: &str = "VNECVYCYYAWN";

pub fn is_placeholder_profile_arn(arn: &str) -> bool {
    arn == BUILDER_ID_PROFILE_ARN
}

pub fn is_enterprise_fallback_profile_arn(arn: &str) -> bool {
    arn.contains(&format!(
        ":{ENTERPRISE_FALLBACK_ACCOUNT_ID}:profile/{ENTERPRISE_FALLBACK_PROFILE_ID}"
    ))
}

pub fn enterprise_fallback_profile_arn(region: Option<&str>) -> String {
    let region = if region.unwrap_or("us-east-1").starts_with("eu-") {
        "eu-central-1"
    } else {
        "us-east-1"
    };
    format!(
        "arn:aws:codewhisperer:{region}:{ENTERPRISE_FALLBACK_ACCOUNT_ID}:profile/{ENTERPRISE_FALLBACK_PROFILE_ID}"
    )
}

/// Kiro OAuth 凭证
///
/// `Debug` 输出经过脱敏处理：access_token / refresh_token / client_secret /
/// kiro_api_key / proxy_password 等敏感字段只显示长度，不会泄露明文。
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KiroCredentials {
    /// 凭据唯一标识符（自增 ID）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,

    /// 访问令牌
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,

    /// 刷新令牌
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,

    /// Profile ARN
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,

    /// 过期时间 (RFC3339 格式)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,

    /// 认证方式 (social / idc / external_idp / api_key)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,

    /// 身份提供商（KAM 导出字段：BuilderId / Enterprise / Github / Google / IAM_SSO）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,

    /// OIDC Client ID (IdC 认证需要)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,

    /// OIDC Client Secret (IdC 认证需要)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,

    /// SSO Start URL（Enterprise / IAM Identity Center 账号专用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,

    /// External IdP OIDC issuer URL（external_idp 认证需要）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,

    /// External IdP OIDC token endpoint（可选，通常由 issuerUrl discovery 得到）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,

    /// External IdP OIDC scopes（空格分隔）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,

    /// External IdP OIDC audience（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,

    /// External IdP OIDC login hint（可选，仅登录流程使用；导入时保留）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_hint: Option<String>,

    /// 凭据优先级（数字越小优先级越高，默认为 0）
    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: u32,

    /// 凭据级 Region 配置（用于 OIDC token 刷新）
    /// 未配置时回退到 config.json 的全局 region
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// 凭据级 Auth Region（用于 Token 刷新）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// 凭据级 API Region（用于 API 请求）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    /// 凭据级 Machine ID 配置（可选）
    /// 未配置时回退到 config.json 的 machineId；都未配置时由 refreshToken 派生
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,

    /// 用户邮箱（从 Anthropic API 获取）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// 订阅等级（KIRO PRO+ / KIRO FREE 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub subscription_title: Option<String>,

    /// 凭据级代理 URL（可选）
    /// 支持 http/https/socks5 协议
    /// 特殊值 "direct" 表示显式不使用代理（即使全局配置了代理）
    /// 未配置时回退到全局代理配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,

    /// 凭据级代理认证用户名（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,

    /// 凭据级代理认证密码（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_password: Option<String>,

    /// 凭据是否被禁用（默认为 false）
    #[serde(default)]
    pub disabled: bool,

    /// Kiro API Key（headless 模式）
    /// 格式: ksk_xxxxxxxx
    /// 设置后直接作为 Bearer Token 使用，无需 refreshToken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiro_api_key: Option<String>,

    /// 端点名称（可选）
    ///
    /// 决定该凭据走哪套 Kiro API。未配置时回退到 `config.defaultEndpoint`（默认 "ide"）。
    /// 端点名必须在启动时注册的端点 registry 中存在。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// 判断是否为零（用于跳过序列化）
fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// 仅显示长度，不暴露明文。例如 `Some(42 chars)` 或 `None`。
fn fmt_redacted(value: &Option<String>) -> String {
    match value {
        Some(s) if !s.is_empty() => format!("Some({} chars)", s.chars().count()),
        Some(_) => "Some(<empty>)".to_string(),
        None => "None".to_string(),
    }
}

impl std::fmt::Debug for KiroCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 单独脱敏所有可能含密钥/Token 的字段，其他元数据正常打印
        f.debug_struct("KiroCredentials")
            .field("id", &self.id)
            .field("access_token", &fmt_redacted(&self.access_token))
            .field("refresh_token", &fmt_redacted(&self.refresh_token))
            .field("profile_arn", &self.profile_arn)
            .field("expires_at", &self.expires_at)
            .field("auth_method", &self.auth_method)
            .field("provider", &self.provider)
            .field("client_id", &fmt_redacted(&self.client_id))
            .field("client_secret", &fmt_redacted(&self.client_secret))
            .field("start_url", &self.start_url)
            .field("issuer_url", &self.issuer_url)
            .field("token_endpoint", &self.token_endpoint)
            .field("scopes", &self.scopes)
            .field("audience", &self.audience)
            .field("login_hint", &self.login_hint)
            .field("priority", &self.priority)
            .field("region", &self.region)
            .field("auth_region", &self.auth_region)
            .field("api_region", &self.api_region)
            .field("machine_id", &fmt_redacted(&self.machine_id))
            .field("email", &self.email)
            .field("subscription_title", &self.subscription_title)
            .field("proxy_url", &self.proxy_url)
            .field("proxy_username", &self.proxy_username)
            .field("proxy_password", &fmt_redacted(&self.proxy_password))
            .field("disabled", &self.disabled)
            .field("kiro_api_key", &fmt_redacted(&self.kiro_api_key))
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

fn canonicalize_auth_method_value(value: &str) -> &str {
    if value.eq_ignore_ascii_case("builder-id") || value.eq_ignore_ascii_case("iam") {
        "idc"
    } else if value.eq_ignore_ascii_case("api_key") || value.eq_ignore_ascii_case("apikey") {
        "api_key"
    } else if value.eq_ignore_ascii_case("external_idp")
        || value.eq_ignore_ascii_case("external-idp")
        || value.eq_ignore_ascii_case("externalIdp")
        || value.eq_ignore_ascii_case("externel_idp")
        || value.eq_ignore_ascii_case("externel-idp")
        || value.eq_ignore_ascii_case("externelIdp")
    {
        "external_idp"
    } else {
        value
    }
}

/// 凭据配置（支持单对象或数组格式）
///
/// 自动识别配置文件格式：
/// - 单对象格式（旧格式，向后兼容）
/// - 数组格式（新格式，支持多凭据）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CredentialsConfig {
    /// 单个凭据（旧格式）
    Single(KiroCredentials),
    /// 多凭据数组（新格式）
    Multiple(Vec<KiroCredentials>),
}

impl CredentialsConfig {
    /// 从文件加载凭据配置
    ///
    /// - 如果文件不存在，返回空数组
    /// - 如果文件内容为空，返回空数组
    /// - 支持单对象或数组格式
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();

        // 文件不存在时返回空数组
        if !path.exists() {
            return Ok(CredentialsConfig::Multiple(vec![]));
        }

        let content = fs::read_to_string(path)?;

        // 文件为空时返回空数组
        if content.trim().is_empty() {
            return Ok(CredentialsConfig::Multiple(vec![]));
        }

        let config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// 转换为按优先级排序的凭据列表
    pub fn into_sorted_credentials(self) -> Vec<KiroCredentials> {
        match self {
            CredentialsConfig::Single(mut cred) => {
                cred.canonicalize_auth_method();
                vec![cred]
            }
            CredentialsConfig::Multiple(mut creds) => {
                // 按优先级排序（数字越小优先级越高）
                creds.sort_by_key(|c| c.priority);
                for cred in &mut creds {
                    cred.canonicalize_auth_method();
                }
                creds
            }
        }
    }

    /// 判断是否为多凭据格式（数组格式）
    pub fn is_multiple(&self) -> bool {
        matches!(self, CredentialsConfig::Multiple(_))
    }
}

impl KiroCredentials {
    /// 特殊值：显式不使用代理
    pub const PROXY_DIRECT: &'static str = "direct";

    /// 获取默认凭证文件路径
    pub fn default_credentials_path() -> &'static str {
        "credentials.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    pub fn effective_auth_region<'a>(&'a self, config: &'a Config) -> &'a str {
        self.auth_region
            .as_deref()
            .or(self.region.as_deref())
            .unwrap_or(config.effective_auth_region())
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先级：凭据.api_region > config.api_region > config.region
    pub fn effective_api_region<'a>(&'a self, config: &'a Config) -> &'a str {
        self.api_region
            .as_deref()
            .unwrap_or(config.effective_api_region())
    }

    /// 获取有效的代理配置
    /// 优先级：凭据代理 > 全局代理 > 无代理
    /// 特殊值 "direct" 表示显式不使用代理（即使全局配置了代理）
    pub fn effective_proxy(&self, global_proxy: Option<&ProxyConfig>) -> Option<ProxyConfig> {
        match self.proxy_url.as_deref() {
            Some(url) if url.eq_ignore_ascii_case(Self::PROXY_DIRECT) => None,
            Some(url) => {
                let mut proxy = ProxyConfig::new(url);
                if let (Some(username), Some(password)) =
                    (&self.proxy_username, &self.proxy_password)
                {
                    proxy = proxy.with_auth(username, password);
                }
                Some(proxy)
            }
            None => global_proxy.cloned(),
        }
    }

    pub fn canonicalize_auth_method(&mut self) {
        let auth_method = match &self.auth_method {
            Some(m) => m,
            None => return,
        };

        let canonical = canonicalize_auth_method_value(auth_method);
        if canonical != auth_method {
            self.auth_method = Some(canonical.to_string());
        }
    }

    #[allow(dead_code)]
    pub fn fill_default_profile_arn(&mut self) -> bool {
        self.fill_default_profile_arn_with_region(None)
    }

    pub fn fill_default_profile_arn_with_region(&mut self, default_region: Option<&str>) -> bool {
        if self.is_api_key_credential() {
            return false;
        }

        let has_real_profile_arn = self
            .profile_arn
            .as_deref()
            .is_some_and(|arn| !is_placeholder_profile_arn(arn));
        if has_real_profile_arn {
            return false;
        }

        let is_social = self
            .auth_method
            .as_deref()
            .map(|m| m.eq_ignore_ascii_case("social"))
            .unwrap_or(false)
            || self
                .provider
                .as_deref()
                .map(|p| p.eq_ignore_ascii_case("github") || p.eq_ignore_ascii_case("google"))
                .unwrap_or(false);

        let is_enterprise = self.is_enterprise_identity();

        self.profile_arn = Some(if is_social {
            SOCIAL_PROFILE_ARN.to_string()
        } else if is_enterprise {
            enterprise_fallback_profile_arn(self.effective_region_hint().or(default_region))
        } else {
            BUILDER_ID_PROFILE_ARN.to_string()
        });
        true
    }

    fn is_social_login(&self) -> bool {
        self.auth_method
            .as_deref()
            .map(|m| m.eq_ignore_ascii_case("social"))
            .unwrap_or(false)
            || self
                .provider
                .as_deref()
                .map(|p| p.eq_ignore_ascii_case("github") || p.eq_ignore_ascii_case("google"))
                .unwrap_or(false)
    }

    pub fn is_external_idp_credential(&self) -> bool {
        self.auth_method
            .as_deref()
            .map(|m| canonicalize_auth_method_value(m).eq_ignore_ascii_case("external_idp"))
            .unwrap_or(false)
            || self
                .provider
                .as_deref()
                .map(|p| p.eq_ignore_ascii_case("ExternalIdp"))
                .unwrap_or(false)
    }

    pub fn is_enterprise_identity(&self) -> bool {
        let has_enterprise_start_url = self
            .start_url
            .as_deref()
            .map(|url| {
                let normalized = url.trim().trim_end_matches('/');
                !normalized.is_empty() && normalized != "https://view.awsapps.com/start"
            })
            .unwrap_or(false);
        let has_builder_id_start_url = self
            .start_url
            .as_deref()
            .map(|url| url.trim().trim_end_matches('/') == "https://view.awsapps.com/start")
            .unwrap_or(false);
        // Some Kiro Account Manager exports store Enterprise/IdC credentials with
        // authMethod=idc but without provider/startUrl after migration. Treat those
        // as Enterprise so placeholder BuilderId profileArn can be self-healed, while
        // preserving explicit BuilderId IdC credentials.
        let idc_without_builder_start_url = self
            .auth_method
            .as_deref()
            .map(|m| m.eq_ignore_ascii_case("idc"))
            .unwrap_or(false)
            && !has_builder_id_start_url;

        self.auth_method
            .as_deref()
            .map(|m| {
                m.eq_ignore_ascii_case("external_idp")
                    || m.eq_ignore_ascii_case("external-idp")
                    || m.eq_ignore_ascii_case("externalIdp")
                    || m.eq_ignore_ascii_case("externel_idp")
                    || m.eq_ignore_ascii_case("externel-idp")
                    || m.eq_ignore_ascii_case("externelIdp")
                    || m.eq_ignore_ascii_case("enterprise")
                    || m.eq_ignore_ascii_case("iam_sso")
            })
            .unwrap_or(false)
            || self
                .provider
                .as_deref()
                .map(|p| p.eq_ignore_ascii_case("enterprise") || p.eq_ignore_ascii_case("iam_sso"))
                .unwrap_or(false)
            || idc_without_builder_start_url
            || has_enterprise_start_url
    }

    pub fn should_fetch_enterprise_profile_arn(&self) -> bool {
        self.is_enterprise_identity()
            && self.profile_arn.as_deref().is_none_or(|arn| {
                is_placeholder_profile_arn(arn) || is_enterprise_fallback_profile_arn(arn)
            })
    }

    fn effective_region_hint(&self) -> Option<&str> {
        self.api_region
            .as_deref()
            .or(self.region.as_deref())
            .or(self.auth_region.as_deref())
    }

    pub fn effective_profile_arn(&self) -> Option<&str> {
        match self.profile_arn.as_deref() {
            Some(arn) if !is_placeholder_profile_arn(arn) => Some(arn),
            _ => None,
        }
    }

    pub fn streaming_profile_arn(&self) -> Option<String> {
        if self.is_api_key_credential() {
            return None;
        }

        self.profile_arn.clone().or_else(|| {
            Some(if self.is_social_login() {
                SOCIAL_PROFILE_ARN.to_string()
            } else if self.is_enterprise_identity() {
                enterprise_fallback_profile_arn(self.effective_region_hint())
            } else {
                BUILDER_ID_PROFILE_ARN.to_string()
            })
        })
    }

    /// 检查凭据是否支持 Opus 模型
    ///
    /// Free 账号不支持 Opus 模型，需要 PRO 或更高等级订阅
    pub fn supports_opus(&self) -> bool {
        match &self.subscription_title {
            Some(title) => {
                let title_upper = title.to_uppercase();
                // 如果包含 FREE，则不支持 Opus
                !title_upper.contains("FREE")
            }
            // 如果还没有获取订阅信息，暂时允许（首次使用时会获取）
            None => true,
        }
    }

    /// 检查是否为 API Key 凭据
    ///
    /// API Key 凭据直接使用 kiro_api_key 作为 Bearer Token，无需 refreshToken
    pub fn is_api_key_credential(&self) -> bool {
        self.kiro_api_key.is_some()
            || self
                .auth_method
                .as_deref()
                .map(|m| m.eq_ignore_ascii_case("api_key") || m.eq_ignore_ascii_case("apikey"))
                .unwrap_or(false)
    }
}

#[cfg(test)]
impl KiroCredentials {
    fn from_json(json_string: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json_string)
    }

    fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::Config;

    #[test]
    fn test_from_json() {
        let json = r#"{
            "accessToken": "test_token",
            "refreshToken": "test_refresh",
            "profileArn": "arn:aws:test",
            "expiresAt": "2024-01-01T00:00:00Z",
            "authMethod": "social"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.access_token, Some("test_token".to_string()));
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.profile_arn, Some("arn:aws:test".to_string()));
        assert_eq!(creds.expires_at, Some("2024-01-01T00:00:00Z".to_string()));
        assert_eq!(creds.auth_method, Some("social".to_string()));
    }

    #[test]
    fn test_from_json_with_unknown_keys() {
        let json = r#"{
            "accessToken": "test_token",
            "unknownField": "should be ignored"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.access_token, Some("test_token".to_string()));
    }

    #[test]
    fn test_to_json() {
        let creds = KiroCredentials {
            id: None,
            access_token: Some("token".to_string()),
            refresh_token: None,
            profile_arn: None,
            expires_at: None,
            auth_method: Some("social".to_string()),
            provider: None,
            client_id: None,
            client_secret: None,
            start_url: None,
            issuer_url: None,
            token_endpoint: None,
            scopes: None,
            audience: None,
            login_hint: None,
            priority: 0,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            kiro_api_key: None,
            endpoint: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("accessToken"));
        assert!(json.contains("authMethod"));
        assert!(!json.contains("refreshToken"));
        // priority 为 0 时不序列化
        assert!(!json.contains("priority"));
    }

    #[test]
    fn test_default_credentials_path() {
        assert_eq!(
            KiroCredentials::default_credentials_path(),
            "credentials.json"
        );
    }

    #[test]
    fn test_enterprise_fill_default_profile_arn_uses_region_fallback() {
        let mut creds = KiroCredentials {
            provider: Some("Enterprise".to_string()),
            region: Some("eu-west-1".to_string()),
            ..Default::default()
        };

        assert!(creds.fill_default_profile_arn());
        assert_eq!(
            creds.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:eu-central-1:610548660232:profile/VNECVYCYYAWN")
        );
    }

    #[test]
    fn test_enterprise_replaces_builder_placeholder_profile_arn() {
        let mut creds = KiroCredentials {
            auth_method: Some("external_idp".to_string()),
            api_region: Some("us-west-2".to_string()),
            profile_arn: Some(BUILDER_ID_PROFILE_ARN.to_string()),
            ..Default::default()
        };

        assert!(creds.fill_default_profile_arn());
        assert_eq!(
            creds.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:610548660232:profile/VNECVYCYYAWN")
        );
    }

    #[test]
    fn test_idc_auth_method_is_enterprise_identity_without_start_url() {
        let mut creds = KiroCredentials {
            auth_method: Some("idc".to_string()),
            region: Some("us-east-1".to_string()),
            profile_arn: Some(BUILDER_ID_PROFILE_ARN.to_string()),
            ..Default::default()
        };

        assert!(creds.is_enterprise_identity());
        assert!(creds.should_fetch_enterprise_profile_arn());
        assert!(creds.fill_default_profile_arn());
        assert_eq!(
            creds.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:610548660232:profile/VNECVYCYYAWN")
        );
    }

    #[test]
    fn test_social_fill_default_profile_arn_uses_social_arn() {
        let mut creds = KiroCredentials {
            provider: Some("Github".to_string()),
            ..Default::default()
        };

        assert!(creds.fill_default_profile_arn());
        assert_eq!(creds.profile_arn.as_deref(), Some(SOCIAL_PROFILE_ARN));
    }

    #[test]
    fn test_effective_profile_arn_skips_builder_placeholder() {
        let mut creds = KiroCredentials {
            profile_arn: Some(BUILDER_ID_PROFILE_ARN.to_string()),
            ..Default::default()
        };
        assert_eq!(creds.effective_profile_arn(), None);

        creds.profile_arn = Some(SOCIAL_PROFILE_ARN.to_string());
        assert_eq!(creds.effective_profile_arn(), Some(SOCIAL_PROFILE_ARN));

        let real = "arn:aws:codewhisperer:us-east-1:123456789012:profile/REAL123";
        creds.profile_arn = Some(real.to_string());
        assert_eq!(creds.effective_profile_arn(), Some(real));
    }

    #[test]
    fn test_streaming_profile_arn_supplies_official_defaults() {
        let explicit_builder = KiroCredentials {
            profile_arn: Some(BUILDER_ID_PROFILE_ARN.to_string()),
            refresh_token: Some("r".to_string()),
            ..Default::default()
        };
        assert_eq!(
            explicit_builder.streaming_profile_arn().as_deref(),
            Some(BUILDER_ID_PROFILE_ARN)
        );

        let real = "arn:aws:codewhisperer:us-east-1:123456789012:profile/REAL123";
        let explicit_real = KiroCredentials {
            profile_arn: Some(real.to_string()),
            refresh_token: Some("r".to_string()),
            ..Default::default()
        };
        assert_eq!(explicit_real.streaming_profile_arn().as_deref(), Some(real));

        let builder = KiroCredentials {
            refresh_token: Some("r".to_string()),
            ..Default::default()
        };
        assert_eq!(
            builder.streaming_profile_arn().as_deref(),
            Some(BUILDER_ID_PROFILE_ARN)
        );

        let social = KiroCredentials {
            auth_method: Some("social".to_string()),
            ..Default::default()
        };
        assert_eq!(
            social.streaming_profile_arn().as_deref(),
            Some(SOCIAL_PROFILE_ARN)
        );

        let enterprise = KiroCredentials {
            auth_method: Some("idc".to_string()),
            region: Some("us-east-1".to_string()),
            ..Default::default()
        };
        assert_eq!(
            enterprise.streaming_profile_arn().as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:610548660232:profile/VNECVYCYYAWN")
        );

        let api_key = KiroCredentials {
            kiro_api_key: Some("ksk_test".to_string()),
            auth_method: Some("api_key".to_string()),
            ..Default::default()
        };
        assert_eq!(api_key.streaming_profile_arn(), None);
    }

    #[test]
    fn test_enterprise_identity_detected_from_start_url() {
        let enterprise = KiroCredentials {
            auth_method: Some("idc".to_string()),
            start_url: Some("https://example.awsapps.com/start".to_string()),
            ..Default::default()
        };
        assert!(enterprise.is_enterprise_identity());

        let builder = KiroCredentials {
            auth_method: Some("idc".to_string()),
            start_url: Some("https://view.awsapps.com/start/".to_string()),
            ..Default::default()
        };
        assert!(!builder.is_enterprise_identity());
    }

    #[test]
    fn test_priority_default() {
        let json = r#"{"refreshToken": "test"}"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.priority, 0);
    }

    #[test]
    fn test_priority_explicit() {
        let json = r#"{"refreshToken": "test", "priority": 5}"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.priority, 5);
    }

    #[test]
    fn test_credentials_config_single() {
        let json = r#"{"refreshToken": "test", "expiresAt": "2025-12-31T00:00:00Z"}"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(config, CredentialsConfig::Single(_)));
    }

    #[test]
    fn test_credentials_config_multiple() {
        let json = r#"[
            {"refreshToken": "test1", "priority": 1},
            {"refreshToken": "test2", "priority": 0}
        ]"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(config, CredentialsConfig::Multiple(_)));
        assert_eq!(config.into_sorted_credentials().len(), 2);
    }

    #[test]
    fn test_credentials_config_priority_sorting() {
        let json = r#"[
            {"refreshToken": "t1", "priority": 2},
            {"refreshToken": "t2", "priority": 0},
            {"refreshToken": "t3", "priority": 1}
        ]"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        let list = config.into_sorted_credentials();

        // 验证按优先级排序
        assert_eq!(list[0].refresh_token, Some("t2".to_string())); // priority 0
        assert_eq!(list[1].refresh_token, Some("t3".to_string())); // priority 1
        assert_eq!(list[2].refresh_token, Some("t1".to_string())); // priority 2
    }

    // ============ Region 字段测试 ============

    #[test]
    fn test_region_field_parsing() {
        // 测试解析包含 region 字段的 JSON
        let json = r#"{
            "refreshToken": "test_refresh",
            "region": "us-east-1"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.region, Some("us-east-1".to_string()));
    }

    #[test]
    fn test_region_field_missing_backward_compat() {
        // 测试向后兼容：不包含 region 字段的旧格式 JSON
        let json = r#"{
            "refreshToken": "test_refresh",
            "authMethod": "social"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.region, None);
    }

    #[test]
    fn test_region_field_serialization() {
        let creds = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some("test".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: None,
            provider: None,
            client_id: None,
            client_secret: None,
            start_url: None,
            issuer_url: None,
            token_endpoint: None,
            scopes: None,
            audience: None,
            login_hint: None,
            priority: 0,
            region: Some("eu-west-1".to_string()),
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            kiro_api_key: None,
            endpoint: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("region"));
        assert!(json.contains("eu-west-1"));
    }

    #[test]
    fn test_region_field_none_not_serialized() {
        let creds = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some("test".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: None,
            provider: None,
            client_id: None,
            client_secret: None,
            start_url: None,
            issuer_url: None,
            token_endpoint: None,
            scopes: None,
            audience: None,
            login_hint: None,
            priority: 0,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            kiro_api_key: None,
            endpoint: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("region"));
    }

    // ============ MachineId 字段测试 ============

    #[test]
    fn test_machine_id_field_parsing() {
        let machine_id = "a".repeat(64);
        let json = format!(
            r#"{{
                "refreshToken": "test_refresh",
                "machineId": "{machine_id}"
            }}"#
        );

        let creds = KiroCredentials::from_json(&json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.machine_id, Some(machine_id));
    }

    #[test]
    fn test_machine_id_field_serialization() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.machine_id = Some("b".repeat(64));

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("machineId"));
    }

    #[test]
    fn test_machine_id_field_none_not_serialized() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.machine_id = None;

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("machineId"));
    }

    #[test]
    fn test_multiple_credentials_with_different_regions() {
        // 测试多凭据场景下不同凭据使用各自的 region
        let json = r#"[
            {"refreshToken": "t1", "region": "us-east-1"},
            {"refreshToken": "t2", "region": "eu-west-1"},
            {"refreshToken": "t3"}
        ]"#;

        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        let list = config.into_sorted_credentials();

        assert_eq!(list[0].region, Some("us-east-1".to_string()));
        assert_eq!(list[1].region, Some("eu-west-1".to_string()));
        assert_eq!(list[2].region, None);
    }

    #[test]
    fn test_region_field_with_all_fields() {
        // 测试包含所有字段的完整 JSON
        let json = r#"{
            "id": 1,
            "accessToken": "access",
            "refreshToken": "refresh",
            "profileArn": "arn:aws:test",
            "expiresAt": "2025-12-31T00:00:00Z",
            "authMethod": "idc",
            "clientId": "client123",
            "clientSecret": "secret456",
            "priority": 5,
            "region": "ap-northeast-1"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.id, Some(1));
        assert_eq!(creds.access_token, Some("access".to_string()));
        assert_eq!(creds.refresh_token, Some("refresh".to_string()));
        assert_eq!(creds.profile_arn, Some("arn:aws:test".to_string()));
        assert_eq!(creds.expires_at, Some("2025-12-31T00:00:00Z".to_string()));
        assert_eq!(creds.auth_method, Some("idc".to_string()));
        assert_eq!(creds.client_id, Some("client123".to_string()));
        assert_eq!(creds.client_secret, Some("secret456".to_string()));
        assert_eq!(creds.priority, 5);
        assert_eq!(creds.region, Some("ap-northeast-1".to_string()));
    }

    #[test]
    fn test_external_idp_fields_and_auth_alias() {
        let json = r#"{
            "refreshToken": "short-refresh",
            "authMethod": "externel_idp",
            "provider": "ExternalIdp",
            "issuerUrl": "https://idp.example.com",
            "tokenEndpoint": "https://idp.example.com/oauth/token",
            "clientId": "client123",
            "scopes": "openid profile email offline_access",
            "audience": "api://kiro",
            "loginHint": "user@example.com"
        }"#;

        let mut creds = KiroCredentials::from_json(json).unwrap();
        creds.canonicalize_auth_method();

        assert_eq!(creds.auth_method.as_deref(), Some("external_idp"));
        assert!(creds.is_external_idp_credential());
        assert_eq!(creds.issuer_url.as_deref(), Some("https://idp.example.com"));
        assert_eq!(
            creds.token_endpoint.as_deref(),
            Some("https://idp.example.com/oauth/token")
        );
        assert_eq!(
            creds.scopes.as_deref(),
            Some("openid profile email offline_access")
        );
        assert_eq!(creds.audience.as_deref(), Some("api://kiro"));
        assert_eq!(creds.login_hint.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn test_region_roundtrip() {
        // 测试序列化和反序列化的往返一致性
        let original = KiroCredentials {
            id: Some(42),
            access_token: Some("token".to_string()),
            refresh_token: Some("refresh".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: Some("social".to_string()),
            provider: None,
            client_id: None,
            client_secret: None,
            start_url: None,
            issuer_url: None,
            token_endpoint: None,
            scopes: None,
            audience: None,
            login_hint: None,
            priority: 3,
            region: Some("us-west-2".to_string()),
            auth_region: None,
            api_region: None,
            machine_id: Some("c".repeat(64)),
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            kiro_api_key: None,
            endpoint: None,
        };

        let json = original.to_pretty_json().unwrap();
        let parsed = KiroCredentials::from_json(&json).unwrap();

        assert_eq!(parsed.id, original.id);
        assert_eq!(parsed.access_token, original.access_token);
        assert_eq!(parsed.refresh_token, original.refresh_token);
        assert_eq!(parsed.priority, original.priority);
        assert_eq!(parsed.region, original.region);
        assert_eq!(parsed.machine_id, original.machine_id);
    }

    // ============ auth_region / api_region 字段测试 ============

    #[test]
    fn test_auth_region_field_parsing() {
        let json = r#"{
            "refreshToken": "test_refresh",
            "authRegion": "eu-central-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.auth_region, Some("eu-central-1".to_string()));
        assert_eq!(creds.api_region, None);
    }

    #[test]
    fn test_api_region_field_parsing() {
        let json = r#"{
            "refreshToken": "test_refresh",
            "apiRegion": "ap-southeast-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.api_region, Some("ap-southeast-1".to_string()));
        assert_eq!(creds.auth_region, None);
    }

    #[test]
    fn test_auth_api_region_serialization() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.auth_region = Some("eu-west-1".to_string());
        creds.api_region = Some("us-west-2".to_string());

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("authRegion"));
        assert!(json.contains("eu-west-1"));
        assert!(json.contains("apiRegion"));
        assert!(json.contains("us-west-2"));
    }

    #[test]
    fn test_auth_api_region_none_not_serialized() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.auth_region = None;
        creds.api_region = None;

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("authRegion"));
        assert!(!json.contains("apiRegion"));
    }

    #[test]
    fn test_auth_api_region_roundtrip() {
        let mut original = KiroCredentials::default();
        original.refresh_token = Some("refresh".to_string());
        original.region = Some("us-east-1".to_string());
        original.auth_region = Some("eu-west-1".to_string());
        original.api_region = Some("ap-northeast-1".to_string());

        let json = original.to_pretty_json().unwrap();
        let parsed = KiroCredentials::from_json(&json).unwrap();

        assert_eq!(parsed.region, original.region);
        assert_eq!(parsed.auth_region, original.auth_region);
        assert_eq!(parsed.api_region, original.api_region);
    }

    #[test]
    fn test_backward_compat_no_auth_api_region() {
        // 旧格式 JSON 不包含 authRegion/apiRegion，应正常解析
        let json = r#"{
            "refreshToken": "test_refresh",
            "region": "us-east-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.region, Some("us-east-1".to_string()));
        assert_eq!(creds.auth_region, None);
        assert_eq!(creds.api_region, None);
    }

    // ============ effective_auth_region / effective_api_region 优先级测试 ============

    #[test]
    fn test_effective_auth_region_credential_auth_region_highest() {
        // 凭据.auth_region > 凭据.region > config.auth_region > config.region
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());
        creds.auth_region = Some("cred-auth-region".to_string());

        assert_eq!(creds.effective_auth_region(&config), "cred-auth-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_credential_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());
        // auth_region 未设置

        assert_eq!(creds.effective_auth_region(&config), "cred-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_config_auth_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let creds = KiroCredentials::default();
        // auth_region 和 region 均未设置

        assert_eq!(creds.effective_auth_region(&config), "config-auth-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_config_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        // config.auth_region 未设置

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_auth_region(&config), "config-region");
    }

    #[test]
    fn test_effective_api_region_credential_api_region_highest() {
        // 凭据.api_region > config.api_region > config.region
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.api_region = Some("config-api-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.api_region = Some("cred-api-region".to_string());

        assert_eq!(creds.effective_api_region(&config), "cred-api-region");
    }

    #[test]
    fn test_effective_api_region_fallback_to_config_api_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.api_region = Some("config-api-region".to_string());

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_api_region(&config), "config-api-region");
    }

    #[test]
    fn test_effective_api_region_fallback_to_config_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_api_region(&config), "config-region");
    }

    #[test]
    fn test_effective_api_region_ignores_credential_region() {
        // 凭据.region 不参与 api_region 的回退链
        let mut config = Config::default();
        config.region = "config-region".to_string();

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());

        assert_eq!(creds.effective_api_region(&config), "config-region");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响
        let mut config = Config::default();
        config.region = "default".to_string();

        let mut creds = KiroCredentials::default();
        creds.auth_region = Some("auth-only".to_string());
        creds.api_region = Some("api-only".to_string());

        assert_eq!(creds.effective_auth_region(&config), "auth-only");
        assert_eq!(creds.effective_api_region(&config), "api-only");
    }

    // ============ 凭据级代理优先级测试 ============

    #[test]
    fn test_effective_proxy_credential_overrides_global() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("socks5://cred:1080".to_string());

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, Some(ProxyConfig::new("socks5://cred:1080")));
    }

    #[test]
    fn test_effective_proxy_credential_with_auth() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("http://proxy:3128".to_string());
        creds.proxy_username = Some("user".to_string());
        creds.proxy_password = Some("pass".to_string());

        let result = creds.effective_proxy(Some(&global));
        let expected = ProxyConfig::new("http://proxy:3128").with_auth("user", "pass");
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_effective_proxy_direct_bypasses_global() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("direct".to_string());

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, None);
    }

    #[test]
    fn test_effective_proxy_direct_case_insensitive() {
        let global = ProxyConfig::new("http://global:8080");
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("DIRECT".to_string());

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, None);
    }

    #[test]
    fn test_effective_proxy_fallback_to_global() {
        let global = ProxyConfig::new("http://global:8080");
        let creds = KiroCredentials::default();

        let result = creds.effective_proxy(Some(&global));
        assert_eq!(result, Some(ProxyConfig::new("http://global:8080")));
    }

    #[test]
    fn test_effective_proxy_none_when_no_proxy() {
        let creds = KiroCredentials::default();
        let result = creds.effective_proxy(None);
        assert_eq!(result, None);
    }
}
