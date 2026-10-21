//! Media download (`docs/port/relay.md` §5n.6).
//!
//! Relay media (`<origin>/media/<sha256>[.ext]`) needs a fresh Blossom
//! `get` authorization on every request, which only this process can sign,
//! so the UI fetches it here rather than through a generic HTTP image
//! loader. Other `https://` URLs (profile pictures, custom emoji hosted
//! elsewhere) load unauthenticated, as the reference webview loads them.
//!
//! Fetches run on the session's tokio runtime and resolve to the whole
//! body. Recent bodies stay in a small in-memory cache, and bodies of any
//! age on disk ([`crate::media_disk`]), standing in for the webview cache
//! the reference relies on (`max-age=3600`).
//!
//! Players, downloads and the clipboard use buzz-desktop's media services
//! instead, the code buzz-app's Tauri host runs: players stream from its
//! loopback listener with ranged reads through its block cache, and saves
//! and copies go through its checks. buzz-desktop serves only HTTPS relays,
//! so on a plain-HTTP development relay those fall back to [`Media::fetch`].

use crate::origin::RelayOrigin;
use buzz_desktop::EventTemplate;
use buzz_desktop::media::stream::MediaStream;
use buzz_identity::{Identity, MediaVerb};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::Semaphore;
use url::Url;

/// Largest relay media body (`MAX_MEDIA` in the reference proxy).
const MAX_RELAY_MEDIA: usize = 100 * 1024 * 1024;
/// Largest body from another host. The UI only loads images from them.
const MAX_PUBLIC_MEDIA: usize = 25 * 1024 * 1024;
/// Bytes kept in the cache, and the largest body worth caching.
const CACHE_BUDGET: usize = 64 * 1024 * 1024;
const CACHE_ENTRY_LIMIT: usize = 16 * 1024 * 1024;
/// Downloads in flight at once, so a screen of avatars cannot starve the
/// relay connection.
const SLOTS: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    #[error("This media URL is not supported")]
    Unsupported,
    #[error("The media is too large")]
    TooLarge,
    #[error("The media server answered {0}")]
    Status(u16),
    #[error("{0}")]
    Transport(String),
}

/// Media downloads for one community. Cheap to clone.
#[derive(Clone)]
pub struct Media(Arc<Inner>);

struct Inner {
    origin: RelayOrigin,
    identity: Arc<Identity>,
    runtime: tokio::runtime::Handle,
    /// Signed requests never follow redirects, so the authorization cannot
    /// leak to another host.
    relay: reqwest::Client,
    public: reqwest::Client,
    slots: Semaphore,
    cache: Mutex<Cache>,
    /// Started on first use; `None` if it could not bind.
    stream: OnceLock<Option<MediaStream>>,
}

/// Signs buzz-desktop's Blossom media tokens with this identity, and
/// nothing else ([`Identity::sign_media_event`]).
#[derive(Clone)]
pub struct MediaSigner(Arc<Identity>);

impl buzz_desktop::Signer for MediaSigner {
    fn sign(
        &self,
        event: EventTemplate,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, String>> + Send {
        let signed = self
            .0
            .sign_media_event(event.kind, event.created_at, &event.tags, &event.content)
            .map_err(|error| error.to_string())
            .and_then(|event| serde_json::to_value(&event).map_err(|error| error.to_string()));
        std::future::ready(signed)
    }
}

impl Media {
    pub fn new(
        runtime: tokio::runtime::Handle,
        origin: RelayOrigin,
        identity: Arc<Identity>,
    ) -> Self {
        let relay = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            .build()
            .expect("build the media client");
        let public = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("build the media client");
        Self(Arc::new(Inner {
            origin,
            identity,
            runtime,
            relay,
            public,
            slots: Semaphore::new(SLOTS),
            cache: Mutex::new(Cache::default()),
            stream: OnceLock::new(),
        }))
    }

    pub(crate) fn signer(&self) -> MediaSigner {
        MediaSigner(self.0.identity.clone())
    }

    /// Whether buzz-desktop serves this relay's media: only HTTPS origins.
    fn desktop_media(&self, url: &str) -> bool {
        self.is_relay_media(url) && buzz_desktop::media::media_url(url).is_some()
    }

    /// A loopback `http://127.0.0.1` URL a player can stream `url` from,
    /// reading ranges as it plays. `None` when buzz-desktop cannot serve
    /// it; fetch the whole body then.
    pub fn stream_url(&self, url: &str) -> Option<String> {
        if !self.desktop_media(url) {
            return None;
        }
        let stream = self.0.stream.get_or_init(|| {
            MediaStream::start(self.signer(), &self.0.runtime)
                .inspect_err(|error| tracing::warn!(%error, "could not start the media listener"))
                .ok()
        });
        Some(format!("{}{}", stream.as_ref()?.base(), encode(url)))
    }

    /// Saves `url` into `directory` (Downloads) under the sender's name,
    /// never replacing a file there, and returns the saved path
    /// (`media_download`).
    pub async fn download(
        &self,
        url: &str,
        name: Option<&str>,
        directory: PathBuf,
    ) -> Result<PathBuf, String> {
        if self.desktop_media(url) {
            let signer = self.signer();
            let source = scheme_url(url);
            let name = name.unwrap_or_default().to_owned();
            return self
                .0
                .runtime
                .spawn(async move {
                    buzz_desktop::media::download(&signer, &source, &name, || Some(directory)).await
                })
                .await
                .map_err(|_| "Media download interrupted".to_string())?;
        }
        let bytes = self
            .fetch(url)
            .await
            .map_err(|_| "Media download failed".to_string())?;
        let name = download_name(name, url);
        self.0
            .runtime
            .spawn_blocking(move || save(&directory, &name, &bytes))
            .await
            .map_err(|_| "Media download interrupted".to_string())?
    }

    /// The decoded RGBA pixels of an image, for the clipboard:
    /// `(width, height, rgba)` (`media_copy_image`).
    pub async fn copy_image(&self, url: &str) -> Result<(usize, usize, Vec<u8>), String> {
        if self.desktop_media(url) {
            let (signer, source) = (self.signer(), scheme_url(url));
            return self
                .0
                .runtime
                .spawn(async move { buzz_desktop::media::copy_image(&signer, &source).await })
                .await
                .map_err(|_| "Could not fetch image".to_string())?;
        }
        let bytes = self
            .fetch(url)
            .await
            .map_err(|_| "Could not fetch image".to_string())?;
        self.0
            .runtime
            .spawn_blocking(move || {
                let image = image::load_from_memory(&bytes)
                    .map_err(|_| "Could not decode image".to_string())?
                    .into_rgba8();
                Ok((
                    image.width() as usize,
                    image.height() as usize,
                    image.into_raw(),
                ))
            })
            .await
            .map_err(|_| "Could not decode image".to_string())?
    }

    /// Whether `url` is this relay's media: same origin, path
    /// `/media/<64 lowercase hex>[.ext]` with `ext` `[a-z0-9]{1,8}` or
    /// `thumb.jpg`, and no query or fragment (`relay.rs` `media_url`).
    pub fn is_relay_media(&self, url: &str) -> bool {
        relay_media_url(&self.0.origin, still(&self.0.origin, url)).is_some()
    }

    /// The relay's thumbnail for its own media, which avatars use
    /// (`mediaUrl(..., "small")`). `None` for any other URL.
    pub fn thumbnail(&self, url: &str) -> Option<String> {
        let url = relay_media_url(&self.0.origin, still(&self.0.origin, url))?;
        let name = url.path().strip_prefix("/media/")?;
        let hash = name.split('.').next()?;
        let mut thumb = url.clone();
        thumb.set_path(&format!("/media/{hash}.thumb.jpg"));
        Some(thumb.to_string())
    }

    pub(crate) fn origin(&self) -> &RelayOrigin {
        &self.0.origin
    }

    /// Warms avatars before they show (`media.prepare`): this relay's
    /// thumbnails or public images, fetched in the background into the
    /// cache.
    pub fn prefetch_avatars(&self, urls: Vec<String>) {
        for url in urls {
            let url = self.thumbnail(&url).unwrap_or(url);
            if self.0.cache.lock().expect("media cache").contains(&url) {
                continue;
            }
            let media = self.clone();
            self.0.runtime.spawn(async move {
                let _ = media.fetch(&url).await;
            });
        }
    }

    pub(crate) fn runtime(&self) -> &tokio::runtime::Handle {
        &self.0.runtime
    }

    /// The whole body of `url`: signed for this relay's media, plain for
    /// other `https://` URLs, refused for anything else.
    pub async fn fetch(&self, url: &str) -> Result<Arc<[u8]>, MediaError> {
        let url = still(&self.0.origin, url);
        let target = if let Some(url) = relay_media_url(&self.0.origin, url) {
            Target::Relay(url)
        } else {
            let url = Url::parse(url).map_err(|_| MediaError::Unsupported)?;
            if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
                return Err(MediaError::Unsupported);
            }
            Target::Public(url)
        };
        let key = target.url().to_string();
        if let Some(bytes) = self.0.cache.lock().expect("media cache").get(&key) {
            return Ok(bytes);
        }
        let inner = self.0.clone();
        let bytes = self
            .0
            .runtime
            .spawn(async move { inner.load(target).await })
            .await
            .map_err(|error| MediaError::Transport(error.to_string()))??;
        self.0
            .cache
            .lock()
            .expect("media cache")
            .insert(key, bytes.clone());
        Ok(bytes)
    }
}

/// Writes a body to disk behind the caller, if it is small enough to keep.
fn keep(key: String, bytes: Arc<[u8]>) {
    if bytes.len() > CACHE_ENTRY_LIMIT {
        return;
    }
    if let Some(disk) = crate::media_disk::disk() {
        tokio::task::spawn_blocking(move || disk.write(&key, &bytes));
    }
}

enum Target {
    Relay(Url),
    Public(Url),
}

impl Target {
    fn url(&self) -> &Url {
        match self {
            Self::Relay(url) | Self::Public(url) => url,
        }
    }
}

impl Inner {
    /// The body from disk if it was kept, else downloaded and kept. Other
    /// hosts' images older than an hour show as kept and refresh behind.
    async fn load(self: Arc<Self>, target: Target) -> Result<Arc<[u8]>, MediaError> {
        let key = target.url().to_string();
        let Some(disk) = crate::media_disk::disk() else {
            return self.download(target).await;
        };
        let read = key.clone();
        let kept = tokio::task::spawn_blocking(move || disk.read(&read))
            .await
            .ok()
            .flatten();
        if let Some(kept) = kept {
            if matches!(target, Target::Public(_)) && kept.age > crate::media_disk::PUBLIC_FRESH {
                tokio::spawn(async move {
                    if let Ok(bytes) = self.download(target).await {
                        keep(key, bytes);
                    }
                });
            }
            return Ok(kept.bytes.into());
        }
        let bytes = self.download(target).await?;
        keep(key, bytes.clone());
        Ok(bytes)
    }

    async fn download(&self, target: Target) -> Result<Arc<[u8]>, MediaError> {
        let _slot = self.slots.acquire().await.expect("media slots stay open");
        let (request, limit) = match &target {
            Target::Relay(url) => {
                let auth = self
                    .identity
                    .media_auth(MediaVerb::Get, &server(url), None)
                    .map_err(|error| MediaError::Transport(error.to_string()))?;
                (
                    self.relay.get(url.as_str()).header("Authorization", auth),
                    MAX_RELAY_MEDIA,
                )
            }
            Target::Public(url) => (self.public.get(url.as_str()), MAX_PUBLIC_MEDIA),
        };
        let mut response = request
            .send()
            .await
            .map_err(|error| MediaError::Transport(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(MediaError::Status(status.as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(MediaError::TooLarge);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| MediaError::Transport(error.to_string()))?
        {
            if bytes.len() + chunk.len() > limit {
                return Err(MediaError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes.into())
    }
}

/// A relay URL, percent-encoded as one path segment (`encodeURIComponent`).
fn encode(url: &str) -> String {
    const COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    percent_encoding::utf8_percent_encode(url, COMPONENT).to_string()
}

/// The `buzz-media://localhost/<relay URL>` source buzz-desktop's download
/// and copy take (`convertFileSrc(url, "buzz-media")`).
fn scheme_url(url: &str) -> String {
    format!("buzz-media://localhost/{}", encode(url))
}

/// The sender's file name with nothing that could make it a path, else the
/// URL's last segment.
fn download_name(name: Option<&str>, url: &str) -> String {
    let clean = |value: &str| {
        value
            .chars()
            .map(|c| {
                if c.is_control() || matches!(c, '/' | '\\') {
                    ' '
                } else {
                    c
                }
            })
            .collect::<String>()
            .trim()
            .trim_start_matches('.')
            .trim()
            .to_string()
    };
    name.map(clean)
        .filter(|name| !name.is_empty())
        .or_else(|| {
            url.rsplit('/')
                .next()
                .map(clean)
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "File".into())
}

/// Writes `body` to a new file in `directory`, numbering the name until one
/// is free, as buzz-desktop's `save_download` does.
fn save(directory: &std::path::Path, name: &str, body: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write as _;
    let (stem, extension) = name
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map_or((name, String::new()), |(stem, ext)| {
            (stem, format!(".{ext}"))
        });
    for index in 0..1000 {
        let candidate = if index == 0 {
            name.to_owned()
        } else {
            format!("{stem} ({index}){extension}")
        };
        let path = directory.join(candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                return match file.write_all(body).and_then(|()| file.sync_all()) {
                    Ok(()) => Ok(path),
                    Err(_) => {
                        drop(file);
                        let _ = std::fs::remove_file(&path);
                        Err("Media download failed".into())
                    }
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err("Media download failed".into()),
        }
    }
    Err("Media download failed".into())
}

/// `host[:port]` as the relay checks the Blossom `server` tag (`URL.host`).
pub(crate) fn server(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

/// The still image of this relay's media (`mediaUrl`): buzz desktop
/// appends `#buzz-anim=<url>` to an animated avatar. A fragment never
/// reaches the relay and the media rules refuse one, so the still loads.
fn still<'a>(origin: &RelayOrigin, url: &'a str) -> &'a str {
    if url.starts_with(&format!("{}/media/", origin.http())) {
        url.split('#').next().unwrap_or(url)
    } else {
        url
    }
}

fn relay_media_url(origin: &RelayOrigin, value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    let base = Url::parse(&origin.http()).ok()?;
    if url.origin() != base.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let name = url.path().strip_prefix("/media/")?;
    let (hash, extension) = name.split_once('.').unwrap_or((name, ""));
    let extension_ok = extension.is_empty()
        || extension == "thumb.jpg"
        || (extension.len() <= 8
            && extension
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
    let hash_ok = hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (hash_ok && extension_ok).then_some(url)
}

/// Least recently used bodies within [`CACHE_BUDGET`].
#[derive(Default)]
struct Cache {
    entries: HashMap<String, (Arc<[u8]>, u64)>,
    bytes: usize,
    tick: u64,
}

impl Cache {
    fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    fn get(&mut self, key: &str) -> Option<Arc<[u8]>> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(key).map(|(bytes, used)| {
            *used = tick;
            bytes.clone()
        })
    }

    fn insert(&mut self, key: String, bytes: Arc<[u8]>) {
        if bytes.len() > CACHE_ENTRY_LIMIT {
            return;
        }
        self.tick += 1;
        self.bytes += bytes.len();
        if let Some((old, _)) = self.entries.insert(key, (bytes, self.tick)) {
            self.bytes -= old.len();
        }
        while self.bytes > CACHE_BUDGET {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some((old, _)) = self.entries.remove(&oldest) {
                self.bytes -= old.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn recognizes_only_this_relays_media() {
        let origin = RelayOrigin::parse("wss://relay.example.com").unwrap();
        let ok = |url: &str| relay_media_url(&origin, url).is_some();
        assert!(ok(&format!("https://relay.example.com/media/{HASH}.jpg")));
        assert!(ok(&format!("https://relay.example.com/media/{HASH}")));
        assert!(ok(&format!(
            "https://relay.example.com/media/{HASH}.thumb.jpg"
        )));
        assert!(!ok(&format!("https://other.example.com/media/{HASH}.jpg")));
        assert!(!ok(&format!("http://relay.example.com/media/{HASH}.jpg")));
        assert!(!ok(&format!(
            "https://relay.example.com/media/{HASH}.jpg?x=1"
        )));
        assert!(!ok(&format!("https://relay.example.com/media/{HASH}.JPG")));
        // An empty query or fragment still counts; so does a double
        // extension or an animated avatar's link.
        assert!(!ok(&format!("https://relay.example.com/media/{HASH}?")));
        assert!(!ok(&format!("https://relay.example.com/media/{HASH}#")));
        assert!(!ok(&format!(
            "https://relay.example.com/media/{HASH}.tar.gz"
        )));
        assert!(!ok(&format!(
            "https://relay.example.com/media/{HASH}.png#buzz-anim=x"
        )));
        assert!(!ok("https://relay.example.com/media/abc.jpg"));
        assert!(!ok(&format!("https://relay.example.com/files/{HASH}.jpg")));

        let local = RelayOrigin::parse("ws://127.0.0.1:53000").unwrap();
        let url = format!("http://127.0.0.1:53000/media/{HASH}.png");
        assert_eq!(
            server(&relay_media_url(&local, &url).unwrap()),
            "127.0.0.1:53000"
        );
    }

    #[test]
    fn thumbnails_relay_media_only() {
        let media = Media::new(
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .handle()
                .clone(),
            RelayOrigin::parse("wss://relay.example.com").unwrap(),
            Arc::new(Identity::ephemeral()),
        );
        assert_eq!(
            media.thumbnail(&format!("https://relay.example.com/media/{HASH}.png")),
            Some(format!("https://relay.example.com/media/{HASH}.thumb.jpg"))
        );
        assert_eq!(media.thumbnail("https://example.com/a.png"), None);
        // An animated avatar shows its still, as a thumbnail too.
        let animated = format!(
            "https://relay.example.com/media/{HASH}.png#buzz-anim=https%3A%2F%2Frelay.example.com%2Fmedia%2F{HASH}.gif"
        );
        assert_eq!(
            media.thumbnail(&animated),
            Some(format!("https://relay.example.com/media/{HASH}.thumb.jpg"))
        );
        assert!(media.is_relay_media(&animated));
        assert_eq!(
            still(media.origin(), &animated),
            format!("https://relay.example.com/media/{HASH}.png")
        );
        // Other hosts keep their fragment.
        assert_eq!(
            still(media.origin(), "https://example.com/a.png#x"),
            "https://example.com/a.png#x"
        );
    }

    #[test]
    fn cache_evicts_the_least_recently_used() {
        let mut cache = Cache::default();
        let big = || Arc::<[u8]>::from(vec![0u8; CACHE_ENTRY_LIMIT]);
        for key in ["a", "b", "c", "d"] {
            cache.insert(key.into(), big());
        }
        assert!(cache.get("a").is_some());
        cache.insert("e".into(), big());
        assert!(cache.get("a").is_some());
        assert!(cache.get("b").is_none());
        assert_eq!(cache.bytes, CACHE_BUDGET);
    }
}
