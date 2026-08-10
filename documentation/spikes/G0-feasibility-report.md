# G0 Feasibility Report — Phase 0 Decision Memo

| Field | Value |
|---|---|
| Document ID | DUET-G0-001 |
| Gate | G0 (`design.md` approval gates table) |
| Consolidates | S-1 … S-8, `T-0.9.1` |
| Date | 2026-08-10 |
| Status | Final |

## Decision: **Proceed** (not proceed-with-changes, not choose Iced)

None of the three kill criteria in `task.md` Phase 0 fired:

- S-1 did not show the table failing 120 Hz *and* the delegate API forcing per-row allocation. The delegate itself is allocation-free; sort passes with a known one-line mitigation.
- S-2 did not fail with no workaround under ~10 days. It failed at the GPUI-native level but has a costed 5–7 day workaround, well under the ~10-day threshold `task.md` treats as the line for "not shippable."
- No spike surfaced a problem outside `design.md`'s own anticipated risk register (§7.4) — every finding below already had a named risk ID (R-G1…R-G9) or an accepted fallback shape.

**ADR-001 status: Accepted, unconditionally.** The "conditional on Phase 0 spikes S-1…S-6" clause in `design.md` §7.3 is now resolved — proceed with GPUI + gpui-component. Two items carry forward as tracked P0/P1 engineering costs rather than architecture risk (S-2's clipboard fallback, S-3's DnD-outbound defer), and one item (S-1's frame-time AC) carries forward as an **unverified assumption**, not a finding against GPUI — see below.

## Spike-by-spike verdicts

| Spike | Question | Verdict | Carries forward as |
|---|---|---|---|
| S-1 | 1M-row table at 120Hz | **Pass** (sort, RSS-growth, allocation-per-row) / **Inconclusive** (frame time — no 120Hz display in this environment) | T-4.2.1 must re-run this spike's harness on real 120Hz hardware before that task's AC is signed off. RSS baseline (~248MB before any data) is a real budget-eating fact for NFR-06, not a spike artifact. |
| S-2 | Clipboard custom MIME types | **Fail at GPUI level, pass with fallback** | `duet-platform`'s clipboard integration is now known-scope: ~5–7 engineer-days via `smithay-client-toolkit` + `wl_data_device`, not "figure it out during T-5.3.3." X11 (`x11rb`) support is the largest unbudgeted remainder — add explicitly to T-5.3.3's estimate. |
| S-3 | Cross-app drag & drop | **Pass (inbound)** / **Fail, deferred to P1 (outbound)** | Inbound needs no extra work — GPUI's `on_drop::<ExternalPaths>()` already works. Outbound is removed from the 1.0 scope per R-G3's own documented fallback ("degrade to P1-deferred if only intra-app DnD is achievable"). Recommend an upstream `gpui` contribution as the eventual real fix rather than a second Wayland connection hack. |
| S-4 | Directory enumeration strategy | **Pass**, with headroom | `d_type`-aware enumeration for name/type listing, parallel batched `statx` for full metadata — both confirmed 3.5–7x faster than naive stat-everything, and Phase 3's T-3.1.1/T-3.1.2 numeric targets clear with 2–7x headroom on this hardware. Feeds directly into T-3.1.1/T-3.1.2 as the chosen strategy, no further spike needed. |
| S-5 | Copy strategy ladder | **Pass**, io_uring **no-go** | FICLONE/copy_file_range/cp are all at parity on this hardware (btrfs reflink and coreutils already share the fast path). `fadvise(DONTNEED)` cuts page-cache growth 25–180x. io_uring's batched-submission prototype was *slower* than a plain ficlone/copy_file_range loop (~16.1k vs ~25.6k files/s) — the >15% win `design.md` §9.3 requires to adopt it did not materialize. Drop io_uring from the T-5.1.4 implementation plan; revisit only if a future prototype closes that gap. |
| S-6 | Text input / IME | **Pass** (paste, emoji/ZWJ, IME plumbing) / **Gap found** (RTL) / **Untestable here** (live CJK composition) | `gpui-component` 0.5.1's `Input` does not do BiDi visual reordering for RTL — this is a real product defect to fix or accept for 1.0, not a spike artifact; file it against whichever task owns the path bar (T-4.3.4) now rather than discovering it late. Live CJK composition is inherently untestable by any automated harness (it's IME-engine-mediated, several process hops from GPUI) — T-4.3.4's manual acceptance pass is the only place this can ever be verified, confirmed structurally, not just deferred by convenience. |
| S-7 | WASM plugin round-trip | **Pass**, both AC | Median 0.76–1.16µs/call (60–130x under the 100µs budget), epoch interruption kills a `loop {}` guest in ~2.0s as configured. No changes to the `duet-plugin` design in §9.9. Build tooling note: this sandbox lacked `rustup`/a prebuilt wasm32 sysroot — real dev machines need `rustup target add wasm32-wasip2`; not a project-level finding. |
| S-8 | Packaging shape | **Pass**, with gotchas | Flatpak and AppImage both build and run (process-level verified). Native `cargo build` output is glibc-incompatible with the Flatpak runtime — must build inside the Flatpak SDK sandbox (`rust-stable` extension), not on the host, which is a real Phase 11 packaging-pipeline requirement (T-11.1.1/T-11.1.3) to design in from the start. AppImage's hand-assembled AppDir doesn't bundle shared libs — acceptable since Duet doesn't depend on GTK/Qt/KDE, but confirm this holds once real system deps (e.g. archive codecs) are added. Container test itself (NFR-10) couldn't run here (no Docker/Podman access) — `ldd`/`strings` dependency analysis is a credible proxy but T-11.1.1/T-10.2.3 should still run a real container test before release. Baseline sizes: native 14.7MB / Flatpak 15.1MB / AppImage 6.4MB for a bare GPUI window — against NFR-09's ≤40MB target, roughly 40% of budget is spent before any application code exists. |

## Updates to carry into `design.md` and `task.md`

1. **§7.4 risk register** — see the diff applied to `design.md` alongside this report. R-G1 (unchanged, no churn observed but not exercised), R-G2 and R-G3 downgraded from "risk" to "known, costed work" with the numbers above. R-G8 gets a new, more precise sub-finding (RTL BiDi gap). A new line item is warranted for the S-1 RSS-baseline finding, which §7.4 didn't anticipate as its own risk (gpui-component's own init cost, not a scaling problem).
2. **NFR-05 (frame time)** is **not yet empirically validated** for this project — S-1 could only confirm GPUI's own per-cell render cost (~60µs/frame, far under budget) but not full display-refresh frame time, because no 120Hz output exists in this environment. This should be flagged as an open validation item for T-4.2.1 and T-10.3.1, not silently treated as passed.
3. **T-5.1.4 (copy strategy ladder)**: drop io_uring from the implementation plan per S-5; the ladder is FICLONE → copy_file_range → buffered+fadvise only for 1.0.
4. **T-5.3.3 (clipboard)**: re-estimate using S-2's 5-7 day figure plus an explicit X11 line item; the existing 5-day estimate in `task.md` is in the right range but was previously a guess, now it's measured.
5. **T-9.1.12 (drag & drop)**: scope outbound DnD out of 1.0 per S-3; either track an upstream `gpui` issue/PR or explicitly re-timebox a second attempt at the raw-Wayland-connection approach post-1.0.
6. **New backlog item**: RTL BiDi rendering gap in `gpui-component`'s `Input` (from S-6) — either an upstream fix/PR or a documented 1.0 limitation; needs an owner decision before T-4.3.4.
7. **T-11.1.1/T-11.1.3 (packaging)**: build Flatpak artifacts inside the SDK sandbox, not on the host, per S-8's glibc finding; budget a real container test (Docker/Podman) that this spike could not run.

## What Phase 0 did *not* resolve

- **NFR-05 frame time at 120Hz** — genuinely unverified, not merely "assumed fine." First real 120Hz-capable hardware available to the project should re-run S-1's harness before T-4.2.1 is signed off.
- **Firefox's exact drag MIME-type offering** (S-3 step 6) — the source-level verdict for inbound DnD doesn't depend on this, but it's real uncertainty that a live test would resolve if/when interactive tooling is available.
- **A real container test for NFR-10** (S-8) — proxy evidence only.
- **Live CJK IME composition** (S-6) — structurally deferred to a human acceptance pass, not resolvable by any spike.

None of these block G0 — they're each already correctly scoped to a later, more appropriate task rather than Phase 0's own timebox.
