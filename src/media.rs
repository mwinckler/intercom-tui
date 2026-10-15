//! Untrusted message media and token-free, bounded file transfers.
use crate::{
    format,
    model::{ContentBlock, Media},
};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{ImageReader, RgbaImage};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use scraper::{ElementRef, Html, Node};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::AsyncWriteExt;

pub const MAX_ATTACHMENTS: usize = 10;
pub const MAX_UPLOAD_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_DOWNLOAD_BYTES: u64 = 100 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

pub enum Preview {
    Loading,
    Ready(Arc<RgbaImage>),
    Failed(String),
}

#[derive(Default)]
pub struct Images {
    pub entries: BTreeMap<String, Preview>,
    order: Vec<String>,
}

impl Images {
    pub fn begin(&mut self, url: &str, retry: bool) -> bool {
        if self
            .entries
            .values()
            .filter(|v| matches!(v, Preview::Loading))
            .count()
            >= 2
            || self
                .entries
                .get(url)
                .is_some_and(|v| !retry || !matches!(v, Preview::Failed(_)))
        {
            return false;
        }
        if self.entries.len() >= 24 {
            if let Some(index) = self
                .order
                .iter()
                .position(|key| !matches!(self.entries.get(key), Some(Preview::Loading)))
            {
                self.entries.remove(&self.order.remove(index));
            } else {
                return false;
            }
        }
        self.order.retain(|key| key != url);
        self.order.push(url.into());
        self.entries.insert(url.into(), Preview::Loading);
        true
    }
}

pub fn automatic(media: &Media) -> bool {
    if media.width == 1 || media.height == 1 || !format::safe_url(&media.url) {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(&media.url) else {
        return false;
    };
    url.scheme() == "https" && url.port().is_none() && trusted_host(url.host_str().unwrap_or(""))
}

fn trusted_host(host: &str) -> bool {
    [
        "intercomcdn.com",
        "intercomassets.com",
        "intercom-attachments-1.com",
        "intercom-attachments-2.com",
        "intercom-attachments-3.com",
        "intercom-attachments-4.com",
        "intercom-attachments-5.com",
        "intercom-attachments-6.com",
        "intercom-attachments-7.com",
        "intercom-attachments-8.com",
        "intercom-attachments-9.com",
    ]
    .iter()
    .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

/// Stable message identities must not change when signed URLs rotate.
pub fn identity(url: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(url) else {
        return String::new();
    };
    url.set_fragment(None);
    let stable: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| {
            let key = key.to_lowercase();
            ![
                "signature",
                "sig",
                "expires",
                "token",
                "policy",
                "key-pair-id",
                "awsaccesskeyid",
            ]
            .contains(&key.as_str())
                && !key.starts_with("x-amz-")
                && !key.starts_with("x-goog-")
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.set_query(None);
    if !stable.is_empty() {
        url.query_pairs_mut().extend_pairs(stable);
    }
    url.into()
}

fn asset_key(url: &str) -> String {
    if let Ok(parsed) = reqwest::Url::parse(url)
        && parsed.path().starts_with("/i/o/")
        && trusted_host(parsed.host_str().unwrap_or(""))
    {
        return format!("intercom:{}", parsed.path());
    }
    identity(url)
}

#[derive(Default)]
struct Content {
    pending: String,
    active: Vec<(String, String)>,
    blocks: Vec<ContentBlock>,
}

impl Content {
    fn flush(&mut self) {
        let mut html = std::mem::take(&mut self.pending);
        for (tag, _) in self.active.iter().rev() {
            html.push_str(&format!("</{tag}>"));
        }
        if !format::plain(&html).trim().is_empty() {
            self.blocks.push(ContentBlock::Text { html });
        }
        for (_, opening) in &self.active {
            self.pending.push_str(opening);
        }
    }

    fn walk(&mut self, element: ElementRef<'_>, link: &str) {
        let tag = element.value().name();
        if ["script", "style", "template", "head"].contains(&tag) {
            return;
        }
        if tag == "img" {
            let url = element.attr("src").unwrap_or("");
            let alt = format::safe_text(element.attr("alt").unwrap_or("Image"));
            if format::safe_url(url) {
                self.flush();
                self.blocks.push(ContentBlock::Image(Media {
                    url: url.into(),
                    alt,
                    width: dimension(element.attr("width")),
                    height: dimension(element.attr("height")),
                    link_url: if format::safe_url(link) { link } else { url }.into(),
                    ..Default::default()
                }));
            } else {
                self.pending
                    .push_str(&format::escape(&format!("[{alt} unavailable]")));
            }
            return;
        }
        let link = if tag == "a" {
            element
                .attr("href")
                .filter(|url| format::safe_url(url))
                .unwrap_or("")
        } else {
            link
        };
        let allowed = [
            "b",
            "strong",
            "i",
            "em",
            "a",
            "s",
            "del",
            "code",
            "pre",
            "p",
            "div",
            "li",
            "ul",
            "ol",
            "blockquote",
            "h1",
            "h2",
            "h3",
            "tr",
        ]
        .contains(&tag);
        if tag == "br" || tag == "hr" {
            self.pending.push_str("<br>");
            return;
        }
        if allowed {
            let opening = if tag == "a" {
                format!("<a href=\"{}\">", format::escape(link))
            } else {
                format!("<{tag}>")
            };
            self.pending.push_str(&opening);
            self.active.push((tag.into(), opening));
        }
        for child in element.children() {
            match child.value() {
                Node::Text(text) => self.pending.push_str(&format::escape(text)),
                Node::Element(_) => {
                    if let Some(child) = ElementRef::wrap(child) {
                        self.walk(child, link);
                    }
                }
                _ => (),
            }
        }
        if allowed {
            self.active.pop();
            self.pending.push_str(&format!("</{tag}>"));
        }
    }
}

fn dimension(value: Option<&str>) -> u32 {
    value
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0)
        .min(32768)
}

pub fn content(body: &str, attachments: &Value) -> Vec<ContentBlock> {
    let html = Html::parse_fragment(body);
    let mut content = Content::default();
    content.walk(html.root_element(), "");
    content.flush();
    for attachment in attachments.as_array().into_iter().flatten() {
        let url = attachment["url"].as_str().unwrap_or("");
        if !format::safe_url(url) {
            continue;
        }
        let content_type = format::safe_text(
            attachment["content_type"]
                .as_str()
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_lowercase()
                .as_str(),
        );
        let path = reqwest::Url::parse(url).unwrap().path().to_lowercase();
        let is_image = content_type.starts_with("image/")
            || content_type.is_empty()
                && ["png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "svg"]
                    .iter()
                    .any(|ext| path.ends_with(&format!(".{ext}")));
        let media = Media {
            url: url.into(),
            link_url: url.into(),
            name: format::safe_text(
                attachment["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        path.rsplit('/')
                            .next()
                            .filter(|s| !s.is_empty())
                            .unwrap_or("Attachment")
                    }),
            ),
            alt: format::safe_text(attachment["alt"].as_str().unwrap_or("")),
            content_type,
            size: attachment["filesize"]
                .as_u64()
                .or_else(|| attachment["filesize"].as_str().and_then(|v| v.parse().ok()))
                .unwrap_or(0),
            width: dimension(attachment["width"].as_str())
                .max(attachment["width"].as_u64().unwrap_or(0).min(32768) as u32),
            height: dimension(attachment["height"].as_str())
                .max(attachment["height"].as_u64().unwrap_or(0).min(32768) as u32),
        };
        let mut duplicate = false;
        for block in &mut content.blocks {
            if let ContentBlock::Image(existing) | ContentBlock::Attachment(existing) = block
                && asset_key(&existing.url) == asset_key(url)
            {
                if existing.name.is_empty() {
                    existing.name = media.name.clone();
                }
                if existing.width == 0 {
                    existing.width = media.width;
                }
                if existing.height == 0 {
                    existing.height = media.height;
                }
                existing.content_type = media.content_type.clone();
                existing.size = media.size;
                duplicate = true;
            }
        }
        if !duplicate {
            content.blocks.push(if is_image {
                ContentBlock::Image(media)
            } else {
                ContentBlock::Attachment(media)
            });
        }
    }
    content.blocks
}

pub fn local_path(value: &str) -> Result<PathBuf> {
    if value.is_empty() || value.contains('\0') {
        bail!("Choose a local file path");
    }
    let path = if value.starts_with("file:") {
        let url = reqwest::Url::parse(value).context("Invalid local file URL")?;
        if url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            bail!("Choose a local file path");
        }
        url.to_file_path()
            .map_err(|_| anyhow::anyhow!("Choose a local file path"))?
    } else {
        if value.contains("://") {
            bail!("Choose a local file, not a URL");
        }
        crate::storage::expand_home(Path::new(value))
    };
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}

fn open_attachment(path: &str) -> Result<File> {
    let path = local_path(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(&path).with_context(|| {
        format!(
            "Cannot read attachment: {}. Select the file again.",
            format::safe_text(&path.display().to_string())
        )
    })?;
    if !file.metadata()?.is_file() {
        bail!("Attachment is not a regular file");
    }
    Ok(file)
}

pub fn validate_files(paths: &[String]) -> Result<()> {
    if paths.len() > MAX_ATTACHMENTS {
        bail!("Choose up to 10 attachments");
    }
    let mut total = 0u64;
    for path in paths {
        total = total.saturating_add(open_attachment(path)?.metadata()?.len());
    }
    if total > MAX_UPLOAD_BYTES {
        bail!("Attachments must total 20 MiB or less");
    }
    Ok(())
}

pub fn upload_files(paths: &[String]) -> Result<Vec<Value>> {
    validate_files(paths)?;
    let mut total = 0;
    let mut files = vec![];
    for path in paths {
        let mut data = vec![];
        open_attachment(path)?
            .take(MAX_UPLOAD_BYTES - total + 1)
            .read_to_end(&mut data)
            .context("Cannot read attachment; message was not sent")?;
        total += data.len() as u64;
        if total > MAX_UPLOAD_BYTES {
            bail!("Attachments must total 20 MiB or less");
        }
        let path = local_path(path)?;
        let name = path
            .file_name()
            .context("Invalid attachment filename")?
            .to_string_lossy();
        files.push(json!({"name":name, "content_type":mime_guess::from_path(&path).first_or_octet_stream().essence_str(), "data":STANDARD.encode(data)}));
    }
    Ok(files)
}

fn client(automatic: bool) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .user_agent(concat!("intercom-tui/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5
                || !format::safe_url(attempt.url().as_str())
                || automatic
                    && !self::automatic(&Media {
                        url: attempt.url().to_string(),
                        ..Default::default()
                    })
            {
                attempt.error("Unsupported media redirect")
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}

async fn response(url: &str, automatic: bool, limit: u64) -> Result<reqwest::Response> {
    if !format::safe_url(url) {
        bail!("Invalid HTTP(S) media URL");
    }
    if automatic
        && !self::automatic(&Media {
            url: url.into(),
            ..Default::default()
        })
    {
        bail!("External images require explicit permission to load");
    }
    // This client has no API headers, cookies, or credentials, including on redirects.
    let response = client(automatic)?.get(url).send().await.map_err(|_| {
        anyhow::anyhow!("Cannot load media. Check your connection or load it explicitly.")
    })?;
    if !response.status().is_success() {
        bail!("Cannot load media. Refresh the conversation for a current URL.");
    }
    if response.content_length().is_some_and(|n| n > limit) {
        bail!("Media exceeds the {} MiB limit", limit / 1024 / 1024);
    }
    Ok(response)
}

pub async fn download(url: &str, destination: &Path) -> Result<()> {
    if !destination.is_absolute() {
        bail!("Choose an absolute destination");
    }
    if destination.symlink_metadata().is_ok() {
        bail!("Destination already exists. Choose a new filename.");
    }
    let mut response = response(url, false, MAX_DOWNLOAD_BYTES).await?;
    let expected = response.content_length();
    let parent = destination.parent().context("Invalid destination")?;
    let temporary = tempfile::Builder::new()
        .prefix(".intercom-download-")
        .tempfile_in(parent)
        .context("Cannot create a file in the destination folder")?;
    let mut file = tokio::fs::File::from_std(temporary.reopen()?);
    let mut total = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("Attachment download was incomplete. Try again."))?
    {
        total += chunk.len() as u64;
        if total > MAX_DOWNLOAD_BYTES {
            bail!("Attachment exceeds the 100 MiB download limit");
        }
        file.write_all(&chunk)
            .await
            .context("Cannot save attachment")?;
    }
    if expected.is_some_and(|n| n != total) {
        bail!("Attachment download was incomplete. Try again.");
    }
    file.sync_all().await?;
    drop(file);
    temporary.persist_noclobber(destination).map_err(|_| {
        anyhow::anyhow!("Cannot save attachment. Choose a new filename in a writable folder.")
    })?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub async fn load_image(url: &str, automatic: bool) -> Result<Arc<RgbaImage>> {
    let mut response = response(url, automatic, MAX_IMAGE_BYTES).await?;
    let mut bytes = vec![];
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("Image download failed. Refresh and try again."))?
    {
        if bytes.len() + chunk.len() > MAX_IMAGE_BYTES as usize {
            bail!("Image exceeds the 20 MiB preview limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    tokio::task::spawn_blocking(move || decode(&bytes))
        .await
        .context("Image decoder stopped")?
}

fn decode(bytes: &[u8]) -> Result<Arc<RgbaImage>> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| anyhow::anyhow!("Preview unavailable (unsupported or oversized image). Open the original or download it."))?;
    Ok(Arc::new(
        image
            .thumbnail(image.width().min(640), image.height().min(480))
            .to_rgba8(),
    ))
}

/// Unicode half blocks work without terminal-specific graphics protocols.
pub fn image_lines(image: &RgbaImage, columns: u16, rows: u16) -> Vec<Line<'static>> {
    let resized = image::DynamicImage::ImageRgba8(image.clone())
        .thumbnail(
            (columns.max(1) as u32).min(image.width()),
            (rows.max(1) as u32 * 2).min(image.height()),
        )
        .to_rgba8();
    let color = |pixel: &image::Rgba<u8>| {
        let alpha = pixel[3] as u16;
        let blend = |c: u8| ((c as u16 * alpha + 24 * (255 - alpha)) / 255) as u8;
        Color::Rgb(blend(pixel[0]), blend(pixel[1]), blend(pixel[2]))
    };
    (0..resized.height())
        .step_by(2)
        .map(|y| {
            Line::from(
                (0..resized.width())
                    .map(|x| {
                        Span::styled(
                            "▀",
                            Style::default().fg(color(resized.get_pixel(x, y))).bg(
                                if y + 1 < resized.height() {
                                    color(resized.get_pixel(x, y + 1))
                                } else {
                                    Color::Reset
                                },
                            ),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

pub fn size(bytes: u64) -> String {
    if bytes == 0 {
        String::new()
    } else if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / 1048576.0)
    }
}

pub fn download_name(media: &Media) -> String {
    let name = media
        .label()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("attachment")
        .trim()
        .to_owned();
    if name.is_empty() || name == "." || name == ".." {
        "attachment".into()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[test]
    fn splits_inline_images_in_order_and_balances_formatting() {
        let blocks = content(
            "<p><b>Before <a href='https://example.org/full'><img src='https://example.org/p.png' alt='diagram' width='80' height='40'></a> after</b></p>",
            &Value::Null,
        );
        assert_eq!(blocks.len(), 3);
        let ContentBlock::Text { html } = &blocks[0] else {
            panic!()
        };
        assert_eq!(format::plain(html), "Before ");
        assert!(html.contains("</b>"));
        let ContentBlock::Image(image) = &blocks[1] else {
            panic!()
        };
        assert_eq!(image.alt, "diagram");
        assert_eq!((image.width, image.height), (80, 40));
        assert_eq!(image.link_url, "https://example.org/full");
        let ContentBlock::Text { html } = &blocks[2] else {
            panic!()
        };
        assert!(html.contains("<b>"));
        assert!(format::plain(html).contains("after"));
    }

    #[test]
    fn attachments_merge_with_inline_images_but_repeated_images_remain() {
        let body = "<img src='https://downloads.intercomcdn.com/i/o/123/p.png?signature=one'><img src='https://downloads.intercomcdn.com/i/o/123/p.png?signature=one'>";
        let blocks = content(
            body,
            &json!([
                {"url":"https://workspace.intercom-attachments-1.com/i/o/123/p.png?signature=two", "name":"Diagram.png", "content_type":"image/png", "width":80},
                {"url":"https://example.org/manual.pdf", "name":"Manual.pdf", "content_type":"application/pdf", "filesize":"1234"},
                {"url":"file:///etc/passwd", "name":"unsafe"}
            ]),
        );
        assert_eq!(blocks.len(), 3);
        for block in &blocks[..2] {
            let ContentBlock::Image(media) = block else {
                panic!()
            };
            assert_eq!(media.name, "Diagram.png");
            assert_eq!(media.width, 80);
        }
        let ContentBlock::Attachment(media) = &blocks[2] else {
            panic!()
        };
        assert_eq!(media.size, 1234);
        assert_eq!(media.content_type, "application/pdf");
        assert_eq!(
            identity("https://example.org/a?part=2&X-Amz-Signature=old&expires=1#x"),
            identity("https://example.org/a?part=2&X-Amz-Signature=new&expires=2")
        );
        assert_ne!(
            identity("https://example.org/a?part=1"),
            identity("https://example.org/a?part=2")
        );
    }

    #[test]
    fn rejects_unsafe_urls_and_never_automatically_loads_external_or_tracking_images() {
        for url in [
            "file:///etc/passwd",
            "data:image/png;base64,abc",
            "cid:email",
            "https://user:secret@example.org/x",
            "https://example.org\\x",
            "https://example.org/x\x1b",
            "https://example.org:bad/x",
            "//example.org/x",
            "https:example.org/x",
        ] {
            assert!(!format::safe_url(url), "{url}");
            let blocks = content(
                &format!("<img src='{}' alt='label'>", format::escape(url)),
                &Value::Null,
            );
            assert!(!blocks.iter().any(|b| matches!(b, ContentBlock::Image(_))));
        }
        for url in [
            "http://downloads.intercomcdn.com/a",
            "https://evilintercomcdn.com/a",
            "https://intercomcdn.com.evil.org/a",
            "https://external.org/a",
            "https://intercomcdn.com:444/a",
        ] {
            assert!(!automatic(&Media {
                url: url.into(),
                ..Default::default()
            }));
        }
        let mut image = Media {
            url: "https://downloads.intercomcdn.com/a".into(),
            ..Default::default()
        };
        assert!(automatic(&image));
        image.width = 1;
        assert!(!automatic(&image));
        image.width = 20;
        image.height = 1;
        assert!(!automatic(&image));
    }

    #[test]
    fn local_files_are_bounded_regular_files_and_encoded_at_send_time() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("hello world.txt");
        std::fs::write(&path, b"hello\0world").unwrap();
        let paths = vec![path.to_string_lossy().into_owned()];
        let files = upload_files(&paths).unwrap();
        assert_eq!(files[0]["name"], "hello world.txt");
        assert_eq!(files[0]["content_type"], "text/plain");
        assert_eq!(
            STANDARD.decode(files[0]["data"].as_str().unwrap()).unwrap(),
            b"hello\0world"
        );
        assert_eq!(
            local_path(reqwest::Url::from_file_path(&path).unwrap().as_str()).unwrap(),
            path
        );
        assert!(local_path("file://remote/etc/passwd").is_err());
        assert!(local_path("file:///etc/passwd?x=1").is_err());
        assert!(local_path("https://example.org/a").is_err());
        assert!(local_path("bad\0path").is_err());
        assert!(validate_files(&vec![paths[0].clone(); 11]).is_err());
        assert!(validate_files(&[root.path().to_string_lossy().into_owned()]).is_err());
        File::create(&path)
            .unwrap()
            .set_len(MAX_UPLOAD_BYTES + 1)
            .unwrap();
        assert!(
            upload_files(&paths)
                .unwrap_err()
                .to_string()
                .contains("20 MiB")
        );
        std::fs::remove_file(&path).unwrap();
        assert!(upload_files(&paths).is_err());
    }

    #[tokio::test]
    async fn downloads_follow_safe_redirects_without_credentials_or_overwriting() {
        let source = MockServer::start().await;
        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/redirect"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/file", target.uri())),
            )
            .mount(&source)
            .await;
        Mock::given(path("/file"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"file bytes"))
            .mount(&target)
            .await;
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("saved");
        download(&format!("{}/redirect", source.uri()), &destination)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"file bytes");
        for server in [&source, &target] {
            for request in server.received_requests().await.unwrap() {
                assert!(!request.headers.contains_key("authorization"));
                assert!(!request.headers.contains_key("intercom-version"));
                assert!(!request.headers.contains_key("cookie"));
            }
        }
        assert!(
            download(&format!("{}/redirect", source.uri()), &destination)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"file bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&destination)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    async fn raw_server(response: String) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/file", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let _ = stream.write_all(response.as_bytes()).await;
        });
        (url, task)
    }

    #[tokio::test]
    async fn oversized_and_truncated_downloads_leave_no_files() {
        let root = tempfile::tempdir().unwrap();
        for (headers, body, error) in [
            (
                format!("Content-Length: {}", MAX_DOWNLOAD_BYTES + 1),
                "",
                "100 MiB",
            ),
            ("Content-Length: 100".into(), "short", "incomplete"),
        ] {
            let (url, task) = raw_server(format!(
                "HTTP/1.1 200 OK\r\n{headers}\r\nConnection: close\r\n\r\n{body}"
            ))
            .await;
            assert!(
                download(&url, &root.path().join("file"))
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(error)
            );
            task.await.unwrap();
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        }
        let server = MockServer::start().await;
        Mock::given(path("/bad"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "file:///etc/passwd"),
            )
            .mount(&server)
            .await;
        assert!(
            download(&format!("{}/bad", server.uri()), &root.path().join("file"))
                .await
                .is_err()
        );
        assert!(
            load_image(&format!("{}/external", server.uri()), true)
                .await
                .is_err()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn previews_decode_static_images_fit_without_cropping_and_bound_the_cache() {
        let pixels = RgbaImage::from_pixel(100, 50, image::Rgba([255, 0, 0, 255]));
        let mut bytes = Cursor::new(vec![]);
        image::DynamicImage::ImageRgba8(pixels.clone())
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let server = MockServer::start().await;
        Mock::given(path("/image"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.into_inner()))
            .mount(&server)
            .await;
        let image = load_image(&format!("{}/image", server.uri()), false)
            .await
            .unwrap();
        assert_eq!(image.dimensions(), (100, 50));
        let lines = image_lines(&image, 20, 10);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].width(), 20);
        assert!(decode(b"not an image").is_err());
        let mut cache = Images::default();
        assert!(cache.begin("first", false));
        assert!(cache.begin("second", false));
        assert!(!cache.begin("third", false));
        cache
            .entries
            .insert("first".into(), Preview::Failed("failed".into()));
        assert!(!cache.begin("first", false));
        assert!(cache.begin("first", true));
        cache
            .entries
            .insert("second".into(), Preview::Ready(image.clone()));
        cache
            .entries
            .insert("first".into(), Preview::Ready(image.clone()));
        for index in 0..40 {
            let url = index.to_string();
            assert!(cache.begin(&url, false));
            cache.entries.insert(url, Preview::Ready(image.clone()));
        }
        assert_eq!(cache.entries.len(), 24);
    }
}
