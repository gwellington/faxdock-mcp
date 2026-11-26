use std::{
    collections::HashMap,
    fs, io,
    path::Path,
    sync::{Arc, LazyLock, Mutex},
};

use regex::Regex;
use reqwest::{Client, Url};
use serde_json::{Value, json};
use tokio::{fs::File, io::AsyncWriteExt, sync::watch};

use crate::{FaxLine, MAX_PDF_BYTES, has_pdf_header, iso_date};

static FAX_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$").unwrap());
static PATH_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^s3(?:\.[a-z0-9-]+)?\.amazonaws\.com$").unwrap());
static BUCKET_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^faxes-prod\.s3(?:\.[a-z0-9-]+)?\.amazonaws\.com$").unwrap());

pub fn list(directory: &Path, page: usize) -> io::Result<Value> {
    let mut faxes = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".pdf"))
            .filter(|id| FAX_ID.is_match(id))
        else {
            continue;
        };
        let Ok(info) = entry.metadata() else { continue };
        if info.is_file() {
            faxes.push(json!({
                "fax_id": id, "file": format!("inbox/{id}.pdf"), "bytes": info.len(),
                "saved_at": iso_date(info.modified()?)
            }));
        }
    }
    faxes.sort_by(|left, right| right["saved_at"].as_str().cmp(&left["saved_at"].as_str()));
    let total_pages = faxes.len().div_ceil(20);
    let faxes: Vec<_> = faxes.into_iter().skip((page - 1) * 20).take(20).collect();
    Ok(json!({"page": page, "total_pages": total_pages, "faxes": faxes}))
}

pub struct Inbox {
    line: FaxLine,
    client: Client,
    downloads: Mutex<HashMap<String, watch::Receiver<Option<bool>>>>,
}

impl Inbox {
    pub fn new(line: FaxLine, client: Client) -> Self {
        Self {
            line,
            client,
            downloads: Mutex::new(HashMap::new()),
        }
    }

    /// A false result asks Telnyx to retry the callback while its URL is valid.
    pub async fn receive(self: &Arc<Self>, event: &Value) -> bool {
        let payload = &event["payload"];
        if event["event_type"] != "fax.received"
            || self.line.from.is_empty()
            || self.line.connection_id.is_empty()
            || payload["to"] != self.line.from
            || payload["connection_id"] != self.line.connection_id
            || payload["direction"] != "inbound"
            || payload["status"] != "received"
        {
            return true;
        }
        let Some(id) = payload["fax_id"].as_str().filter(|id| FAX_ID.is_match(id)) else {
            return false;
        };
        let mut download = {
            let mut downloads = self.downloads.lock().expect("inbox download state");
            if let Some(active) = downloads.get(id) {
                active.clone()
            } else {
                if downloads.len() >= 2 {
                    return false;
                }
                let (result, active) = watch::channel(None);
                let id = id.to_owned();
                downloads.insert(id.clone(), active.clone());
                let inbox = Arc::clone(self);
                let url = payload["media_url"].clone();
                // Finish the archive even if the HTTP client disconnects.
                tokio::spawn(async move {
                    let saved = save(Path::new("/inbox"), &inbox.client, &id, &url)
                        .await
                        .is_ok();
                    let mut downloads = inbox.downloads.lock().expect("inbox download state");
                    result.send_replace(Some(saved));
                    downloads.remove(&id);
                });
                active
            }
        };
        download
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|saved| *saved)
            .unwrap_or(false)
    }
}

fn media_url(value: &Value, id: &str) -> Option<Url> {
    let url = Url::parse(value.as_str()?).ok()?;
    let host = url.host_str()?;
    let path_style = PATH_HOST.is_match(host) && url.path().starts_with("/faxes-prod/");
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || (!path_style && !BUCKET_HOST.is_match(host))
        || !url.path().ends_with(&format!("/{id}.pdf"))
        || !url.query_pairs().any(|(key, _)| key == "X-Amz-Signature")
    {
        return None;
    }
    Some(url)
}

fn already_saved(destination: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(destination) {
        Ok(info) if info.is_file() => Ok(true),
        Ok(_) => Err(io::Error::other("Inbound archive path is occupied.")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

async fn save(directory: &Path, client: &Client, id: &str, url: &Value) -> io::Result<()> {
    let destination = directory.join(format!("{id}.pdf"));
    if already_saved(&destination)? {
        return Ok(());
    }
    let url = media_url(url, id)
        .ok_or_else(|| io::Error::other("Unexpected inbound fax media origin."))?;
    let unavailable = || io::Error::other("Inbound fax media unavailable.");
    let response = client.get(url).send().await.map_err(|_| unavailable())?;
    if !response.status().is_success() {
        return Err(unavailable());
    }
    archive(directory, &destination, response).await
}

async fn archive(
    directory: &Path,
    destination: &Path,
    mut response: reqwest::Response,
) -> io::Result<()> {
    let temporary = tempfile::Builder::new()
        .prefix(".download-")
        .tempfile_in(directory)?;
    let (file, path) = temporary.into_parts();
    let mut file = File::from_std(file);
    let mut bytes = 0;
    let mut header = Vec::with_capacity(1024);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| io::Error::other("Inbound fax media unavailable."))?
    {
        bytes += chunk.len() as u64;
        if bytes > MAX_PDF_BYTES {
            return Err(io::Error::other("Inbound fax PDF is too large."));
        }
        header.extend_from_slice(&chunk[..chunk.len().min(1024 - header.len())]);
        file.write_all(&chunk).await?;
    }
    if !has_pdf_header(&header) {
        return Err(io::Error::other("Inbound fax has no PDF header."));
    }
    file.sync_all().await?;
    drop(file);
    match path.persist_noclobber(destination) {
        Ok(()) => Ok(()),
        Err(error)
            if error.error.kind() == io::ErrorKind::AlreadyExists
                && already_saved(destination)? =>
        {
            Ok(())
        }
        Err(error) => Err(error.error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const FAX: &str = "44444444-4444-4444-8444-444444444444";

    #[test]
    fn only_accepts_the_fax_id_on_a_signed_telnyx_bucket_url() {
        let path = format!("/{FAX}.pdf?X-Amz-Signature=test");
        assert!(
            media_url(
                &json!(format!(
                    "https://s3.us-east-1.amazonaws.com/faxes-prod{path}"
                )),
                FAX
            )
            .is_some()
        );
        assert!(
            media_url(
                &json!(format!(
                    "https://faxes-prod.s3.us-east-1.amazonaws.com/inbound{path}"
                )),
                FAX
            )
            .is_some()
        );
        for bad in [
            format!("https://faxes-prod.s3.us-east-1.amazonaws.com.example.invalid{path}"),
            format!("https://s3.us-east-1.amazonaws.com/another-bucket{path}"),
            format!("https://faxes-prod.s3.amazonaws.com/{FAX}.pdf/other.pdf?X-Amz-Signature=test"),
            format!("https://faxes-prod.s3.amazonaws.com/{FAX}.pdf"),
            format!("http://faxes-prod.s3.amazonaws.com{path}"),
        ] {
            assert!(media_url(&json!(bad), FAX).is_none());
        }
    }

    #[test]
    fn lists_old_and_new_inbox_filenames_but_skips_symlinks() {
        let inbox = tempfile::tempdir().unwrap();
        let existing = inbox.path().join(format!("{FAX}.pdf"));
        fs::write(&existing, b"%PDF-test").unwrap();
        symlink(
            &existing,
            inbox
                .path()
                .join("55555555-5555-4555-8555-555555555555.pdf"),
        )
        .unwrap();
        let result = list(inbox.path(), 1).unwrap();
        assert_eq!(result["total_pages"], 1);
        let faxes = result["faxes"].as_array().unwrap();
        assert_eq!(faxes.len(), 1);
        assert_eq!(faxes[0]["file"], format!("inbox/{FAX}.pdf"));
    }

    #[tokio::test]
    async fn archives_pdf_privately_without_overwriting_and_discards_invalid_media() {
        fn response(bytes: &'static [u8]) -> reqwest::Response {
            hyper::Response::new(reqwest::Body::from(bytes)).into()
        }

        let inbox = tempfile::tempdir().unwrap();
        let pdf = inbox.path().join(format!("{FAX}.pdf"));
        archive(inbox.path(), &pdf, response(b"invalid media"))
            .await
            .unwrap_err();
        assert_eq!(fs::read_dir(inbox.path()).unwrap().count(), 0);
        archive(inbox.path(), &pdf, response(b"%PDF-archived"))
            .await
            .unwrap();
        assert_eq!(
            fs::metadata(&pdf).unwrap().permissions().mode() & 0o777,
            0o600
        );
        archive(inbox.path(), &pdf, response(b"%PDF-replacement"))
            .await
            .unwrap();
        assert_eq!(fs::read(&pdf).unwrap(), b"%PDF-archived");
        assert_eq!(fs::read_dir(inbox.path()).unwrap().count(), 1);
    }
}
