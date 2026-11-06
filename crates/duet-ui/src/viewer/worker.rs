// SPDX-License-Identifier: MIT
//! The viewer's I/O thread (T-5.3.6): everything that touches the file
//! runs here, never on the UI thread. The entity sends [`Request`]s
//! down a channel and receives [`Reply`]s on another; page requests
//! coalesce (only the newest matters while the user holds a key),
//! searches never drop.
//!
//! A second thread builds the sparse line index (`index.rs`) for text
//! files and reports progress through the same reply channel; it stops
//! when the handle is dropped (the viewer closed or moved on).
//!
//! Both are plain named threads rather than `spawn_blocking` tasks on
//! the core Tokio runtime, deliberately: the serve loop lives as long as
//! the viewer, and `tokio::runtime::Runtime`'s `Drop` waits for every
//! blocking task to finish -- a test that drops its runtime before the
//! GPUI entities (every `#[gpui::test]` does) would then hang forever
//! on a loop that is waiting for those very entities to drop its
//! sender. The UI-thread rule is what matters here, and these threads
//! honour it exactly as the command line's reader threads do.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use gpui::RenderImage;

use super::hex::{self, HexLayout, HexRow};
use super::index::{self, LineIndex};
use super::search::{self, SearchSpec};
use super::source::{ByteSource, FileSource};
use super::text::{self, Detected, HEAD_BYTES, Page, Pager, Position, TextEncoding, Walker};

/// Where a text page should start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Anchor {
    /// Exactly here (a re-render after a resize or a mode change).
    At(Position),
    /// `delta` visual rows from `from`, clamped to the file.
    Scroll {
        from: Position,
        delta: i64,
    },
    Start,
    End,
    /// The row containing this byte.
    Offset(u64),
    /// 0-based line; waits briefly for the index to reach it.
    Line(u64),
    /// 0.0 ..= 100.0.
    Percent(f64),
}

#[derive(Debug)]
pub(crate) enum Request {
    TextPage {
        anchor: Anchor,
        rows: usize,
        /// `None`: no wrapping.
        cols: Option<usize>,
        encoding: Option<TextEncoding>,
        highlight: Option<Range<u64>>,
    },
    HexPage {
        first_row: u64,
        rows: usize,
        layout: HexLayout,
        highlight: Option<Range<u64>>,
    },
    Image,
    Search {
        query: String,
        from: u64,
        backward: bool,
        fold_case: bool,
        encoding: Option<TextEncoding>,
    },
}

pub(crate) enum Reply {
    Opened {
        len: u64,
        detected: Detected,
        /// `duet_meta::sniff` on the head.
        mime: Option<&'static str>,
    },
    Text {
        page: Page,
        encoding: TextEncoding,
        /// 0-based line of the top row, when indexed.
        line: Option<u64>,
        total_lines: Option<u64>,
    },
    Hex {
        rows: Vec<HexRow>,
        first_row: u64,
        row_count: u64,
    },
    Image {
        image: Arc<RenderImage>,
        width: u32,
        height: u32,
    },
    SearchHit(Range<u64>),
    SearchMiss,
    SearchProgress(f32),
    IndexProgress {
        fraction: f32,
        complete: bool,
    },
    /// Human-readable; the viewer shows it in its status line.
    Error(String),
}

/// The entity's end: dropping it ends both blocking tasks.
pub(crate) struct WorkerHandle {
    requests: mpsc::Sender<Request>,
    stop: Arc<AtomicBool>,
    /// Set to abandon the search in flight (Esc).
    pub(crate) search_cancel: Arc<AtomicBool>,
}

impl WorkerHandle {
    pub(crate) fn send(&self, request: Request) {
        let _ = self.requests.send(request);
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.search_cancel.store(true, Ordering::Relaxed);
    }
}

pub(crate) type ReplySender = tokio::sync::mpsc::UnboundedSender<Reply>;

/// Opens `path` on its own thread and serves requests until the handle
/// is dropped. `build_index`: scan the file for line numbers in the
/// background (the F3 viewer); the quick view passes `false` so a large
/// file under the cursor costs one page, not a full read.
pub(crate) fn spawn(path: PathBuf, replies: ReplySender, build_index: bool) -> WorkerHandle {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let search_cancel = Arc::new(AtomicBool::new(false));
    let handle = WorkerHandle {
        requests: tx,
        stop: stop.clone(),
        search_cancel: search_cancel.clone(),
    };
    let spawned = std::thread::Builder::new()
        .name("duet-viewer".to_string())
        .spawn(move || {
            serve(&path, rx, replies, stop, search_cancel, build_index);
        });
    if let Err(e) = spawned {
        tracing::error!(target: "duet_ui::viewer", error = %e, "viewer thread failed to start");
    }
    handle
}

struct Worker {
    path: PathBuf,
    src: FileSource,
    detected: Detected,
    index: Option<Arc<Mutex<LineIndex>>>,
    replies: ReplySender,
    search_cancel: Arc<AtomicBool>,
}

fn serve(
    path: &Path,
    rx: mpsc::Receiver<Request>,
    replies: ReplySender,
    stop: Arc<AtomicBool>,
    search_cancel: Arc<AtomicBool>,
    build_index: bool,
) {
    let src = match FileSource::open(path) {
        Ok(src) => src,
        Err(e) => {
            let _ = replies.send(Reply::Error(format!("Can't open: {e}")));
            return;
        }
    };
    let mut src = src;
    let head = match src.read(0, HEAD_BYTES) {
        Ok(h) => h,
        Err(e) => {
            let _ = replies.send(Reply::Error(format!("Can't read: {e}")));
            return;
        }
    };
    let detected = text::detect(&head);
    let mime = duet_meta::sniff(&head);
    let len = src.len();
    if replies
        .send(Reply::Opened {
            len,
            detected,
            mime,
        })
        .is_err()
    {
        return;
    }

    // The line index, for text that is worth counting.
    let is_image = mime.is_some_and(|m| m.starts_with("image/"));
    let index = (build_index && !detected.binary && !is_image).then(|| {
        let index = Arc::new(Mutex::new(LineIndex::new(detected.bom, len)));
        let builder_index = index.clone();
        let builder_path = path.to_path_buf();
        let builder_replies = replies.clone();
        let builder_stop = stop.clone();
        let enc = detected.encoding;
        let builder = std::thread::Builder::new()
            .name("duet-viewer-index".to_string())
            .spawn(move || {
                let Ok(mut src) = FileSource::open(&builder_path) else {
                    return;
                };
                let mut last_report = Instant::now();
                let result = index::build(&mut src, enc, &builder_index, &builder_stop, || {
                    if last_report.elapsed() >= Duration::from_millis(100) {
                        last_report = Instant::now();
                        let fraction = builder_index
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .progress();
                        let _ = builder_replies.send(Reply::IndexProgress {
                            fraction,
                            complete: false,
                        });
                    }
                });
                if result.is_ok() && !builder_stop.load(Ordering::Relaxed) {
                    let _ = builder_replies.send(Reply::IndexProgress {
                        fraction: 1.0,
                        complete: true,
                    });
                }
            });
        if let Err(e) = builder {
            tracing::error!(target: "duet_ui::viewer", error = %e, "index thread failed to start");
        }
        index
    });

    let mut worker = Worker {
        path: path.to_path_buf(),
        src,
        detected,
        index,
        replies,
        search_cancel,
    };
    while let Ok(first) = rx.recv() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Coalesce: the newest page request wins; searches and image
        // loads are kept, in order, and served first.
        let mut page: Option<Request> = None;
        let mut others = Vec::new();
        let mut queue = vec![first];
        while let Ok(more) = rx.try_recv() {
            queue.push(more);
        }
        for request in queue {
            match request {
                Request::TextPage { .. } | Request::HexPage { .. } => page = Some(request),
                other => others.push(other),
            }
        }
        for request in others.into_iter().chain(page) {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let reply = worker.handle(request);
            if worker.replies.send(reply).is_err() {
                return;
            }
        }
    }
}

impl Worker {
    fn pager(&self, encoding: Option<TextEncoding>) -> Pager {
        let mut pager = Pager::new(self.detected);
        if let Some(enc) = encoding
            && enc != self.detected.encoding
        {
            pager.enc = enc;
            // A manual override keeps a real BOM skipped only when it
            // belongs to the chosen encoding.
            pager.bom = 0;
        }
        pager
    }

    fn handle(&mut self, request: Request) -> Reply {
        let result = match request {
            Request::TextPage {
                anchor,
                rows,
                cols,
                encoding,
                highlight,
            } => self.text_page(anchor, rows, cols, encoding, highlight),
            Request::HexPage {
                first_row,
                rows,
                layout,
                highlight,
            } => self.hex_page(first_row, rows, layout, highlight),
            Request::Image => self.image(),
            Request::Search {
                query,
                from,
                backward,
                fold_case,
                encoding,
            } => self.search(&query, from, backward, fold_case, encoding),
        };
        result.unwrap_or_else(|e| Reply::Error(e.to_string()))
    }

    fn text_page(
        &mut self,
        anchor: Anchor,
        rows: usize,
        cols: Option<usize>,
        encoding: Option<TextEncoding>,
        highlight: Option<Range<u64>>,
    ) -> std::io::Result<Reply> {
        let pager = self.pager(encoding);
        let rows = rows.max(1);
        let len = self.src.len();
        let line_target = match anchor {
            Anchor::Line(line) => Some(line),
            _ => None,
        };
        let line_offset = match line_target {
            Some(line) => Some(self.wait_for_line(pager.enc, line)?),
            None => None,
        };
        let mut walker = Walker::new(pager, &mut self.src, cols, highlight);
        let top = match anchor {
            Anchor::At(pos) => pos,
            Anchor::Scroll { from, delta } => walker.scroll(from, delta, rows)?,
            Anchor::Start => walker.start(),
            Anchor::End => walker.end_top(rows)?,
            Anchor::Offset(offset) => walker.position_at(offset)?,
            Anchor::Line(_) => match line_offset.flatten() {
                Some(offset) => walker.position_at(offset)?,
                None => {
                    return Ok(Reply::Error(
                        "That line isn't indexed yet (or doesn't exist)".to_string(),
                    ));
                }
            },
            Anchor::Percent(pct) => {
                let offset = ((len as f64) * (pct.clamp(0.0, 100.0) / 100.0)) as u64;
                let pos = walker.position_at(offset.min(len.saturating_sub(1)))?;
                let floor = walker.end_top(rows)?;
                pos.min(floor)
            }
        };
        // Clamp a stale `At`/`Scroll` position after a resize.
        let floor = walker.end_top(rows)?;
        let top = if top > floor { floor } else { top };
        let page = walker.page(top, rows)?;
        let (line, total_lines) = match &self.index {
            Some(index) => {
                let total = index
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .total_lines();
                let line = if encoding.is_some_and(|e| e != self.detected.encoding) {
                    None
                } else {
                    index::line_of_offset(&mut self.src, pager.enc, index, top.row_start)?
                };
                (line, total)
            }
            None => (None, None),
        };
        Ok(Reply::Text {
            page,
            encoding: pager.enc,
            line,
            total_lines,
        })
    }

    /// Waits up to a second for the index to reach `line`.
    fn wait_for_line(&mut self, enc: TextEncoding, line: u64) -> std::io::Result<Option<u64>> {
        let Some(index) = self.index.clone() else {
            return Ok(None);
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(offset) = index::offset_of_line(&mut self.src, enc, &index, line)? {
                return Ok(Some(offset));
            }
            let complete = index.lock().unwrap_or_else(|e| e.into_inner()).complete;
            if complete || Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn hex_page(
        &mut self,
        first_row: u64,
        rows: usize,
        layout: HexLayout,
        highlight: Option<Range<u64>>,
    ) -> std::io::Result<Reply> {
        let row_count = hex::row_count(self.src.len(), layout.width);
        let floor = row_count.saturating_sub(rows as u64);
        let first_row = first_row.min(floor);
        let rows = hex::rows(&mut self.src, layout, first_row, rows, highlight)?;
        Ok(Reply::Hex {
            rows,
            first_row,
            row_count,
        })
    }

    fn search(
        &mut self,
        query: &str,
        from: u64,
        backward: bool,
        fold_case: bool,
        encoding: Option<TextEncoding>,
    ) -> std::io::Result<Reply> {
        let pager = self.pager(encoding);
        let spec = SearchSpec {
            needle: pager.enc.encode(query),
            fold_case,
            unit: pager.enc.unit(),
            bom: pager.bom,
        };
        if spec.needle.is_empty() {
            return Ok(Reply::SearchMiss);
        }
        self.search_cancel.store(false, Ordering::Relaxed);
        let replies = self.replies.clone();
        let mut last = Instant::now();
        let mut progress = |fraction: f32| {
            if last.elapsed() >= Duration::from_millis(100) {
                last = Instant::now();
                let _ = replies.send(Reply::SearchProgress(fraction));
            }
        };
        let hit = if backward {
            search::search_backward(
                &mut self.src,
                &spec,
                from,
                &self.search_cancel,
                &mut progress,
            )?
        } else {
            search::search_forward(
                &mut self.src,
                &spec,
                from,
                &self.search_cancel,
                &mut progress,
            )?
        };
        Ok(match hit {
            Some(offset) => Reply::SearchHit(offset..offset + spec.needle.len() as u64),
            None => Reply::SearchMiss,
        })
    }

    fn image(&mut self) -> std::io::Result<Reply> {
        const MAX_BYTES: u64 = 256 << 20;
        const MAX_SIDE: u32 = 4096;
        if self.src.len() > MAX_BYTES {
            return Ok(Reply::Error("Image is larger than 256 MiB".to_string()));
        }
        let bytes = std::fs::read(&self.path)?;
        let is_svg = self
            .path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"))
            || bytes.starts_with(b"<svg")
            || bytes.starts_with(b"<?xml");
        let (bgra, width, height) = if is_svg {
            let Ok(tree) = usvg::Tree::from_data(&bytes, &usvg::Options::default()) else {
                return Ok(Reply::Error("Not a valid SVG".to_string()));
            };
            let size = tree.size();
            let (w, h) = (size.width(), size.height());
            if w <= 0.0 || h <= 0.0 {
                return Ok(Reply::Error("Empty SVG".to_string()));
            }
            let scale = (MAX_SIDE as f32 / w.max(h)).min(1.0);
            let (pw, ph) = ((w * scale).ceil() as u32, (h * scale).ceil() as u32);
            let Some(mut pixmap) = resvg::tiny_skia::Pixmap::new(pw.max(1), ph.max(1)) else {
                return Ok(Reply::Error("SVG too large".to_string()));
            };
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(scale, scale),
                &mut pixmap.as_mut(),
            );
            let mut bgra = pixmap.take();
            for pixel in bgra.chunks_exact_mut(4) {
                pixel.swap(0, 2);
                let alpha = pixel[3];
                if alpha > 0 && alpha < 255 {
                    let a = f32::from(alpha) / 255.0;
                    for channel in &mut pixel[..3] {
                        *channel = (f32::from(*channel) / a).min(255.0) as u8;
                    }
                }
            }
            (bgra, pw.max(1), ph.max(1))
        } else {
            let reader =
                image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
            let decoded = match reader.decode() {
                Ok(d) => d,
                Err(e) => return Ok(Reply::Error(format!("Can't decode image: {e}"))),
            };
            let mut rgba = decoded.into_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            if w > MAX_SIDE || h > MAX_SIDE {
                let scale = MAX_SIDE as f32 / w.max(h) as f32;
                rgba = image::imageops::resize(
                    &rgba,
                    ((w as f32 * scale) as u32).max(1),
                    ((h as f32 * scale) as u32).max(1),
                    image::imageops::FilterType::Triangle,
                );
            }
            let (w, h) = (rgba.width(), rgba.height());
            let mut bgra = rgba.into_raw();
            for pixel in bgra.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            (bgra, w, h)
        };
        let Some(buffer) = image::ImageBuffer::from_raw(width, height, bgra) else {
            return Ok(Reply::Error("Image buffer mismatch".to_string()));
        };
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        Ok(Reply::Image {
            image,
            width,
            height,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};
    use std::time::Duration;

    use tokio::sync::mpsc::UnboundedReceiver;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn open(rt: &tokio::runtime::Runtime, path: &Path) -> (WorkerHandle, UnboundedReceiver<Reply>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = rt;
        let handle = spawn(path.to_path_buf(), tx, true);
        (handle, rx)
    }

    /// The next reply that isn't index progress.
    fn recv(rt: &tokio::runtime::Runtime, rx: &mut UnboundedReceiver<Reply>, secs: u64) -> Reply {
        loop {
            let reply = rt
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(secs), rx.recv()).await
                })
                .expect("a reply within the deadline")
                .expect("worker alive");
            match reply {
                Reply::IndexProgress { .. } | Reply::SearchProgress(_) => continue,
                other => return other,
            }
        }
    }

    fn text_rows(reply: Reply) -> Vec<String> {
        match reply {
            Reply::Text { page, .. } => page.rows.into_iter().map(|r| r.text).collect(),
            Reply::Error(e) => panic!("worker error: {e}"),
            _ => panic!("expected a text page"),
        }
    }

    fn page(anchor: Anchor, rows: usize, cols: Option<usize>) -> Request {
        Request::TextPage {
            anchor,
            rows,
            cols,
            encoding: None,
            highlight: None,
        }
    }

    /// A file under `/dev/shm`, removed on drop (also on panic).
    struct ShmFile(PathBuf);

    impl ShmFile {
        fn create(name: &str) -> Option<(Self, std::fs::File)> {
            let dir = Path::new("/dev/shm");
            if !dir.is_dir() {
                return None;
            }
            let path = dir.join(format!("duet-viewer-{}-{name}", std::process::id()));
            let file = std::fs::File::create(&path).ok()?;
            Some((Self(path), file))
        }
    }

    impl Drop for ShmFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn opens_pages_searches_and_decodes_images() {
        let rt = runtime();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.txt");
        let body: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &body).unwrap();
        let (handle, mut rx) = open(&rt, &path);
        match recv(&rt, &mut rx, 5) {
            Reply::Opened { len, detected, .. } => {
                assert_eq!(len, body.len() as u64);
                assert_eq!(detected.encoding.name(), "UTF-8");
                assert!(!detected.binary);
            }
            _ => panic!("expected Opened first"),
        }
        handle.send(page(Anchor::Start, 10, None));
        let rows = text_rows(recv(&rt, &mut rx, 5));
        assert_eq!(rows.len(), 10);
        assert_eq!((rows[0].as_str(), rows[9].as_str()), ("line 1", "line 10"));
        handle.send(page(Anchor::End, 10, None));
        let rows = text_rows(recv(&rt, &mut rx, 5));
        assert_eq!(
            (rows[0].as_str(), rows[9].as_str()),
            ("line 191", "line 200")
        );
        let top = Position {
            row_start: 0,
            piece: 0,
        };
        handle.send(page(
            Anchor::Scroll {
                from: top,
                delta: 5,
            },
            10,
            None,
        ));
        assert_eq!(text_rows(recv(&rt, &mut rx, 5))[0], "line 6");
        handle.send(page(Anchor::Line(49), 10, None));
        match recv(&rt, &mut rx, 5) {
            Reply::Text { page, line, .. } => {
                assert_eq!(page.rows[0].text, "line 50");
                assert_eq!(line, Some(49), "the index resolves the top row's line");
            }
            _ => panic!("expected a text page"),
        }
        handle.send(page(Anchor::Percent(50.0), 10, None));
        let rows = text_rows(recv(&rt, &mut rx, 5));
        assert!(rows[0].starts_with("line 1"), "{rows:?}");
        // Wrapped at 4 columns, "line 1" is two pieces.
        handle.send(page(Anchor::Start, 3, Some(4)));
        assert_eq!(text_rows(recv(&rt, &mut rx, 5)), ["line", " 1", "line"]);

        handle.send(Request::HexPage {
            first_row: 0,
            rows: 2,
            layout: HexLayout {
                width: 16,
                decimal: false,
                chars_only: false,
            },
            highlight: None,
        });
        match recv(&rt, &mut rx, 5) {
            Reply::Hex {
                rows,
                first_row,
                row_count,
            } => {
                assert_eq!(first_row, 0);
                assert_eq!(row_count, (body.len() as u64).div_ceil(16));
                assert!(
                    rows[0].text.starts_with("00000000  6C 69 6E 65 20 31 0A"),
                    "{}",
                    rows[0].text
                );
            }
            _ => panic!("expected a hex page"),
        }

        let needle_at = body.find("line 150\n").unwrap() as u64;
        handle.send(Request::Search {
            query: "LINE 150".to_string(),
            from: 0,
            backward: false,
            fold_case: true,
            encoding: None,
        });
        match recv(&rt, &mut rx, 5) {
            Reply::SearchHit(range) => assert_eq!(range, needle_at..needle_at + 8),
            _ => panic!("expected a hit"),
        }
        handle.send(Request::Search {
            query: "line 150".to_string(),
            from: needle_at,
            backward: true,
            fold_case: false,
            encoding: None,
        });
        assert!(matches!(recv(&rt, &mut rx, 5), Reply::SearchMiss));
        handle.send(Request::Image);
        assert!(matches!(recv(&rt, &mut rx, 5), Reply::Error(_)));
        drop(handle);

        // A real PNG decodes to a RenderImage of its size.
        let png = dir.path().join("p.png");
        let mut buffer = image::RgbaImage::new(3, 2);
        buffer.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        buffer.save(&png).unwrap();
        let (handle, mut rx) = open(&rt, &png);
        match recv(&rt, &mut rx, 5) {
            Reply::Opened { mime, .. } => assert_eq!(mime, Some("image/png")),
            _ => panic!("expected Opened"),
        }
        handle.send(Request::Image);
        match recv(&rt, &mut rx, 5) {
            Reply::Image { width, height, .. } => assert_eq!((width, height), (3, 2)),
            Reply::Error(e) => panic!("{e}"),
            _ => panic!("expected an image"),
        }
    }

    /// T-5.3.6 AC: "A 5 GB log opens in <= 300 ms and scrolls to the end
    /// instantly." A sparse file on tmpfs: real lines at both ends, a
    /// hole in the middle, so the test costs no disk and no RAM.
    #[test]
    fn five_gb_log_opens_and_reaches_the_end_within_the_budget() {
        const SIZE: u64 = 5 << 30;
        let Some((shm, mut file)) = ShmFile::create("5g.log") else {
            eprintln!("no /dev/shm; skipping");
            return;
        };
        let head: String = (1..=1000).map(|i| format!("start {i}\n")).collect();
        let tail: String = (1..=1000).map(|i| format!("tail {i}\n")).collect();
        if file.write_all(head.as_bytes()).is_err()
            || file.set_len(SIZE).is_err()
            || file
                .seek(SeekFrom::Start(SIZE - tail.len() as u64))
                .is_err()
            || file.write_all(tail.as_bytes()).is_err()
            || file.sync_all().is_err()
        {
            eprintln!("/dev/shm can't hold a sparse 5 GB file here; skipping");
            return;
        }
        drop(file);

        let rt = runtime();
        let started = Instant::now();
        let (handle, mut rx) = open(&rt, &shm.0);
        match recv(&rt, &mut rx, 10) {
            Reply::Opened { len, .. } => assert_eq!(len, SIZE),
            _ => panic!("expected Opened"),
        }
        handle.send(page(Anchor::Start, 40, None));
        let rows = text_rows(recv(&rt, &mut rx, 10));
        let opened_in = started.elapsed();
        assert_eq!(rows[0], "start 1");

        let end_started = Instant::now();
        handle.send(page(Anchor::End, 40, None));
        let rows = text_rows(recv(&rt, &mut rx, 10));
        let end_in = end_started.elapsed();
        assert_eq!(rows.last().map(String::as_str), Some("tail 1000"));
        assert_eq!(rows[0], "tail 961");

        let mid_started = Instant::now();
        handle.send(page(Anchor::Percent(50.0), 40, None));
        let rows = text_rows(recv(&rt, &mut rx, 10));
        let mid_in = mid_started.elapsed();
        assert_eq!(rows.len(), 40);
        assert!(
            rows[0].chars().all(|c| c == '\u{2400}'),
            "the hole shows as NULs"
        );
        assert_eq!(rows[0].chars().count(), 4096, "a 4 KiB chop per row");

        eprintln!("5 GB: first page {opened_in:?}, End {end_in:?}, 50% {mid_in:?} (debug build)");
        let budget = Duration::from_millis(300);
        assert!(opened_in <= budget, "first page took {opened_in:?}");
        assert!(end_in <= budget, "End took {end_in:?}");
        assert!(mid_in <= budget, "50% took {mid_in:?}");
    }

    /// T-5.3.6 AC: "a minified 200 MB single-line JSON does not hang".
    #[test]
    fn a_200_mb_single_line_json_pages_wraps_and_searches() {
        const SIZE: usize = 200 << 20;
        let Some((shm, mut file)) = ShmFile::create("200m.json") else {
            eprintln!("no /dev/shm; skipping");
            return;
        };
        let unit = br#"{"key":"value","n":123456},"#;
        let mut block = Vec::with_capacity(1 << 20);
        while block.len() + unit.len() <= 1 << 20 {
            block.extend_from_slice(unit);
        }
        let mut written = 0usize;
        let needle_block = 199;
        let mut block_ix = 0;
        let mut needle_at = 0u64;
        let mut ok = true;
        while written < SIZE {
            let mut chunk = block.clone();
            if block_ix == needle_block {
                needle_at = written as u64 + 1000;
                chunk[1000..1006].copy_from_slice(b"NEEDLE");
            }
            if file.write_all(&chunk).is_err() {
                ok = false;
                break;
            }
            written += chunk.len();
            block_ix += 1;
        }
        if !ok || file.sync_all().is_err() {
            eprintln!("/dev/shm can't hold 200 MB here; skipping");
            return;
        }
        drop(file);

        let rt = runtime();
        let started = Instant::now();
        let (handle, mut rx) = open(&rt, &shm.0);
        let len = match recv(&rt, &mut rx, 10) {
            Reply::Opened { len, .. } => len,
            _ => panic!("expected Opened"),
        };
        handle.send(page(Anchor::Start, 30, None));
        let rows = text_rows(recv(&rt, &mut rx, 10));
        let opened_in = started.elapsed();
        assert_eq!(rows.len(), 30);
        assert_eq!(rows[0].len(), 4096, "the line is chopped into 4 KiB rows");

        let end_started = Instant::now();
        handle.send(page(Anchor::End, 30, None));
        let rows = text_rows(recv(&rt, &mut rx, 10));
        let end_in = end_started.elapsed();
        assert_eq!(rows.len(), 30);
        assert!(rows.last().unwrap().ends_with("},"));

        // Wrapped at 100 columns the same rows become 41 pieces each.
        handle.send(page(Anchor::End, 30, Some(100)));
        let rows = text_rows(recv(&rt, &mut rx, 10));
        assert_eq!(rows.len(), 30);
        assert!(rows.iter().all(|r| r.chars().count() <= 100));

        let search_started = Instant::now();
        handle.send(Request::Search {
            query: "NEEDLE".to_string(),
            from: 0,
            backward: false,
            fold_case: false,
            encoding: None,
        });
        let hit = match recv(&rt, &mut rx, 60) {
            Reply::SearchHit(range) => range,
            Reply::SearchMiss => panic!("the needle is there"),
            _ => panic!("expected a search result"),
        };
        let search_in = search_started.elapsed();
        assert_eq!(hit.start, needle_at);
        handle.send(Request::TextPage {
            anchor: Anchor::Offset(hit.start),
            rows: 30,
            cols: None,
            encoding: None,
            highlight: Some(hit.clone()),
        });
        match recv(&rt, &mut rx, 10) {
            Reply::Text { page, .. } => {
                let row = &page.rows[0];
                let hl = row.highlight.clone().expect("the hit row is highlighted");
                assert_eq!(&row.text[hl], "NEEDLE");
            }
            _ => panic!("expected a text page"),
        }
        eprintln!(
            "200 MB: first page {opened_in:?}, End {end_in:?}, search {search_in:?} (debug build), len {len}"
        );
        assert!(
            opened_in <= Duration::from_millis(300),
            "first page took {opened_in:?}"
        );
        assert!(end_in <= Duration::from_millis(300), "End took {end_in:?}");
    }
}
