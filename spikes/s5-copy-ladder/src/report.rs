use serde::Serialize;
use std::time::Duration;

#[derive(Serialize, Clone)]
pub struct Measurement {
    pub corpus: String, // "large" | "small"
    pub strategy: String,
    pub bytes: u64,
    pub files: u64,
    pub duration_ms: f64,
    pub throughput_mib_s: f64,
    pub cached_kb_before: u64,
    pub cached_kb_after: u64,
    pub cached_kb_delta: i64,
    pub verified: Option<bool>,
    pub notes: String,
}

impl Measurement {
    // A wide constructor is fine for a benchmark data-point record in a
    // spike; not worth a builder for one call site per strategy.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        corpus: &str,
        strategy: &str,
        bytes: u64,
        files: u64,
        duration: Duration,
        cached_before: u64,
        cached_after: u64,
        verified: Option<bool>,
        notes: impl Into<String>,
    ) -> Self {
        let secs = duration.as_secs_f64();
        let mib = bytes as f64 / (1024.0 * 1024.0);
        let throughput = if secs > 0.0 {
            mib / secs
        } else {
            f64::INFINITY
        };
        Measurement {
            corpus: corpus.to_string(),
            strategy: strategy.to_string(),
            bytes,
            files,
            duration_ms: duration.as_secs_f64() * 1000.0,
            throughput_mib_s: throughput,
            cached_kb_before: cached_before,
            cached_kb_after: cached_after,
            cached_kb_delta: cached_after as i64 - cached_before as i64,
            verified,
            notes: notes.into(),
        }
    }
}

pub fn print_table(rows: &[Measurement]) {
    println!(
        "{:<8} {:<22} {:>12} {:>8} {:>10} {:>14} {:>12} {:>8}",
        "corpus", "strategy", "bytes", "files", "time_ms", "MiB/s", "cache_dKB", "ok"
    );
    for r in rows {
        println!(
            "{:<8} {:<22} {:>12} {:>8} {:>10.1} {:>14.1} {:>12} {:>8}",
            r.corpus,
            r.strategy,
            r.bytes,
            r.files,
            r.duration_ms,
            r.throughput_mib_s,
            r.cached_kb_delta,
            match r.verified {
                Some(true) => "yes",
                Some(false) => "NO",
                None => "-",
            }
        );
        if !r.notes.is_empty() {
            println!("           note: {}", r.notes);
        }
    }
}
