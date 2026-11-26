use std::{
    convert::Infallible,
    env, io,
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, LazyLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::{BodyExt, Empty, LengthLimitError, Limited};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    header::{ALLOW, HeaderMap},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use ipnet::Ipv4Net;
use ring::signature::{ED25519, UnparsedPublicKey};
use serde_json::Value;
use tokio::{net::TcpListener, sync::Semaphore, time::timeout};

use crate::{FaxLine, WEBHOOK_PATH, WEBHOOK_PORT, https_client, inbox::Inbox};

const MAX_BODY: usize = 64 * 1024;
const MAX_SKEW: u64 = 300;

// Published Telnyx fax webhook ranges for all regions.
// https://support.telnyx.com/en/articles/1130687-whitelisting-telnyx-ip-addresses
static ALLOWED: LazyLock<Vec<Ipv4Net>> = LazyLock::new(|| {
    [
        "192.76.120.128/29",
        "64.16.239.40/31",
        "64.16.239.49/32",
        "64.16.239.50/32",
        "64.16.239.67/32",
        "64.16.239.72/32",
        "64.16.239.73/32",
        "192.76.120.136/29",
        "64.16.239.99/32",
        "64.16.239.100/32",
        "64.16.239.114/31",
        "192.76.120.144/29",
        "64.16.239.2/31",
        "64.16.239.83/32",
        "64.16.239.84/32",
        "192.76.120.160/29",
        "185.246.41.0/29",
        "185.246.41.8/29",
        "185.246.41.16/29",
        "185.246.41.24/29",
        "103.115.244.0/29",
        "103.115.244.8/29",
        "103.115.244.32/29",
    ]
    .into_iter()
    .map(|cidr| cidr.parse().expect("valid fax webhook CIDR"))
    .collect()
});

struct Receiver {
    key: [u8; 32],
    inbox: Arc<Inbox>,
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(first)
}

fn allowed_source(headers: &HeaderMap) -> bool {
    one_header(headers, "tailscale-funnel-request") == Some("?1")
        && one_header(headers, "x-forwarded-for")
            .and_then(|value| value.parse::<Ipv4Addr>().ok())
            .is_some_and(|source| ALLOWED.iter().any(|network| network.contains(&source)))
}

fn decode_base64(value: &str) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(value))
}

fn credentials(headers: &HeaderMap, now: u64) -> Option<(&str, Vec<u8>)> {
    let timestamp = one_header(headers, "telnyx-timestamp")?;
    if timestamp.is_empty()
        || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
        || now.abs_diff(timestamp.parse::<u64>().ok()?) > MAX_SKEW
    {
        return None;
    }
    let signature = decode_base64(one_header(headers, "telnyx-signature-ed25519")?).ok()?;
    (signature.len() == 64).then_some((timestamp, signature))
}

fn valid_signature(key: &[u8; 32], timestamp: &str, raw: &[u8], signature: &[u8]) -> bool {
    let mut signed = Vec::with_capacity(timestamp.len() + 1 + raw.len());
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b'|');
    signed.extend_from_slice(raw);
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&signed, signature)
        .is_ok()
}

impl Receiver {
    async fn handle(&self, request: Request<Incoming>) -> StatusCode {
        if request.uri().path_and_query().map(|uri| uri.as_str()) != Some(WEBHOOK_PATH) {
            return StatusCode::NOT_FOUND;
        }
        if request.method() != Method::POST {
            return StatusCode::METHOD_NOT_ALLOWED;
        }
        let (parts, body) = request.into_parts();
        if !allowed_source(&parts.headers) {
            return StatusCode::FORBIDDEN;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let Some((timestamp, signature)) = credentials(&parts.headers, now) else {
            return StatusCode::UNAUTHORIZED;
        };
        let raw = match timeout(
            Duration::from_secs(10),
            Limited::new(body, MAX_BODY).collect(),
        )
        .await
        {
            Ok(Ok(collected)) => collected.to_bytes(),
            Ok(Err(error)) if error.is::<LengthLimitError>() => {
                return StatusCode::PAYLOAD_TOO_LARGE;
            }
            Ok(Err(_)) => return StatusCode::BAD_REQUEST,
            Err(_) => return StatusCode::REQUEST_TIMEOUT,
        };
        if !valid_signature(&self.key, timestamp, &raw, &signature) {
            return StatusCode::UNAUTHORIZED;
        }
        let Ok(event) = serde_json::from_slice::<Value>(&raw) else {
            return StatusCode::BAD_REQUEST;
        };
        if self.inbox.receive(&event["data"]).await {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

pub async fn serve() -> io::Result<()> {
    let bind: IpAddr = env::var("TELNYX_WEBHOOK_BIND_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1".into())
        .parse()
        .map_err(|_| io::Error::other("Set TELNYX_WEBHOOK_BIND_ADDRESS to an IP address."))?;
    let public_key = env::var("TELNYX_WEBHOOK_PUBLIC_KEY").unwrap_or_default();
    let key = decode_base64(public_key.trim())
        .ok()
        .and_then(|key| key.try_into().ok())
        .ok_or_else(|| {
            io::Error::other(
                "Set TELNYX_WEBHOOK_PUBLIC_KEY to the Webhook Signing Public Key from Telnyx.",
            )
        })?;
    let http = https_client(Duration::from_secs(12))
        .map_err(|_| io::Error::other("The inbox HTTPS client could not start."))?;
    let receiver = Arc::new(Receiver {
        key,
        inbox: Arc::new(Inbox::new(FaxLine::from_env(), http)),
    });
    let listener = TcpListener::bind((bind, WEBHOOK_PORT)).await?;
    let connections = Arc::new(Semaphore::new(64));
    eprintln!("Telnyx webhook listening on {WEBHOOK_PORT}{WEBHOOK_PATH}");
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
            continue;
        };
        let receiver = Arc::clone(&receiver);
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(|request| async {
                let status = receiver.handle(request).await;
                let mut response = Response::new(Empty::<Bytes>::new());
                *response.status_mut() = status;
                if status == StatusCode::METHOD_NOT_ALLOWED {
                    response
                        .headers_mut()
                        .insert(ALLOW, "POST".parse().expect("valid Allow header"));
                }
                Ok::<_, Infallible>(response)
            });
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(5))
                .max_buf_size(16 * 1024)
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    #[test]
    fn requires_single_funnel_ip_and_fresh_signature_over_raw_body() {
        let signing = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
        let key = signing.public_key().as_ref().try_into().unwrap();
        let raw = br#"{"data": {}}"#;
        let signature = signing.sign(b"1000|{\"data\": {}}");
        let mut headers = HeaderMap::new();
        headers.insert("tailscale-funnel-request", "?1".parse().unwrap());
        headers.insert("x-forwarded-for", "192.76.120.129".parse().unwrap());
        headers.insert("telnyx-timestamp", "1000".parse().unwrap());
        headers.insert(
            "telnyx-signature-ed25519",
            STANDARD.encode(signature.as_ref()).parse().unwrap(),
        );
        assert!(allowed_source(&headers));
        let (timestamp, signature) = credentials(&headers, 1200).unwrap();
        assert!(valid_signature(key, timestamp, raw, &signature));
        assert!(!valid_signature(key, timestamp, b"{}", &signature));
        assert!(credentials(&headers, 1301).is_none());
        headers.append("x-forwarded-for", "192.76.120.130".parse().unwrap());
        assert!(!allowed_source(&headers));
    }
}
