# Archive Report: Pathfinding Should Avoid Holes

**Status**: COMPLETE
**Archived**: 2026-06-11
**Archive reason**: Normal SDD cycle completion — implemented, verified, all tasks complete.

---

## Summary

The A* pathfinder now rejects floor-change tiles (holes, pits) in its walkability closure, matching TFS `queryAdd(FLAG_PATHFINDING)` behavior. Two production lines changed: `get_path_matching()` closure in `map.rs` and `do_move_monster()` step filter in `movement.rs`. 5 new tests written. 528 tests pass, clippy clean.

## Engram Artifact Observation IDs

| Artifact | Observation ID |
|----------|---------------|
| Proposal | #531 |
| Spec | #533 |
| Design | #532 |
| Tasks | #534 |
| Apply-progress | #535 |
| Verify-report | #537 |
| Archive-report | *(this observation)* |

## Specs Synced

| Domain | Action | Details |
|--------|--------|---------|
| player-auto-walk | Modified | REQ-AW-02 updated: floor-change rejection added + 2 new scenarios (floor-change destination rejected, floor-change tile avoided mid-path) |
| pathfinding | Already up to date | REQ-PF-01, REQ-PF-02, REQ-PF-03 already present in main spec |

## Archive Contents (filesystem)

- `openspec/changes/archive/2026-06-11-pathfinding-avoid-holes/proposal.md`
- `openspec/changes/archive/2026-06-11-pathfinding-avoid-holes/specs/player-auto-walk/spec.md`
- `openspec/changes/archive/2026-06-11-pathfinding-avoid-holes/design.md`
- `openspec/changes/archive/2026-06-11-pathfinding-avoid-holes/tasks.md`
- `openspec/changes/archive/2026-06-11-pathfinding-avoid-holes/verify-report.md`
- `openspec/changes/archive/2026-06-11-pathfinding-avoid-holes/exploration.md`
- `openspec/changes/archive/2026-06-11-pathfinding-avoid-holes/archive-report.md`

## Task Completion

All 9/9 tasks marked `[x]` in tasks artifact. Verify report confirmed all tasks complete with no CRITICAL issues.

## Verification Results

- **Status**: PASS
- **Tests**: 528 passed, 0 failed
- **Clippy**: Clean
- **Production changes**: 2 lines (map.rs + movement.rs)
- **New tests**: 5 (1 unit map.rs, 2 unit movement.rs, 2 integration mod.rs)

## Source of Truth Updated

- `openspec/specs/player-auto-walk/spec.md` — REQ-AW-02 updated with floor-change rejection

## Risks

None. Minimal production change (2 lines), well-tested at unit and integration levels, manual step behavior unaffected.

## Intentional Warnings

None. Clean archive with no stale checkboxes or partial artifacts.
