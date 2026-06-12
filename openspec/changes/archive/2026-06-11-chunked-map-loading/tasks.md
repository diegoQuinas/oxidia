# Tasks: Chunked Map Loading

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | ~1000-1200 |
| 400-line budget risk | High |
| Chained PRs recommended | Yes |
| Suggested split | PR 1 (core types) → PR 2 (prechunker+boot) → PR 3 (game integration) |
| Delivery strategy | auto-chain |
| Chain strategy | stacked-to-main |

Decision needed before apply: Yes
Chained PRs recommended: Yes
Chain strategy: pending
400-line budget risk: High

### Suggested Work Units

| Unit | Goal | Likely PR | Base | Notes |
|------|------|-----------|------|-------|
| 1 | Chunk, ChunkManager, ChunkedMap, WorldMeta types + tests | PR 1 | main | Game unchanged; types coexist with StaticMap |
| 2 | prechunk binary, boot path, delete map_cache | PR 2 | main | Depends on PR 1 types; server boots with ChunkedMap |
| 3 | SweepChunks, teleport guard, pathfinding edge, game_service wiring | PR 3 | main | Depends on PR 1+2; changes Game to use ChunkManager |

## Phase 1: Core Types (PR 1)

- [x] 1.1 Add `ChunkId = (i16, i16, u8)`, `CHUNK_DIM = 256` to `map.rs`
- [x] 1.2 Add `Chunk` struct with tile data (chunk-relative coords), derive Serialize/Deserialize
- [x] 1.3 Add `WorldMeta` struct (spawn, towns, item_meta)
- [x] 1.4 Add `ChunkManager` with `chunk_id`, `ensure_loaded`, `tile_at`, `is_walkable`, `is_blocked`, `floor_change_at`, `sweep`, `pin`
- [x] 1.5 Add `ChunkedMap` wrapping `ChunkManager`, implement `TileSource`
- [x] 1.6 Change `MergedTiles.base` from `&StaticMap` to generic `B: TileSource` (supports both StaticMap and ChunkedMap)
- [x] 1.7 Update `world/src/lib.rs` re-exports for `Chunk`, `ChunkManager`, `ChunkedMap`, `WorldMeta`
- [x] 1.8 Test: Chunk bincode round-trip
- [x] 1.9 Test: ChunkManager::sweep keeps required, evicts orphans
- [x] 1.10 Test: `chunks_around(Position)` returns correct 27 ChunkIds
- [x] 1.11 `cargo test` green, `cargo clippy -D warnings` clean

## Phase 2: Pre-chunker + Boot (PR 2)

- [x] 2.1 Create `crates/server/src/bin/prechunk.rs` — parse OTBM, group tiles by ChunkId, serialize via bincode, write `data/chunks/{z}/{x}_{y}.chunk`
- [x] 2.2 Write `data/chunks/fingerprint` (SHA-256 of OTBM + items.otb)
- [x] 2.3 Add `prechunk` to workspace `Cargo.toml` as a `[[bin]]`
- [x] 2.4 Modify `main.rs`: remove OTBM parse, build WorldMeta, spawn with ChunkedMap
- [x] 2.5 Delete `crates/server/src/map_cache.rs`
- [x] 2.6 Test: prechunker produces correct chunk files from tiny_map fixture
- [x] 2.7 `cargo test` green, `cargo clippy -D warnings` clean

## Phase 3: Game Integration (PR 3)

- [x] 3.1 Change `Game.map: Arc<StaticMap>` → `Game.chunks: ChunkManager` in `game/mod.rs`
- [x] 3.2 Change `WorldHandle.map` → `WorldHandle.meta: Arc<WorldMeta>` in `game/mod.rs`
- [x] 3.3 Update all `self.map.*` tile-access call sites to `self.chunks.*` across game modules
- [x] 3.4 Add `Command::SweepChunks` variant, wire to `ChunkManager::sweep`
- [x] 3.5 Spawn 5s tokio interval in `spawn()`, emit `SweepChunks` each tick
- [x] 3.6 Modify `do_teleport`: call `ensure_loaded(destination_chunks)` before move commit
- [x] 3.7 Modify pathfinding `is_walkable` closure: trigger `ensure_loaded` within 20 tiles of chunk edge
- [x] 3.8 Change `build_enter_world_burst`: `&StaticMap` → `&WorldMeta`
- [x] 3.9 Update `game_service.rs` call sites: `world.map.as_ref()` → `world.meta.as_ref()`
- [x] 3.10 Pin spawn chunks in `ChunkManager` at boot (never unload)
- [x] 3.11 Test: teleport loads destination chunks (actor test)
- [x] 3.12 Test: login with prechunked map, sweep active
- [x] 3.13 `cargo test` green, `cargo clippy -D warnings` clean

## Open Items (ask user)

- Chain strategy: **stacked-to-main** (fast, independent merges) vs **feature-branch-chain** (PR 1→tracker, PR 2→PR1-branch, PR 3→PR2-branch, tracker merges to main). Stacked recommended for speed.
