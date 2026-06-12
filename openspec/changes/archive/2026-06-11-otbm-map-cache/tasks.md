# Tasks: OTBM Map Cache

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | ~200–260 |
| 400-line budget risk | Low |
| Chained PRs recommended | No |
| Suggested split | Single PR |
| Delivery strategy | single-pr |

Decision needed before apply: No
Chained PRs recommended: No
Chain strategy: size-exception
400-line budget risk: Low

### Suggested Work Units

| Unit | Goal | Likely PR | Notes |
|------|------|-----------|-------|
| 1 | All phases below | PR 1 | Single PR, ~200–260 lines |

## Phase 1: Dependencies

- [x] 1.1 Add `sha2 = "0.10"`, `bincode = "1"` to workspace `Cargo.toml` `[workspace.dependencies]`
- [x] 1.2 Add `sha2.workspace`, `bincode.workspace` to `crates/server/Cargo.toml`
- [x] 1.3 Add `serde.workspace = true` to `crates/world/Cargo.toml`
- [x] 1.4 Add `serde.workspace = true` to `crates/protocol/Cargo.toml`
- [x] 1.5 Add `serde.workspace = true` to `crates/formats/Cargo.toml`

## Phase 2: Serde Derives

- [x] 2.1 Derive `Serialize, Deserialize` on `Position` in `crates/world/src/lib.rs`; add `use serde::{Serialize, Deserialize}`
- [x] 2.2 Derive `Serialize, Deserialize` on `StaticMap` (`#[serde(skip)]` on `item_meta`), `TileStack`, `ItemMeta`, `EquipSlot` in `crates/world/src/map.rs`
- [x] 2.3 Derive `Serialize, Deserialize` on `WireItem` in `crates/protocol/src/map_description.rs`
- [x] 2.4 Derive `Serialize, Deserialize` on `FloorChange` in `crates/formats/src/items_xml.rs` and `Town` in `crates/formats/src/otbm.rs`

## Phase 3: Cache Module

- [x] 3.1 Create `crates/server/src/map_cache.rs` with `try_load()`, `write()`, and `cache_path()` — `spawn_blocking` for IO, SHA-256 of (map.otbm ‖ items.otb), `bincode` serialize/deserialize, `#[serde(skip)] item_meta` repopulated after deserialization

## Phase 4: Wire Cache into Startup

- [x] 4.1 Modify `crates/server/src/main.rs` — insert cache check after reads and before parse; call `write()` on cache miss; add `mod map_cache`

## Phase 5: Tests

- [x] 5.1 Unit: round-trip test — parse fixture → serialize → deserialize → assert `tiles`, `blocked`, `spawn` match (world crate, using `tiny_map()`)
- [x] 5.2 Unit: stale fingerprint test — different items.otb bytes → different hex hash
- [x] 5.3 Unit: `item_meta` skip test — serialize with metadata, deserialize, assert `item_meta` is empty
- [x] 5.4 Integration: cache miss — no `.oxcache` → full parse → `.oxcache` written (server crate, temp dir)
- [x] 5.5 Integration: cache hit — pre-staged `.oxcache` → deserialize → no parse triggered
- [x] 5.6 Integration: corrupt cache → fallback to full parse (graceful degradation)
