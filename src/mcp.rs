use std::io::{self, BufRead, Read, Write};

use serde_json::{Value, json};

use crate::{FaxLine, fax::FaxTools};

const VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_FRAME: usize = 64 * 1024;

fn read_frame(input: &mut impl BufRead, frame: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    frame.clear();
    let count = input.take((limit + 1) as u64).read_until(b'\n', frame)?;
    if count > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MCP input frame exceeded its size limit.",
        ));
    }
    if frame.last() == Some(&b'\n') {
        frame.pop();
        if frame.last() == Some(&b'\r') {
            frame.pop();
        }
    }
    Ok(count != 0)
}

fn reply(
    output: &mut impl Write,
    id: &Value,
    message: Result<Value, (i32, &'static str)>,
) -> io::Result<()> {
    let response = match message {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    };
    serde_json::to_writer(&mut *output, &response)?;
    output.write_all(b"\n")?;
    output.flush()
}

pub async fn serve() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut frame = Vec::new();
    // The launcher provides the credential first, then forwards Codex's input.
    if !read_frame(&mut input, &mut frame, 4096)? {
        return Ok(());
    }
    let api_key = String::from_utf8(frame).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid Telnyx credential encoding.",
        )
    })?;
    let mut tools = FaxTools::new(FaxLine::from_env(), &api_key)
        .map_err(|_| io::Error::other("The Telnyx HTTPS client could not start."))?;
    drop(api_key);
    let mut frame = Vec::new();

    while read_frame(&mut input, &mut frame, MAX_FRAME)? {
        let Ok(message) = serde_json::from_slice::<Value>(&frame) else {
            reply(&mut output, &Value::Null, Err((-32700, "Parse error")))?;
            continue;
        };
        let id = message.get("id");
        let method = message.get("method").and_then(Value::as_str);
        let Some(method) = method.filter(|_| {
            message["jsonrpc"] == "2.0" && id.is_none_or(|id| id.is_number() || id.is_string())
        }) else {
            reply(&mut output, &Value::Null, Err((-32600, "Invalid request")))?;
            continue;
        };
        let Some(id) = id else { continue };
        let params = &message["params"];
        let result = match method {
            "initialize" => {
                let version = params["protocolVersion"]
                    .as_str()
                    .filter(|version| VERSIONS.contains(version))
                    .unwrap_or(VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "telnyx-fax", "version": env!("CARGO_PKG_VERSION")}
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": fax_schemas()})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let args = params
                    .get("arguments")
                    .filter(|args| !args.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match tools.call(name, &args).await {
                    Ok(value) => {
                        Ok(json!({"content": [{"type": "text", "text": value.to_string()}]}))
                    }
                    Err(error) => {
                        Ok(json!({"isError": true, "content": [{"type": "text", "text": error}]}))
                    }
                }
            }
            _ => Err((-32601, "Method not found")),
        };
        reply(&mut output, id, result)?;
    }
    Ok(())
}

pub const PHONE: &str = r"^\+[1-9][0-9]{6,14}$";
pub const UUID: &str = r"^[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}$";
const PDF_NAME: &str = r"^[a-zA-Z0-9][a-zA-Z0-9 ._-]{0,119}\.[pP][dD][fF]$";

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required, "additionalProperties": false },
        "annotations": { "readOnlyHint": read_only, "openWorldHint": !matches!(name, "fax_preview" | "fax_inbox") }
    })
}

pub fn fax_schemas() -> [Value; 7] {
    let destination = json!({
        "file": {"type": "string", "pattern": PDF_NAME, "description": "Exact PDF filename in the local outbox."},
        "to": {"type": "string", "pattern": PHONE, "description": "Verified receiving fax number in E.164 format."}
    });
    let id = json!({"type": "string", "pattern": UUID, "description": "Telnyx fax ID."});
    let page = json!({"type": "integer", "minimum": 1, "maximum": 10_000, "description": "Page number, starting at 1."});
    let mut send = destination.clone();
    send["preview_id"] = json!({"type": "string", "pattern": UUID});
    [
        tool(
            "fax_preview",
            "Check a staged PDF and destination locally. Returns a preview ID for user review before sending.",
            destination,
            &["file", "to"],
            true,
        ),
        tool(
            "fax_send",
            "Transmit a staged PDF through Telnyx. Obtain explicit user approval for the exact file and receiving number first. Requires a fresh matching preview. Check history before retrying an uncertain result.",
            send,
            &["file", "to", "preview_id"],
            false,
        ),
        tool(
            "fax_status",
            "Fetch current send or receive status for a fax on this Telnyx fax application.",
            json!({"fax_id": id}),
            &["fax_id"],
            true,
        ),
        tool(
            "fax_history",
            "List one page of inbound or outbound Telnyx faxes for the configured line and application. Defaults to outbound.",
            json!({"page": page, "direction": {"type": "string", "enum": ["inbound", "outbound"]}}),
            &[],
            true,
        ),
        tool(
            "fax_inbox",
            "List received PDFs archived to the local inbox. Returns file metadata only and works without a Telnyx API key.",
            json!({"page": page}),
            &[],
            true,
        ),
        tool(
            "fax_cancel",
            "Ask Telnyx to cancel an outbound fax still in progress. Obtain explicit user approval first and check status afterward.",
            json!({"fax_id": id}),
            &["fax_id"],
            false,
        ),
        tool(
            "fax_account_check",
            "Check API key authentication and outbound fax application readiness without sending a fax.",
            json!({}),
            &[],
            true,
        ),
    ]
}
