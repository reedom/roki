//! `linear_graphql` agent tool.
//!
//! Forwards a single GraphQL operation to Linear using the daemon-owned token.
//! The token never appears in tool input, output, or any error variant — every
//! string returned to the agent passes through [`redact`] before it leaves the
//! tool boundary (req 7.4).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::{RateLimitState, RateLimited, Tool, ToolError};
use crate::config::SecretString;

/// Default Linear GraphQL endpoint. Constructor-injected in tests so they can
/// point at a `wiremock` server instead of the real Linear API.
pub const DEFAULT_LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";

const INPUT_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "additionalProperties": false,
  "required": ["query"],
  "properties": {
    "query": {
      "type": "string",
      "description": "A GraphQL document containing exactly one operation."
    },
    "variables": {
      "type": "object",
      "description": "Variables for the GraphQL operation.",
      "additionalProperties": true
    }
  }
}"#;

const OUTPUT_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "additionalProperties": true,
  "properties": {
    "data": {},
    "errors": {
      "type": "array"
    }
  }
}"#;

/// `linear_graphql` proxy tool.
pub struct LinearGraphqlTool {
    endpoint: String,
    token: SecretString,
    http: reqwest::Client,
    rate_limit: Arc<dyn RateLimitState>,
}

impl LinearGraphqlTool {
    /// Build a tool pointing at `endpoint`. Callers in production pass
    /// [`DEFAULT_LINEAR_ENDPOINT`]; tests pass a `wiremock` URL.
    pub fn new(
        endpoint: impl Into<String>,
        token: SecretString,
        rate_limit: Arc<dyn RateLimitState>,
    ) -> Result<Self, ToolError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|err| ToolError::Network {
                message: redact(&err.to_string(), token.expose()),
            })?;
        Ok(Self {
            endpoint: endpoint.into(),
            token,
            http,
            rate_limit,
        })
    }

    /// Test-only constructor that injects a pre-built reqwest client.
    #[cfg(test)]
    pub(crate) fn with_client(
        endpoint: impl Into<String>,
        token: SecretString,
        http: reqwest::Client,
        rate_limit: Arc<dyn RateLimitState>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            token,
            http,
            rate_limit,
        }
    }
}

#[async_trait]
impl Tool for LinearGraphqlTool {
    fn name(&self) -> &'static str {
        "linear-graphql"
    }

    fn description(&self) -> &'static str {
        "Proxy a single Linear GraphQL operation through the daemon-owned token."
    }

    fn input_schema(&self) -> &'static str {
        INPUT_SCHEMA
    }

    fn output_schema(&self) -> &'static str {
        OUTPUT_SCHEMA
    }

    async fn call(&self, input: Value) -> Result<Value, ToolError> {
        // 1. Parse the agent-supplied input.
        let request = parse_input(input)?;

        // 2. Multi-operation guard runs before any side effects (req 7.3).
        let op_count = count_operations(&request.query);
        if op_count == 0 {
            return Err(ToolError::InvalidInput {
                reason: "query must contain at least one GraphQL operation".to_string(),
            });
        }
        if 2 <= op_count {
            return Err(ToolError::MultipleOperations);
        }

        // 3. Ask the shared rate-limit state whether we are clear to send.
        if let Err(RateLimited {
            retry_after_seconds,
        }) = self.rate_limit.before_call().await
        {
            return Err(ToolError::RateLimited {
                retry_after_seconds,
            });
        }

        // 4. Forward to Linear. Every error string from this point on passes
        // through `redact` so the daemon-owned token cannot leak (req 7.4).
        let body = serde_json::json!({
            "query": request.query,
            "variables": request.variables,
        });

        let response = self
            .http
            .post(&self.endpoint)
            .header(
                reqwest::header::AUTHORIZATION,
                self.token.expose().to_string(),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|err| ToolError::Network {
                message: redact(&err.to_string(), self.token.expose()),
            })?;

        let status = response.status();
        let retry_after = parse_retry_after(response.headers());

        self.rate_limit
            .record_response(status.as_u16(), retry_after)
            .await;

        if status.as_u16() == 429 {
            return Err(ToolError::RateLimited {
                retry_after_seconds: retry_after,
            });
        }

        if !status.is_success() {
            // Drain the body for tracing context but never echo it back to the
            // agent — only the status code is safe to return.
            let _ = response.text().await;
            return Err(ToolError::LinearHttpError {
                status: status.as_u16(),
            });
        }

        let payload: Value = response.json().await.map_err(|err| ToolError::Network {
            message: redact(&err.to_string(), self.token.expose()),
        })?;

        // 5. Final defence in depth: scrub the token from anywhere it may have
        // leaked into the response payload before handing it back.
        Ok(scrub_value(payload, self.token.expose()))
    }
}

#[derive(Debug)]
struct ParsedInput {
    query: String,
    variables: Value,
}

fn parse_input(input: Value) -> Result<ParsedInput, ToolError> {
    let obj = input.as_object().ok_or_else(|| ToolError::InvalidInput {
        reason: "input must be a JSON object".to_string(),
    })?;
    let query = obj
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput {
            reason: "missing required field 'query'".to_string(),
        })?
        .to_string();
    let variables = obj
        .get("variables")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    if !variables.is_object() {
        return Err(ToolError::InvalidInput {
            reason: "'variables' must be an object".to_string(),
        });
    }
    Ok(ParsedInput { query, variables })
}

/// Count top-level GraphQL operations.
///
/// We do not need a full GraphQL parser here. The proxy is intentionally
/// agnostic of the schema (req 7.2) and only enforces the single-operation
/// invariant (req 7.3). The counter walks the document with a small state
/// machine that:
/// - skips `#` line comments and double-quoted strings (incl. block strings);
/// - tracks brace depth so keywords inside selection sets are ignored;
/// - matches the shorthand form (a leading `{` selection set) as one operation.
pub(crate) fn count_operations(query: &str) -> usize {
    let bytes = query.as_bytes();
    let mut idx = 0usize;
    let mut depth: i32 = 0;
    let mut count = 0usize;
    // True once a top-level operation has begun (either a named keyword or a
    // shorthand `{`) and not yet finished. The flag resets when depth returns
    // to 0, so the next top-level construct is counted as a new operation.
    let mut in_operation = false;

    while idx < bytes.len() {
        let byte = bytes[idx];
        match byte {
            b'#' => {
                // Skip to end of line.
                while idx < bytes.len() && bytes[idx] != b'\n' {
                    idx += 1;
                }
            }
            b'"' => {
                // Handle block strings ("""...""") and regular strings.
                if idx + 3 <= bytes.len() && &bytes[idx..idx + 3] == b"\"\"\"" {
                    idx += 3;
                    while idx + 3 <= bytes.len() && &bytes[idx..idx + 3] != b"\"\"\"" {
                        idx += 1;
                    }
                    idx = idx.saturating_add(3).min(bytes.len());
                } else {
                    idx += 1;
                    while idx < bytes.len() {
                        match bytes[idx] {
                            b'\\' => idx += 2,
                            b'"' => {
                                idx += 1;
                                break;
                            }
                            _ => idx += 1,
                        }
                    }
                }
            }
            b'{' => {
                if depth == 0 && !in_operation {
                    // Anonymous shorthand operation: `{ field }`.
                    count += 1;
                    in_operation = true;
                }
                depth += 1;
                idx += 1;
            }
            b'}' => {
                depth -= 1;
                if depth <= 0 {
                    depth = 0;
                    // The current top-level operation just closed; the next
                    // keyword or shorthand `{` starts a new one.
                    in_operation = false;
                }
                idx += 1;
            }
            _ if depth == 0 && !in_operation && is_keyword_start(bytes, idx) => {
                let (matched_len, _kind) =
                    keyword_at(bytes, idx).expect("is_keyword_start guarantees a match");
                count += 1;
                in_operation = true;
                idx += matched_len;
            }
            _ => {
                idx += 1;
            }
        }
    }
    count
}

fn is_keyword_start(bytes: &[u8], idx: usize) -> bool {
    // Keywords can only start at a word boundary.
    if idx > 0 {
        let prev = bytes[idx - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return false;
        }
    }
    keyword_at(bytes, idx).is_some()
}

fn keyword_at(bytes: &[u8], idx: usize) -> Option<(usize, &'static str)> {
    const KEYWORDS: &[&str] = &["query", "mutation", "subscription"];
    for kw in KEYWORDS {
        let kb = kw.as_bytes();
        let end = idx + kb.len();
        if end <= bytes.len() && &bytes[idx..end] == kb {
            // Ensure the next character is not part of an identifier — that
            // would make this a longer name like `mutationFoo`.
            let next_ok =
                end == bytes.len() || (!bytes[end].is_ascii_alphanumeric() && bytes[end] != b'_');
            if next_ok {
                return Some((kb.len(), *kw));
            }
        }
    }
    None
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Replace every occurrence of `token` in `text` with a fixed mask. Empty
/// tokens are not scrubbed — the constructor guarantees a non-empty value via
/// [`SecretString`] (config layer rejects empty tokens at startup).
pub(crate) fn redact(text: &str, token: &str) -> String {
    if token.is_empty() {
        return text.to_string();
    }
    text.replace(token, "***")
}

/// Walk a JSON value and scrub any string occurrence of `token`.
pub(crate) fn scrub_value(value: Value, token: &str) -> Value {
    if token.is_empty() {
        return value;
    }
    match value {
        Value::String(s) => {
            if s.contains(token) {
                Value::String(s.replace(token, "***"))
            } else {
                Value::String(s)
            }
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| scrub_value(v, token)).collect())
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k, scrub_value(v, token));
            }
            Value::Object(out)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_zero_operations_in_empty_document() {
        assert_eq!(count_operations(""), 0);
        assert_eq!(count_operations("   \n  "), 0);
    }

    #[test]
    fn counts_single_named_operation() {
        let doc = "query GetIssue($id: String!) { issue(id: $id) { id title } }";
        assert_eq!(count_operations(doc), 1);
    }

    #[test]
    fn counts_single_shorthand_operation() {
        assert_eq!(count_operations("{ viewer { id } }"), 1);
    }

    #[test]
    fn counts_two_named_operations() {
        let doc = "query A { viewer { id } } mutation B { issueCreate(input: {}) { success } }";
        assert_eq!(count_operations(doc), 2);
    }

    #[test]
    fn ignores_keywords_inside_selection_sets() {
        // "mutation" appears as a field name, not as a top-level operation.
        let doc = "query A { mutation { id } }";
        assert_eq!(count_operations(doc), 1);
    }

    #[test]
    fn ignores_keywords_inside_strings_and_comments() {
        let doc = r#"# query inside a comment
query A { node(id: "mutation { evil }") { id } }
"#;
        assert_eq!(count_operations(doc), 1);
    }

    #[test]
    fn redact_replaces_every_token_occurrence() {
        let token = "lin_api_super_secret_value";
        let msg = format!(
            "request to https://api.linear.app failed with header Authorization: {token}; retry sent {token}"
        );
        let scrubbed = redact(&msg, token);
        assert!(!scrubbed.contains(token));
        assert!(scrubbed.contains("***"));
    }

    #[test]
    fn redact_is_noop_for_empty_token() {
        assert_eq!(redact("hello", ""), "hello");
    }

    #[test]
    fn scrub_value_walks_nested_strings() {
        let token = "lin_secret";
        let value = serde_json::json!({
            "errors": [
                { "message": format!("auth failed: {token}") },
                { "message": "ok" }
            ],
            "data": {
                "leak": format!("Authorization: {token}")
            }
        });
        let scrubbed = scrub_value(value, token);
        let s = serde_json::to_string(&scrubbed).unwrap();
        assert!(!s.contains(token));
        assert!(s.contains("***"));
    }

    #[tokio::test]
    async fn rejects_multi_operation_documents_without_http_call() {
        // No reqwest client is needed because the multi-op guard runs first.
        let tool = LinearGraphqlTool::with_client(
            "http://127.0.0.1:1/should-never-be-called",
            SecretString::new("lin_test_token"),
            reqwest::Client::new(),
            Arc::new(crate::tools::NoopRateLimit),
        );

        let input = serde_json::json!({
            "query": "query A { viewer { id } } query B { viewer { id } }",
            "variables": {}
        });

        let err = tool.call(input).await.unwrap_err();
        assert!(matches!(err, ToolError::MultipleOperations));
    }

    #[tokio::test]
    async fn rejects_zero_operation_documents() {
        let tool = LinearGraphqlTool::with_client(
            "http://127.0.0.1:1/should-never-be-called",
            SecretString::new("lin_test_token"),
            reqwest::Client::new(),
            Arc::new(crate::tools::NoopRateLimit),
        );
        let input = serde_json::json!({
            "query": "# only a comment, no operation\n",
            "variables": {}
        });
        let err = tool.call(input).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn injected_token_in_synthetic_failure_is_redacted_before_return() {
        // Construct a Network error string that contains the token, then run
        // it through the same redaction path the tool uses. This test pins
        // the contract that no error variant returned to the caller carries
        // the raw token (req 7.4).
        let token = "lin_api_super_secret_value";
        let synthetic =
            format!("connection failed while sending Authorization: {token} to upstream");
        let err = ToolError::Network {
            message: redact(&synthetic, token),
        };

        let rendered = err.to_string();
        assert!(
            !rendered.contains(token),
            "rendered error leaked the token: {rendered}"
        );
        assert!(rendered.contains("***"));
    }
}
