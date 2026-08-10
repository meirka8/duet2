mod alloc_track;
mod store;
mod table_delegate;

use std::time::{Duration, Instant};

use gpui::{
    px, point, size, App, AppContext as _, Application, Bounds, Context, Entity, IntoElement,
    ParentElement as _, Render, Styled as _, Window, WindowBounds, WindowOptions,
};
use gpui_component::table::{ColumnSort, Table, TableDelegate as _, TableState};

use alloc_track::CountingAllocator;
use store::EntryStore;
use table_delegate::FileTableDelegate;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

// ---- Benchmark tuning constants -------------------------------------------------

/// Frames rendered before we start sampling, to let font/layout caches warm up.
const WARMUP_FRAMES: usize = 30;
/// Frames sampled during the scroll benchmark.
const SCROLL_FRAMES: usize = 1500;
/// Simulated scroll speed, in pixels of content moved per frame.
const SCROLL_PX_PER_FRAME: f32 = 420.0;
/// Every Nth stable row is pre-selected before scrolling, to exercise the
/// multi-selection highlight path at a realistic (large) selection size.
const SELECTION_STRIDE: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Warmup,
    Scrolling,
    Done,
}

struct BenchState {
    phase: Phase,
    frame_ix: usize,
    last_instant: Instant,
    samples: Vec<Duration>,
    scroll_y: f32,
    rss_after_load_kb: u64,
    rss_after_first_paint_kb: u64,
    rss_after_scroll_kb: u64,
    rss_after_sort_kb: u64,
    alloc_snapshot_during_scroll: Option<alloc_track::AllocSnapshot>,
    sort_results: Vec<(&'static str, Duration)>,
    data_bytes: usize,
    gen_duration: Duration,
}

struct RootView {
    table: Entity<TableState<FileTableDelegate>>,
    bench: BenchState,
}

impl RootView {
    fn on_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let delta = now.duration_since(self.bench.last_instant);
        self.bench.last_instant = now;

        match self.bench.phase {
            Phase::Warmup => {
                self.bench.frame_ix += 1;
                if self.bench.frame_ix >= WARMUP_FRAMES {
                    self.bench.phase = Phase::Scrolling;
                    self.bench.frame_ix = 0;
                    self.bench.rss_after_first_paint_kb = alloc_track::rss_kb();
                    alloc_track::reset();
                    alloc_track::set_tracking(true);
                }
            }
            Phase::Scrolling => {
                if self.bench.frame_ix > 0 {
                    // Skip the very first delta (it includes warmup->scroll transition work).
                    self.bench.samples.push(delta);
                }
                self.bench.scroll_y -= SCROLL_PX_PER_FRAME;
                self.table.update(cx, |state, cx| {
                    set_vertical_offset(state, point(px(0.0), px(self.bench.scroll_y)));
                    cx.notify();
                });

                self.bench.frame_ix += 1;
                if self.bench.frame_ix >= SCROLL_FRAMES {
                    alloc_track::set_tracking(false);
                    self.bench.alloc_snapshot_during_scroll = Some(alloc_track::snapshot());
                    self.bench.rss_after_scroll_kb = alloc_track::rss_kb();
                    self.run_sorts(window, cx);
                    self.bench.rss_after_sort_kb = alloc_track::rss_kb();
                    self.bench.phase = Phase::Done;
                    self.print_report();
                    cx.quit();
                    return;
                }
            }
            Phase::Done => return,
        }

        self.schedule_next_frame(window, cx);
    }

    fn schedule_next_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.entity();
        window.on_next_frame(move |window, cx| {
            entity.update(cx, |this, cx| this.on_frame(window, cx));
        });
    }

    fn run_sorts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Exercise a numeric column (size) and a string column (name),
        // ascending, each over the full 1,000,000 rows. This is invoked
        // directly against the delegate rather than via a simulated header
        // click, so it exercises exactly the sort code path a real click
        // would run, without needing synthetic mouse input.
        for (label, col_ix) in [("size (u64 key)", 1usize), ("name (byte-slice key)", 0usize)] {
            self.table.update(cx, |state, cx| {
                state
                    .delegate_mut()
                    .perform_sort(col_ix, ColumnSort::Ascending, window, cx);
            });
            let dur = self
                .table
                .read(cx)
                .delegate()
                .last_sort_duration
                .unwrap_or_default();
            self.bench.sort_results.push((label, dur));
        }
    }

    fn print_report(&self) {
        let b = &self.bench;
        let mut sorted = b.samples.clone();
        sorted.sort_unstable();
        let p50 = percentile(&sorted, 0.50);
        let p99 = percentile(&sorted, 0.99);
        let max = sorted.last().copied().unwrap_or_default();
        let mean: Duration = if sorted.is_empty() {
            Duration::default()
        } else {
            sorted.iter().sum::<Duration>() / sorted.len() as u32
        };
        let over_budget = sorted.iter().filter(|d| d.as_secs_f64() > 1.0 / 120.0).count();

        println!("\n===== S-1 spike report =====");
        println!("rows: {}", store::ROW_COUNT);
        println!(
            "data generation: {:?} ({:.1} MB accountable SoA+arena bytes)",
            b.gen_duration,
            b.data_bytes as f64 / (1024.0 * 1024.0)
        );
        println!("RSS after data generation:      {:>8} kB", b.rss_after_load_kb);
        println!("RSS after first paint (warm):    {:>8} kB", b.rss_after_first_paint_kb);
        println!("RSS after scroll benchmark:      {:>8} kB", b.rss_after_scroll_kb);
        println!("RSS after sort benchmark:        {:>8} kB", b.rss_after_sort_kb);
        println!();
        println!("-- scroll frame time ({} samples, {} frames simulated) --", sorted.len(), SCROLL_FRAMES);
        println!("  p50: {:?}", p50);
        println!("  p99: {:?}", p99);
        println!("  max: {:?}", max);
        println!("  mean: {:?}", mean);
        println!(
            "  frames over 8.3ms (120Hz budget): {} / {} ({:.2}%)",
            over_budget,
            sorted.len(),
            100.0 * over_budget as f64 / sorted.len().max(1) as f64
        );
        println!();
        if let Some(snap) = b.alloc_snapshot_during_scroll {
            println!("-- global allocator activity during {} scroll frames --", SCROLL_FRAMES);
            println!("  alloc events:   {}", snap.alloc_count);
            println!("  alloc bytes:    {}", snap.alloc_bytes);
            println!("  dealloc events: {}", snap.dealloc_count);
            println!("  dealloc bytes:  {}", snap.dealloc_bytes);
            println!("  realloc events: {}", snap.realloc_count);
            println!(
                "  alloc events / frame: {:.3}",
                snap.alloc_count as f64 / SCROLL_FRAMES as f64
            );
            println!(
                "  alloc events / (frame * visible row): investigate manually; visible rows ~= window_height / row_height"
            );
        }
        println!();
        println!("-- sort of {} rows --", store::ROW_COUNT);
        for (label, dur) in &b.sort_results {
            println!("  {label}: {dur:?}");
        }
        println!("=============================\n");
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::default();
    }
    let ix = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[ix.min(sorted.len() - 1)]
}

/// Directly set the vertical scroll offset of a uniform-list-backed table,
/// simulating a fast programmatic scroll (rather than jumping row-to-row via
/// `scroll_to_item`, which would not exercise the same incremental
/// re-virtualisation a real trackpad/wheel scroll does).
fn set_vertical_offset(state: &mut TableState<FileTableDelegate>, offset: gpui::Point<gpui::Pixels>) {
    state
        .vertical_scroll_handle
        .0
        .borrow()
        .base_handle
        .set_offset(offset);
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
            .size_full()
            .bg(gpui::rgb(0x1e1e1e))
            .child(Table::new(&self.table).stripe(true).bordered(true))
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);
        let rss_baseline_kb = alloc_track::rss_kb();
        println!(
            "RSS after Application::new()+gpui_component::init (framework baseline, no data, no window): {} kB",
            rss_baseline_kb
        );

        println!("Generating {} synthetic rows (SoA)...", store::ROW_COUNT);
        let gen_start = Instant::now();
        let store = EntryStore::generate(store::ROW_COUNT);
        let gen_duration = gen_start.elapsed();
        let data_bytes = store.approx_bytes();
        println!("Generation done in {:?}, ~{:.1} MB", gen_duration, data_bytes as f64 / (1024.0*1024.0));
        let rss_after_load_kb = alloc_track::rss_kb();

        let bounds = Bounds::centered(None, size(px(1000.0), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                let mut delegate = FileTableDelegate::new(store);
                delegate.selection.select_stride(SELECTION_STRIDE);

                let table = cx.new(|cx| TableState::new(delegate, window, cx));
                // Park the keyboard cursor somewhere mid-list so the
                // TableState-owned "selected row" highlight (our keyboard
                // cursor) is exercised too.
                table.update(cx, |state, cx| {
                    state.set_selected_row(500, cx);
                });

                let root = cx.new(|_cx| RootView {
                    table,
                    bench: BenchState {
                        phase: Phase::Warmup,
                        frame_ix: 0,
                        last_instant: Instant::now(),
                        samples: Vec::with_capacity(SCROLL_FRAMES),
                        scroll_y: 0.0,
                        rss_after_load_kb,
                        rss_after_first_paint_kb: 0,
                        rss_after_scroll_kb: 0,
                        rss_after_sort_kb: 0,
                        alloc_snapshot_during_scroll: None,
                        sort_results: Vec::new(),
                        data_bytes,
                        gen_duration,
                    },
                });

                root.update(cx, |this, cx| {
                    this.schedule_next_frame(window, cx);
                });

                root
            },
        )
        .unwrap();
    });
}
