#!/usr/bin/env bash
# ADR-002 enforcement (T-2.1.2): only crates/duet-ui and crates/duet-widgets
# may *write* gpui/gpui-component code (`use gpui::...` etc). Every other
# crate in the workspace must stay UI-framework-agnostic so the shell is
# replaceable (design.md section 7.4 / section 8.1).
#
# Checks two places a dependency can hide:
#   1. Each crate's own Cargo.toml (direct dependency, any dependency table:
#      [dependencies], [dev-dependencies], [build-dependencies]). No crate
#      may declare this directly except duet-ui/duet-widgets -- not even
#      crates/duet, see below.
#   2. Cargo.lock, if present, for a transitive dependency reaching gpui via
#      some other crate (defence in depth -- (1) alone would miss a crate
#      that pulls in gpui indirectly through a non-obvious path).
#
# crates/duet (the binary) is an intentional, narrow exception to check
# (2) only: T-4.1.1 has it call `duet_ui::run()` to bootstrap the window,
# so gpui is unavoidably in its transitive graph -- every gpui app's
# binary entry point ends up linking gpui. What ADR-002 actually protects
# is that `duet`'s own source never writes `use gpui::...` (check (1)
# still runs on it, uncompromised) and that the core service crates
# (duet-vfs, duet-ops, duet-index, duet-search, duet-meta, duet-commands,
# duet-config, duet-plugin, duet-platform, ...) never gain gpui in their
# graph by any path.
#
# Usage: ./scripts/check-gpui-isolation.sh
# Exits non-zero with a clear message on the first violation found.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

ALLOWED=("crates/duet-ui" "crates/duet-widgets")
# Transitive-only exception -- see the comment block above.
ALLOWED_TRANSITIVE=("crates/duet-ui" "crates/duet-widgets" "crates/duet")

is_allowed() {
    local dir="$1"
    for a in "${ALLOWED[@]}"; do
        [[ "$dir" == "$a" ]] && return 0
    done
    return 1
}

is_allowed_transitive() {
    local dir="$1"
    for a in "${ALLOWED_TRANSITIVE[@]}"; do
        [[ "$dir" == "$a" ]] && return 0
    done
    return 1
}

violations=0

# --- (1) direct Cargo.toml dependencies ---
while IFS= read -r -d '' toml; do
    crate_dir="$(dirname "$toml")"
    rel="${crate_dir#./}"
    if is_allowed "$rel"; then
        continue
    fi
    if grep -Eq '^\s*(gpui|gpui-component)\s*=' "$toml"; then
        echo "VIOLATION: $toml declares a direct dependency on gpui/gpui-component."
        echo "  Only crates/duet-ui and crates/duet-widgets may depend on gpui (ADR-002)."
        violations=$((violations + 1))
    fi
done < <(find crates helpers plugins-sdk -name Cargo.toml -print0)

# --- (2) transitive check via Cargo.lock, if it exists yet ---
# (Phase 2 skeletons have no gpui dependency anywhere yet, so Cargo.lock
# won't exist or won't mention gpui -- this check earns its keep starting
# Phase 4, when duet-widgets first adds the real dependency.)
if [[ -f Cargo.lock ]] && grep -q 'name = "gpui"' Cargo.lock; then
    for crate_dir in crates/* helpers/* plugins-sdk; do
        [[ -f "$crate_dir/Cargo.toml" ]] || continue
        if is_allowed_transitive "$crate_dir"; then
            continue
        fi
        name=$(grep -m1 '^name' "$crate_dir/Cargo.toml" | sed -E 's/name\s*=\s*"(.*)"/\1/')
        # cargo tree -e normal -i gpui shows every path back to gpui; -p scopes to one package.
        if cargo tree -p "$name" -e normal 2>/dev/null | grep -q 'gpui'; then
            echo "VIOLATION: $crate_dir ($name) transitively depends on gpui via some other crate."
            echo "  Only crates/duet-ui and crates/duet-widgets may depend on gpui (ADR-002)."
            violations=$((violations + 1))
        fi
    done
fi

if [[ $violations -gt 0 ]]; then
    echo ""
    echo "$violations violation(s) found. See design.md ADR-002 / section 8.1's dependency rule."
    exit 1
fi

echo "OK: no crate outside duet-ui/duet-widgets depends on gpui."
