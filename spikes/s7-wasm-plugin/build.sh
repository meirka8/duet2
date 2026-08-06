#!/usr/bin/env bash
# S-7 spike build pipeline.
#
# This environment has no rustup and no prebuilt wasm32-* std/sysroot (no
# sudo to install the `rust-wasm` pacman package or use rustup target add).
# The workaround, which turned out to be a load-bearing finding of this
# spike (see documentation/spikes/S-7.md "Toolchain friction"):
#
#   1. `rust-src` IS installed (system package), so `-Z build-std` can
#      rebuild std from source for a target with no prebuilt sysroot.
#      `-Z` requires RUSTC_BOOTSTRAP=1 to unlock on a stable-channel rustc.
#   2. wasm32-wasip2 needs a prebuilt wasi-libc (`-lc`) that isn't present
#      either (that's a C static lib, build-std can't produce it). So the
#      guest targets wasm32-unknown-unknown instead, which has no libc
#      dependency at all -- and, as a side effect, cannot import WASI even
#      if it wanted to, which is a nice fit for "zero ambient authority".
#   3. wasm32-unknown-unknown output is a plain core wasm module, not a
#      Component. `wasm-tools component new` wraps it into one. No WASI
#      adapter is needed because this guest has zero host imports.
#   4. The cdylib link step still needs the `wasm-component-ld` binary
#      (normally a rustup component); installed via `cargo install`
#      instead.
#
# `wasm-component-ld` and `wasm-tools` are installed once via:
#   cargo install wasm-component-ld wasm-tools
# and are expected on PATH (~/.cargo/bin).

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

export PATH="$HOME/.cargo/bin:$PATH"

for tool in wasm-tools wasm-component-ld; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing $tool on PATH -- run: cargo install $tool" >&2
        exit 1
    fi
done

build_guest() {
    local dir=$1 crate=$2
    echo "== building guest $crate (wasm32-unknown-unknown, build-std) =="
    (
        cd "$dir"
        RUSTC_BOOTSTRAP=1 cargo build -Z build-std=std,panic_abort \
            --target wasm32-unknown-unknown --release
    )
    echo "== componentizing $crate =="
    wasm-tools component new \
        "$dir/target/wasm32-unknown-unknown/release/${crate//-/_}.wasm" \
        -o "$dir/${crate//-/_}.component.wasm"
    wasm-tools validate "$dir/${crate//-/_}.component.wasm"
    echo "== $dir/${crate//-/_}.component.wasm =="
    wasm-tools component wit "$dir/${crate//-/_}.component.wasm"
}

build_guest guest-content guest-content
build_guest guest-loop guest-loop

echo "== building host harness =="
(cd host && cargo build --release)

echo
echo "done. run:"
echo "  ./host/target/release/bench"
echo "  ./host/target/release/interrupt"
