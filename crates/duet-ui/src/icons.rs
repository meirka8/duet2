// SPDX-License-Identifier: MIT
//! T-4.2.6: file-type icons in the table (FR-CFG-04 "icon theme
//! resolution from the XDG icon theme", design.md §9.8 "rasterised once
//! per (icon, size, scale) and cached in a GPU atlas").
//!
//! Three pieces, split by which thread they may run on:
//!
//! - [`IconTables`] -- the shared-mime-info database and the XDG theme
//!   chain (`duet_meta`). Loading them reads a few hundred KB of
//!   `globs2`/`index.theme` files, so it happens on the core Tokio runtime
//!   right after startup and lands in an `Arc<OnceLock>` every
//!   `FileTableDelegate` holds; until it is ready the delegate marks its
//!   rows' icons unresolved and fills them in on the first frame after
//!   ([`FileTableDelegate::resolve_pending_icons`]).
//! - [`rasterise`] -- one icon file to one square of straight-alpha BGRA
//!   pixels at the wanted physical size: SVG through `resvg` (crisp at any
//!   scale factor), PNG through `image` (resampled when the theme has no
//!   exact size). Pure and off-thread; only the resulting bytes cross to
//!   the UI thread.
//! - [`IconCache`] -- a GPUI `Global` mapping `(icon identity, physical
//!   px)` to the `Arc<RenderImage>` the `img` element draws from GPUI's
//!   sprite atlas. **Bounded**: a byte budget ([`DEFAULT_BUDGET_BYTES`],
//!   16 px icons at 2x are 4 KiB each, so the default holds ~2000) with
//!   least-recently-rendered eviction, and every evicted image is handed
//!   to `App::drop_image` so its atlas tiles are released too -- the
//!   "atlas memory bounded and evicted" half of the task's AC. Misses
//!   are cached as well (a theme with no icon for a type is asked once).
//!
//! Per frame the hot path is one `HashMap` probe and one `Arc` clone per
//! visible Name cell -- no allocation, no I/O, no decode -- which is what
//! keeps scrolling inside NFR-05. A miss schedules one load and draws an
//! empty 16 px slot so the text never shifts when the icon arrives.
//!
//! What this deliberately does not do yet: emblems (symlink arrows,
//! unreadable overlays), symlink targets (the listing doesn't follow
//! them, so a symlink gets `inode-symlink`), XPM (no decoder; modern
//! themes ship SVG/PNG), and reacting to a theme change at runtime
//! (`appearance.icon_theme` is read once at startup like every other
//! appearance key). See `documentation/known_issues.md`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use duet_meta::{EntryClass, IconResolver, MimeDb, entry_icon_names};
use gpui::{App, BorrowAppContext as _, Global, RenderImage};

/// Icon edge in logical pixels -- fits the table's 26 px compact rows
/// with room to spare and is the size every theme ships.
pub(crate) const ICON_PX: f32 = 16.0;

/// Default atlas byte budget for [`IconCache`]: 8 MiB.
pub(crate) const DEFAULT_BUDGET_BYTES: usize = 8 * 1024 * 1024;

/// The lookup tables -- see the module doc comment.
pub struct IconTables {
    mime: MimeDb,
    resolver: IconResolver,
}

impl IconTables {
    /// Loads the system database and the theme chain for `theme`
    /// (`None`/`"system"` = detect, see `duet_meta::detect_theme_name`).
    /// File I/O; call off the UI thread.
    pub(crate) fn load(theme: Option<&str>) -> Self {
        let theme = duet_meta::detect_theme_name(theme);
        let data_dirs = mime_data_dirs();
        Self {
            mime: MimeDb::load(&data_dirs),
            resolver: IconResolver::new(&theme, duet_meta::default_base_dirs()),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_parts(mime: MimeDb, resolver: IconResolver) -> Self {
        Self { mime, resolver }
    }

    /// The candidate icon names for a listing entry, best first.
    pub(crate) fn icon_names(&self, file_name: &str, class: EntryClass) -> Vec<String> {
        entry_icon_names(&self.mime, file_name, class)
    }

    pub(crate) fn resolver(&self) -> &IconResolver {
        &self.resolver
    }

    /// See `MimeDb::is_literal`.
    pub(crate) fn is_literal_name(&self, file_name: &str) -> bool {
        self.mime.is_literal(file_name)
    }
}

/// `$XDG_DATA_HOME` then each `$XDG_DATA_DIRS`, where `mime/globs2` lives.
fn mime_data_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local").join("share")))
    {
        dirs.push(home);
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from),
    );
    dirs
}

/// A loaded icon's pixels: `size` x `size`, straight-alpha BGRA (what
/// GPUI's sprite atlas expects, see `gpui::ImageAssetLoader`).
pub(crate) struct Rasterised {
    pub(crate) size: u32,
    pub(crate) bgra: Vec<u8>,
}

impl Rasterised {
    /// Wraps the pixels as the `RenderImage` the `img` element draws.
    pub(crate) fn into_render_image(self) -> Option<RenderImage> {
        let buffer = image::ImageBuffer::from_raw(self.size, self.size, self.bgra)?;
        Some(RenderImage::new(vec![image::Frame::new(buffer)]))
    }
}

/// Renders the icon file at `path` to a `px` x `px` square -- see the
/// module doc comment. `None` for a format this can't decode or a file
/// that fails to parse; the caller caches that as a miss.
pub(crate) fn rasterise(path: &Path, px: u32) -> Option<Rasterised> {
    if px == 0 {
        return None;
    }
    let is_svg = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"));
    if is_svg {
        let bytes = std::fs::read(path).ok()?;
        let tree = usvg::Tree::from_data(&bytes, &usvg::Options::default()).ok()?;
        let size = tree.size();
        let (w, h) = (size.width(), size.height());
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        // Fit the (usually square) drawing inside the square, centred.
        let scale = px as f32 / w.max(h);
        let dx = (px as f32 - w * scale) / 2.0;
        let dy = (px as f32 - h * scale) / 2.0;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(px, px)?;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy),
            &mut pixmap.as_mut(),
        );
        // tiny-skia hands back premultiplied RGBA.
        let mut bgra = pixmap.take();
        for pixel in bgra.chunks_exact_mut(4) {
            premultiplied_rgba_to_straight_bgra(pixel);
        }
        Some(Rasterised { size: px, bgra })
    } else {
        let decoded = image::ImageReader::open(path).ok()?.decode().ok()?;
        let mut rgba = decoded.into_rgba8();
        if rgba.width() != px || rgba.height() != px {
            rgba = image::imageops::resize(&rgba, px, px, image::imageops::FilterType::Lanczos3);
        }
        let mut bgra = rgba.into_raw();
        for pixel in bgra.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        Some(Rasterised { size: px, bgra })
    }
}

/// The same conversion `gpui` applies to its own SVG rasterisations
/// (`gpui::color::swap_rgba_pa_to_bgra`): un-premultiply, then swap to
/// BGRA byte order.
fn premultiplied_rgba_to_straight_bgra(pixel: &mut [u8]) {
    pixel.swap(0, 2);
    let alpha = pixel[3];
    if alpha > 0 && alpha < 255 {
        let a = f32::from(alpha) / 255.0;
        for channel in &mut pixel[..3] {
            *channel = (f32::from(*channel) / a).min(255.0) as u8;
        }
    }
}

/// An icon's identity for the cache: the candidate-name chain joined,
/// which is unique per (MIME type / entry class) and cheap to hash.
pub(crate) type IconId = Arc<str>;

/// Cache key: identity plus physical pixel size (scale factor folded in).
type CacheKey = (IconId, u32);

struct CacheEntry {
    /// `None` is a cached miss: no theme provides this icon.
    image: Option<Arc<RenderImage>>,
    bytes: usize,
    last_used: u64,
}

/// The bounded GPU-side icon cache -- see the module doc comment.
pub struct IconCache {
    tables: Arc<OnceLock<IconTables>>,
    tokio_handle: tokio::runtime::Handle,
    enabled: bool,
    budget_bytes: usize,
    used_bytes: usize,
    entries: HashMap<CacheKey, CacheEntry>,
    /// Keys whose load is in flight, so a miss is scheduled once, not
    /// once per frame until it lands.
    pending: std::collections::HashSet<CacheKey>,
    /// Monotonic "frame-ish" clock for LRU: bumped per touch.
    tick: u64,
    /// Images evicted by [`Self::insert`], waiting for `App::drop_image`
    /// (the cache has no `App` of its own; see [`Self::take_evicted`]).
    evicted: Vec<Arc<RenderImage>>,
}

impl Global for IconCache {}

impl IconCache {
    /// Creates the cache with the tables still loading (`tables` is
    /// filled by the caller's background task).
    fn new(
        tables: Arc<OnceLock<IconTables>>,
        tokio_handle: tokio::runtime::Handle,
        enabled: bool,
        budget_bytes: usize,
    ) -> Self {
        Self {
            tables,
            tokio_handle,
            enabled,
            budget_bytes,
            used_bytes: 0,
            entries: HashMap::new(),
            pending: std::collections::HashSet::new(),
            tick: 0,
            evicted: Vec::new(),
        }
    }

    /// Installs the global for the running app: the tables load on the
    /// Tokio runtime and every window is refreshed once they are in, so
    /// rows listed before that get their icons on the next frame.
    /// `enabled = false` (`appearance.show_icons = false`) installs a
    /// cache that never loads anything and lets every table skip the
    /// icon slot entirely.
    pub fn install(
        cx: &mut App,
        tokio_handle: tokio::runtime::Handle,
        theme: Option<String>,
        enabled: bool,
    ) {
        let tables: Arc<OnceLock<IconTables>> = Arc::new(OnceLock::new());
        cx.set_global(Self::new(
            tables.clone(),
            tokio_handle.clone(),
            enabled,
            DEFAULT_BUDGET_BYTES,
        ));
        if !enabled {
            return;
        }
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio_handle.spawn(async move {
            let loaded = IconTables::load(theme.as_deref());
            tracing::info!(
                target: "duet_ui::icons",
                chain = ?loaded.resolver.chain(),
                mime_rules = !loaded.mime.is_empty(),
                "icon tables loaded"
            );
            let _ = tables.set(loaded);
            let _ = tx.send(());
        });
        cx.spawn(async move |cx| {
            let _ = rx.await;
            let _ = cx.update(|cx| cx.refresh_windows());
        })
        .detach();
    }

    /// Test seam: a cache whose tables are already loaded, with an
    /// explicit budget.
    #[cfg(test)]
    pub(crate) fn with_tables(
        tokio_handle: tokio::runtime::Handle,
        tables: IconTables,
        budget_bytes: usize,
    ) -> Self {
        let cell = Arc::new(OnceLock::new());
        let _ = cell.set(tables);
        Self::new(cell, tokio_handle, true, budget_bytes)
    }

    /// The tables handle every delegate keeps (see the module doc
    /// comment); `None` while icons are disabled.
    pub fn tables(&self) -> Option<Arc<OnceLock<IconTables>>> {
        self.enabled.then(|| self.tables.clone())
    }

    #[cfg(test)]
    pub(crate) fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Entries holding an image (misses excluded).
    #[cfg(test)]
    pub(crate) fn image_count(&self) -> usize {
        self.entries.values().filter(|e| e.image.is_some()).count()
    }

    /// Whether an image (not a miss) is cached for `id` at any size.
    #[cfg(test)]
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.entries
            .iter()
            .any(|((key_id, _), entry)| key_id.as_ref() == id && entry.image.is_some())
    }

    /// The image for `id` at `px` physical pixels if it is cached; on a
    /// first miss, schedules the load (`names` are the theme candidates,
    /// `logical_px`/`scale` what to ask the theme for) and returns `None`
    /// -- the caller draws an empty slot this frame and the windows are
    /// refreshed when the pixels land. A cached miss returns `None` for
    /// good.
    pub(crate) fn get_or_load(
        &mut self,
        id: &IconId,
        names: &Arc<Vec<String>>,
        logical_px: u32,
        scale: u32,
        cx: &mut App,
    ) -> Option<Arc<RenderImage>> {
        if !self.enabled {
            return None;
        }
        let px = logical_px * scale;
        let key = (id.clone(), px);
        self.tick += 1;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.tick;
            return entry.image.clone();
        }
        if self.pending.contains(&key) {
            return None;
        }
        // Tables still loading: nothing to look up yet, and the refresh
        // that follows their arrival re-runs this.
        self.tables.get()?;
        self.pending.insert(key.clone());
        let tables = self.tables.clone();
        let names = names.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<Rasterised>>();
        self.tokio_handle.spawn(async move {
            let result = tables.get().and_then(|tables| {
                let path = tables.resolver().locate_first(&names, logical_px, scale)?;
                rasterise(&path, px)
            });
            let _ = tx.send(result);
        });
        cx.spawn(async move |cx| {
            let result = rx.await.unwrap_or(None);
            let _ = cx.update(|cx| {
                let image = result.and_then(Rasterised::into_render_image).map(Arc::new);
                cx.update_global::<IconCache, ()>(|cache, cx| {
                    cache.insert(key, image);
                    // Evicted images must leave the sprite atlas too.
                    for image in cache.take_evicted() {
                        cx.drop_image(image, None);
                    }
                });
                cx.refresh_windows();
            });
        })
        .detach();
        None
    }

    /// Records a finished load (`None` = miss) and evicts least-recently
    /// used entries until the budget holds again -- never the entry just
    /// inserted, which is about to be drawn; the evicted images are
    /// queued for [`Self::take_evicted`].
    fn insert(&mut self, key: CacheKey, image: Option<Arc<RenderImage>>) {
        self.pending.remove(&key);
        let bytes = image
            .as_ref()
            .map_or(0, |_| (key.1 as usize) * (key.1 as usize) * 4);
        self.tick += 1;
        if let Some(old) = self.entries.insert(
            key.clone(),
            CacheEntry {
                image,
                bytes,
                last_used: self.tick,
            },
        ) {
            self.used_bytes -= old.bytes;
            if let Some(image) = old.image {
                self.evicted.push(image);
            }
        }
        self.used_bytes += bytes;
        while self.used_bytes > self.budget_bytes {
            let Some(victim) = self
                .entries
                .iter()
                .filter(|(k, e)| e.image.is_some() && **k != key)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                self.used_bytes -= entry.bytes;
                if let Some(image) = entry.image {
                    self.evicted.push(image);
                }
            }
        }
    }

    /// Images evicted since the last call, for `App::drop_image`.
    fn take_evicted(&mut self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut self.evicted)
    }

    /// Test seam for the eviction rule without a theme or a runtime.
    #[cfg(test)]
    pub(crate) fn insert_for_test(
        &mut self,
        id: &str,
        px: u32,
        image: Option<Arc<RenderImage>>,
    ) -> Vec<Arc<RenderImage>> {
        self.insert((Arc::from(id), px), image);
        self.take_evicted()
    }

    /// Test seam: marks `id` at `px` as most recently used.
    #[cfg(test)]
    pub(crate) fn touch_for_test(&mut self, id: &str, px: u32) {
        self.tick += 1;
        if let Some(entry) = self.entries.get_mut(&(Arc::from(id), px)) {
            entry.last_used = self.tick;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    fn fixture_tables() -> IconTables {
        let mut mime = MimeDb::default();
        mime.parse_globs2("50:text/plain:*.txt\n");
        IconTables::from_parts(mime, IconResolver::new("none", Vec::new()))
    }

    fn image(px: u32) -> Arc<RenderImage> {
        Arc::new(
            Rasterised {
                size: px,
                bgra: vec![0; (px * px * 4) as usize],
            }
            .into_render_image()
            .unwrap(),
        )
    }

    #[test]
    fn rasterise_renders_svg_and_png_to_straight_bgra_squares() {
        let dir = tempfile::tempdir().unwrap();
        let svg = dir.path().join("red.svg");
        std::fs::write(
            &svg,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#ff0000"/></svg>"##,
        )
        .unwrap();
        let out = rasterise(&svg, 32).expect("svg renders");
        assert_eq!(out.size, 32);
        assert_eq!(out.bgra.len(), 32 * 32 * 4);
        assert_eq!(&out.bgra[..4], &[0, 0, 255, 255], "opaque red as BGRA");

        // Half-transparent green: premultiplied by tiny-skia, must come
        // back straight (G = 255, not 128).
        let half = dir.path().join("half.svg");
        std::fs::write(
            &half,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#00ff00" fill-opacity="0.5"/></svg>"##,
        )
        .unwrap();
        let out = rasterise(&half, 4).unwrap();
        assert_eq!(out.bgra[3], 128);
        assert!(
            out.bgra[1] >= 250,
            "green un-premultiplied, got {}",
            out.bgra[1]
        );

        // PNG at the wrong size is resampled to the requested square.
        let png = dir.path().join("blue.png");
        let mut buffer = image::RgbaImage::new(8, 8);
        for pixel in buffer.pixels_mut() {
            *pixel = image::Rgba([0, 0, 255, 255]);
        }
        buffer.save(&png).unwrap();
        let out = rasterise(&png, 16).unwrap();
        assert_eq!(out.size, 16);
        assert_eq!(&out.bgra[..4], &[255, 0, 0, 255], "opaque blue as BGRA");

        assert!(rasterise(&dir.path().join("missing.svg"), 16).is_none());
        let xpm = dir.path().join("old.xpm");
        std::fs::write(&xpm, b"/* XPM */").unwrap();
        assert!(
            rasterise(&xpm, 16).is_none(),
            "no XPM decoder, a clean miss"
        );
        assert!(rasterise(&svg, 0).is_none());
    }

    #[test]
    fn cache_evicts_least_recently_used_within_the_byte_budget_and_hands_images_back() {
        let rt = runtime();
        // Budget for exactly two 16 px icons (1 KiB each).
        let mut cache =
            IconCache::with_tables(rt.handle().clone(), fixture_tables(), 2 * 16 * 16 * 4);
        assert!(cache.insert_for_test("a", 16, Some(image(16))).is_empty());
        assert!(cache.insert_for_test("b", 16, Some(image(16))).is_empty());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.used_bytes(), 2 * 1024);

        // A miss costs nothing and never evicts.
        assert!(cache.insert_for_test("miss", 16, None).is_empty());
        assert_eq!(cache.len(), 3);

        // "a" is older than "b"; touching it makes "b" the victim.
        cache.touch_for_test("a", 16);
        let evicted = cache.insert_for_test("c", 16, Some(image(16)));
        assert_eq!(evicted.len(), 1, "one image over budget, one evicted");
        assert!(cache.contains("a"));
        assert!(!cache.contains("b"), "least recently used went");
        assert!(cache.contains("c"));
        assert_eq!(cache.used_bytes(), 2 * 1024);

        // Replacing an entry returns the old image for dropping.
        let evicted = cache.insert_for_test("c", 16, Some(image(16)));
        assert_eq!(evicted.len(), 1);
        assert_eq!(cache.used_bytes(), 2 * 1024);

        // An entry larger than the whole budget still lands (the newest
        // is never the victim) and pushes everything else out.
        let evicted = cache.insert_for_test("big", 64, Some(image(64)));
        assert_eq!(evicted.len(), 2);
        assert!(cache.contains("big"));
        assert_eq!(cache.used_bytes(), 64 * 64 * 4);
    }

    #[test]
    fn tables_give_candidate_names_per_entry_class() {
        let tables = fixture_tables();
        assert_eq!(
            tables.icon_names("a.txt", EntryClass::File),
            ["text-plain", "text-x-generic", "unknown"]
        );
        assert_eq!(tables.icon_names("d", EntryClass::Directory)[0], "folder");
        assert!(tables.resolver().is_empty());
    }
}
