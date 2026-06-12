# Proposal: OTBM Map Cache

## Intent

The server parses a ~114 MB OTBM map on **every** startup, performing a recursive tree walk, per-tile OTB lookups, and precomputation of blocked/floor_change/protection_zone/block_projectile/tile_height HashSets. This takes >30s and blocks accepting connections. The cache eliminates redundant parses when neither `map.otbm` nor `items.otb` has changed.

## Scope

### In Scope
- Binary snapshot of `StaticMap` via `bincode::serialize`/`deserialize`
- SHA-256 fingerprint of (map.otbm ‖ items.otb) as cache key
- Transparent load: cache hit → <1s deserialize, cache miss → parse + serialize
- `data/cache/map.<hash>.oxcache` storage path
- `tokio::task::spawn_blocking` for filesystem IO
- TDD: cache-miss round-trip test, cache-hit deserialization test, stale-fingerprint invalidation test

### Out of Scope
- LRU eviction or multiple cache entries (single map, one entry)
- Compression of the cache blob
- Watch-fs auto-invalidation (manual restart is fine)

## Capabilities

### New Capabilities
None — pure performance optimization, no spec-level behavior change.

### Modified Capabilities
None

## Approach

1. Add `serde` (present) + `bincode` (new dep) to `world` crate.
2. Derive `Serialize`/`Deserialize` on `StaticMap` and its transitive types (`TileStack`, `WireItem`, `FloorChange`, `Town`, `ItemMeta`).
3. Extract cache path logic into `data/cache/map.<sha256_hex>.oxcache`.
4. In `main.rs` startup: compute SHA-256 of concatenated (map.otbm bytes ‖ items.otb bytes), check for existing cache file, read via `spawn_blocking` + `bincode::deserialize`.
5. On miss: parse normally, then `spawn_blocking` + `bincode::serialize` to disk.
6. `load_item_metadata` runs after deserialization (not snapshot) since it depends on items.xml which is not part of the fingerprint.

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `crates/world/Cargo.toml` | Modified | Add `bincode`, `serde` (dep already in workspace) |
| `crates/world/src/map.rs` | Modified | Derive `Serialize`/`Deserialize` on `StaticMap` and field types |
| `crates/server/src/main.rs` | Modified | Cache check/hit/miss logic at startup (lines 64-91) |
| `crates/world/src/game/mod.rs` | None | No change — `Arc<StaticMap>` unchanged |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Serialization version drift (struct field changes break old caches) | Medium | SHA-256 key changes on items.otb/map.otbm change; manual `rm -rf data/cache` on structural code changes |
| `bincode` panics on unknown enum variants during deserialize | Low | `bincode::DefaultOptions` with `deserializer` — catch and fall back to full parse |
| Cache write fails (permissions, disk full) | Low | Log warning, continue — no crash |

## Rollback Plan

Delete `data/cache/` directory and restart the server. The code path falls through to the existing full parse. To fully revert the code, revert the commit and restart.

## Dependencies

- `bincode = "1"` (releases: `2` exists, but `1` is proven; the author should be explicit in `design`)

## Success Criteria

- [ ] Cold startup (no cache) produces `data/cache/map.<hash>.oxcache` on first run
- [ ] Warm startup (cache present) loads map in <1s (measured in test)
- [ ] Modifying `items.otb` produces a different cache key (miss → re-parse)
- [ ] Full round-trip test: parse → serialize → deserialize → assert structural equality
