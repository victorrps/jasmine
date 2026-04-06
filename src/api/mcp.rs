use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::config::AppConfig;
use crate::errors::AppError;
use crate::services::{pdf_parser, schema_extractor};

// ── MCP JSON-RPC Types ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

impl JsonRpcResponse {
    fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<serde_json::Value>, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError { code, message }),
        }
    }
}

// ── MCP Protocol Types ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ServerInfo {
    name: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
    capabilities: Capabilities,
    #[serde(rename = "serverInfo")]
    server_info: ServerInfo,
}

#[derive(Debug, Serialize)]
struct Capabilities {
    tools: ToolsCapability,
}

#[derive(Debug, Serialize)]
struct ToolsCapability {
    #[serde(rename = "listChanged")]
    list_changed: bool,
}

#[derive(Debug, Serialize)]
struct ToolDef {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ToolResult {
    content: Vec<ToolContent>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ToolContent {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

// ── MCP Handler ──────────────────────────────────────────────────────────────

/// POST /mcp — MCP Streamable HTTP endpoint (JSON-RPC).
pub async fn mcp_handler(
    body: web::Json<JsonRpcRequest>,
    _pool: web::Data<SqlitePool>,
    config: web::Data<AppConfig>,
) -> Result<HttpResponse, AppError> {
    let req = body.into_inner();

    let response = match req.method.as_str() {
        "initialize" => handle_initialize(req.id),
        "notifications/initialized" => return Ok(HttpResponse::Ok().finish()),
        "tools/list" => handle_list_tools(req.id),
        "tools/call" => handle_call_tool(req.id, req.params, &config).await,
        "ping" => JsonRpcResponse::success(req.id, serde_json::json!({})),
        _ => JsonRpcResponse::error(req.id, -32601, format!("Method not found: {}", req.method)),
    };

    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .json(response))
}

fn handle_initialize(id: Option<serde_json::Value>) -> JsonRpcResponse {
    let result = InitializeResult {
        protocol_version: "2025-03-26".into(),
        capabilities: Capabilities {
            tools: ToolsCapability {
                list_changed: false,
            },
        },
        server_info: ServerInfo {
            name: "docforge".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
    };
    JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default())
}

fn handle_list_tools(id: Option<serde_json::Value>) -> JsonRpcResponse {
    let tools = vec![
        ToolDef {
            name: "parse_document".into(),
            description: "Parse a PDF document and return structured markdown, text, tables, and metadata. Accepts base64-encoded PDF data.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pdf_base64": {
                        "type": "string",
                        "description": "Base64-encoded PDF file content"
                    }
                },
                "required": ["pdf_base64"]
            }),
        },
        ToolDef {
            name: "extract_fields".into(),
            description: "Extract structured fields from a PDF according to a JSON schema. Uses Claude Haiku for intelligent extraction.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pdf_base64": {
                        "type": "string",
                        "description": "Base64-encoded PDF file content"
                    },
                    "schema": {
                        "type": "object",
                        "description": "JSON Schema defining the fields to extract"
                    }
                },
                "required": ["pdf_base64", "schema"]
            }),
        },
    ];

    JsonRpcResponse::success(id, serde_json::json!({ "tools": tools }))
}

async fn handle_call_tool(
    id: Option<serde_json::Value>,
    params: serde_json::Value,
    config: &AppConfig,
) -> JsonRpcResponse {
    let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    match tool_name {
        "parse_document" => call_parse_document(id, arguments).await,
        "extract_fields" => call_extract_fields(id, arguments, config).await,
        _ => JsonRpcResponse::error(id, -32602, format!("Unknown tool: {tool_name}")),
    }
}

async fn call_parse_document(
    id: Option<serde_json::Value>,
    arguments: serde_json::Value,
) -> JsonRpcResponse {
    let pdf_b64 = match arguments.get("pdf_base64").and_then(|v| v.as_str()) {
        Some(b) => b,
        None => {
            return JsonRpcResponse::error(id, -32602, "Missing required field: pdf_base64".into())
        }
    };

    let bytes = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, pdf_b64) {
        Ok(b) => b,
        Err(e) => return JsonRpcResponse::error(id, -32602, format!("Invalid base64: {e}")),
    };

    const MAX_PDF_BYTES: usize = 50 * 1024 * 1024;
    if bytes.len() > MAX_PDF_BYTES {
        return JsonRpcResponse::error(id, -32602, "PDF exceeds maximum size of 50MB".into());
    }
    if bytes.len() < 64 || &bytes[..5] != b"%PDF-" {
        return JsonRpcResponse::error(id, -32602, "Not a valid PDF file".into());
    }

    match tokio::task::spawn_blocking(move || pdf_parser::parse_pdf(&bytes, "pdftoppm")).await {
        Ok(Ok(result)) => {
            let text = match serde_json::to_string_pretty(&result) {
                Ok(t) => t,
                Err(e) => {
                    return JsonRpcResponse::error(id, -32603, format!("Serialization error: {e}"))
                }
            };
            let tool_result = ToolResult {
                content: vec![ToolContent {
                    content_type: "text".into(),
                    text,
                }],
                is_error: None,
            };
            match serde_json::to_value(tool_result) {
                Ok(v) => JsonRpcResponse::success(id, v),
                Err(e) => JsonRpcResponse::error(id, -32603, format!("Serialization error: {e}")),
            }
        }
        Ok(Err(e)) => JsonRpcResponse::error(id, -32000, format!("Parse failed: {e}")),
        Err(e) => JsonRpcResponse::error(id, -32000, format!("Internal error: {e}")),
    }
}

async fn call_extract_fields(
    id: Option<serde_json::Value>,
    arguments: serde_json::Value,
    config: &AppConfig,
) -> JsonRpcResponse {
    let pdf_b64 = match arguments.get("pdf_base64").and_then(|v| v.as_str()) {
        Some(b) => b,
        None => {
            return JsonRpcResponse::error(id, -32602, "Missing required field: pdf_base64".into())
        }
    };

    let schema = match arguments.get("schema") {
        Some(s) => s.clone(),
        None => return JsonRpcResponse::error(id, -32602, "Missing required field: schema".into()),
    };

    let bytes = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, pdf_b64) {
        Ok(b) => b,
        Err(e) => return JsonRpcResponse::error(id, -32602, format!("Invalid base64: {e}")),
    };

    const MAX_PDF_BYTES: usize = 50 * 1024 * 1024;
    if bytes.len() > MAX_PDF_BYTES {
        return JsonRpcResponse::error(id, -32602, "PDF exceeds maximum size of 50MB".into());
    }
    if bytes.len() < 64 || &bytes[..5] != b"%PDF-" {
        return JsonRpcResponse::error(id, -32602, "Not a valid PDF file".into());
    }

    let parse_result = match tokio::task::spawn_blocking(move || pdf_parser::parse_pdf(&bytes, "pdftoppm")).await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return JsonRpcResponse::error(id, -32000, format!("Parse failed: {e}")),
        Err(e) => return JsonRpcResponse::error(id, -32000, format!("Internal error: {e}")),
    };

    let extracted = match schema_extractor::extract_with_schema(
        &parse_result.document.markdown,
        &schema,
        config.anthropic_api_key.as_deref(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return JsonRpcResponse::error(id, -32000, format!("Extraction failed: {e}")),
    };

    let output = serde_json::json!({
        "document": parse_result.document,
        "extracted": extracted,
        "usage": parse_result.usage,
    });

    let text = match serde_json::to_string_pretty(&output) {
        Ok(t) => t,
        Err(e) => return JsonRpcResponse::error(id, -32603, format!("Serialization error: {e}")),
    };
    let tool_result = ToolResult {
        content: vec![ToolContent {
            content_type: "text".into(),
            text,
        }],
        is_error: None,
    };
    match serde_json::to_value(tool_result) {
        Ok(v) => JsonRpcResponse::success(id, v),
        Err(e) => JsonRpcResponse::error(id, -32603, format!("Serialization error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── JsonRpcResponse constructors ─────────────────────────────────────────

    #[test]
    fn success_response_sets_jsonrpc_version() {
        let resp = JsonRpcResponse::success(Some(serde_json::json!(1)), serde_json::json!({}));
        assert_eq!(resp.jsonrpc, "2.0");
    }

    #[test]
    fn success_response_sets_result_and_clears_error() {
        let resp =
            JsonRpcResponse::success(Some(serde_json::json!(42)), serde_json::json!({"ok": true}));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn success_response_preserves_id() {
        let id = serde_json::json!("req-99");
        let resp = JsonRpcResponse::success(Some(id.clone()), serde_json::json!({}));
        assert_eq!(resp.id, Some(id));
    }

    #[test]
    fn success_response_allows_null_id() {
        let resp = JsonRpcResponse::success(None, serde_json::json!({}));
        assert!(resp.id.is_none());
    }

    #[test]
    fn error_response_sets_jsonrpc_version() {
        let resp = JsonRpcResponse::error(None, -32601, "Method not found".into());
        assert_eq!(resp.jsonrpc, "2.0");
    }

    #[test]
    fn error_response_sets_error_and_clears_result() {
        let resp = JsonRpcResponse::error(Some(serde_json::json!(1)), -32601, "Not found".into());
        assert!(resp.error.is_some());
        assert!(resp.result.is_none());
    }

    #[test]
    fn error_response_captures_code_and_message() {
        let resp = JsonRpcResponse::error(None, -32602, "Invalid params: missing field".into());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "Invalid params: missing field");
    }

    // ── handle_initialize ────────────────────────────────────────────────────

    #[test]
    fn initialize_returns_correct_protocol_version() {
        let resp = handle_initialize(Some(serde_json::json!(1)));
        let result = resp.result.expect("initialize must have a result");
        assert_eq!(result["protocolVersion"], "2025-03-26");
    }

    #[test]
    fn initialize_result_contains_server_info_with_docforge_name() {
        let resp = handle_initialize(None);
        let result = resp.result.expect("initialize must have a result");
        assert_eq!(result["serverInfo"]["name"], "docforge");
    }

    #[test]
    fn initialize_result_server_info_version_is_non_empty() {
        let resp = handle_initialize(None);
        let result = resp.result.expect("initialize must have a result");
        let version = result["serverInfo"]["version"].as_str().unwrap_or("");
        assert!(!version.is_empty(), "serverInfo.version must be non-empty");
    }

    #[test]
    fn initialize_result_capabilities_tools_list_changed_is_false() {
        let resp = handle_initialize(None);
        let result = resp.result.expect("initialize must have a result");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
    }

    #[test]
    fn initialize_preserves_id() {
        let id = serde_json::json!("init-1");
        let resp = handle_initialize(Some(id.clone()));
        assert_eq!(resp.id, Some(id));
    }

    #[test]
    fn initialize_error_field_is_none() {
        let resp = handle_initialize(None);
        assert!(resp.error.is_none());
    }

    // ── handle_list_tools ────────────────────────────────────────────────────

    #[test]
    fn list_tools_returns_exactly_two_tools() {
        let resp = handle_list_tools(None);
        let result = resp.result.expect("tools/list must have a result");
        let tools = result["tools"].as_array().expect("tools must be an array");
        assert_eq!(
            tools.len(),
            2,
            "expected exactly 2 tools, got {}",
            tools.len()
        );
    }

    #[test]
    fn list_tools_first_tool_is_parse_document() {
        let resp = handle_list_tools(None);
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "parse_document");
    }

    #[test]
    fn list_tools_second_tool_is_extract_fields() {
        let resp = handle_list_tools(None);
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools[1]["name"], "extract_fields");
    }

    #[test]
    fn list_tools_parse_document_requires_pdf_base64() {
        let resp = handle_list_tools(None);
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        let schema = &tools[0]["inputSchema"];
        let required = schema["required"]
            .as_array()
            .expect("required must be array");
        assert!(
            required.iter().any(|v| v == "pdf_base64"),
            "parse_document must require pdf_base64"
        );
    }

    #[test]
    fn list_tools_extract_fields_requires_pdf_base64_and_schema() {
        let resp = handle_list_tools(None);
        let result = resp.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        let schema = &tools[1]["inputSchema"];
        let required = schema["required"]
            .as_array()
            .expect("required must be array");
        assert!(
            required.iter().any(|v| v == "pdf_base64"),
            "extract_fields must require pdf_base64"
        );
        assert!(
            required.iter().any(|v| v == "schema"),
            "extract_fields must require schema"
        );
    }

    #[test]
    fn list_tools_error_field_is_none() {
        let resp = handle_list_tools(None);
        assert!(resp.error.is_none());
    }

    #[test]
    fn list_tools_preserves_id() {
        let id = serde_json::json!(7);
        let resp = handle_list_tools(Some(id.clone()));
        assert_eq!(resp.id, Some(id));
    }

    // ── parse_document — validation of base64 and PDF magic bytes ───────────

    #[tokio::test]
    async fn call_parse_document_returns_error_on_invalid_base64() {
        let args = serde_json::json!({ "pdf_base64": "not-valid-base64!!!" });
        let resp = call_parse_document(Some(serde_json::json!(1)), args).await;
        let err = resp.error.expect("must return error for invalid base64");
        assert_eq!(err.code, -32602);
        assert!(
            err.message.contains("Invalid base64") || err.message.contains("base64"),
            "error message should mention base64, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn call_parse_document_returns_error_when_pdf_base64_missing() {
        let args = serde_json::json!({});
        let resp = call_parse_document(None, args).await;
        let err = resp
            .error
            .expect("must return error when pdf_base64 is missing");
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("pdf_base64"));
    }

    #[tokio::test]
    async fn call_parse_document_returns_error_for_non_pdf_bytes() {
        // Base64-encode bytes that are NOT a PDF (missing %PDF- magic)
        let fake_bytes = b"This is just plain text, not a PDF at all.";
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, fake_bytes);
        let args = serde_json::json!({ "pdf_base64": b64 });
        let resp = call_parse_document(Some(serde_json::json!(2)), args).await;
        let err = resp.error.expect("must return error for non-PDF content");
        assert_eq!(err.code, -32602);
        assert!(
            err.message.contains("PDF") || err.message.contains("valid"),
            "error should mention invalid PDF, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn call_parse_document_returns_error_for_empty_bytes() {
        // Empty payload is not a PDF
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"");
        let args = serde_json::json!({ "pdf_base64": b64 });
        let resp = call_parse_document(Some(serde_json::json!(3)), args).await;
        let err = resp.error.expect("must return error for empty content");
        assert_eq!(err.code, -32602);
    }

    // ── unknown method ───────────────────────────────────────────────────────
    // (tested via the public mcp_handler in integration tests; here we confirm
    //  the error constructor produces the correct -32601 code shape)

    #[test]
    fn unknown_method_error_has_correct_code() {
        let resp = JsonRpcResponse::error(
            Some(serde_json::json!(5)),
            -32601,
            "Method not found: unknown.method".into(),
        );
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
        assert!(err.message.contains("Method not found"));
    }
}
