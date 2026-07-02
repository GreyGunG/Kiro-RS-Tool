//! Kiro CLI 端点（Amazon Q for CLI）
//!
//! 对应 Kiro CLI / Amazon Q for CLI 使用的 AWS JSON 协议端点：
//! - URL: `https://runtime.{api_region}.kiro.dev/`（根路径 + x-amz-target 头）
//! - Content-Type: `application/x-amz-json-1.0`
//! - User-Agent: aws-sdk-rust 格式
//! - 请求体 origin: `KIRO_CLI`
//!
//! 适用于使用 `ksk_` 前缀 API Key 的凭据。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};

pub const CLI_ENDPOINT_NAME: &str = "cli";

pub struct CliEndpoint;

impl CliEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_api_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("runtime.{}.kiro.dev", self.api_region(ctx))
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererstreaming/0.1.16551 os/{} lang/rust/1.92.0 md/appVersion-{} app/AmazonQ-For-CLI",
            ctx.config.system_version, ctx.config.kiro_version,
        )
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererstreaming/0.1.16551 os/{} lang/rust/1.92.0 m/F app/AmazonQ-For-CLI",
            ctx.config.system_version,
        )
    }
}

impl Default for CliEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for CliEndpoint {
    fn name(&self) -> &'static str {
        CLI_ENDPOINT_NAME
    }

    fn content_type(&self) -> &'static str {
        "application/x-amz-json-1.0"
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://runtime.{}.kiro.dev/", self.api_region(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://q.{}.amazonaws.com/mcp", self.api_region(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header(
                "x-amz-target",
                "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
            )
            .header("x-amzn-codewhisperer-optout", "false")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
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
            .header("host", self.host(ctx))
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
        let body = rewrite_cli_body(body);
        inject_profile_arn(&body, &ctx.credentials.streaming_profile_arn())
    }
}

/// 将 profile_arn 注入到 CLI runtime 请求体 JSON 根对象。
///
/// Kiro CLI 2.6.0 的真实请求在 Enterprise/IdC 账号下同样携带 top-level
/// `profileArn`；否则 runtime 端会返回 `profileArn is required for this request`。
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

/// 将请求体转换为 KIRO_CLI 格式。只改协议字段，避免误改工具 schema 里的
/// `origin` / `modelId` 属性。
fn rewrite_cli_body(body: &str) -> String {
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&body) else {
        return body.to_string();
    };

    rewrite_origin_and_model(&mut json);
    strip_unsupported_cli_model_fields(&mut json);
    serde_json::to_string(&json).unwrap_or_else(|_| body.to_string())
}

fn strip_unsupported_cli_model_fields(json: &mut serde_json::Value) {
    let Some(fields) = json
        .get_mut("additionalModelRequestFields")
        .and_then(|v| v.as_object_mut())
    else {
        return;
    };

    // Captured Kiro CLI 2.6.0 `/effort xhigh` sends only
    // `additionalModelRequestFields.output_config.effort`. The IDE bundle may add
    // `thinking:{type:"adaptive",display:"summarized"}`, but sending that through
    // the CLI endpoint does not match the real client.
    fields.remove("thinking");
    if fields.is_empty() {
        json.as_object_mut()
            .expect("root request must be an object")
            .remove("additionalModelRequestFields");
    }
}

fn set_user_input_for_cli(uim: &mut serde_json::Value) {
    let Some(obj) = uim.as_object_mut() else {
        return;
    };
    if obj.contains_key("origin") {
        obj.insert(
            "origin".to_string(),
            serde_json::Value::String("KIRO_CLI".to_string()),
        );
    }
}

fn rewrite_origin_and_model(json: &mut serde_json::Value) {
    let Some(state) = json
        .get_mut("conversationState")
        .and_then(|v| v.as_object_mut())
    else {
        return;
    };
    if let Some(uim) = state
        .get_mut("currentMessage")
        .and_then(|v| v.get_mut("userInputMessage"))
    {
        set_user_input_for_cli(uim);
    }

    if let Some(history) = state.get_mut("history").and_then(|v| v.as_array_mut()) {
        for msg in history.iter_mut() {
            if let Some(user_input) = msg.get_mut("userInputMessage") {
                set_user_input_for_cli(user_input);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::credentials::{BUILDER_ID_PROFILE_ARN, KiroCredentials};
    use crate::model::config::Config;

    #[test]
    fn test_cli_api_url_uses_runtime_kiro_dev() {
        let endpoint = CliEndpoint::new();
        let config = Config::default();
        let credentials = KiroCredentials::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };

        assert_eq!(
            endpoint.api_url(&ctx),
            "https://runtime.us-east-1.kiro.dev/"
        );
    }

    #[test]
    fn test_set_origin_kiro_cli_current_message() {
        let body = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"content":"hi","origin":"AI_EDITOR"}}}}"#;
        let result = rewrite_cli_body(body);
        assert!(result.contains("\"origin\":\"KIRO_CLI\""));
        assert!(!result.contains("\"origin\":\"AI_EDITOR\""));
    }

    #[test]
    fn test_set_origin_kiro_cli_history() {
        let body = r#"{"conversationState":{"history":[{"userInputMessage":{"content":"hi","origin":"AI_EDITOR"}},{"userInputMessage":{"content":"hello","origin":"AI_EDITOR"}}],"currentMessage":{"userInputMessage":{"origin":"AI_EDITOR"}}}}"#;
        let result = rewrite_cli_body(body);
        assert!(!result.contains("\"origin\":\"AI_EDITOR\""));
        assert_eq!(result.matches("\"origin\":\"KIRO_CLI\"").count(), 3);
    }

    #[test]
    fn test_set_origin_kiro_cli_no_origin() {
        let body = r#"{"conversationState":{}}"#;
        assert_eq!(rewrite_cli_body(body), r#"{"conversationState":{}}"#);
    }

    #[test]
    fn test_cli_rewrite_only_changes_user_input_origin() {
        let body = r#"{
            "conversationState": {
                "agentContinuationId": "keep-me",
                "currentMessage": {
                    "userInputMessage": {
                        "origin": "AI_EDITOR",
                        "modelId": "claude-opus-4.8",
                        "userInputMessageContext": {
                            "tools": [{
                                "toolSpecification": {
                                    "name": "test",
                                    "description": "test",
                                    "inputSchema": {
                                        "json": {
                                            "type": "object",
                                            "properties": {
                                                "origin": {"type": "string", "description": "AI_EDITOR"},
                                                "modelId": {"type": "string", "default": "claude-opus-4.8"}
                                            }
                                        }
                                    }
                                }
                            }]
                        }
                    }
                }
            }
        }"#;

        let result = rewrite_cli_body(body);
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["conversationState"]["agentContinuationId"], "keep-me");
        let uim = &json["conversationState"]["currentMessage"]["userInputMessage"];
        assert_eq!(uim["origin"], "KIRO_CLI");
        assert_eq!(uim["modelId"], "claude-opus-4.8");
        let props = &uim["userInputMessageContext"]["tools"][0]["toolSpecification"]["inputSchema"]
            ["json"]["properties"];
        assert_eq!(props["origin"]["description"], "AI_EDITOR");
        assert_eq!(props["modelId"]["default"], "claude-opus-4.8");
    }

    #[test]
    fn test_cli_rewrite_removes_ide_thinking_wrapper() {
        let body = r#"{
            "conversationState": {
                "currentMessage": {
                    "userInputMessage": {
                        "origin": "AI_EDITOR",
                        "modelId": "claude-opus-4.8"
                    }
                }
            },
            "additionalModelRequestFields": {
                "thinking": {"type":"adaptive","display":"summarized"},
                "output_config": {"effort":"xhigh"}
            }
        }"#;

        let result = rewrite_cli_body(body);
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(
            json["additionalModelRequestFields"]
                .get("thinking")
                .is_none()
        );
        assert_eq!(
            json["additionalModelRequestFields"]["output_config"]["effort"],
            "xhigh"
        );
    }

    #[test]
    fn test_cli_rewrite_preserves_reasoning_schema_path() {
        let body = r#"{
            "conversationState": {
                "currentMessage": {
                    "userInputMessage": {
                        "origin": "AI_EDITOR",
                        "modelId": "claude-opus-4.8"
                    }
                }
            },
            "additionalModelRequestFields": {
                "thinking": {"type":"adaptive","display":"summarized"},
                "reasoning": {"effort":"high"}
            }
        }"#;

        let result = rewrite_cli_body(body);
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
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
    fn test_cli_transform_injects_profile_arn_for_enterprise() {
        let endpoint = CliEndpoint::new();
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.profile_arn =
            Some("arn:aws:codewhisperer:us-east-1:123:profile/CLI".to_string());
        let ctx = RequestContext {
            credentials: &credentials,
            token: "token",
            machine_id: "machine",
            config: &config,
        };
        let body = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"origin":"AI_EDITOR"}}}}"#;

        let result = endpoint.transform_api_body(body, &ctx);
        let json: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/CLI"
        );
        assert_eq!(
            json["conversationState"]["currentMessage"]["userInputMessage"]["origin"],
            "KIRO_CLI"
        );
    }

    #[test]
    fn test_cli_mcp_header_skips_builder_placeholder_profile_arn() {
        let endpoint = CliEndpoint::new();
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
    fn test_cli_external_idp_adds_token_type_header() {
        let endpoint = CliEndpoint::new();
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

        let req = endpoint
            .decorate_api(reqwest::Client::new().post("https://example.com"), &ctx)
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get("TokenType").and_then(|v| v.to_str().ok()),
            Some("EXTERNAL_IDP")
        );
    }
}
