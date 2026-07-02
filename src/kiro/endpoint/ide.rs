//! Kiro IDE 端点
//!
//! 对应 Kiro IDE 客户端目前使用的 AWS CodeWhisperer 端点：
//! - API: `https://q.{api_region}.amazonaws.com/generateAssistantResponse`
//!   （External IdP 使用 `https://codewhisperer.{api_region}.amazonaws.com/generateAssistantResponse`）
//! - MCP: `https://q.{api_region}.amazonaws.com/mcp`
//!
//! 请求头使用 aws-sdk-js User-Agent 标识。请求体会在根对象上注入 `profileArn`。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};
use crate::kiro::kiro_version;

/// Kiro IDE 端点名称
pub const IDE_ENDPOINT_NAME: &str = "ide";

/// Kiro IDE 端点
pub struct IdeEndpoint;

impl IdeEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_api_region(ctx.config)
    }

    fn api_host(&self, ctx: &RequestContext<'_>) -> String {
        if ctx.credentials.is_external_idp_credential() {
            return format!("codewhisperer.{}.amazonaws.com", self.api_region(ctx));
        }
        format!("q.{}.amazonaws.com", self.api_region(ctx))
    }

    fn mcp_host(&self, ctx: &RequestContext<'_>) -> String {
        format!("q.{}.amazonaws.com", self.api_region(ctx))
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
            kiro_version::effective(&ctx.config.kiro_version),
            ctx.machine_id
        )
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            ctx.config.system_version,
            ctx.config.node_version,
            kiro_version::effective(&ctx.config.kiro_version),
            ctx.machine_id
        )
    }
}

impl Default for IdeEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for IdeEndpoint {
    fn name(&self) -> &'static str {
        IDE_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/generateAssistantResponse", self.api_host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/mcp", self.mcp_host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.api_host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some((name, value)) = ctx.credentials.bearer_token_type_header() {
            req = req.header(name, value);
        }
        req
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.mcp_host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(arn) = ctx.credentials.effective_profile_arn() {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        if let Some((name, value)) = ctx.credentials.bearer_token_type_header() {
            req = req.header(name, value);
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        let body = inject_ide_thinking_fields(body);
        inject_profile_arn(&body, &ctx.credentials.streaming_profile_arn())
    }
}

/// 将 profile_arn 注入到请求体 JSON 根对象
fn inject_profile_arn(request_body: &str, profile_arn: &Option<String>) -> String {
    if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body) {
        if let Some(arn) = profile_arn {
            json["profileArn"] = serde_json::Value::String(arn.clone());
            if let Ok(body) = serde_json::to_string(&json) {
                return body;
            }
        }
    }
    request_body.to_string()
}

fn inject_ide_thinking_fields(request_body: &str) -> String {
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body) else {
        return request_body.to_string();
    };

    let Some(fields) = json
        .get_mut("additionalModelRequestFields")
        .and_then(|v| v.as_object_mut())
    else {
        return request_body.to_string();
    };

    if !fields.contains_key("output_config") || fields.contains_key("thinking") {
        return request_body.to_string();
    }

    fields.insert(
        "thinking".to_string(),
        serde_json::json!({
            "type": "adaptive",
            "display": "summarized"
        }),
    );
    serde_json::to_string(&json).unwrap_or_else(|_| request_body.to_string())
}

#[cfg(test)]
mod tests {
    use super::{inject_ide_thinking_fields, inject_profile_arn};
    use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
    use crate::kiro::model::credentials::{BUILDER_ID_PROFILE_ARN, KiroCredentials};
    use crate::model::config::Config;
    use serde_json::Value;

    #[test]
    fn test_inject_profile_arn_with_some() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC".to_string());
        let result = inject_profile_arn(body, &arn);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_with_none() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let result = inject_profile_arn(body, &None);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_overwrites_existing() {
        let body = r#"{"conversationState":{},"profileArn":"old-arn"}"#;
        let arn = Some("new-arn".to_string());
        let result = inject_profile_arn(body, &arn);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "new-arn");
    }

    #[test]
    fn test_inject_profile_arn_invalid_json() {
        let body = "not-valid-json";
        let arn = Some("arn:test".to_string());
        let result = inject_profile_arn(body, &arn);
        assert_eq!(result, "not-valid-json");
    }

    #[test]
    fn test_inject_profile_arn_keeps_enterprise_idc_arn() {
        let body = r#"{"conversationState":{},"profileArn":"old-arn"}"#;
        let arn = Some("new-arn".to_string());
        let result = inject_profile_arn(body, &arn);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "new-arn");
    }

    #[test]
    fn test_ide_injects_thinking_for_output_config_effort() {
        let body = r#"{"conversationState":{},"additionalModelRequestFields":{"output_config":{"effort":"xhigh"}}}"#;
        let result = inject_ide_thinking_fields(body);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["additionalModelRequestFields"]["thinking"]["type"],
            "adaptive"
        );
        assert_eq!(
            json["additionalModelRequestFields"]["thinking"]["display"],
            "summarized"
        );
        assert_eq!(
            json["additionalModelRequestFields"]["output_config"]["effort"],
            "xhigh"
        );
    }

    #[test]
    fn test_ide_preserves_existing_thinking_field() {
        let body = r#"{"additionalModelRequestFields":{"thinking":{"type":"disabled"},"output_config":{"effort":"low"}}}"#;
        let result = inject_ide_thinking_fields(body);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["additionalModelRequestFields"]["thinking"]["type"],
            "disabled"
        );
    }

    #[test]
    fn test_ide_does_not_inject_thinking_for_reasoning_schema_path() {
        let body = r#"{"additionalModelRequestFields":{"reasoning":{"effort":"high"}}}"#;
        let result = inject_ide_thinking_fields(body);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(
            json["additionalModelRequestFields"]
                .get("thinking")
                .is_none()
        );
        assert_eq!(
            json["additionalModelRequestFields"]["reasoning"]["effort"],
            "high"
        );
    }

    #[test]
    fn test_ide_mcp_header_skips_builder_placeholder_profile_arn() {
        let endpoint = super::IdeEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials {
            profile_arn: Some(BUILDER_ID_PROFILE_ARN.to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };

        let req = endpoint
            .decorate_mcp(reqwest::Client::new().post("https://example.com"), &ctx)
            .build()
            .unwrap();
        assert!(req.headers().get("x-amzn-kiro-profile-arn").is_none());
    }

    #[test]
    fn test_ide_external_idp_uses_codewhisperer_api_surface() {
        let endpoint = super::IdeEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials {
            auth_method: Some("external_idp".to_string()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };

        assert_eq!(
            endpoint.api_url(&ctx),
            "https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse"
        );

        let req = endpoint
            .decorate_api(reqwest::Client::new().post(endpoint.api_url(&ctx)), &ctx)
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get("TokenType").and_then(|v| v.to_str().ok()),
            Some("EXTERNAL_IDP")
        );
        assert_eq!(
            req.headers().get("host").and_then(|v| v.to_str().ok()),
            Some("codewhisperer.us-east-1.amazonaws.com")
        );
    }
}
