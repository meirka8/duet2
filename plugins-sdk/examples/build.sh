#!/usr/bin/env bash
# T-2.6.1 WIT validation build. Builds a minimal stub guest component for
# each of the five plugin-class worlds in plugins-sdk/wit/, then builds and
# runs host-check, which does a real wasmtime host<->guest round trip
# against the content-plugin stub (open-granted resource path included).
#
# These are validation-only stub plugins, not the reference plugins from
# task.md T-8.1.9 (EXIF columns, a toy archive format, a toy VFS) -- see
# each crate's doc comment. Toolchain recipe (wasm32-unknown-unknown +
# `-Z build-std` + `wasm-tools component new`) is the one spikes/s7-wasm-plugin/build.sh
# established; see its comment block for why (no rustup/sudo in this
# sandbox -- T-2.6.1/T-8.1.1 should get a real `rustup target add
# wasm32-wasip2` setup for CI/dev machines instead of this workaround).

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
}

build_guest stub-content stub-content
build_guest stub-packer stub-packer
build_guest stub-fs stub-fs
build_guest stub-viewer stub-viewer
build_guest stub-command stub-command

echo "== building + running host-check (host bindgen for all 5 worlds + a real content round trip) =="
(cd host-check && cargo build --release && ./target/release/host-check)

echo
echo "done: wit-bindgen generated guest bindings for all five worlds, wasmtime::component::bindgen! generated host bindings for all five worlds, and a real host<->guest call succeeded through the content-plugin-world (including the granted-stream resource path)."
