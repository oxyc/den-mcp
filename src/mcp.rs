//! MCP over Streamable HTTP, by hand: one JSON-RPC message per POST to `/mcp`, answered with one JSON body. The
//! server keeps no session and never opens a stream of its own, which the transport allows — so there is no
//! `Mcp-Session-Id` and `GET /mcp` is a 405. Only what a tools server needs: `initialize`, `ping`, `tools/list` and
//! `tools/call`.

use serde_json::{json, Value};

/// The protocol versions this server speaks, newest first. A client asking for another is answered with the newest.
pub const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

const INSTRUCTIONS: &str = "Den is a household's film and series discovery index (~47k titles, not every title \
    that exists). Every title has a url to its page in Den Web — give it to the person: posters, ratings and where to \
    watch are there, not here. Titles are {type, id}; pass them to other Den tools as given. For exact criteria, \
    resolve names to ids with den_filter_values, then den_filter_titles or den_find_people. \"Like X\" is den_similar \
    (search X first for its id). Say when a list may be incomplete.";

/// JSON-RPC's error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// One message, read: a request to answer, or something that takes no answer (a notification, or a response
/// from the client to a request this server never makes).
pub enum Message {
    Request { id: Value, method: String, params: Value },
    NoReply,
}

/// The message in a POST body, or the error to answer it with.
pub fn read(body: &[u8]) -> Result<Message, Value> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| error(Value::Null, PARSE_ERROR, "Parse error"))?;
    if value.is_array() {
        return Err(error(Value::Null, INVALID_REQUEST, "Batches are not supported"));
    }
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(error(
            value.get("id").cloned().unwrap_or(Value::Null),
            INVALID_REQUEST,
            "Not JSON-RPC 2.0",
        ));
    }
    let Some(method) = value.get("method") else {
        return Ok(Message::NoReply);
    };
    let Some(id) = value.get("id").filter(|id| id.is_string() || id.is_number()).cloned() else {
        return Ok(Message::NoReply);
    };
    let Some(method) = method.as_str() else {
        return Err(error(id, INVALID_REQUEST, "method is not a string"));
    };
    let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
    Ok(Message::Request { id, method: method.to_owned(), params })
}

pub fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The answer to `initialize`.
pub fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked.filter(|v| VERSIONS.contains(v)).unwrap_or(VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "den-mcp", "title": "Den", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

/// A tool's answer as `tools/call` returns it: compact JSON text, or the tool's error for the model to act on.
pub fn tool_result(answer: Result<Value, crate::tools::ToolError>) -> Value {
    match answer {
        Ok(value) => json!({ "content": [{ "type": "text", "text": value.to_string() }] }),
        Err(crate::tools::ToolError(message)) => {
            json!({ "content": [{ "type": "text", "text": message }], "isError": true })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_is_one_request_a_notification_or_an_error() {
        let request = read(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        assert!(matches!(request, Message::Request { ref method, .. } if method == "tools/list"));
        assert!(matches!(
            read(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Ok(Message::NoReply)
        ));
        assert!(matches!(read(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#), Ok(Message::NoReply)));
        assert_eq!(read(b"{nope").err().unwrap()["error"]["code"], PARSE_ERROR);
        assert_eq!(read(b"[]").err().unwrap()["error"]["code"], INVALID_REQUEST);
        assert_eq!(read(br#"{"id":1,"method":"x"}"#).err().unwrap()["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn initialize_agrees_a_version_this_server_speaks() {
        assert_eq!(initialize(&json!({ "protocolVersion": "2025-06-18" }))["protocolVersion"], "2025-06-18");
        assert_eq!(initialize(&json!({ "protocolVersion": "1999-01-01" }))["protocolVersion"], VERSIONS[0]);
        assert_eq!(initialize(&json!({}))["capabilities"]["tools"]["listChanged"], false);
    }
}
