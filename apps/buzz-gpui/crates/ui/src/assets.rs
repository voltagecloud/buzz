//! The asset source: Buzz icons and brand marks, falling back to gpui-kit's
//! own icon bundle so its components keep their glyphs.

use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../assets"]
#[include = "icons/tabler/**/*.svg"]
#[include = "icons/custom/*.svg"]
#[include = "icons/custom/*.png"]
#[include = "icons/brand/*.svg"]
#[include = "icons/brand/*.png"]
#[include = "avatar-packs/*.png"]
struct Embedded;

/// Pass to `Application::with_assets`.
///
/// Paths may carry a `?stroke=<width>` suffix; the SVG is then served with
/// its `stroke-width` rewritten, so one embedded Tabler file covers the
/// 1.5, 2 and 2.5 weights buzz-app uses.
#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let (file, stroke) = match path.split_once("?stroke=") {
            Some((file, stroke)) => (file, Some(stroke)),
            None => (path, None),
        };
        if let Some(asset) = Embedded::get(file) {
            return Ok(Some(match stroke {
                Some(width) => {
                    let svg = String::from_utf8_lossy(&asset.data).replace(
                        "stroke-width=\"2\"",
                        &format!("stroke-width=\"{}\"", width.parse::<f32>()?),
                    );
                    Cow::Owned(svg.into_bytes())
                }
                None => asset.data,
            }));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths: Vec<SharedString> = Embedded::iter()
            .filter(|name| name.starts_with(path))
            .map(|name| name.into_owned().into())
            .collect();
        paths.extend(gpui_kit::assets::Assets.list(path)?);
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IconName;

    #[test]
    fn every_icon_is_embedded() {
        for icon in IconName::ALL {
            assert!(
                Assets.load(icon.path()).unwrap().is_some(),
                "{} missing at {}",
                icon.name(),
                icon.path()
            );
        }
    }

    #[test]
    fn stroke_override_rewrites_width() {
        let svg = Assets
            .load("icons/tabler/outline/hash.svg?stroke=2.5")
            .unwrap()
            .unwrap();
        let svg = std::str::from_utf8(&svg).unwrap();
        assert!(svg.contains("stroke-width=\"2.5\""));
    }
}
