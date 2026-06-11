# Archive Report: Click-to-Move Pathfinding

**Change**: click-to-pathfind
**Archived**: 2026-06-10
**Artifact Store**: Hybrid (OpenSpec + Engram)
**Verdict at Archive**: CONDITIONAL PASS (user-approved archive as-is)

---

## Delivered

The change implements server-side click-to-move pathfinding for the Tibia server:
- Parses `0x64` auto-walk packets, derives target destination from client-sent direction steps
- Computes server-side A\* path via `get_path_matching()` independent of client path
- Drains one direction per MonsterAiTick with proper creature-speed step-time gating
- Full state lifecycle: `go_to_position: Option<Position>` on PlayerState, cleared on arrival/PZ/ESC/manual move/failure

### Tasks
- **12/12 tasks** complete and marked `[x]`
- All implementation tasks verified in apply-progress and confirm-passing tests

### Build & Tests
- **467 tests passing**, 0 failed
- `cargo clippy -D` warnings: ✅ clean (post-verify fix applied for the `drop(p)` issue)

---

## Post-Verify Changes Applied

After the verify report was generated and before archiving, the user requested and two additional fixes were applied:

### 1. Step-Time Gating (Bug Fix)
- **What**: Added `last_walk_ms: Option<Instant>` field to `PlayerState`, a `step_time()` method computing creature-speed-adjusted step interval, and gating in the AI tick so auto-walk respects creature movement speed.
- **Why**: Player was walking every 100ms (one per AI tick) instead of the correct interval (~454ms for speed 220, per Tibia protocol). This was effectively teleportation.
- **Impact**: Auto-walk now moves at the correct speed — one step per `step_time()` interval, not one step per tick.

### 2. Removed "You have arrived." Message (Spec Drift)
- **What**: Removed the `0xB4` MESSAGE_INFO_DESCR "You have arrived." green status message on reaching the goto destination.
- **Why**: The user requested removal — no arrival message should be displayed.
- **Impact**: REQ-AW-05 now behaves differently from the written spec. The spec requires "You have arrived." but the code no longer sends it. **This is a documented intentional deviation.** The archive report records this divergence; the spec should be updated if the team decides to keep this behavior permanently.

---

## CONDITIONAL PASS — User Override Documentation

The verify report (2026-06-10) issued a **CONDITIONAL PASS** with 3 WARNING-level missing test scenarios:

| Req | Missing Test | Code Status |
|-----|-------------|-------------|
| REQ-AW-08 | Goto into PZ rejected | Code implements check in `do_go_to_position()` (PZ destination validation) |
| REQ-AW-09 | No path stops auto-walk | Code implements empty-path handling (clears goto + sends "There is no way.") |
| REQ-AW-09 | Repath limit terminates | Code implements 3-consecutive-failure termination in AI tick |

All three have correct code implementation — only automated test coverage is missing. No CRITICAL issues were found.

**User override**: The user explicitly chose to archive as-is with the CONDITIONAL PASS rather than adding the 3 missing tests. This override is recorded for audit trail purposes.

---

## Spec Sync

| Domain | Action | Details |
|--------|--------|---------|
| `player-auto-walk` | Created (new domain) | Full spec copied from delta to `openspec/specs/player-auto-walk/spec.md` |

No merge was needed — `player-auto-walk` did not previously exist as a main spec. The delta IS the full spec.

---

## Engram Artifact Lineage (for traceability)

| Artifact | Observation ID |
|----------|---------------|
| `sdd/click-to-pathfind/explore` | #392 |
| `sdd/click-to-pathfind/proposal` | #393 |
| `sdd/click-to-pathfind/spec` | #394 |
| `sdd/click-to-pathfind/design` | #396 |
| `sdd/click-to-pathfind/tasks` | #397 |
| `sdd/click-to-pathfind/verify-report` | #403 |
| `sdd/click-to-pathfind/archive-report` | (this — newly saved) |

---

## Archive Contents

```
openspec/changes/archive/2026-06-10-click-to-pathfind/
├── exploration.md       ✅
├── proposal.md          ✅
├── specs/
│   └── player-auto-walk/
│       └── spec.md      ✅
├── design.md            ✅
├── tasks.md             ✅ (12/12 complete)
├── verify-report.md     ✅
└── archive-report.md    ✅ (this file)
```

---

## Risks & Notes

1. **Spec drift on REQ-AW-05**: Arrival message removed post-verify. The main spec at `openspec/specs/player-auto-walk/spec.md` still requires it. If the no-arrival-message behavior is to be permanent, the spec should be updated via a follow-up change.
2. **3 untested scenarios**: The user chose to archive without them. These are low-risk — all three have correct code — but a future change should add tests for completeness.
3. **Step-time gating** is new post-verify code. While it passes all 467 tests, it was not independently verified by the verify phase. A regression risk exists (low — the change is simple: timestamp check before moving).
