use std::{
    collections::VecDeque,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Duration, Instant, SystemTime},
};

use regex::Regex;
use reqwest::{
    Client, Method, Url,
    header::{ACCEPT, AUTHORIZATION, HeaderValue},
    multipart,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    FaxLine, MAX_PDF_BYTES, has_pdf_header, https_client, inbox, iso_date,
    mcp::{PHONE, UUID, fax_schemas},
};

const API_BASE: &str = "https://api.telnyx.com/v2/";
const PREVIEW_LIFETIME: Duration = Duration::from_secs(600);
const UNKNOWN_SEND: &str =
    "Fax submission status is unknown. Check fax history before attempting another send.";
const UNKNOWN_CANCEL: &str =
    "Cancellation status is unknown. Check fax status before trying again.";
const UNREADABLE: &str = "Telnyx returned an unreadable response.";
const MAX_RESPONSE: usize = 2 * 1024 * 1024;
static PHONE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(PHONE).unwrap());
static UUID_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(UUID).unwrap());
static CONNECTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{1,24}$").unwrap());
static KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._-]+$").unwrap());
static DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.+\-]+Z?$").unwrap());
static REASON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]{0,95}$").unwrap());
static SCHEMAS: LazyLock<[Value; 7]> = LazyLock::new(fax_schemas);

type FaxResult<T> = Result<T, String>;

struct Preview {
    id: Uuid,
    file: String,
    to: String,
    hash: [u8; 32],
    expires: Instant,
}

pub struct FaxTools {
    line: FaxLine,
    outbox: PathBuf,
    inbox: PathBuf,
    previews: VecDeque<Preview>,
    authorization: FaxResult<HeaderValue>,
    client: Client,
    base: Url,
}

impl FaxTools {
    pub fn new(line: FaxLine, api_key: &str) -> reqwest::Result<Self> {
        let authorization = if api_key.is_empty() {
            Err("The Telnyx API key is unavailable. Configure its 1Password reference and reopen Codex.".into())
        } else if !KEY.is_match(api_key) {
            Err("The Telnyx API key has an invalid format.".into())
        } else {
            let mut header =
                HeaderValue::from_str(&format!("Bearer {api_key}")).expect("validated ASCII key");
            header.set_sensitive(true);
            Ok(header)
        };
        Ok(Self {
            line,
            outbox: "/outbox".into(),
            inbox: "/inbox".into(),
            previews: VecDeque::new(),
            authorization,
            client: https_client(Duration::from_secs(30))?,
            base: Url::parse(API_BASE).expect("valid Telnyx API URL"),
        })
    }

    async fn api(
        &self,
        method: Method,
        url: Url,
        form: Option<multipart::Form>,
        uncertain: Option<&'static str>,
    ) -> FaxResult<Value> {
        let key = self.authorization.as_ref().map_err(Clone::clone)?;
        let expected = if method == Method::GET { 200 } else { 202 };
        let request = self
            .client
            .request(method, url)
            .header(AUTHORIZATION, key.clone())
            .header(ACCEPT, "application/json");
        let request = match form {
            Some(form) => request.multipart(form),
            None => request,
        };
        let mut response = request
            .send()
            .await
            .map_err(|_| uncertain.unwrap_or("The Telnyx API could not be reached."))?;
        let status = response.status().as_u16();
        if status != expected {
            if let Some(uncertain) =
                uncertain.filter(|_| status < 400 || status == 408 || status >= 500)
            {
                return Err(uncertain.into());
            }
            return Err(format!("Telnyx returned HTTP {status}."));
        }
        let error = uncertain.unwrap_or(UNREADABLE);
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| error)? {
            if bytes.len() + chunk.len() > MAX_RESPONSE {
                return Err(error.into());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| error.into())
    }

    fn owns_fax(&self, data: &Value) -> bool {
        data["connection_id"] == self.line.connection_id
            && ((data["direction"] == "outbound" && data["from"] == self.line.from)
                || (data["direction"] == "inbound" && data["to"] == self.line.from))
    }

    async fn get_fax(&self, id: &str) -> FaxResult<Value> {
        let response = self
            .api(Method::GET, self.url(&format!("faxes/{id}")), None, None)
            .await?;
        if !self.owns_fax(&response["data"]) {
            return Err(
                "This fax does not belong to the configured fax line and application.".into(),
            );
        }
        summarize(&response["data"])
    }

    fn url(&self, path: &str) -> Url {
        self.base.join(path).expect("valid relative fax endpoint")
    }

    pub async fn call(&mut self, name: &str, args: &Value) -> FaxResult<Value> {
        let schema = SCHEMAS
            .iter()
            .find(|tool| tool["name"] == name)
            .ok_or("Unknown fax tool.")?;
        validate(&schema["inputSchema"], args)?;
        if !PHONE_REGEX.is_match(&self.line.from) || !CONNECTION.is_match(&self.line.connection_id)
        {
            return Err("Set TELNYX_FAX_FROM and TELNYX_FAX_CONNECTION_ID in .env.".into());
        }
        let string = |key: &str| args[key].as_str().unwrap_or_default();
        let page = args["page"].as_f64().unwrap_or(1.0) as usize;
        match name {
            "fax_preview" => self.preview(string("file"), string("to")),
            "fax_send" => {
                self.send(string("file"), string("to"), string("preview_id"))
                    .await
            }
            "fax_status" => self.get_fax(string("fax_id")).await,
            "fax_history" => {
                self.history(page, args["direction"].as_str().unwrap_or("outbound"))
                    .await
            }
            "fax_inbox" => inbox::list(&self.inbox, page)
                .map_err(|_| "The local fax inbox could not be read.".into()),
            "fax_cancel" => self.cancel(string("fax_id")).await,
            "fax_account_check" => self.account_check().await,
            _ => unreachable!("dispatch covers all fax tool schemas"),
        }
    }

    fn preview(&mut self, file: &str, to: &str) -> FaxResult<Value> {
        let bytes = read_pdf(&self.outbox, file)?;
        let now = Instant::now();
        self.previews.retain(|preview| preview.expires > now);
        if self.previews.len() == 32 {
            self.previews.pop_front();
        }
        let id = Uuid::new_v4();
        self.previews.push_back(Preview {
            id,
            file: file.into(),
            to: to.into(),
            hash: Sha256::digest(&bytes).into(),
            expires: now + PREVIEW_LIFETIME,
        });
        Ok(json!({
            "preview_id": id.to_string(), "file": file, "from": self.line.from, "to": to,
            "bytes": bytes.len(), "expires_at": iso_date(SystemTime::now() + PREVIEW_LIFETIME)
        }))
    }

    async fn send(&mut self, file: &str, to: &str, id: &str) -> FaxResult<Value> {
        let id = Uuid::parse_str(id)
            .map_err(|_| "Use a fresh preview for this exact file and destination.")?;
        let index = self
            .previews
            .iter()
            .position(|preview| {
                preview.id == id
                    && preview.expires > Instant::now()
                    && preview.file == file
                    && preview.to == to
            })
            .ok_or("Use a fresh preview for this exact file and destination.")?;
        let preview = self.previews.remove(index).expect("preview index exists");
        let bytes = read_pdf(&self.outbox, file)?;
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        if preview.hash != hash {
            return Err("The PDF changed after preview. Preview it again.".into());
        }
        let pdf = multipart::Part::bytes(bytes)
            .file_name("fax.pdf")
            .mime_str("application/pdf")
            .expect("valid PDF media type");
        let form = multipart::Form::new()
            .text("from", self.line.from.clone())
            .text("to", to.to_owned())
            .text("connection_id", self.line.connection_id.clone())
            .text("store_media", "false")
            .text("store_preview", "false")
            .part("contents", pdf);
        let response = self
            .api(
                Method::POST,
                self.url("faxes"),
                Some(form),
                Some(UNKNOWN_SEND),
            )
            .await?;
        let mut fax = summarize(&response["data"]).map_err(|_| UNKNOWN_SEND)?;
        fax["submission"] = json!("accepted");
        Ok(fax)
    }

    async fn history(&self, page: usize, direction: &str) -> FaxResult<Value> {
        let mut url = self.url("faxes");
        let line_filter = if direction == "outbound" {
            "filter[from][eq]"
        } else {
            "filter[to][eq]"
        };
        url.query_pairs_mut()
            .append_pair("filter[direction][eq]", direction)
            .append_pair(line_filter, &self.line.from)
            .append_pair("page[size]", "20")
            .append_pair("page[number]", &page.to_string());
        let response = self.api(Method::GET, url, None, None).await?;
        let faxes = response["data"]
            .as_array()
            .ok_or("Telnyx returned an unreadable fax history.")?
            .iter()
            .filter(|fax| fax["direction"] == direction && self.owns_fax(fax))
            .map(summarize)
            .collect::<FaxResult<Vec<_>>>()?;
        let mut result = json!({"page": page, "faxes": faxes});
        if let Some(pages) = response["meta"]["total_pages"].as_i64() {
            result["total_pages"] = json!(pages);
        }
        Ok(result)
    }

    async fn cancel(&self, id: &str) -> FaxResult<Value> {
        let fax = self.get_fax(id).await?;
        if fax["direction"] != "outbound" {
            return Err("Only outbound faxes can be canceled.".into());
        }
        let status = fax["status"].as_str().unwrap_or_default();
        if matches!(status, "delivered" | "failed" | "canceled") {
            return Err(format!("This fax is already {status}."));
        }
        self.api(
            Method::POST,
            self.url(&format!("faxes/{id}/actions/cancel")),
            None,
            Some(UNKNOWN_CANCEL),
        )
        .await?;
        Ok(json!({"fax_id": id, "cancellation": "accepted"}))
    }

    async fn account_check(&self) -> FaxResult<Value> {
        let url = self.url(&format!("fax_applications/{}", self.line.connection_id));
        let response = self.api(Method::GET, url, None, None).await?;
        let data = &response["data"];
        if data["id"] != self.line.connection_id {
            return Err("Telnyx returned an unexpected fax application.".into());
        }
        let profile = &data["outbound"]["outbound_voice_profile_id"];
        Ok(json!({
            "from": self.line.from, "application_id": self.line.connection_id, "authenticated": true,
            "application_active": data["active"] == true,
            "outbound_profile_configured": profile.as_str().is_some_and(|id| !id.is_empty())
        }))
    }
}

fn validate(schema: &Value, args: &Value) -> FaxResult<()> {
    let args = args
        .as_object()
        .ok_or("Supply the required tool arguments.")?;
    let required = schema["required"]
        .as_array()
        .expect("tool schema required list");
    if required
        .iter()
        .any(|key| !args.contains_key(key.as_str().expect("required key")))
    {
        return Err("Supply the required tool arguments.".into());
    }
    for (key, value) in args {
        let field = schema["properties"]
            .get(key)
            .ok_or("An unexpected tool argument was provided.")?;
        let valid = if let Some(pattern) = field["pattern"].as_str() {
            let regex = Regex::new(pattern).expect("valid tool schema pattern");
            value.as_str().is_some_and(|value| regex.is_match(value))
        } else if let Some(choices) = field["enum"].as_array() {
            choices.contains(value)
        } else {
            value.as_f64().is_some_and(|number| {
                number.fract() == 0.0
                    && number >= field["minimum"].as_f64().expect("page minimum")
                    && number <= field["maximum"].as_f64().expect("page maximum")
            })
        };
        if !valid {
            return Err(format!("Invalid {key}."));
        }
    }
    Ok(())
}

fn read_pdf(outbox: &Path, name: &str) -> FaxResult<Vec<u8>> {
    let open_error = "The staged PDF could not be opened. Use a regular file in the outbox.";
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(outbox.join(name))
        .map_err(|_| open_error)?;
    let info = file.metadata().map_err(|_| open_error)?;
    let size = info.len();
    if !info.is_file() || !(5..=MAX_PDF_BYTES).contains(&size) {
        return Err("Use a regular PDF file no larger than 20 MB.".into());
    }
    let mut bytes = Vec::with_capacity(size as usize);
    (&mut file)
        .take(size + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| open_error)?;
    if bytes.len() as u64 != size || file.metadata().map_err(|_| open_error)?.len() != size {
        return Err("The PDF changed while reading. Preview it again when it is ready.".into());
    }
    if !has_pdf_header(&bytes) {
        return Err("The staged file lacks a PDF header.".into());
    }
    Ok(bytes)
}

fn summarize(data: &Value) -> FaxResult<Value> {
    let id = data["id"]
        .as_str()
        .filter(|id| UUID_REGEX.is_match(id))
        .ok_or("Telnyx returned a record without a valid fax ID.")?;
    let status = match data["status"].as_str() {
        Some(
            state @ ("queued" | "media.processed" | "originated" | "sending" | "delivered"
            | "failed" | "initiated" | "receiving" | "media.processing" | "received"
            | "canceled"),
        ) => state,
        _ => "unknown",
    };
    let mut fax = json!({"fax_id": id, "status": status});
    if matches!(data["direction"].as_str(), Some("outbound" | "inbound")) {
        fax["direction"] = data["direction"].clone();
    }
    for (field, pattern) in [
        ("from", &*PHONE_REGEX),
        ("to", &*PHONE_REGEX),
        ("created_at", &*DATE),
        ("failure_reason", &*REASON),
    ] {
        if data[field]
            .as_str()
            .is_some_and(|value| pattern.is_match(value))
        {
            fax[field] = data[field].clone();
        }
    }
    Ok(fax)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};
    use hyper::{Response, body::Bytes, server::conn::http1, service::service_fn};
    use hyper_util::rt::TokioIo;
    use std::{convert::Infallible, fs, os::unix::fs::symlink};
    use tokio::net::TcpListener;

    const SOURCE: &str = "+12025550100";
    const DESTINATION: &str = "+12025550101";
    const FAX: &str = "44444444-4444-4444-8444-444444444444";

    fn tools(outbox: &Path) -> FaxTools {
        let mut tools = FaxTools::new(
            FaxLine {
                from: SOURCE.into(),
                connection_id: "999".into(),
            },
            "test-key",
        )
        .unwrap();
        tools.outbox = outbox.into();
        tools
    }

    #[tokio::test]
    async fn preview_binds_file_and_destination_and_is_consumed() {
        let outbox = tempfile::tempdir().unwrap();
        let file = outbox.path().join("mock.pdf");
        fs::write(&file, b"%PDF-original").unwrap();
        symlink(&file, outbox.path().join("linked.pdf")).unwrap();
        let mut tools = tools(outbox.path());
        assert!(tools.preview("linked.pdf", DESTINATION).is_err());
        let preview = tools.preview("mock.pdf", DESTINATION).unwrap();
        let id = preview["preview_id"].as_str().unwrap();
        assert!(
            tools
                .send("mock.pdf", SOURCE, id)
                .await
                .unwrap_err()
                .contains("fresh preview")
        );
        fs::write(file, b"%PDF-modified").unwrap();
        assert!(
            tools
                .send("mock.pdf", DESTINATION, id)
                .await
                .unwrap_err()
                .contains("PDF changed")
        );
        assert!(
            tools
                .send("mock.pdf", DESTINATION, id)
                .await
                .unwrap_err()
                .contains("fresh preview")
        );
    }

    #[tokio::test]
    async fn send_uploads_the_reviewed_bytes_directly_and_sanitizes_response() {
        let outbox = tempfile::tempdir().unwrap();
        fs::write(outbox.path().join("mock.pdf"), b"%PDF-test-document").unwrap();
        let mut tools = tools(outbox.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        tools.base = Url::parse(&format!("http://{}/v2/", listener.local_addr().unwrap())).unwrap();
        tools.client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let handling = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            http1::Builder::new().keep_alive(false).serve_connection(TokioIo::new(socket), service_fn(|request: hyper::Request<hyper::body::Incoming>| async move {
                assert_eq!(request.method(), Method::POST);
                assert_eq!(request.uri().path(), "/v2/faxes");
                assert_eq!(request.headers()[AUTHORIZATION], "Bearer test-key");
                assert!(request.headers()["content-type"].to_str().unwrap().starts_with("multipart/form-data; boundary="));
                let body = request.into_body().collect().await.unwrap().to_bytes();
                let body = String::from_utf8(body.to_vec()).unwrap();
                for (field, value) in [("from", SOURCE), ("to", DESTINATION), ("connection_id", "999"), ("store_media", "false"), ("store_preview", "false")] {
                    assert!(body.contains(&format!("name=\"{field}\"\r\n\r\n{value}\r\n")));
                }
                assert!(body.contains("name=\"contents\"; filename=\"fax.pdf\""));
                assert!(body.contains("%PDF-test-document"));
                let response = json!({"data": {"id": FAX, "status": "queued", "media_url": "private-signed-url"}});
                Ok::<_, Infallible>(Response::builder().status(202).body(Full::new(Bytes::from(response.to_string()))).unwrap())
            })).await.unwrap();
        });
        let preview = tools.preview("mock.pdf", DESTINATION).unwrap();
        let result = tools
            .send(
                "mock.pdf",
                DESTINATION,
                preview["preview_id"].as_str().unwrap(),
            )
            .await
            .unwrap();
        handling.await.unwrap();
        assert_eq!(
            result,
            json!({"fax_id": FAX, "status": "queued", "submission": "accepted"})
        );
    }
}
