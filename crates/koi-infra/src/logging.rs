//! 日志中使用的通用敏感字段脱敏工具。
//!
//! 事件和模型协议日志需要尽量保留完整上下文，便于排查 Agent 的决策过程；同时不能
//! 把配置中的 API key、密码或访问令牌原样写入日志。因此这里只按 JSON 字段名脱敏，
//! 不修改事件存储本身，也不改变发送给模型或工具的实际内容。

use serde_json::{Map, Value};

const SENSITIVE_KEY_MARKERS: [&str; 9] = [
    "password",
    "passwd",
    "token",
    "secret",
    "api_key",
    "apikey",
    "authorization",
    "private_key",
    "access_key",
];

/// 将 JSON 中名称明显表示凭据的字段替换为 `<redacted>`。
pub(crate) fn redact_json(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(redact_object(object)),
        Value::Array(values) => Value::Array(values.into_iter().map(redact_json).collect()),
        value => value,
    }
}

/// 脱敏并序列化 JSON 文本；输入不是 JSON 时原样返回。
pub(crate) fn redact_json_text(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_owned();
    };
    serde_json::to_string(&redact_json(value)).unwrap_or_else(|_| raw.to_owned())
}

fn redact_object(object: Map<String, Value>) -> Map<String, Value> {
    object
        .into_iter()
        .map(|(key, value)| {
            let lower_key = key.to_ascii_lowercase();
            if SENSITIVE_KEY_MARKERS
                .iter()
                .any(|marker| lower_key.contains(marker))
            {
                (key, Value::String("<redacted>".into()))
            } else {
                (key, redact_json(value))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{redact_json, redact_json_text};
    use serde_json::json;

    #[test]
    fn redacts_nested_credential_fields_without_hiding_reasoning_text() {
        let value = redact_json(json!({
            "api_key": "secret-key",
            "nested": {
                "accessToken": "secret-token",
                "reasoning": "先检查服务状态，再查看日志",
            },
            "items": [{"password": "secret-password"}],
        }));
        assert_eq!(value["api_key"], "<redacted>");
        assert_eq!(value["nested"]["accessToken"], "<redacted>");
        assert_eq!(value["nested"]["reasoning"], "先检查服务状态，再查看日志");
        assert_eq!(value["items"][0]["password"], "<redacted>");
    }

    #[test]
    fn non_json_sse_marker_is_kept() {
        assert_eq!(redact_json_text("[DONE]"), "[DONE]");
    }
}
