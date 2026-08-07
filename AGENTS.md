# Duet Project — Agent Delegation, Git Workflow & Architecture Playbook

## Executive Summary
Duet is a GPU-accelerated, keyboard-first orthodox file manager for Linux (`design.md`) built using Rust and GPUI[cite: 1]. High-level planning follows the waterfall WBS defined in `task.md`[cite: 2].

---

## 1. Subagent Roster & Roles

When decomposing goals into sub-tasks, delegate work to these specialized subagents using `invoke_subagent`:

| Agent | Specialty & Primary Crate Scope | Key Directives |
|---|---|---|
| `@backend-rust` | Systems Rust, `duet-vfs`, `duet-ops`, `duet-index`, `duet-platform`[cite: 1] | UI-thread blocking guard (`T-3.1.6`), crash-safe journaling (`FR-OPS-07`), SoA memory layout (`NFR-06`)[cite: 1, 2]. |
| `@frontend-gpui` | GPUI UI, `duet-ui`, `duet-widgets`[cite: 1] | Virtualized table rendering ($120\text{ Hz}$), zero per-frame allocation during scroll, TC keybindings (`FR-CFG-02`)[cite: 1, 2]. |
| `@ux-psychologist` | Human factors, visual contrast, progress estimation | Reduces user friction, ensures dark/light contrast, keeps operation ETAs smooth, enforces Total Commander muscle-memory fidelity[cite: 1]. |
| `@qa-chaos` | Testing, benchmarks, fault injection, `tests/`, `benches/`[cite: 1] | Data-safety verification (`NFR-08`), SIGKILL/ENOSPC injection, VFS conformance runs, performance regression checks[cite: 1, 2]. |
| `@plugin-wasm` | WASM Sandbox, `duet-plugin`, WIT specifications[cite: 1] | Wasmtime host integration, zero-ambient-authority capability model, fuel/epoch execution caps[cite: 1]. |

---

## 2. Volute MCP Thought Tracking Directive

To maintain cognitive context across complex multi-step refactors, long-running agent tasks, and phase transitions:

1. **Continuous Intent Logging:** Whenever taking on a non-trivial architectural decision, starting a new WBS task (`T-x.x.x`), or evaluating a trade-off, call the Volute MCP tool to log your immediate thoughts, intentions, and structural reasoning.
2. **Phase Boundary Summaries:** At the completion of each task or milestone, log a Volute thought summarizing what was accomplished, any technical debt introduced, and instructions for the next agent session.
3. **Branch/Context Tagging:** Ensure thought entries reference the active git branch or phase task ID so architectural decisions remain traceable.

---

## 3. Git & Pull Request Policy

All agents and main orchestrators must strictly adhere to this Git workflow:

### Feature Branches & Commit Granularity
* **Branch Strategy:** Never commit directly to `main`/`master`. Create dedicated feature or task branches using the pattern `feature/phase-<N>-<task-id>` (e.g., `feature/phase-3-T-3.1.1`).
* **Commit Granularity:** Keep commits reasonably sized, logical, and self-contained. Do not bundle un-related changes into massive "kitchen-sink" commits. Each commit message should follow Conventional Commits (e.g., `feat(vfs): implement getdents64 streaming in LocalFs`).

### Pull Requests & Gate Reviews
* **GitHub CLI (`gh`):** Use the GitHub CLI (`gh pr create`) to open pull requests.
* **PR Scope:** Feature PRs into phase integration branches can cover related task clusters, but PRs targeting the primary branch (`main`) **must align with phase gates (G0, G1, G2, G3, etc.)**.
* **Phase Gate Discipline:** 
  * Reaching a phase gate (e.g., Gate G3 for Alpha Build) **requires a single, consolidated Master PR** into `main`.
  * **Human Review Gate:** No phase PR may be merged into `main` automatically. The agent must assemble the PR, verify CI checks pass, tag the user for manual review, and wait for human approval before proceeding to the next phase.

---

## 4. Architectural Boundaries & Non-Negotiables

1. **Isolation Rule (ADR-002):** Only `duet-ui` and `duet-widgets` may import `gpui` or `gpui-component`[cite: 1]. The core engine must remain completely UI-agnostic[cite: 1].
2. **UI Thread Discipline:** Never run disk I/O, `stat` calls, network requests, or blocking syscalls on the UI thread[cite: 1]. Always assert the non-blocking guard (`T-3.1.6`)[cite: 2].
3. **Data Safety First:** File modifications must be written through append-only journals (`~/.local/state/duet/jobs/*.journal`) and partial staging files[cite: 1].
4. **TC Priority (Persona P1):** Default keyboard shortcuts must match Total Commander 11 unless a Linux desktop conflict forces a documented deviation in `design.md` Appendix A[cite: 1].

---

## 5. Delegation Playbook for `/goal` Tasks

When the user provides a `/goal` prompt:

1. **Phase Check:** Cross-reference `task.md` to identify which phase and task IDs (`T-x.x.x`) the goal maps to[cite: 2].
2. **Log Intent:** Record the initial task goal and strategy via the **Volute MCP**.
3. **Branch Creation:** Ensure a feature branch is checked out before making edits (`git checkout -b feature/...`).
4. **Plan & Delegate:**
   - **Data/Backend Logic:** Delegate file structures, VFS traits, or operations planning to `@backend-rust`.
   - **UI Views & Keybindings:** Delegate panel layouts, table delegates, or command palettes to `@frontend-gpui`.
   - **Extension Interfaces:** Delegate WIT specs or WASM sandboxing to `@plugin-wasm`.
   - **UX & Safety Review:** Have `@ux-psychologist` review visual contrast or friction points on complex dialogs.
   - **Testing:** Have `@qa-chaos` write unit, conformance, or fault-injection tests before marking a task complete.
5. **Completion Gate:**
   - Ensure small, logical commits are made during execution.
   - Run Volute MCP to capture closing thoughts and summaries.
   - Open a PR via `gh pr create` and request user review when reaching a Phase Gate.