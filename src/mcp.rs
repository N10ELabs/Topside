use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter,
};
use tracing::{info, warn};

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default, rename = "jsonrpc")]
    pub _jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

pub async fn run_stdio_server_forever() -> Result<()> {
    info!("starting stdio mcp server");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut writer = BufWriter::new(stdout);

    run_rpc_session(&mut reader, &mut writer).await
}

async fn run_rpc_session<R, W>(reader: &mut BufReader<R>, writer: &mut BufWriter<W>) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    while let Some((req, framing)) = read_next_request(reader).await? {
        let id = req.id.clone();
        let response = handle_request(req);

        if let Some(id) = id {
            let payload = match response {
                Ok(result) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }),
                Err(err) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": err,
                }),
            };

            write_response(writer, framing, &payload).await?;
        }
    }

    Ok(())
}

fn handle_request(req: RpcRequest) -> std::result::Result<Value, Value> {
    match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": {"name": "n10e", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "subscribe": false, "listChanged": false }
            }
        })),
        "ping" => Ok(json!({})),
        "resources/list" => Ok(json!({ "resources": [] })),
        "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
        "tools/list" => Ok(json!({ "tools": [] })),
        "tools/call" => {
            let name = req
                .params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| rpc_err(-32602, "missing tools/call name", None))?;
            Err(rpc_err(
                -32601,
                "unknown tool name",
                Some(json!({ "tool": name })),
            ))
        }
        other => Err(rpc_err(
            -32601,
            "method not found",
            Some(json!({ "method": other })),
        )),
    }
}

#[derive(Debug, Clone, Copy)]
enum MessageFraming {
    JsonLine,
    ContentLength,
}

async fn read_next_request(
    reader: &mut BufReader<impl AsyncRead + Unpin>,
) -> Result<Option<(RpcRequest, MessageFraming)>> {
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(None);
        }

        if line.trim().is_empty() {
            continue;
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);

        if trimmed.starts_with('{') {
            let req: RpcRequest = match serde_json::from_str(trimmed) {
                Ok(req) => req,
                Err(err) => {
                    warn!(error = %err, "invalid stdio JSON request");
                    continue;
                }
            };
            return Ok(Some((req, MessageFraming::JsonLine)));
        }

        let Some(content_length) = parse_content_length(trimmed) else {
            warn!(line = trimmed, "invalid mcp stdio prelude");
            continue;
        };

        loop {
            line.clear();
            let read = reader.read_line(&mut line).await?;
            if read == 0 {
                warn!("unexpected EOF while reading mcp headers");
                return Ok(None);
            }
            if line.trim().is_empty() {
                break;
            }
        }

        let mut body = vec![0u8; content_length];
        if let Err(err) = reader.read_exact(&mut body).await {
            warn!(error = %err, "failed reading framed mcp request body");
            return Ok(None);
        }

        let req: RpcRequest = match serde_json::from_slice(&body) {
            Ok(req) => req,
            Err(err) => {
                warn!(error = %err, "invalid framed mcp JSON request");
                continue;
            }
        };

        return Ok(Some((req, MessageFraming::ContentLength)));
    }
}

fn parse_content_length(line: &str) -> Option<usize> {
    let (name, value) = line.split_once(':')?;
    if !name.trim().eq_ignore_ascii_case("content-length") {
        return None;
    }
    value.trim().parse().ok()
}

async fn write_response(
    writer: &mut BufWriter<impl AsyncWrite + Unpin>,
    framing: MessageFraming,
    payload: &Value,
) -> Result<()> {
    let body = payload.to_string();

    match framing {
        MessageFraming::JsonLine => {
            writer.write_all(body.as_bytes()).await?;
            writer.write_all(b"\n").await?;
        }
        MessageFraming::ContentLength => {
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            writer.write_all(header.as_bytes()).await?;
            writer.write_all(body.as_bytes()).await?;
        }
    }

    writer.flush().await?;
    Ok(())
}

fn rpc_err(code: i32, message: &str, data: Option<Value>) -> Value {
    match data {
        Some(data) => json!({ "code": code, "message": message, "data": data }),
        None => json!({ "code": code, "message": message }),
    }
}
