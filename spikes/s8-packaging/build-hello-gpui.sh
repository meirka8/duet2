#!/usr/bin/env bash
# Builds the hello_gpui release binary on the host before packaging.
# See org.duet.HelloGpui.yml for why the Flatpak build consumes this
# prebuilt binary instead of compiling inside the flatpak-builder sandbox.
set -euo pipefail
cd "$(dirname "$0")/hello-gpui"
cargo build --release
echo "Built: $(pwd)/target/release/hello_gpui"
ls -la target/release/hello_gpui
