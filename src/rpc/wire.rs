use serde::Deserialize;
use serde_json::Value;

/// `{"id":.., "method":.., "params":{..}}` - id may be any JSON value or
/// absent entirely (treated as null, matching the C daemon's idNode
/// handling in daemon/nobilis/api.c).
#[derive(Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

pub fn ok_response(id: &Value, result: Value) -> Value {
    serde_json::json!({ "id": id, "result": result })
}

pub fn err_response(id: &Value, message: &str) -> Value {
    serde_json::json!({ "id": id, "error": message })
}

pub fn ok_node() -> Value {
    serde_json::json!({ "ok": true })
}

/// `params.<key>` as a &str, or `default` if missing/wrong type - local
/// trusted socket, so we validate presence but don't fight malformed
/// requests beyond that (matches api.c's p_str/p_int/p_bool helpers).
pub fn p_str<'a>(params: &'a Value, key: &str, default: &'a str) -> &'a str {
    params.get(key).and_then(|v| v.as_str()).unwrap_or(default)
}

pub fn p_str_opt<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(|v| v.as_str())
}

pub fn p_i64(params: &Value, key: &str, default: i64) -> i64 {
    params.get(key).and_then(|v| v.as_i64()).unwrap_or(default)
}

pub fn p_bool(params: &Value, key: &str, default: bool) -> bool {
    params.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}
