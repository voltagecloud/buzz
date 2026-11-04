//! Web link embeds under messages: the page's site name, title,
//! description, image and theme colour from its OpenGraph (or plain
//! `<title>`) tags. A page is read when its message first shows and kept
//! on disk with its image, so later launches show it at once and read it
//! again behind after a day. A page with nothing to show is kept as such;
//! a network failure is tried once more a little later.
//!
//! Message links are untrusted, so the reader reaches only the public
//! internet: names resolving to loopback, private, link-local or
//! carrier-grade NAT (Tailscale) addresses are refused, on every redirect
//! as on the first request. Requests carry no cookies or credentials.

use buzz_ui::{ActiveTheme, TypeStyled};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
/// Pages are read up to `</head>`, and never past this.
const MAX_HTML: usize = 512 * 1024;
const MAX_IMAGE: usize = 8 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const MAX_TITLE: usize = 200;
const MAX_DESCRIPTION: usize = 400;
/// How long a failed read stays failed before it is tried once more.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// Network reads running at once, so a channel full of links queues its
/// pages; kept ones never wait.
const MAX_READS: usize = 6;
/// Kept pages and images older than this are read again after showing.
const FRESH: Duration = Duration::from_secs(24 * 3600);
/// An embed's width at most, with its bar and padding, and its large
/// image's width inside that.
const MAX_WIDTH: f32 = 432.;
const IMAGE_WIDTH: f32 = 400.;
const THUMBNAIL: f32 = 80.;
/// Images wider or taller than this are not decoded.
const MAX_PIXELS_SIDE: u32 = 8192;

/// What a page says about itself.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub site: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    /// An `https://` image, resolved against the page's final address.
    pub image: Option<String>,
    /// Whether the image is a large one under the text (the default)
    /// rather than a thumbnail beside it (`twitter:card` `summary`).
    pub large: bool,
    /// The page's `theme-color`, as `0xRRGGBB`.
    pub color: Option<u32>,
}

impl Page {
    fn is_empty(&self) -> bool {
        self.title.is_none() && self.description.is_none() && self.image.is_none()
    }
}

/// Whether an address is on the public internet.
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                // Unique local fc00::/7 and link-local fe80::/10.
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                // Documentation 2001:db8::/32.
                || (first == 0x2001 && ip.segments()[1] == 0x0db8)
                // NAT64 64:ff9b::/96 can reach anything behind it.
                || (first == 0x0064 && ip.segments()[1] == 0xff9b))
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        // Carrier-grade NAT 100.64.0.0/10, where Tailscale lives.
        || (a == 100 && (b & 0xc0) == 64)
        || (a == 192 && b == 0 && c == 0)
        // Benchmarking 198.18.0.0/15.
        || (a == 198 && (b & 0xfe) == 18)
        || a >= 240)
}

/// An address the reader may request: `http(s)`, no credentials, and an
/// IP-literal host only when it is public. Names are checked as they
/// resolve.
fn allowed(url: &url::Url) -> bool {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(host)) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host != "localhost" && !host.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(ip)) => public(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => public(IpAddr::V6(ip)),
        None => false,
    }
}

/// The system resolver, keeping only public addresses.
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| public(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("not a public address".into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// The app's page reader.
#[derive(Clone)]
struct Reader {
    http: reqwest::Client,
    runtime: tokio::runtime::Handle,
    reads: Arc<tokio::sync::Semaphore>,
}

impl Global for Reader {}

impl Reader {
    fn global(cx: &mut App) -> Self {
        if let Some(reader) = cx.try_global::<Self>() {
            return reader.clone();
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                "Mozilla/5.0 (compatible; buzz-gpui/",
                env!("CARGO_PKG_VERSION"),
                "; link preview)"
            ))
            .connect_timeout(TIMEOUT)
            .dns_resolver(Arc::new(PublicOnly))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    attempt.error("too many redirects")
                } else if allowed(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .expect("build the link preview client");
        let reader = Self {
            http,
            runtime: crate::runtime::Runtime::handle(cx),
            reads: Arc::new(tokio::sync::Semaphore::new(MAX_READS)),
        };
        cx.set_global(reader.clone());
        reader
    }

    /// Runs `task` on the network runtime.
    fn spawn<T, F>(&self, task: F) -> impl Future<Output = Result<T, SharedString>> + use<T, F>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, String>> + Send + 'static,
    {
        let task = self.runtime.spawn(task);
        async move {
            match task.await {
                Ok(result) => result.map_err(SharedString::from),
                Err(_) => Err("load failed".into()),
            }
        }
    }

    /// Runs `load` at most [`MAX_READS`] at once, under the time limit.
    async fn network<T>(
        &self,
        load: impl Future<Output = Result<T, Failure>>,
    ) -> Result<T, Failure> {
        let _read = self.reads.acquire().await;
        tokio::time::timeout(TIMEOUT, load)
            .await
            .unwrap_or_else(|_| Err(Failure::Transient("timed out".into())))
    }
}

/// Why a read showed nothing.
#[derive(Debug)]
enum Failure {
    /// The site answered with nothing to preview, which is kept as an
    /// answer.
    Final(String),
    /// The network failed, so a later read tries again.
    Transient(String),
}

impl Failure {
    fn message(self) -> String {
        match self {
            Self::Final(message) | Self::Transient(message) => message,
        }
    }
}

/// Reads kept on disk for `key`, with their age; blocking work runs off
/// the runtime's threads.
async fn read_kept(key: String) -> Option<(Vec<u8>, Duration)> {
    tokio::task::spawn_blocking(move || buzz_relay_client::read_kept(&key))
        .await
        .ok()
        .flatten()
}

fn keep(key: String, bytes: Vec<u8>) {
    tokio::task::spawn_blocking(move || buzz_relay_client::keep(&key, &bytes));
}

/// GETs `url`, keeping at most `limit` bytes; `stop` ends the read early.
async fn get(
    http: &reqwest::Client,
    url: &str,
    accept: &str,
    limit: usize,
    stop: impl Fn(&[u8]) -> bool,
) -> Result<(reqwest::Response, Vec<u8>), Failure> {
    let parsed = url::Url::parse(url).map_err(|_| Failure::Final("not a URL".into()))?;
    if !allowed(&parsed) {
        return Err(Failure::Final("not a public address".into()));
    }
    let mut response = http
        .get(parsed)
        .header("Accept", accept)
        .send()
        .await
        .map_err(|error| Failure::Transient(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        let message = format!("status {}", status.as_u16());
        // Gone or refused stays so; busy or broken may not.
        return Err(
            if status.is_client_error() && !matches!(status.as_u16(), 408 | 429) {
                Failure::Final(message)
            } else {
                Failure::Transient(message)
            },
        );
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Failure::Transient(e.to_string()))?
    {
        let room = limit - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= limit || stop(&body) {
            break;
        }
    }
    Ok((response, body))
}

async fn load_page(http: reqwest::Client, url: String) -> Result<Page, Failure> {
    let (response, body) = get(
        &http,
        &url,
        "text/html,application/xhtml+xml",
        MAX_HTML,
        |body| contains_ignore_case(body, b"</head>"),
    )
    .await?;
    let html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase())
        .is_none_or(|value| value.contains("html"));
    if !html {
        return Err(Failure::Final("not a page".into()));
    }
    let page = parse(&String::from_utf8_lossy(&body), response.url());
    if page.is_empty() {
        return Err(Failure::Final("nothing to show".into()));
    }
    Ok(page)
}

async fn load_image(http: reqwest::Client, url: String) -> Result<Vec<u8>, Failure> {
    let (_, bytes) = get(&http, &url, "image/*", MAX_IMAGE + 1, |_| false).await?;
    if bytes.len() > MAX_IMAGE {
        return Err(Failure::Final("image too large".into()));
    }
    match super::super::media::image_format(&bytes) {
        Some(ImageFormat::Svg) | None => Err(Failure::Final("not a supported image".into())),
        Some(_) => Ok(bytes),
    }
}

/// A page's preview, `None` for a page with nothing to show: kept on disk,
/// shown from there, and read again behind once it is a day old.
async fn page(reader: Reader, url: String) -> Result<Option<Page>, String> {
    let key = format!("buzz-link-preview:page:{url}");
    let kept = read_kept(key.clone())
        .await
        .and_then(|(bytes, age)| Some((serde_json::from_slice(&bytes).ok()?, age)));
    let fetch = {
        let reader = reader.clone();
        let (url, key) = (url.clone(), key.clone());
        async move {
            let page = match reader
                .network(load_page(reader.http.clone(), url.clone()))
                .await
            {
                Ok(page) => Some(page),
                Err(Failure::Final(error)) => {
                    tracing::debug!(%url, %error, "no link preview");
                    None
                }
                Err(Failure::Transient(error)) => {
                    tracing::debug!(%url, %error, "link preview failed");
                    return Err(error);
                }
            };
            if let Ok(bytes) = serde_json::to_vec(&page) {
                keep(key, bytes);
            }
            Ok(page)
        }
    };
    match kept {
        Some((page, age)) => {
            if age >= FRESH {
                tokio::spawn(fetch);
            }
            Ok(page)
        }
        None => fetch.await,
    }
}

/// An image's bytes, kept on disk like pages.
async fn image_bytes(reader: Reader, url: String) -> Result<Vec<u8>, String> {
    let key = format!("buzz-link-preview:image:{url}");
    let kept = read_kept(key.clone()).await;
    let fetch = {
        let reader = reader.clone();
        let (url, key) = (url.clone(), key.clone());
        async move {
            let bytes = reader
                .network(load_image(reader.http.clone(), url))
                .await
                .map_err(Failure::message)?;
            keep(key, bytes.clone());
            Ok(bytes)
        }
    };
    match kept {
        Some((bytes, age)) => {
            if age >= FRESH {
                tokio::spawn(async move {
                    let _: Result<_, String> = fetch.await;
                });
            }
            Ok(bytes)
        }
        None => fetch.await,
    }
}

/// Decodes an image and scales it down to `size` device pixels (its width,
/// or for a square its shorter side). The GPU samples images without
/// mipmaps, so one drawn much smaller than it is comes out jagged.
fn scaled(bytes: &[u8], size: u32, square: bool) -> Result<Arc<RenderImage>, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_PIXELS_SIDE);
    limits.max_image_height = Some(MAX_PIXELS_SIDE);
    reader.limits(limits);
    let image = reader.decode().map_err(|e| e.to_string())?.to_rgba8();
    let (width, height) = image.dimensions();
    let side = if square { width.min(height) } else { width };
    let image = if side > size {
        let ratio = size as f64 / side as f64;
        let to = |n: u32| ((n as f64 * ratio).round() as u32).max(1);
        image::imageops::resize(
            &image,
            to(width),
            to(height),
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        image
    };
    let (width, height) = image.dimensions();
    let mut pixels = image.into_raw();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    super::super::media::render_image(width, height, pixels).ok_or_else(|| "bad image".into())
}

fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Collapses runs of whitespace and cuts to `max` characters.
fn tidy(text: &str, max: usize) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }
    if text.chars().count() <= max {
        return Some(text);
    }
    let cut: String = text.chars().take(max).collect();
    Some(format!("{}…", cut.trim_end()))
}

/// Reads a page's preview from its `<head>`: OpenGraph first, then
/// Twitter's tags, then `<title>` and `<meta name=description>`.
pub fn parse(html: &str, base: &url::Url) -> Page {
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(html);
    let mut meta: Vec<(String, String)> = Vec::new();
    let mut title: Option<String> = None;
    // The DOM stays alive through the walk: dropping a node empties its
    // descendants' child lists, even those still held here.
    let mut pending: Vec<Handle> = vec![dom.document.clone()];
    while let Some(node) = pending.pop() {
        if let NodeData::Element { name, attrs, .. } = &node.data {
            let attrs = attrs.borrow();
            let attr = |key: &str| {
                attrs
                    .iter()
                    .find(|a| a.name.local.as_ref().eq_ignore_ascii_case(key))
                    .map(|a| a.value.to_string())
            };
            match name.local.as_ref() {
                "meta" => {
                    if let (Some(key), Some(content)) =
                        (attr("property").or_else(|| attr("name")), attr("content"))
                    {
                        meta.push((key.to_ascii_lowercase(), content));
                    }
                }
                "title" if title.is_none() => {
                    let mut text = String::new();
                    for child in node.children.borrow().iter() {
                        if let NodeData::Text { contents } = &child.data {
                            text.push_str(&contents.borrow());
                        }
                    }
                    title = Some(text);
                }
                // Nothing a preview reads lives in the body.
                "body" => continue,
                _ => {}
            }
        }
        pending.extend(node.children.borrow().iter().rev().cloned());
    }
    let first = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            meta.iter()
                .find(|(name, value)| name == key && !value.trim().is_empty())
                .map(|(_, value)| value.clone())
        })
    };
    let image = first(&["og:image:secure_url", "og:image", "og:image:url"])
        .or_else(|| first(&["twitter:image", "twitter:image:src"]))
        .and_then(|image| base.join(image.trim()).ok())
        .filter(|image| image.scheme() == "https" && allowed(image))
        .map(String::from);
    Page {
        site: first(&["og:site_name", "application-name"]).and_then(|s| tidy(&s, MAX_TITLE)),
        title: first(&["og:title", "twitter:title"])
            .or(title)
            .and_then(|t| tidy(&t, MAX_TITLE)),
        description: first(&["og:description", "twitter:description", "description"])
            .and_then(|d| tidy(&d, MAX_DESCRIPTION)),
        image,
        large: first(&["twitter:card"]).is_none_or(|card| card.trim() != "summary"),
        color: first(&["theme-color"]).and_then(|color| hex_color(&color)),
    }
}

/// `#rgb` or `#rrggbb` as `0xRRGGBB`.
fn hex_color(value: &str) -> Option<u32> {
    let hex = value.trim().strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        6 => u32::from_str_radix(hex, 16).ok(),
        3 => {
            let doubled: String = hex.chars().flat_map(|c| [c, c]).collect();
            u32::from_str_radix(&doubled, 16).ok()
        }
        _ => None,
    }
}

enum WebPage {}

impl Asset for WebPage {
    type Source = SharedString;
    type Output = Result<Arc<Page>, SharedString>;

    #[allow(clippy::manual_async_fn)]
    fn load(
        url: SharedString,
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let reader = Reader::global(cx);
        let load = reader.spawn(page(reader.clone(), url.to_string()));
        async move { load.await?.map(Arc::new).ok_or_else(|| "no preview".into()) }
    }
}

/// A page's image at the size it is drawn.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Scaled {
    url: SharedString,
    /// Device pixels: the width, or a square's side.
    size: u32,
    square: bool,
}

enum WebImage {}

impl Asset for WebImage {
    type Source = Scaled;
    type Output = Result<Arc<RenderImage>, SharedString>;

    #[allow(clippy::manual_async_fn)]
    fn load(source: Scaled, cx: &mut App) -> impl Future<Output = Self::Output> + Send + 'static {
        let reader = Reader::global(cx);
        let load = reader.spawn(image_bytes(reader.clone(), source.url.to_string()));
        async move {
            let bytes = load.await?;
            scaled(&bytes, source.size, source.square).map_err(SharedString::from)
        }
    }
}

/// Failed reads already tried again, by asset and source.
#[derive(Default)]
struct Retried(Arc<Mutex<HashSet<(&'static str, String)>>>);

impl Global for Retried {}

/// Forgets a failed asset after [`RETRY_AFTER`], so the next draw reads
/// it again; a second failure is kept for the session.
fn retry_later<A: Asset>(source: A::Source, key: String, window: &Window, cx: &mut App)
where
    A::Source: Send + 'static,
{
    let entry = (std::any::type_name::<A>(), key);
    let set = cx.default_global::<Retried>().0.clone();
    if !set.lock().expect("retried").insert(entry) {
        return;
    }
    window
        .spawn(cx, async move |cx| {
            cx.background_executor().timer(RETRY_AFTER).await;
            let _ = cx.update(|_, cx| cx.remove_asset::<A>(&source));
        })
        .detach();
}

/// The page's image, scaled for `width` logical pixels (or a `square` of
/// that side) on this window's display; `None` while it loads.
fn image_source(url: SharedString, width: f32, square: bool) -> ImageSource {
    ImageSource::Custom(Arc::new(move |window, cx| {
        let source = Scaled {
            url: url.clone(),
            size: (width * window.scale_factor()).ceil() as u32,
            square,
        };
        match window.use_asset::<WebImage>(&source, cx)? {
            Ok(image) => Some(Ok(image)),
            Err(error) => {
                let key = format!("{url}@{}", source.size);
                retry_later::<WebImage>(source, key, window, cx);
                Some(Err(ImageCacheError::Asset(error)))
            }
        }
    }))
}

/// The host an embed names when the page names no site.
fn host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|url| {
            url.host_str()
                .map(|h| h.trim_start_matches("www.").to_string())
        })
        .unwrap_or_else(|| url.to_string())
}

type Open = Rc<dyn Fn(Modifiers, &mut Window, &mut App)>;

/// A web link's preview under its message: a bar in the page's theme
/// colour beside its site, linked title, description and image. Nothing
/// shows while the page loads or when it has nothing to show.
#[derive(IntoElement)]
pub struct Embed {
    id: SharedString,
    url: String,
    open: Open,
}

impl Embed {
    /// `open` opens the link, as clicking it in the body does.
    pub fn new(
        id: SharedString,
        url: String,
        open: impl Fn(Modifiers, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id,
            url,
            open: Rc::new(open),
        }
    }
}

impl RenderOnce for Embed {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let source = SharedString::from(self.url.clone());
        let page = match window.use_asset::<WebPage>(&source, cx) {
            Some(Ok(page)) => page,
            Some(Err(_)) => {
                retry_later::<WebPage>(source, self.url.clone(), window, cx);
                return div().into_any_element();
            }
            None => return div().into_any_element(),
        };
        let c = *cx.colors();
        let bar = page
            .color
            .map_or(c.border_prominent, |color| rgb(color).into());
        let clickable = |id: &str, open: &Open| {
            let open = open.clone();
            div()
                .id(SharedString::from(format!("{}-{id}", self.id)))
                .cursor_pointer()
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    open(event.modifiers(), window, cx)
                })
        };
        let image = |width: f32, square: bool| {
            page.image.clone().map(|image| {
                img(image_source(image.into(), width, square))
                    .rounded(px(4.))
                    .object_fit(ObjectFit::Cover)
                    .with_fallback(|| div().into_any_element())
            })
        };
        let (thumbnail, large) = if page.large {
            (None, image(IMAGE_WIDTH, false))
        } else {
            (image(THUMBNAIL, true), None)
        };
        let text = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .text_caption()
                    .text_color(c.text_subtle)
                    .truncate()
                    .child(page.site.clone().unwrap_or_else(|| host(&self.url))),
            )
            .when_some(page.title.clone(), |d, title| {
                d.child(
                    clickable("title", &self.open)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c.text_link)
                        .hover(|s| s.underline())
                        .line_clamp(2)
                        .text_ellipsis()
                        .child(title),
                )
            })
            .when_some(page.description.clone(), |d, description| {
                d.child(
                    div()
                        .text_body_sm()
                        .text_color(c.text_standard)
                        .line_clamp(4)
                        .text_ellipsis()
                        .child(description),
                )
            });
        div()
            .id(self.id.clone())
            .mt(px(6.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .max_w(px(MAX_WIDTH))
            .pt(px(8.))
            .pr(px(16.))
            .pb(px(16.))
            .pl(px(12.))
            .rounded(px(4.))
            .border_l_4()
            .border_color(bar)
            .bg(c.surface_inset)
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(16.))
                    .child(text)
                    .when_some(thumbnail, |d, thumbnail| {
                        d.child(
                            clickable("thumbnail", &self.open)
                                .flex_none()
                                .child(thumbnail.size(px(THUMBNAIL))),
                        )
                    }),
            )
            .when_some(large, |d, image| {
                d.child(
                    clickable("image", &self.open)
                        .max_w_full()
                        .child(image.w(px(IMAGE_WIDTH)).max_w_full().max_h(px(300.))),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn base() -> url::Url {
        url::Url::parse("https://example.com/a/b").unwrap()
    }

    #[test]
    fn reads_opengraph_before_fallbacks() {
        let html = r#"<!doctype html><html><head>
            <title>Plain  title</title>
            <meta name="description" content="Plain description">
            <meta property="og:title" content=" Open
                Graph title ">
            <meta property="og:description" content="OG description">
            <meta property="og:site_name" content="Example">
            <meta property="og:image" content="/card.png">
            </head><body><meta property="og:title" content="ignored"></body></html>"#;
        assert_eq!(
            parse(html, &base()),
            Page {
                site: Some("Example".into()),
                title: Some("Open Graph title".into()),
                description: Some("OG description".into()),
                image: Some("https://example.com/card.png".into()),
                large: true,
                color: None,
            }
        );
    }

    #[test]
    fn falls_back_to_title_and_description() {
        let html = "<title>Just &amp; a title</title>\
            <meta name=description content='Words'>\
            <meta name=twitter:image content='http://example.com/x.png'>";
        let page = parse(html, &base());
        assert_eq!(page.title.as_deref(), Some("Just & a title"));
        assert_eq!(page.description.as_deref(), Some("Words"));
        // Only https images load.
        assert_eq!(page.image, None);
        assert_eq!(page.site, None);
    }

    #[test]
    fn cuts_long_text() {
        let long = "word ".repeat(200);
        let html = format!("<meta property=og:description content='{long}'>");
        let description = parse(&html, &base()).description.unwrap();
        assert!(description.ends_with('…'));
        assert!(description.chars().count() <= MAX_DESCRIPTION + 1);
    }

    #[test]
    fn refuses_addresses_off_the_public_internet() {
        for url in [
            "http://localhost:8080/",
            "http://app.localhost/",
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://169.254.169.254/latest/meta-data",
            "http://100.100.100.100/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]/",
            "https://user:pass@example.com/",
            "ftp://example.com/",
        ] {
            assert!(!allowed(&url::Url::parse(url).unwrap()), "{url}");
        }
        for url in [
            "https://example.com/",
            "http://93.184.216.34/",
            "https://[2606:4700::1111]/",
        ] {
            assert!(allowed(&url::Url::parse(url).unwrap()), "{url}");
        }
    }

    #[test]
    fn reads_the_card_kind_and_theme_colour() {
        let html = "<meta name=twitter:card content=summary>\
            <meta name=theme-color content='#1e2327'>\
            <meta property=og:title content=T>";
        let page = parse(html, &base());
        assert!(!page.large);
        assert_eq!(page.color, Some(0x1e2327));
        let page = parse(
            "<meta name=theme-color content='#fa0'><title>T</title>",
            &base(),
        );
        assert!(page.large);
        assert_eq!(page.color, Some(0xffaa00));
        assert_eq!(hex_color("red"), None);
        assert_eq!(hex_color("#12345"), None);
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::new(width, height)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn scales_images_down_to_their_drawn_size_only() {
        let size = |image: Arc<RenderImage>| {
            let size = image.size(0);
            (size.width.0, size.height.0)
        };
        assert_eq!(
            size(scaled(&png(1200, 600), 400, false).unwrap()),
            (400, 200)
        );
        assert_eq!(size(scaled(&png(300, 200), 80, true).unwrap()), (120, 80));
        assert_eq!(size(scaled(&png(100, 50), 400, false).unwrap()), (100, 50));
        assert!(scaled(b"not an image", 400, false).is_err());
    }

    #[test]
    fn keeps_pages_and_pages_without_previews() {
        let page = Some(parse("<title>Kept</title>", &base()));
        let kept = serde_json::to_vec(&page).unwrap();
        assert_eq!(serde_json::from_slice::<Option<Page>>(&kept).unwrap(), page);
        let none = serde_json::to_vec(&None::<Page>).unwrap();
        assert_eq!(serde_json::from_slice::<Option<Page>>(&none).unwrap(), None);
    }

    #[test]
    fn names_the_host_without_www() {
        assert_eq!(host("https://www.example.com/x"), "example.com");
        assert_eq!(host("https://docs.rs/"), "docs.rs");
    }
}
