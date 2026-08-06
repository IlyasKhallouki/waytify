//! Album art: fetching, caching, and pulling colours out of it.
//!
//! This lives in the daemon rather than the window for the same reason
//! everything else does. The window is disposable and may not be running when a
//! track changes, and two clients fetching the same image would be two downloads
//! and two caches that can disagree.
//!
//! Colours are extracted so a theme can follow the record. Nothing in the default
//! stylesheet uses them: reacting to album art is a choice a theme makes, not one
//! imposed on everyone.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use waytify_ipc::{ArtColors, Rgb, paths};

/// Art is small and the CDN is nearby. A slow fetch means something is wrong, and
/// a track with no cover is a much better outcome than a stalled update.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Refuse anything implausible for a cover image, so a redirect to something
/// enormous cannot fill the cache directory.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Everything is downscaled to this before colours are counted. Cover art detail
/// is irrelevant to which colours dominate, and this makes extraction cost the
/// same regardless of source resolution.
const SAMPLE_EDGE: u32 = 32;

/// Minimum contrast between the accent and the surface behind it. Below WCAG's
/// 3:1 for large text, a colour picked off the artwork stops being legible.
const MIN_CONTRAST: f32 = 3.0;

/// The surface colour the accent has to stay legible against.
///
/// Matches the background in the bundled stylesheet. A user theme with a very
/// different background can end up with a technically-contrasting accent that
/// still looks wrong, which is a reason to make this configurable later rather
/// than a reason to guess now.
pub const DEFAULT_SURFACE: Rgb = Rgb { r: 0x16, g: 0x18, b: 0x1d };

pub struct Artwork {
    pub path: PathBuf,
    pub colors: Option<ArtColors>,
}

/// Cache identity for a track's artwork.
///
/// The track id when there is one, since that is what changes when the song
/// does. Falls back to the URL for players that do not set a track id.
pub fn key_for(track: &waytify_ipc::Track) -> Option<String> {
    let url = track.art_url.as_ref()?;
    Some(track.id.clone().unwrap_or_else(|| url.clone()))
}

/// Fetch art for a track, or return the cached copy.
///
/// `key` identifies the track. Cache entries are keyed on it rather than on the
/// URL, because Spotify's image URLs are stable but nothing guarantees that in
/// general, and a track is what the caller actually has.
pub async fn fetch(url: &str, key: &str, background: Rgb) -> Result<Artwork> {
    let dir = paths::art_cache_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let stem = cache_key(key);
    let image_path = dir.join(format!("{stem}.img"));
    let colors_path = dir.join(format!("{stem}.colors"));

    if image_path.exists() {
        let colors = std::fs::read_to_string(&colors_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        // Colours may be missing from an older cache entry, or from one written
        // before extraction was added. Recompute rather than going without.
        let colors = match colors {
            Some(c) => Some(c),
            None => recompute(&image_path, &colors_path, background),
        };
        return Ok(Artwork { path: image_path, colors });
    }

    // Local files are already on disk. Copying them into the cache would double
    // the storage for no benefit.
    if let Some(local) = local_path(url) {
        let colors = colors_from_file(&local, background);
        return Ok(Artwork { path: local, colors });
    }

    let bytes = download(url).await?;

    // Cache a thumbnail rather than the original. Covers arrive around 640px and
    // are drawn at a fraction of that, so keeping the full image means decoding
    // far more than is ever displayed, on every track, forever.
    let stored = thumbnail(&bytes).unwrap_or_else(|| bytes.clone());

    // Write to a temporary name and rename into place, so a fetch interrupted
    // halfway cannot leave a truncated image that every later run treats as a
    // valid cache hit.
    let temp = dir.join(format!("{stem}.part"));
    std::fs::write(&temp, &stored).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &image_path)?;

    // Colours come from the original, which has more to work with than a
    // downscaled copy, though in practice the difference is slight.
    let colors = colors_from_bytes(&bytes, background);
    write_colors(&colors_path, colors.as_ref());
    Ok(Artwork { path: image_path, colors })
}

fn recompute(image: &Path, colors_path: &Path, background: Rgb) -> Option<ArtColors> {
    let colors = colors_from_file(image, background);
    write_colors(colors_path, colors.as_ref());
    colors
}

fn write_colors(path: &Path, colors: Option<&ArtColors>) {
    if let Some(c) = colors
        && let Ok(raw) = serde_json::to_string(c)
    {
        let _ = std::fs::write(path, raw);
    }
}

/// `mpris:artUrl` is sometimes a `file://` URL, for local libraries.
fn local_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let path = PathBuf::from(rest);
    path.exists().then_some(path)
}

async fn download(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder().timeout(FETCH_TIMEOUT).build()?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()?;

    if let Some(len) = response.content_length() {
        anyhow::ensure!(len <= MAX_BYTES, "cover image is {len} bytes, which is implausible");
    }
    let bytes = response.bytes().await?;
    anyhow::ensure!(bytes.len() as u64 <= MAX_BYTES, "cover image is larger than expected");
    Ok(bytes.to_vec())
}

/// A filesystem-safe, collision-resistant name for a cache entry.
///
/// Track ids are object paths or URIs full of slashes and colons, so they cannot
/// be used directly. Hashing also bounds the length, which matters because some
/// players use very long ids.
fn cache_key(key: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Longest edge of a cached cover, in pixels.
///
/// Twice the largest size the window draws it at, so it stays sharp on a
/// double-scaled display without storing the full original.
const THUMBNAIL_EDGE: u32 = 256;

/// Downscale to something worth caching. `None` if it cannot be decoded, in
/// which case the original bytes are stored unchanged and GTK gets to try.
fn thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(bytes).ok()?;
    if image.width() <= THUMBNAIL_EDGE && image.height() <= THUMBNAIL_EDGE {
        return None;
    }

    let small = image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE);
    let mut out = std::io::Cursor::new(Vec::new());
    // PNG rather than re-encoding to JPEG: lossless, and at this size the
    // difference in bytes does not matter.
    small.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

fn colors_from_file(path: &Path, background: Rgb) -> Option<ArtColors> {
    let bytes = std::fs::read(path).ok()?;
    colors_from_bytes(&bytes, background)
}

fn colors_from_bytes(bytes: &[u8], background: Rgb) -> Option<ArtColors> {
    let image = image::load_from_memory(bytes).ok()?;
    let small = image
        .resize_exact(SAMPLE_EDGE, SAMPLE_EDGE, image::imageops::FilterType::Triangle)
        .to_rgb8();
    let pixels: Vec<Rgb> =
        small.pixels().map(|p| Rgb { r: p.0[0], g: p.0[1], b: p.0[2] }).collect();
    extract(&pixels, background)
}

/// Pick an accent and a companion from a set of pixels.
///
/// Deliberately separate from any decoding so it can be tested directly.
pub fn extract(pixels: &[Rgb], background: Rgb) -> Option<ArtColors> {
    if pixels.is_empty() {
        return None;
    }

    // Counting exact colours would give one bucket per pixel on a photograph.
    // Quantising to 4 bits per channel groups shades that read as the same
    // colour while keeping distinct hues apart.
    let mut buckets = std::collections::HashMap::<(u8, u8, u8), (u32, u32, u32, u32)>::new();
    for p in pixels {
        let key = (p.r >> 4, p.g >> 4, p.b >> 4);
        let entry = buckets.entry(key).or_insert((0, 0, 0, 0));
        entry.0 += 1;
        entry.1 += u32::from(p.r);
        entry.2 += u32::from(p.g);
        entry.3 += u32::from(p.b);
    }

    // Average the members rather than using the bucket centre, so the result is
    // a colour that actually appears in the artwork.
    let mut candidates: Vec<(u32, Rgb)> = buckets
        .into_values()
        .map(|(n, r, g, b)| (n, Rgb { r: (r / n) as u8, g: (g / n) as u8, b: (b / n) as u8 }))
        .collect();
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.to_hex().cmp(&b.1.to_hex())));

    let vibrant = candidates
        .iter()
        .filter(|(_, c)| is_vibrant(*c))
        .map(|(n, c)| (*n, *c))
        .max_by_key(|(n, _)| *n)
        .map(|(_, c)| c)
        // Nothing saturated enough, which happens with black and white covers.
        // The most common colour is still better than nothing.
        .or_else(|| candidates.first().map(|(_, c)| *c))?;

    let vibrant = ensure_contrast(vibrant, background);

    let muted = candidates
        .iter()
        .filter(|(_, c)| !is_vibrant(*c))
        .map(|(n, c)| (*n, *c))
        .max_by_key(|(n, _)| *n)
        .map(|(_, c)| c)
        .unwrap_or(vibrant);

    Some(ArtColors { vibrant, muted, on_vibrant: readable_on(vibrant) })
}

/// Saturated and mid-brightness. Excludes the near-black and near-white that
/// dominate most covers without saying anything about their colour.
fn is_vibrant(c: Rgb) -> bool {
    let (s, v) = saturation_value(c);
    s >= 0.35 && (0.25..=0.95).contains(&v)
}

fn saturation_value(c: Rgb) -> (f32, f32) {
    let (r, g, b) = (f32::from(c.r), f32::from(c.g), f32::from(c.b));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let saturation = if max <= 0.0 { 0.0 } else { (max - min) / max };
    (saturation, max / 255.0)
}

/// Lighten or darken until the colour is legible against the background.
///
/// Moving away from the background rather than toward a fixed target keeps the
/// hue, so a dark red on a dark surface becomes a brighter red rather than grey.
fn ensure_contrast(color: Rgb, background: Rgb) -> Rgb {
    if color.contrast(background) >= MIN_CONTRAST {
        return color;
    }

    let lighten = background.luminance() < 0.5;
    let mut current = color;
    // Bounded so a pathological input cannot spin here. Sixteen steps is enough
    // to cross the full range at this step size.
    for _ in 0..16 {
        current = if lighten { blend(current, WHITE, 0.12) } else { blend(current, BLACK, 0.12) };
        if current.contrast(background) >= MIN_CONTRAST {
            break;
        }
    }
    current
}

/// Black or white, whichever is legible on top of `color`.
fn readable_on(color: Rgb) -> Rgb {
    if color.contrast(WHITE) >= color.contrast(BLACK) { WHITE } else { BLACK }
}

const WHITE: Rgb = Rgb { r: 255, g: 255, b: 255 };
const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };

fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Rgb { r: mix(a.r, b.r), g: mix(a.g, b.g), b: mix(a.b, b.b) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK_SURFACE: Rgb = Rgb { r: 0x16, g: 0x18, b: 0x1d };
    const LIGHT_SURFACE: Rgb = Rgb { r: 0xf5, g: 0xf5, b: 0xf7 };

    fn fill(colour: Rgb, n: usize) -> Vec<Rgb> {
        vec![colour; n]
    }

    #[test]
    fn no_pixels_means_no_colours() {
        assert!(extract(&[], DARK_SURFACE).is_none());
    }

    #[test]
    fn the_dominant_saturated_colour_wins() {
        let mut pixels = fill(Rgb { r: 200, g: 40, b: 40 }, 60);
        pixels.extend(fill(Rgb { r: 40, g: 60, b: 200 }, 20));
        let colors = extract(&pixels, DARK_SURFACE).unwrap();
        assert!(
            colors.vibrant.r > colors.vibrant.b,
            "expected the red to win: {:?}",
            colors.vibrant
        );
    }

    #[test]
    fn near_black_pixels_do_not_become_the_accent() {
        // Most cover art is mostly dark. Counting by frequency alone would pick
        // the background of the image every time.
        let mut pixels = fill(Rgb { r: 8, g: 8, b: 10 }, 900);
        pixels.extend(fill(Rgb { r: 220, g: 90, b: 30 }, 60));
        let colors = extract(&pixels, DARK_SURFACE).unwrap();
        assert!(colors.vibrant.r > 150, "accent came out too dark: {:?}", colors.vibrant);
    }

    #[test]
    fn a_greyscale_cover_still_produces_something() {
        let pixels = fill(Rgb { r: 128, g: 128, b: 128 }, 100);
        let colors = extract(&pixels, DARK_SURFACE).unwrap();
        assert!(colors.vibrant.contrast(DARK_SURFACE) >= MIN_CONTRAST);
    }

    #[test]
    fn the_accent_is_always_legible_on_the_surface() {
        // A dark accent on a dark surface is the case that looks broken.
        for colour in
            [Rgb { r: 20, g: 20, b: 60 }, Rgb { r: 5, g: 40, b: 5 }, Rgb { r: 60, g: 0, b: 0 }]
        {
            let colors = extract(&fill(colour, 50), DARK_SURFACE).unwrap();
            let contrast = colors.vibrant.contrast(DARK_SURFACE);
            assert!(contrast >= MIN_CONTRAST, "{colour:?} gave contrast {contrast}");
        }
    }

    #[test]
    fn contrast_adjustment_follows_the_surface() {
        // The same pale colour must darken on a light surface and lighten on a
        // dark one, rather than always moving the same direction.
        let pale = Rgb { r: 230, g: 220, b: 120 };
        let on_light = extract(&fill(pale, 50), LIGHT_SURFACE).unwrap().vibrant;
        assert!(on_light.contrast(LIGHT_SURFACE) >= MIN_CONTRAST);
        assert!(on_light.luminance() < pale.luminance(), "should have darkened");
    }

    #[test]
    fn foreground_is_readable_on_the_accent() {
        for colour in [Rgb { r: 250, g: 240, b: 200 }, Rgb { r: 20, g: 10, b: 60 }] {
            let colors = extract(&fill(colour, 50), DARK_SURFACE).unwrap();
            assert!(
                colors.on_vibrant.contrast(colors.vibrant) >= 4.5,
                "{:?} on {:?} is not readable",
                colors.on_vibrant,
                colors.vibrant
            );
        }
    }

    #[test]
    fn cache_keys_are_filesystem_safe_and_stable() {
        let key = cache_key("/com/spotify/track/4uLU6hMCjMI75M1A2tKUQC");
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()), "{key}");
        assert_eq!(key, cache_key("/com/spotify/track/4uLU6hMCjMI75M1A2tKUQC"));
        assert_ne!(key, cache_key("/com/spotify/track/other"));
    }

    #[test]
    fn file_urls_are_recognised_without_copying() {
        assert!(local_path("https://i.scdn.co/image/abc").is_none());
        // A path that does not exist is not usable either.
        assert!(local_path("file:///definitely/not/here.png").is_none());
    }
}
