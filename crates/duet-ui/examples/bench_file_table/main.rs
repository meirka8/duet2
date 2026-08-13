//! T-4.2.1's 1,000,000-row scroll benchmark.
//!
//! Builds a real `duet_index::DirectoryModel` from T-3.3.4's deterministic,
//! in-memory synthetic-corpus generator (`duet_bench::corpus`), wraps it in
//! the real `duet_ui::file_table::FileTableDelegate`/`TableState` this task
//! built (not a parallel benchmark-only delegate), and measures frame time
//! plus allocator activity while simulating a fast programmatic scroll --
//! the same methodology Phase 0's S-1 spike used
//! (`spikes/s1-virtualised-table/src/main.rs`), reused rather than
//! reinvented, right down to reusing its `alloc_track` counting-allocator
//! pattern (see `alloc_track.rs` alongside this file).
//!
//! Run with: `cargo run -p duet-ui --release --example bench_file_table`
//! (release matters here -- see the printed report's note on why).
//!
//! # What this does and does not verify
//!
//! This sandbox has no 120Hz display, so "scrolls at monitor refresh" in
//! the literal, end-to-end (compositor + vsync) sense cannot be checked
//! here. What *is* measured, and printed honestly as such below: (a)
//! `gpui-component`'s own per-frame render cost for producing/laying out
//! the visible rows (relative timing, not calibrated against a real
//! compositor), and (b) allocator activity strictly during the scroll
//! window -- the counting-allocator check the AC actually asks for. Same
//! stance S-1's spike took; see its report
//! (`documentation/spikes/S-1.md`) for the precedent.

mod alloc_track;

use std::time::{Duration, Instant};

use duet_bench::corpus::{self, CorpusScale};
use duet_index::DirectoryModel;
use duet_types::EntryId;
use duet_ui::file_table::FileTableDelegate;
use duet_widgets::table::{ColumnSort, Table, TableDelegate as _, TableState};
use gpui::{
    App, AppContext as _, Application, Bounds, Context, Entity, IntoElement, ParentElement as _,
    Render, Styled as _, Window, WindowBounds, WindowOptions, point, px, size,
};

use alloc_track::CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

// ---- Benchmark tuning constants (mirrors S-1's spike) ----------------------

/// Frames rendered before sampling starts, to let font/layout caches warm up.
const WARMUP_FRAMES: usize = 30;
/// Frames sampled during the scroll benchmark.
const SCROLL_FRAMES: usize = 1500;
/// Simulated scroll speed, in pixels of content moved per frame.
const SCROLL_PX_PER_FRAME: f32 = 420.0;
/// Every Nth row (by display position at load time) is pre-selected, to
/// exercise the multi-selection highlight path (`render_tr`) at a
/// realistic, large selection size while scrolling.
const SELECTION_STRIDE: usize = 5;

const SEED: u64 = 0xD0E7_B3A5_C0DE_5EED;

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
    row_count: usize,
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
                    // Skip the very first delta -- it includes warmup->scroll
                    // transition work, not steady-state scroll cost.
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

    /// Exercises `DirectoryModel::sort_by` (via `perform_sort`, the same
    /// entry point a real header click drives) over the full 1M rows for a
    /// numeric column (size) and the name column -- not part of the AC,
    /// but cheap to report alongside the scroll numbers and it exercises
    /// `FileTableDelegate::rebuild_row_text`'s own 1M-row cost, which the
    /// scroll phase alone (post-warmup, cache already built) does not.
    fn run_sorts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (label, col_ix) in [
            ("size (u64 key)", 1usize),
            ("name (byte-slice key)", 0usize),
        ] {
            let start = Instant::now();
            self.table.update(cx, |state, cx| {
                state
                    .delegate_mut()
                    .perform_sort(col_ix, ColumnSort::Ascending, window, cx);
            });
            self.bench.sort_results.push((label, start.elapsed()));
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
        let over_budget = sorted
            .iter()
            .filter(|d| d.as_secs_f64() > 1.0 / 120.0)
            .count();

        println!("\n===== T-4.2.1 bench_file_table report =====");
        println!("rows: {}", b.row_count);
        println!(
            "data generation (corpus::plan + EntryStore::push + sort_by): {:?} ({:.1} MB accountable SoA+arena bytes)",
            b.gen_duration,
            b.data_bytes as f64 / (1024.0 * 1024.0)
        );
        println!(
            "RSS after data generation:      {:>8} kB",
            b.rss_after_load_kb
        );
        println!(
            "RSS after first paint (warm):    {:>8} kB",
            b.rss_after_first_paint_kb
        );
        println!(
            "RSS after scroll benchmark:      {:>8} kB",
            b.rss_after_scroll_kb
        );
        println!(
            "RSS after sort benchmark:        {:>8} kB",
            b.rss_after_sort_kb
        );
        println!();
        println!(
            "-- scroll frame time ({} samples, {} frames simulated) --",
            sorted.len(),
            SCROLL_FRAMES
        );
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
            println!(
                "-- global allocator activity during {} scroll frames --",
                SCROLL_FRAMES
            );
            println!("  alloc events:   {}", snap.alloc_count);
            println!("  alloc bytes:    {}", snap.alloc_bytes);
            println!("  dealloc events: {}", snap.dealloc_count);
            println!("  dealloc bytes:  {}", snap.dealloc_bytes);
            println!("  realloc events: {}", snap.realloc_count);
            println!(
                "  alloc events / frame: {:.4}",
                snap.alloc_count as f64 / SCROLL_FRAMES as f64
            );
            println!(
                "  total allocator events (alloc+dealloc+realloc) / frame: {:.4}",
                snap.total_events() as f64 / SCROLL_FRAMES as f64
            );
        }
        println!();
        println!(
            "-- sort of {} rows (perform_sort -> DirectoryModel::sort_by + rebuild_row_text) --",
            b.row_count
        );
        for (label, dur) in &b.sort_results {
            println!("  {label}: {dur:?}");
        }
        println!();
        println!("-- honesty note --");
        println!("  This sandbox has no 120Hz display: the frame-time numbers above are");
        println!("  gpui-component's own render-cost timing (relative, this-box-only), NOT a");
        println!("  calibrated 120Hz-compositor measurement. The allocator numbers ARE the real");
        println!("  AC check (zero allocations while scrolling); the frame-time numbers are");
        println!("  supporting evidence, not a substitute for the real-hardware check.");
        println!("=============================================\n");
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::default();
    }
    let ix = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[ix.min(sorted.len() - 1)]
}

/// Directly sets the vertical scroll offset of the table's uniform-list
/// backing, simulating a fast programmatic scroll (as opposed to jumping
/// row-to-row via `scroll_to_item`, which would not exercise the same
/// incremental re-virtualisation a real trackpad/wheel scroll does). Same
/// approach S-1's spike used against the same `gpui-component` version.
fn set_vertical_offset(
    state: &mut TableState<FileTableDelegate>,
    offset: gpui::Point<gpui::Pixels>,
) {
    state
        .vertical_scroll_handle
        .0
        .borrow()
        .base_handle
        .set_offset(offset);
}

/// Builds a real `DirectoryModel` from T-3.3.4's deterministic in-memory
/// corpus generator (`duet_bench::corpus::plan`) -- no filesystem I/O, no
/// synthetic parallel data structure, the same `EntryStore::push` path a
/// real directory listing goes through.
fn build_synthetic_model(scale: CorpusScale, seed: u64) -> (DirectoryModel, Duration, usize) {
    let start = Instant::now();
    let plan = corpus::plan(scale, seed);

    let mut model = DirectoryModel::new();
    {
        let entries = model.entries_mut();
        for entry in &plan.entries {
            entries.push(&entry.name, &entry.metadata);
        }
    }
    // `entries_mut()`'s own doc comment: callers must call `sort_by`
    // afterward to bring `order`/`full_order` in sync -- this is that call.
    model.sort_by(duet_index::SortColumn::Name, true);

    let gen_duration = start.elapsed();
    let bytes = model.entries().approx_bytes();
    (model, gen_duration, bytes)
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component_init(cx);
        let rss_baseline_kb = alloc_track::rss_kb();
        println!(
            "RSS after Application::new()+init (framework baseline, no data, no window): {} kB",
            rss_baseline_kb
        );

        println!(
            "Generating {} synthetic rows via duet_bench::corpus::plan...",
            CorpusScale::OneM.entry_count()
        );
        let (mut model, gen_duration, data_bytes) = build_synthetic_model(CorpusScale::OneM, SEED);
        println!(
            "Generation (plan + EntryStore::push + sort_by) done in {gen_duration:?}, ~{:.1} MB",
            data_bytes as f64 / (1024.0 * 1024.0)
        );
        let row_count = model.order().len();

        // Pre-select every Nth row, by stable EntryId, before construction
        // -- exercises the selection-highlight path at a realistic size.
        let stride_ids: Vec<EntryId> = model
            .order()
            .iter()
            .step_by(SELECTION_STRIDE)
            .map(|&ix| EntryId::new(ix))
            .collect();
        model.select_many(stride_ids);

        let rss_after_load_kb = alloc_track::rss_kb();

        let bounds = Bounds::centered(None, size(px(1000.0), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                let mut delegate = FileTableDelegate::new(model);
                // The real `FileTable` path (`duet-ui/src/file_table.rs`)
                // starts `loading: true` until a background listing
                // completes; this benchmark already has its full model, so
                // it must flip that off itself or every frame would render
                // gpui-component's loading skeleton instead of real rows.
                delegate.set_loading(false);

                let table = cx.new(|cx| TableState::new(delegate, window, cx));
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
                        row_count,
                    },
                });

                root.update(cx, |this, cx| {
                    this.schedule_next_frame(window, cx);
                });

                root
            },
        )
        .expect("failed to open the benchmark window");
    });
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
            .size_full()
            .bg(gpui::rgb(0x1e1e1e))
            .child(Table::new(&self.table).stripe(true).bordered(true))
    }
}

/// gpui-component's own `init` is off-limits outside `duet-widgets` (R-G7's
/// façade rule -- this example lives in `duet-ui`, which may see plain
/// `gpui` but not gpui-component directly); `duet_widgets::init` is the
/// façade entry point every other window-bootstrap path in this workspace
/// already goes through (see `workspace.rs::run`).
///
/// Also installs Duet's own [`duet_widgets::theme::TokenPalette`] global
/// (built-in dark, no `themes/*.toml` override -- this benchmark has no
/// need for `duet-ui`'s full `ThemeController` hot-reload machinery):
/// `FileTableDelegate::render_tr`'s selection highlight reads
/// `TokenPalette::current(cx)`, which panics if nothing ever installed one
/// -- exactly what the real app's `theme_controller::ThemeController`
/// does before any window opens (`workspace.rs::run`); this is the
/// minimal equivalent for a benchmark binary that skips the rest of that
/// bootstrap.
fn gpui_component_init(cx: &mut App) {
    duet_widgets::init(cx);
    duet_widgets::theme::TokenPalette::built_in(duet_widgets::theme::ThemeMode::Dark).install(cx);
}
