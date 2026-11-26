mod fax;
mod inbox;
mod mcp;
mod webhook;

use std::{
    env,
    io::{self, BufRead, BufReader, Write},
    net::TcpStream,
    time::Duration,
};

use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::{Client, redirect::Policy};

const MAX_PDF_BYTES: u64 = 20_000_000;
const WEBHOOK_PATH: &str = "/telnyx/webhook";
const WEBHOOK_PORT: u16 = 8787;

struct FaxLine {
    from: String,
    connection_id: String,
}

impl FaxLine {
    fn from_env() -> Self {
        Self {
            from: env::var("TELNYX_FAX_FROM").unwrap_or_default(),
            connection_id: env::var("TELNYX_FAX_CONNECTION_ID").unwrap_or_default(),
        }
    }
}

fn https_client(timeout: Duration) -> reqwest::Result<Client> {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .timeout(timeout)
        .build()
}

fn iso_date(time: std::time::SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn has_pdf_header(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(1024)]
        .windows(5)
        .any(|window| window == b"%PDF-")
}

fn healthcheck() -> io::Result<()> {
    let timeout = Duration::from_secs(2);
    let address = ([127, 0, 0, 1], WEBHOOK_PORT).into();
    let mut socket = TcpStream::connect_timeout(&address, timeout)?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    write!(
        socket,
        "GET {WEBHOOK_PATH} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    let mut status = String::new();
    BufReader::new(socket).read_line(&mut status)?;
    if status.split_ascii_whitespace().nth(1) == Some("405") {
        Ok(())
    } else {
        Err(io::Error::other("The webhook route is unavailable."))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let result = match env::args().nth(1).as_deref() {
        Some("webhook") => webhook::serve().await,
        Some("mcp") => mcp::serve().await,
        Some("healthcheck") => healthcheck(),
        _ => Err(io::Error::other(
            "Usage: telnyx-fax webhook|mcp|healthcheck",
        )),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
