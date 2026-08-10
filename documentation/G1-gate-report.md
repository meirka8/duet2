# G1 Gate Report — Requirements Baseline

| Field | Value |
|---|---|
| Document ID | DUET-G1-001 |
| Gate | G1 (`design.md` approval gates table) |
| Consolidates | T-1.2.1, T-1.3.1, T-1.4.1, T-1.4.2, T-1.5.1, T-1.6.1 |
| Date | 2026-08-11 |
| Status | Final |

## Decision: **Pass, with two explicitly scoped gaps carried forward**

Gate G1's exit criteria (`task.md`, Phase 1 header) are: *"`design.md` §4–§6 frozen; every requirement has a priority, an owner phase, and an acceptance test sketch; the keymap appendix is complete."* Below is where each clause stands, stated plainly rather than rounded up.

| Clause | Status |
|---|---|
| Every requirement has a priority | **Met.** The NFR table (§6.8) was missing a `Pri` column entirely before this pass — added and justified per-row. All FR-* rows already had one; reviewed and confirmed against personas, none deleted. |
| Every requirement has an acceptance test sketch | **Partially met, by design.** T-1.3.1's own AC scopes this to **P0 only** ("100% P0 coverage"), which is what was delivered (`docs/acceptance-sketches.md`, every P0 row across FR-NAV/SEL/OPS/VFS/TOOL/CFG and P0 NFRs). The phase-level exit criteria's wording ("every requirement") is broader than the task-level AC that operationalizes it — this is a pre-existing inconsistency in the WBS, not a shortfall introduced here. P1/P2 requirements do not yet have sketches; flagged as a real gap rather than silently satisfied. |
| Keymap appendix is complete | **Functionally complete, verification-limited.** Appendix A now exists in `design.md` with 151 bindings (target ≥150), sourced from `docs/keymap-tc.csv`. Of those, 102 are `known` confidence, 27 `inferred`, 22 `uncertain` — because T-1.1.1 (the hands-on TC field study this task formally depends on) was explicitly skipped for this pass (see the deviation note at the top of Phase 1 in `task.md`). **This needs a human with real TC 11 access to spot-check the `inferred`/`uncertain` rows before it can be called truly verified.** |

None of these gaps block moving into Phase 2 — architecture work doesn't depend on the unresolved P1/P2 sketches or the unverified keymap rows, and both are cleanly scoped for later (P1/P2 sketches can be written incrementally as those tasks come up in later phases; keymap verification is a good candidate for the first person with a Windows/Wine TC install to spend an hour on).

## What each task produced

### T-1.2.1 — Requirement review pass
Reviewed all of `design.md` §6 against the four personas (§3). No requirement deleted — the existing draft's priorities were already well-justified, tracking the real domain knowledge already embedded in §5's competitive analysis. Two corrections: the missing NFR `Pri` column (above), and NFR-11 split into its two practically-independent halves (keyboard-completeness promoted to P0 — it's load-bearing for both P1 and P3 — with screen-reader support remaining the documented P2 gap it already was via R-G4).

### T-1.3.1 — P0 acceptance-test sketches
`docs/acceptance-sketches.md`. One-or-two-sentence sketches for all P0 rows. Several sketches explicitly cross-reference open questions the G0 report already surfaced rather than re-asserting them as settled — e.g. NFR-05's sketch notes the still-unverified 120Hz frame-time claim, NFR-06's notes S-1's ~248MB gpui-component baseline, NFR-10's notes that only proxy (not container) evidence exists yet. This keeps the acceptance sketches honest about what's actually been shown versus assumed.

### T-1.4.1 / T-1.4.2 — TC keymap extraction + Duet Appendix A
`docs/keymap-tc.csv` (151 bindings, confidence-graded) and `design.md` Appendix A. **Notable finding: zero forced key remaps.** Every Linux-convention conflict the WBS anticipated (Ctrl+C/V/X, Ctrl+W, F10) resolved via context-scoping or turned out not to be a real conflict once TC's actual default was checked, rather than needing Duet to deviate from TC:

- **Ctrl+C/V/X**: adopted as-is, context-scoped — file-clipboard semantics when a panel has focus, ordinary text-clipboard semantics inside text fields (command line, rename, path bar). GPUI's context-predicate system already supports this distinction natively.
- **Ctrl+W**: adopted as-is — TC's own binding (`tab.close`) already matches the Linux/browser convention. There was no real conflict to resolve.
- **F10**: adopted as-is (`menu.activate`). Two caveats documented rather than remapped: `mc.toml` (the alternate base keymap) needs its own `F10=Quit` binding per mc's convention, and GNOME Terminal's habit of intercepting F10 is relevant once T-9.1.11's embedded terminal exists.
- **Ctrl+A** (found during the pass, not originally flagged): TC's own binding is Change Attributes, not select-all. Adopted as-is per persona P1's "wrong default keybinding is a bug" standard, resolved via the same context-scoping approach (Ctrl+A means select-all only inside text fields), with a suggested first-run tooltip for newcomers expecting the Linux-wide convention.

Two adopted-as-is bindings are flagged as onboarding friction worth documenting prominently (not changing): **F2** is Refresh, not Rename (TC's actual default), and **F5** is Copy, not Refresh — the latter is also the strongest existing argument for a high-contrast copy/move confirmation affordance, tying into the destructive-action UX concerns `ux-psychologist`'s role already covers.

### T-1.5.1 — Command catalogue
`docs/commands.md`, 307 commands across 22 categories (target ≥200). Every command id already named in `design.md` §9.4's keymap extract table resolves to a catalogued entry, verified programmatically with a traceability table in the doc itself. Includes placeholder id patterns for all five FR-PLUG-02 plugin classes (`plugin.command.<id>`, `plugin.content.<id>`, etc.) so plugin-registered commands have a defined namespace from day one.

### T-1.6.1 — Config schema draft
`docs/config-schema.md`. Four schemas (`settings.toml` — 12 groups, ~40 keys; `keymap.toml` + base-file layering; `connections.toml` — profile metadata only, no secrets, per §9.10's keyring-only policy; a ~35-token theme palette plus 8 spacing tokens), each with a TOML example that was actually parsed (not eyeballed) to confirm validity. One inconsistency was caught and fixed during this consolidation: the schema branch was cut before FR-NAV-13 (the fuzzy quick-search spec) landed, so it originally used a guessed 1000ms timeout citing FR-NAV-07 — corrected to FR-NAV-13's canonical 1200ms default before merging.

## Carry-forward items for later phases

1. **Keymap verification**: spot-check the 49 `inferred`/`uncertain` rows in `docs/keymap-tc.csv` against a real TC 11 install. Good candidate for whenever someone on the project has Wine/TC access; not a blocker for Phase 2.
2. **P1/P2 acceptance sketches**: not yet written. Lower urgency — T-10.4.1 (the full acceptance pass) is Phase 10, and these can be filled in incrementally.
3. **F5=Copy / F2=Refresh onboarding friction**: no action needed for the requirements/architecture phases, but worth remembering for whichever task ends up owning first-run UX (nothing currently explicitly owns a "first-run tooltip" or onboarding pass — worth a WBS addition when Phase 9's polish work is scoped).
4. **FR-NAV-13 (fuzzy quick-search)**: fully specified in this pass (design.md §6.1, §9.2 implementation note, T-4.3.3's AC updated, config schema updated). No further Phase 1 work needed; ready for Phase 4 implementation.
