# Vendored, patched dependencies (ADR-007)

This directory holds byte-for-byte copies of two crates.io releases with a
small local patch each, wired in through the root `Cargo.toml`'s
`[patch.crates-io]` table. Nothing else in the workspace is aware of them:
every crate still declares the same versions it always did (`gpui = "0.2.2"`,
and `blade-graphics 0.7.1` via gpui), Cargo just resolves those two
packages from here instead of the registry.

| Directory | Upstream | Patch |
|---|---|---|
| `gpui/` | `gpui 0.2.2` (crates.io) | `src/platform/blade/blade_renderer.rs`: `BladeRenderer::draw` recreates the swapchain and re-acquires when the acquired frame is out of date |
| `blade-graphics/` | `blade-graphics 0.7.1` (crates.io) | `src/vulkan/mod.rs`: `Frame::is_out_of_date()` accessor (the field it reads is private upstream) |

Everything not needed to *build* was dropped from the copies (gpui's
`examples/`, `docs/`, `tests/` and their `[[example]]`/`[[test]]` targets;
blade-graphics's `etc/`; both registry `Cargo.lock`s). The sources are
otherwise untouched, and every patched hunk is marked `DUET PATCH`.

## Why

Reported during UAT on 2026-09-03 (GNOME 4x / Mutter, Wayland): maximizing
the Duet window made it freeze "at a random time" afterwards, the window
still visible but never repainting. `WAYLAND_DEBUG=1` traces pinned it
down:

1. Once a maximized (or fullscreen) window covers the whole output, Mutter
   re-evaluates it as a direct-scanout candidate and sends the surface new
   `zwp_linux_dmabuf_feedback_v1` tranches (the first flagged *scanout*).
   *When* that happens depends on focus and pointer changes, hence the
   apparent randomness (0.5s to 21s after the maximize in our traces).
2. Mesa retires the Vulkan swapchain on any dmabuf-feedback change so the
   client can reallocate scanout-compatible buffers: `vkQueuePresentKHR`
   and every following `vkAcquireNextImageKHR` return
   `VK_ERROR_OUT_OF_DATE_KHR` until the swapchain is recreated.
3. `blade_graphics::vulkan::Surface::acquire_frame` answers that with a
   placeholder frame (`image_index: None`) and `gpui::BladeRenderer::draw`
   never checks for it or reconfigures, so nothing is presented ever again
   while the process stays alive. Log signature: an unbroken stream of
   `Acquire failed because the surface is out of date` warnings. (The
   `window not found` errors seen alongside it are unrelated shutdown noise
   from frame callbacks racing window teardown.)

The fix is the standard Vulkan contract every swapchain client has to
honour: on out-of-date, recreate the swapchain at the current size and
acquire again. gpui already has the reconfigure routine
(`update_drawable_size_impl` uses it for resizes); the patch calls it from
`draw` when the acquire fails.

Neither upstream has a fix as of this writing: gpui 0.2.2 is the latest
published version, and blade's current `main` still returns the placeholder
frame from `acquire_frame`.

## Re-applying on a bump (ADR-003)

1. Download the new release source (`cargo vendor` into a scratch directory,
   or `~/.cargo/registry/src/*/<crate>-<version>/` after a `cargo fetch`).
2. Check whether upstream now handles the out-of-date acquire (look for
   `image_index`/out-of-date handling in `BladeRenderer::draw`, and for a
   public accessor on `Frame`). If it does, drop the corresponding
   directory here and its `[patch.crates-io]` line; if not, copy the new
   source over the directory, re-apply the `DUET PATCH` hunks, and trim the
   same non-build files.
3. Rebuild, then run the verification recipe below against the real
   compositor before signing the bump off.

## Verification recipe

Duet ships a diagnostics-only hook for exactly this class of bug, so the
check needs no clicking (the trigger otherwise depends on pointer/focus
timing that is awkward to drive by hand):

```bash
WAYLAND_DEBUG=1 DUET_DEBUG_CHROME_CYCLE_MS=4000 DUET_LOG=info \
  timeout 60 ./target/debug/duet 2>&1 | \
  grep -E "DEBUG: step|tranche_flags\(1\)|out of date|recreating the swapchain|still out of date"
```

`DUET_DEBUG_CHROME_CYCLE_MS=<ms>` makes the window toggle fullscreen on,
off, then maximize, restore, every `<ms>` milliseconds for the life of the
process. What a passing run looks like: at least one `Acquire failed
because the surface is out of date` immediately followed by `surface out of
date; recreating the swapchain at the current size`, never more than one
`Acquire failed` in a row, and presentation continuing afterwards
(`wl_surface#N.attach` keeps appearing in the trace). Failure modes:
`still out of date after recreating` (the patch no longer recovers), or an
unbroken run of `Acquire failed` lines (the patch is missing).

The trigger is the compositor's, not ours, so not every transition
retires the swapchain: Mesa only does so when the feedback it ends up with
actually differs from what it had, and Mutter sometimes offers the scanout
tranche and revokes it again within the same batch (seen in one of our
runs: three `tranche_flags(1)` events, zero retires). A run with the
toggles but no `Acquire failed`/`recreating` pair at all therefore proves
nothing either way; rerun it, ideally while moving the pointer over the
window or changing focus, until a retire shows up. In our runs on
GNOME/Mutter, six transitions in a row each produced exactly one
retire-and-recover.
