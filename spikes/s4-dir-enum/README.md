# S-4: directory enumeration throughput spike

Headless, non-UI benchmark for Phase 0 spike S-4. See
`../../documentation/spikes/S-4.md` for the writeup, full results table,
and comparison against the Phase 3 baseline targets in `task.md`.

Not production code and not part of the main workspace (none exists yet).

## Usage

```sh
cargo build --release

# Generate a deterministic flat corpus (refuses to run into a non-empty dir)
./target/release/s4-dir-enum gen --dir /path/to/corpus --count 100000 --seed 1 --threads 24

# Benchmark one strategy over it: naive | dtype | statx
./target/release/s4-dir-enum bench --dir /path/to/corpus --strategy statx --threads 24 --repeat 5 --label mylabel

# Remove the corpus
./target/release/s4-dir-enum clean --dir /path/to/corpus
```

`bench` prints one `RESULT,label,count,strategy,threads,rep,elapsed_ms,entries,checksum`
CSV line per rep to stdout. `run_sweep.sh` drives the full tmpfs/ext4/btrfs x
100k/1M x naive/dtype/statx matrix used to produce `results.csv` (the raw
data behind the S-4.md report) and cleans up all generated corpora
afterward.
