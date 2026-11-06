// SPDX-License-Identifier: MIT
//! T-5.3.7's performance AC: "content search of a 5 GB tree streams first
//! results in ≤ 500 ms".
//!
//! Builds a 5 GiB tree under `CARGO_TARGET_TMPDIR` -- 100 folders of ten
//! 5 MiB files each, sparse so the test costs no disk space -- with a
//! small matching file in every tenth folder, then times a content search
//! for the match from the root: the first `Hits` event, and the whole
//! walk. Sparse files read back as zeros from the page cache, so this
//! measures the walk and the matcher, not the disk; a cold spinning disk
//! is slower in proportion.
//!
//! Ignored by default (it writes 5 GiB of apparent size and is only
//! meaningful in release). Run with:
//!
//! ```text
//! cargo test -p duet-search --release --test first_hit_latency -- --ignored --nocapture
//! ```

use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use duet_search::{ContentSpec, SearchEvent, SearchSink, SearchSpec};

const FOLDERS: usize = 100;
const FILES_PER_FOLDER: usize = 10;
const FILE_BYTES: u64 = 5 * 1024 * 1024;

#[test]
#[ignore = "writes a 5 GiB sparse tree; run in release with --ignored"]
fn content_search_of_a_5_gib_tree_streams_first_results_within_500_ms() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("first_hit_latency");
    let _ = fs::remove_dir_all(&root);
    for d in 0..FOLDERS {
        let dir = root.join(format!("dir{d:03}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..FILES_PER_FOLDER {
            File::create(dir.join(format!("big{f}.log")))
                .unwrap()
                .set_len(FILE_BYTES)
                .unwrap();
        }
        if d % 10 == 9 {
            fs::write(dir.join("notes.txt"), b"a needle in the haystack\n").unwrap();
        }
    }

    let spec = SearchSpec {
        roots: vec![root.clone()],
        content: Some(ContentSpec {
            text: "needle".into(),
            ..ContentSpec::default()
        }),
        ..SearchSpec::default()
    };
    let started = Instant::now();
    let first_hit: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
    let sink_first = first_hit.clone();
    let sink: SearchSink = Arc::new(move |event| {
        if let SearchEvent::Hits(_) = event {
            sink_first
                .lock()
                .unwrap()
                .get_or_insert_with(|| started.elapsed());
        }
    });
    let progress = duet_search::run(
        spec.compile().unwrap(),
        sink,
        Arc::new(AtomicBool::new(false)),
    );
    let total = started.elapsed();
    let first = first_hit.lock().unwrap().expect("the search found nothing");
    fs::remove_dir_all(&root).unwrap();

    println!(
        "5 GiB tree: first hit after {first:?}, all {} hits after {total:?} \
         ({} MiB read, {} folders)",
        progress.hits,
        progress.bytes >> 20,
        progress.dirs
    );
    assert_eq!(progress.hits, (FOLDERS / 10) as u64);
    assert!(
        first <= Duration::from_millis(500),
        "first hit took {first:?}"
    );
}
