# Design: Chunked Map Loading

## Technical Approach

Split the monolithic `StaticMap` (3.2 GB RAM) into 256×256-tile chunks per floor, loaded on demand by a `ChunkManager` inside the single-threaded game actor. A build-time pre-chunker reads OTBM once and writes `bincode` blobs to `data/chunks/{z}/{x}_{y}.chunk`. At runtime, each player's position drives a 3×3 chunk grid kept loaded; stale chunks are swept every 5s. `WorldHandle.map` retains only metadata (spawn, towns, item_meta).

## Architecture Decisions

| Decision | Option A | Option B (chosen) | Rationale |
|----------|----------|-------------------|-----------|
| Chunk granularity | 1024×1024 | **256×256** | 256 tiles matches viewport scale; 8×8 chunks cover 2048² map. Larger chunks lose memory savings; smaller cause file overhead (16× as many files). |
| Pre-chunker | Integrated in server boot | **Separate `[[bin]]`** | Keeps server startup fast (no OTBM parse). Chunks are build artifacts, like compiled assets. Server verifies chunk fingerprint at boot. |
| WorldHandle.map type | Keep `Arc<StaticMap>` | **`Arc<WorldMeta>`** (spawn + towns + item_meta) | Networking only needs `item_meta` for inventory encoding (game_service:162) and spawn position. Tile data accessed exclusively through actor commands. |
| Sweep interval | Per-tick | **5s tokio interval** | Per-tick would add ~200µs to every loop iteration for a cold-path operation. 5s is TFS-equivalent: enough to free memory without thrashing. |
| Chunk eviction policy | LRU with timestamps | **Required-set diff** | Actor already computes required chunks from player positions. LRU adds complexity without benefit — the required set IS the working set. |
| Chunk on-disk format | Custom binary | **bincode** (serde) | Already a workspace dep (OTBM cache). `Chunk` derives `Serialize/Deserialize`. One file per chunk — no need for index or multi-chunk archives. |

## Data Flow

```
Build-time:
  OTBM file ──► [prechunk binary] ──► data/chunks/{z}/{x}_{y}.chunk (bincode)

Runtime:
  data/chunks/ ──► ChunkManager::ensure_loaded(ChunkId)
                        │
                        ▼
  ChunkManager ──► ChunkedMap ──► StaticMap public API (is_walkable, tile_at, …)
                        │                    │
                        ▼                    ▼
  Game actor             TileSource trait    WorldHandle (item_meta only)
  (do_move, do_teleport, │                   (game_service: inventory encoding)
   pathfinding, sweep)   │
                         ▼
                   map_description::encode (0x64 wire)
```

## Chunk Struct

```rust
// world/src/map.rs — new type
struct Chunk {
    tiles: HashMap<(u8, u8), TileStack>,           // relative coords within chunk
    blocked: HashSet<(u8, u8)>,
    block_projectile: HashSet<(u8, u8)>,
    floor_change: HashMap<(u8, u8), FloorChange>,
    tile_height: HashMap<(u8, u8), u8>,
    protection_zone: HashSet<(u8, u8)>,
}
```

All keys are chunk-relative `(0..=255, 0..=255)`. `Chunk` serializes via `bincode`.

## ChunkManager API

```rust
struct ChunkManager {
    chunks: HashMap<ChunkId, Arc<Chunk>>,
    pinned: HashSet<ChunkId>,
}

type ChunkId = (u8, u8, u8);  // (floor_chunk_x, floor_chunk_y, z)
const CHUNK_DIM: u8 = 256;

impl ChunkManager {
    fn chunk_id(pos: Position) -> ChunkId;
    fn ensure_loaded(&mut self, ids: &[ChunkId]);
    fn tile_at(&self, pos: Position) -> Option<&TileStack>;
    fn is_walkable(&self, pos: Position) -> bool;
    fn is_blocked(&self, pos: Position) -> bool;
    fn floor_change_at(&self, x: i32, y: i32, z: i32) -> FloorChange;
    // …all current StaticMap accessors delegate here
    fn sweep(&mut self, required: &HashSet<ChunkId>);
    fn pin(&mut self, chunks: &[ChunkId]);
}
```

`ChunkManager` owns chunk loading from disk. The actor calls `ensure_loaded` before any tile access.

## Required-Chunk Set Computation

```
For each player at (px, py, pz):
  For dz in -2..=2:
    cz = chunk_z + dz (clamped 0..=15)
    For dcx in -1..=1, dcy in -1..=1:
      insert (cx + dcx, cy + dcy, cz)
```

This yields ~9 chunks per floor × 3 floors = ~27 chunks per player. For 100 players: ~2700 chunk ids (cost: 32 bytes each = ~86 kB).

## Sweep

A new `Command::SweepChunks` fires every 5s from a tokio `interval`. The Game handler computes the required set (all players), adds pinned spawns, and calls `chunk_manager.sweep(&required)`. `sweep` retains only chunk IDs in required ∪ pinned.

## Pre-chunker: `crates/server/src/bin/prechunk.rs`

```
$ cargo run --bin prechunk -- data/world/map.otbm data/items/items.otb
```

1. Parse OTBM into `OtbmMap` via `formats::otbm::parse`
2. Iterate `map.tiles`, compute `ChunkId` for each tile via `(x/256, y/256, z)` as `u8`
3. Group tiles by `ChunkId`, build `Chunk` struct for each group
4. Serialize each `Chunk` via `bincode`, write to `data/chunks/{z}/{x}_{y}.chunk`
5. Write `data/chunks/fingerprint` (SHA-256 of OTBM + items.otb)

## Teleport Integration

In `movement.rs:do_teleport`:

```rust
let dest_ids = chunks_around(to);  // 3×3 grid around destination
self.chunk_manager.ensure_loaded(&dest_ids);
```

Called synchronously before the position commit — the actor blocks on disk I/O for the load. Since teleports are rare and chunks are small (~2-4 kB each bincode-compressed), the latency is acceptable.

## Pathfinding Edge Guard

In `ChunkedMap::get_path_matching` (replaces `StaticMap::get_path_matching`): before each A* expansion step, check if the frontier node `(nx, ny)` is within 20 tiles of its chunk's edge. If so, call `ensure_loaded` on the adjacent chunk. The `is_walkable` closure passed to `pathfinding::get_path_matching` triggers this check.

## TileSource for ChunkedMap

```rust
impl TileSource for ChunkedMap {
    fn tile(&self, x: i32, y: i32, z: i32) -> Option<TileSlices<'_>> {
        let pos = Position::key(x, y, z)?;
        self.chunks.tile_at(pos)  // returns slices from the in-memory chunk
    }
    fn creature_stackpos(&self, x: i32, y: i32, z: i32) -> u8 { … }
}
```

`MergedTiles.base` changes from `&'a StaticMap` to `&'a ChunkedMap`.

## WorldHandle Change

```rust
pub struct WorldHandle {
    tx: mpsc::Sender<Command>,
    pub meta: Arc<WorldMeta>,  // was: pub map: Arc<StaticMap>
}

struct WorldMeta {
    spawn: Position,
    towns: Vec<Town>,
    item_meta: HashMap<u16, ItemMeta>,
}
```

`game_service:291` changes from `world.map.as_ref()` to `world.meta.as_ref()`. `build_enter_world_burst` takes `&WorldMeta` instead of `&StaticMap`.

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/server/src/bin/prechunk.rs` | **Create** | Build-time OTBM chunk splitter binary |
| `crates/world/src/map.rs` | **Modify** | Add `Chunk`, `ChunkManager`, `ChunkedMap`, `WorldMeta`. Keep `TileStack`, `ItemMeta`, `EquipSlot` as-is. Remove `StaticMap` fields replaced by chunks. |
| `crates/world/src/game/mod.rs` | **Modify** | `Game.map` → `Game.chunks: ChunkManager`. `WorldHandle` carries `Arc<WorldMeta>`. Add `Command::SweepChunks`. Spawn sweep interval task. |
| `crates/world/src/game/movement.rs` | **Modify** | `do_teleport`: call `self.chunks.ensure_loaded(dest_chunks)` before move. |
| `crates/server/src/main.rs` | **Modify** | Remove OTBM parse (pre-chunked). Load `WorldMeta` from chunk metadata. Pass `ChunkManager` instead of `Arc<StaticMap>` to spawn. |
| `crates/server/src/map_cache.rs` | **Delete** | Replaced by per-chunk files. |
| `crates/server/src/game_service.rs` | **Modify** | `&StaticMap` → `&WorldMeta` in `build_enter_world_burst`. |
| `crates/world/src/pathfinding.rs` | **Modify** | `get_path_matching` closure now triggers chunk preload on edge proximity. |
| `crates/formats/src/otbm.rs` | **None** | Unchanged. The pre-chunker reuses `parse()` as-is. |
| `crates/protocol/src/map_description.rs` | **None** | `TileSource` trait unchanged. |

## Testing Strategy

| Layer | What to Test | Approach |
|-------|-------------|----------|
| Unit | Chunk bincode round-trip | `Chunk` → serialize → deserialize → assert fields equal |
| Unit | ChunkManager::sweep keeps required, evicts orphans | Construct ChunkManager with 5 chunks, sweep with required set of 2, assert only 2 remain |
| Unit | Required-chunk set from position | `chunks_around(Position)` returns correct 27 ChunkIds |
| Unit | Pathfinding edge guard triggers ensure_loaded | Mock ChunkManager, verify adjacent chunks requested when frontier near edge |
| Integration | Teleport loads destination chunks | Actor test: login player, teleport to distant position, verify tile_at succeeds |
| Integration | Login with prechunked map | `spawn(ChunkWorker::new())` → 0 players → sweep → memory < 600 MB |
| E2E | N/A | Existing movement/LOS tests unchanged — chunking is transparent to protocol |

## Open Questions

- None. All design decisions resolve to documented code patterns.
