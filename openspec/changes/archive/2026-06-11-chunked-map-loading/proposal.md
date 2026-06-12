# Proposal: Chunked Map Loading

## Intent

Full 1098 OTBM map (~2048×2048, 4-5M tiles) takes ~3.2 GB RAM in `StaticMap` because EVERY tile is kept in HashMaps/HashSets — ground items, blocked, floor_change, tile_height, protection_zone, block_projectile. With 0 players. This change splits the map into 256×256 chunks per floor and loads/unloads by player proximity, targeting ~200-600 MB at runtime.

## Scope

### In Scope
- `ChunkManager` actor: owns all chunks, exposes `&TileStack`/`&HashSet` lookups, sweep unloads every 5s
- OTBM pre-chunk at build time: per-chunk binary files in `data/chunks/{z}/{x}_{y}.chunk`
- Per-player 3×3 chunk grid (~27 with floors) loaded by proximity; teleports sync-load destination
- Pin spawn chunks (never unload); preserve `Arc<StaticMap>` lock-free assumption
- Pathfinding edge guard: preload neighbor chunks within 20 tiles of current boundary

### Out of Scope
- Partial OTBM re-parse at runtime (pre-chunk at build time only; server restart to pick up map changes)
- LRU eviction or chunk compression
- Ground-item overlay (`dynamic` HashMap in `Game`) — unchanged, stays on the actor
- File-watch auto-reload of chunks

## Capabilities

### New Capabilities
None — pure architecture change; all spec-level behavior (tile lookup, map encoding, pathfinding, LOS, PZ) remains identical.

### Modified Capabilities
None

## Approach

1. **Build-time pre-chunk**: extend `formats::otbm` with `parse_tile_area` for single-chunk OTBM extraction. Write each chunk as `bincode` blob to `data/chunks/{z}/{x}_{y}.chunk`.
2. **`ChunkManager`**: new struct in `world::map` holding loaded chunks (`HashMap<(i32,i32,u8), Chunk>`). Each `Chunk` mirrors the per-tile HashMaps/HashSets (blocked, floor_change, etc.) for one 256×256 area.
3. **Proximity load**: each player tracks their 3×3 visible chunk grid. On move/login/teleport, call `ensure_loaded` for any newly visible chunk. `Arc<Chunk>` refs are shared — no `RwLock`.
4. **Sweep**: periodic 5s timer unloads chunks not referenced by any player's grid.
5. **Spawn pin**: chunks containing spawn/temple towns are pinned in `ChunkManager` at boot.
6. **Pathfinding guard**: `get_path_matching` preloads neighbor chunks when within 20 tiles of chunk edge.
7. **Teleport**: `do_teleport` calls `ensure_loaded(destination)` synchronously before the move.
8. **Lookup adapters**: `StaticMap` accessors (has_ground, is_blocked, tile_at, etc.) delegate to `ChunkManager::resolve(pos)`.

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `crates/world/src/map.rs` | MAJOR | Add `ChunkManager`, chunk structs, proximity logic. Keep `StaticMap` types for backward compat or adapt to chunk reads |
| `crates/formats/src/otbm.rs` | MODERATE | Add `parse_tile_area` for chunk-scoped OTBM parsing |
| `crates/server/src/main.rs` | MODERATE | Build-time pre-chunk pass; pass `ChunkManager` instead of `Arc<StaticMap>` |
| `crates/server/src/map_cache.rs` | MAJOR | Replace with per-chunk cache read/write |
| `crates/world/src/game/mod.rs` | MODERATE | `Game::map` type changes; `WorldHandle` exposes chunk map |
| `crates/world/src/game/movement.rs` | MINOR | `do_teleport` adds `ensure_loaded` call |
| `crates/world/src/pathfinding.rs` | MINOR | Edge guard preloads neighbor chunks |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Pathfinding across chunk boundaries misses tiles | Medium | Preload neighbors when within 20 tiles of edge; 3×3 grid covers viewport (18×14) |
| Teleport to unloaded chunk causes delay | Low | Sync `ensure_loaded` before move — additive latency measurable but acceptable |
| Pre-chunk format drift vs runtime struct | Low | Hash fingerprint per chunk; rebuild on version mismatch |

## Rollback Plan

Revert the commit and restart. The existing `data/cache/map.*.oxcache` + `StaticMap`-from-formats path still works — no data migration needed.

## Dependencies

- Current `bincode` dep already in workspace (from OTBM cache)
- Pre-chunk build step needs OTBM file access at build time

## Success Criteria

- [ ] Server with 0 players peaks at ≤600 MB RSS (vs 3.2 GB today)
- [ ] Single player standing still keeps exactly 3×3 floor-grid chunks loaded
- [ ] Teleport between distant chunks loads destination chunks on arrival
- [ ] `cargo test` green, `cargo clippy -D warnings` clean
