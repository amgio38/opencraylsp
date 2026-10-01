//! JSON-RPC 2.0 envelope types and the error codes of daemon protocol v1.
//! One message per line on the wire; a line longer than 4 MiB is a parse error.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC "internal error": the daemon failed to produce an answer at all.
/// Reserved for that case — a tool that ran and failed is a `ToolOutput`, not
/// an error of this kind.
pub const INTERNAL_ERROR: i64 = -32603;
pub const PROTOCOL_MISMATCH: i64 = -32001;
pub const NOT_INITIALIZED: i64 = -32002;
pub const WORKSPACE_INVALID: i64 = -32003;
pub const SHUTTING_DOWN: i64 = -32004;
pub const UNKNOWN_LANGUAGE: i64 = -32005;

/// Longest accepted line, in bytes.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// A request (has an `id`) or a notification (no `id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// A request with a numeric id.
    pub fn new(id: u64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id: Some(Value::from(id)),
            method: method.to_owned(),
            params,
        }
    }

    /// A notification (no id, no reply).
    pub fn notification(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id: None,
            method: method.to_owned(),
            params,
        }
    }
}

/// The `error` member of a failed response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A response: exactly one of `result` / `error` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notification_has_no_id_on_the_wire() {
        let v = serde_json::to_value(Request::notification("$/cancel", json!({"id": 1}))).unwrap();
        assert!(v.get("id").is_none());
        assert_eq!(v["jsonrpc"], "2.0");
    }

    #[test]
    fn request_round_trips_with_numeric_id() {
        let r = Request::new(7, "status", json!({}));
        let back: Request = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.id, Some(json!(7)));
    }

    #[test]
    fn response_carries_result_or_error_never_both() {
        let ok = serde_json::to_value(Response::ok(json!(1), json!({"a": 1}))).unwrap();
        assert!(ok.get("error").is_none());
        let err =
            serde_json::to_value(Response::err(json!(1), UNKNOWN_LANGUAGE, "bad", None)).unwrap();
        assert!(err.get("result").is_none());
        assert_eq!(err["error"]["code"], json!(-32005));
    }

    #[test]
    fn missing_params_deserialize_to_null() {
        let r: Request = serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"m"}"#).unwrap();
        assert_eq!(r.params, Value::Null);
    }
}
