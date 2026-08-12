#!/usr/bin/env bash
# gpui-component façade enforcement (T-4.1.2, R-G7).
#
# This is a *narrower* sibling of `check-gpui-isolation.sh` (T-2.1.2,
# ADR-002): that script forbids `gpui`/`gpui-component` everywhere except
# `crates/duet-ui` and `crates/duet-widgets`. This script forbids
# `gpui_component::` specifically -- textual source usage of the
# `gpui-component` crate's own path -- everywhere except
# `crates/duet-widgets`. `crates/duet-ui` is deliberately *not* exempt
# here even though ADR-002 allows it to depend on plain `gpui`: R-G7's
# whole point is that `gpui-component` (a single-maintainer-ish community
# project, design.md §7.4) is only ever reached through `duet-widgets`'s
# façade (`duet_widgets::{table, list, input, select, menu, dialog, toast,
# resizable, theme, layout, compat}`), so a fork or replacement of that
# crate stays local to one crate. `duet-ui` seeing plain `gpui` directly is
# fine and expected (ADR-002); `duet-ui` seeing `gpui_component::` is not.
#
# Checks two places `gpui_component::` can appear:
#   1. Rust source (`*.rs`), textual grep for the `gpui_component::` path
#      prefix -- catches `use gpui_component::...`, fully-qualified calls
#      like `gpui_component::Theme::...`, and anything else spelling the
#      crate name out. Grep-based, matching check-gpui-isolation.sh's own
#      style.
#   2. Each crate's Cargo.toml, for a direct `gpui-component` dependency
#      declaration -- a crate that never writes `gpui_component::` in its
#      own source but still declares the dependency (e.g. only to
#      re-export it under a different name) would defeat the point just as
#      much.
#
# Usage: ./scripts/check-gpui-component-facade.sh
# Exits non-zero with a clear message on the first violation found.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

ALLOWED_DIR="crates/duet-widgets"

violations=0

# --- (1) source-level textual usage ---
while IFS= read -r -d '' rs_file; do
    case "$rs_file" in
        "$ALLOWED_DIR"/*) continue ;;
    esac
    if grep -q 'gpui_component::' "$rs_file"; then
        echo "VIOLATION: $rs_file references gpui_component:: directly."
        echo "  Only crates/duet-widgets may name the gpui_component crate; every other"
        echo "  crate (including crates/duet-ui) must go through duet-widgets's façade (R-G7)."
        violations=$((violations + 1))
    fi
done < <(find crates helpers plugins-sdk benches -name '*.rs' -print0 2>/dev/null)

# --- (2) direct Cargo.toml dependency ---
while IFS= read -r -d '' toml; do
    crate_dir="$(dirname "$toml")"
    rel="${crate_dir#./}"
    if [[ "$rel" == "$ALLOWED_DIR" ]]; then
        continue
    fi
    if grep -Eq '^\s*gpui-component\s*=' "$toml"; then
        echo "VIOLATION: $toml declares a direct dependency on gpui-component."
        echo "  Only crates/duet-widgets may depend on gpui-component directly (R-G7)."
        violations=$((violations + 1))
    fi
done < <(find crates helpers plugins-sdk -name Cargo.toml -print0)

if [[ $violations -gt 0 ]]; then
    echo ""
    echo "$violations violation(s) found. See design.md R-G7 / T-4.1.2's façade rule."
    exit 1
fi

echo "OK: gpui_component:: is only referenced inside crates/duet-widgets."
