#!/usr/bin/env bash
# Full benchmark sweep for spike S-4. Not part of the crate; a throwaway
# driver invoked manually. Generates corpora on each available fs, times
# all three strategies, cleans up after itself.
set -euo pipefail

BIN="$(dirname "$0")/target/release/s4-dir-enum"
LOG="${1:-/tmp/s4-sweep-results.csv}"
: > "$LOG"

# label:base_dir pairs -- base_dir must be on the filesystem named by label.
FS_DIRS=(
  "tmpfs:/dev/shm/s4-bench"
  "ext4:/run/media/meirk/storage_2/s4-bench"
  "btrfs:/home/meirk/s4-bench"
)

SIZES=(100000 1000000)
THREADS=$(nproc)

for entry in "${FS_DIRS[@]}"; do
  label="${entry%%:*}"
  base="${entry#*:}"
  for count in "${SIZES[@]}"; do
    dir="${base}-${count}"
    rm -rf "$dir"
    echo "=== gen fs=$label count=$count dir=$dir ===" >&2
    "$BIN" gen --dir "$dir" --count "$count" --seed 1 --threads "$THREADS" 2>&1 | tee -a "$LOG.gen.log" >&2

    repeat=5
    if [ "$count" -ge 1000000 ]; then
      repeat=3
    fi

    for strategy in naive dtype statx; do
      echo "=== bench fs=$label count=$count strategy=$strategy ===" >&2
      "$BIN" bench --dir "$dir" --strategy "$strategy" --threads "$THREADS" \
        --repeat "$repeat" --label "$label" | tee -a "$LOG"
    done

    "$BIN" clean --dir "$dir" >&2
  done
done

echo "done -> $LOG" >&2
