// SPDX-License-Identifier: MIT
//! Times a search from the command line -- the engine behind Alt+F7,
//! without the UI -- for measuring on real trees:
//!
//! ```text
//! cargo run -p duet-search --release --example search -- <folder> <mask> [text]
//! ```
//!
//! Prints the time to the first hit, the hit count, and the totals.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duet_search::{ContentSpec, SearchEvent, SearchSink, SearchSpec};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(root), Some(mask)) = (args.first(), args.get(1)) else {
        eprintln!("usage: search <folder> <mask> [text]");
        std::process::exit(2);
    };
    let spec = SearchSpec {
        roots: vec![PathBuf::from(root)],
        name: mask.clone(),
        content: args.get(2).map(|text| ContentSpec {
            text: text.clone(),
            ..ContentSpec::default()
        }),
        ..SearchSpec::default()
    };
    let compiled = match spec.compile() {
        Ok(compiled) => compiled,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };
    let started = Instant::now();
    let first: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
    let sink_first = first.clone();
    let sink: SearchSink = Arc::new(move |event| {
        if let SearchEvent::Hits(_) = event {
            sink_first
                .lock()
                .unwrap()
                .get_or_insert_with(|| started.elapsed());
        }
    });
    let progress = duet_search::run(compiled, sink, Arc::new(AtomicBool::new(false)));
    let total = started.elapsed();
    let first = *first.lock().unwrap();
    println!(
        "first hit: {}  hits: {}  total: {total:?}  folders: {}  entries: {}  read: {} MiB  errors: {}",
        first.map_or("none".to_string(), |d| format!("{d:?}")),
        progress.hits,
        progress.dirs,
        progress.entries,
        progress.bytes >> 20,
        progress.errors,
    );
}
