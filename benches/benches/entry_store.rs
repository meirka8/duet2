//! `duet-index::EntryStore` benchmarks — NFR-03 ("directory listing appears
//! responsive") / NFR-04 (large-directory scan) and the "sorting/filtering"
//! side of NFR-05, to the extent either is implementable yet.
//!
//! # Why this is in-memory only
//!
//! `EntryStore` (T-2.5.1, `crates/duet-index/src/entry_store.rs`) never
//! touches the filesystem — it's pure struct-of-arrays storage that a
//! caller populates from whatever `Metadata` a `FileSystem::stat`/
//! `read_dir` produced. Benchmarking it against [`duet_bench::corpus`]'s
//! *planned* entries (no `materialize()` call, no disk) exercises exactly
//! what T-2.5.1 owns — population throughput, name lookup, byte budget —
//! without conflating it with filesystem cost, which `local_listing.rs`
//! covers separately.
//!
//! # What's deferred
//!
//! `DirectoryModel::sort_by` and `apply_diff` are `todo!()` as of this
//! task (T-3.2.2/T-3.2.7 own the actual comparator/diff algorithms) — see
//! `crates/duet-index/src/model.rs`. There is nothing to benchmark there
//! yet; `sort`/`filter` throughput benches belong in this same file once
//! those land, and should be added there rather than in a new file so this
//! doc comment's "what's covered" list stays the single source of truth.

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use duet_bench::corpus::{self, CorpusScale};
use duet_index::EntryStore;

const SEED: u64 = 0xD0E7_B3A5_C0DE_5EED;

/// Scales actually run by default. 1M is included — pure in-memory
/// population is fast enough (no disk I/O) that, unlike
/// `local_listing.rs`'s on-disk corpus, there's no need to gate it behind
/// an opt-in env var.
const SCALES: [CorpusScale; 4] = [
    CorpusScale::Ten,
    CorpusScale::OneK,
    CorpusScale::HundredK,
    CorpusScale::OneM,
];

fn bench_population(c: &mut Criterion) {
    let mut group = c.benchmark_group("entry_store_population");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));
    for scale in SCALES {
        let plan = corpus::plan(scale, SEED);
        group.throughput(Throughput::Elements(plan.entries.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(scale.label()),
            &plan,
            |b, plan| {
                b.iter(|| {
                    let mut store = EntryStore::with_capacity(plan.entries.len());
                    for e in &plan.entries {
                        std::hint::black_box(store.push(&e.name, &e.metadata));
                    }
                    store
                });
            },
        );
    }
    group.finish();
}

/// Sequential name lookup over every id, in push order — the cost a
/// brief-mode render pass or a linear scan (quick-search, "jump to name")
/// pays per frame.
fn bench_name_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("entry_store_name_lookup_scan");
    group.sample_size(10);
    for scale in SCALES {
        let plan = corpus::plan(scale, SEED);
        let mut store = EntryStore::with_capacity(plan.entries.len());
        let ids: Vec<_> = plan
            .entries
            .iter()
            .map(|e| store.push(&e.name, &e.metadata))
            .collect();
        group.throughput(Throughput::Elements(ids.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(scale.label()),
            &(&store, &ids),
            |b, (store, ids)| {
                b.iter(|| {
                    let mut total_len = 0usize;
                    for &id in ids.iter() {
                        total_len += store.name(id).len();
                    }
                    std::hint::black_box(total_len)
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_population, bench_name_lookup);
criterion_main!(benches);
