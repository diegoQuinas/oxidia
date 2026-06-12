# Design: OTBM Map Cache

## Technical Approach

On startup, compute SHA-256 of `(map_bytes ‖ items_bytes)` after both files are read. If `data/cache/map.<hex>.oxcache` exists, deserialize via `bincode` in a blocking thread. On parse cost — both fall back to the normal OTBM parse and persist the binary snapshot afterward. `item_meta` is excluded from the snapshot and always re-populated from `items.xml` after deserialization.

## Architecture Decisions

| Decision | Option A (chosen) | Option B | Tradeoff |
|----------|------------------|----------|----------|
| **Serde derive placement** | Derive on types in their home crates (`WireItem` in protocol, `FloorChange`/`Town` in formats, rest in world) | Custom `Serialize` impls only on `StaticMap` with manual mapping for foreign types | A: idiomatic, zero boilerplate. B: avoids adding serde dep to protocol/formats but introduces fragile manual mapping |
| **Cache checker location** | `server::map_cache` module | Inline in `main.rs` | Module keeps `main.rs` readable and enables future cache utilities (purge, stats) |
| **bincode version** | `bincode = "1"` (workspace, used by server crate) | `bincode = "2"` | 1.x is proven, no migration risk, simple API. 2.x changed config API |
| **SHA-256 input** | `sha2::Sha256::digest(map_bytes_owned ‖ items_bytes_owned)` | Hash of file paths or mtime | Content hash is deterministic across copies/restores; mtime fails on `git clone` |
| **Compression** | None (explicitly out of scope) | gzip/zstd | ~114 MB map ≈ similar cache size; adds dependency and 2-3s overhead. Premature optimization. |

## Data Flow

```
startup
  │
  ├── read items.otb bytes ──┐
  ├── read map.otbm bytes ───┤
  │                           ▼
  │              sha256(map ‖ items) → hex
  │                           │
  │              data/cache/map.<hex>.oxcache exists?
  │                  │                  │
  │               YES ▼              NO ▼
  │         spawn_blocking:        parse OTBM normally
  │         bincode::deserialize   (existing code path)
  │                │                    │
  │           success?            spawn_blocking:
  │           │     │             bincode::serialize → write
  │        YES ▼   NO ▼               │
  │      Arc::new   warn; parse       ▼
  │         │       normally      Arc::new
  │         ▼          │            │
  │      load_item_metadata ◄────────┘
  │         │
  │      Arc<StaticMap> → world::game::spawn
```

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `Cargo.toml` | Modify | Add `sha2 = "0.10"`, `bincode = "1"` to `[workspace.dependencies]` |
| `crates/server/Cargo.toml` | Modify | Add `sha2.workspace`, `bincode.workspace` (serde already present) |
| `crates/server/src/map_cache.rs` | Create | `try_load()` and `write()` helpers + `cache_path(map_hash: &str) -> PathBuf` |
| `crates/server/src/main.rs` | Modify | Insert cache check between reads and parse (lines 64-90); call `write_cache` on miss |
| `crates/world/Cargo.toml` | Modify | Add `serde.workspace = true` |
| `crates/world/src/lib.rs` | Modify | Derive `Serialize, Deserialize` on `Position`; add `use serde::...` |
| `crates/world/src/map.rs` | Modify | Derive `Serialize, Deserialize` on `StaticMap` (skip `item_meta`), `TileStack`, `ItemMeta`, `EquipSlot` |
| `crates/protocol/Cargo.toml` | Modify | Add `serde.workspace = true` |
| `crates/protocol/src/map_description.rs` | Modify | Derive `Serialize, Deserialize` on `WireItem` |
| `crates/formats/Cargo.toml` | Modify | Add `serde.workspace = true` |
| `crates/formats/src/items_xml.rs` | Modify | Derive `Serialize, Deserialize` on `FloorChange` |
| `crates/formats/src/otbm.rs` | Modify | Derive `Serialize, Deserialize` on `Town` |

## Interfaces / Contracts

**Cache file format**: `bincode`-encoded `StaticMap` with these serde attributes:

```rust
#[derive(Serialize, Deserialize)]
pub struct StaticMap {
    tiles: HashMap<(u16, u16, u8), TileStack>,
    // ... all fields derived normally ...
    #[serde(skip)]  // populated post-deserialize by load_item_metadata
    item_meta: HashMap<u16, ItemMeta>,
}
```

**Cache helper signatures** (`crates/server/src/map_cache.rs`):

```rust
/// Returns `Some(StaticMap)` on cache hit, `None` on miss or error.
pub async fn try_load(cache_path: &Path) -> Option<world::map::StaticMap>;

/// Serializes and writes the cache in `spawn_blocking`.
pub async fn write(map: &StaticMap, cache_path: &Path);
```

**Cache path**: `data/cache/map.{sha256_hex}.oxcache`. Directory `data/cache/` is created on first write if missing (existing `data/` directory is guaranteed).

## Testing Strategy

| Layer | What to Test | Approach |
|-------|-------------|----------|
| Unit | Round-trip: parse fixture → serialize → deserialize → assert `tiles`, `blocked`, `spawn` equal | `cargo test` in world crate; `tiny_map()` fixture from existing tests |
| Unit | Stale fingerprint: different items.otb → different cache path | Mock bytes; verify hex differs |
| Unit | `item_meta` skip: serialize with metadata, deserialize, assert `item_meta` is empty | world crate test |
| Integration | Cache miss: no `.oxcache` file → full parse → `.oxcache` written | `cargo test` in server crate; temp dir |
| Integration | Cache hit: pre-existing `.oxcache` → deserialize → no parse triggered | server crate; pre-staged cache file |
| Integration | Corrupt cache → fallback to full parse (graceful degradation) | Write garbage bytes, verify parse runs |

## Migration / Rollout

No migration. First run creates the cache. To rollback: delete `data/cache/` and restart — the server falls through to full parse. To fully revert, revert the commit.

## Open Questions

- [ ] Should we add `--no-cache` CLI flag for debugging? (Out of scope for this change; can be added later)
- [ ] `bincode` serializes `HashMap` as key-value pairs — iteration-order-dependent. This is correct for deserialization but means two parses of the same map produce byte-different cache files. Harmless since SHA-256 key is the same.
