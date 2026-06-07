# M9 — Ground items, stacks, look-at (design)

> Open Tibia server in Rust, protocol **10.98**, client **OTClient Redemption**.
> TFS 1.4.2 (`reference/tfs/`) is the spec oracle — read it, never port line-by-line.
> Architecture: single-actor world (`world::game::GameWorld`), one writer, no locks.

## Scope

Roadmap M9 = "Ground items, stacks, look (examine)". Exploration found that
**static item rendering already works**: `StaticMap` stores the full per-tile
stack (ground + top + down) from the `.otbm`, the encoder serializes it with the
10-thing cap, and the stackable count byte is already on the wire
(`map_description.rs:235`). So M9 reduces to three real pieces:

1. **Item metadata** — load `name`, `article`, `plural`, `description`, `weight`
   so look-at can describe items.
2. **Real stack counts** — parse `OTBM_ATTR_COUNT` from the `.otbm` so gold piles
   render and read with their true count (today every static stackable is `1`).
3. **Look-at (examine)** — inbound `0x8C` (look at a tile thing) and `0x8D`
   (look in battle list), resolve the thing, build the TFS-faithful "You see…"
   text, reply with `0xB4` `MESSAGE_INFO_DESCR`.

Look-at covers **both items and creatures** (resolve the thing at the clicked
stackpos). Creature level is `1` and vocation is "none" until M14 — the flow is
wired, the values fill in later.

### Decisions (locked with roadmap owner)

- **Look-at depth: full TFS** — `"You see <name>. <description>. It weighs X oz."`
  plus GM debug (item id + position).
- **Items + creatures** — resolve the thing at the stackpos; describe whichever.
- **Real `.otbm` counts parsed now** — threaded to both the wire and the look text.
- **`0x8D` (look-in-battle-list) included** — cheap, reuses the creature text.

## Architecture (locked: Enfoque 1)

Look-at logic lives in the **world actor**. It already owns both halves of the
state a look needs — the `StaticMap` (tile stacks, via `Arc`) and the player
registry (creatures + positions). A new `Command::LookAt` enters the same mpsc
the rest of the actor uses; `do_look` builds the text and pushes `0xB4`, exactly
mirroring `do_say` / `do_set_target`. No new locks, single packet builder intact.

The reader side (`game_service::reader_loop`) cannot build the text alone — it
would need a round-trip to the actor for any creature — so it only parses the
opcode and forwards a command. The **protocol crate stays pure wire** (parse the
inbound packet, encode the outbound `0xB4`); the **text assembly lives in
`world`** because it needs the item metadata catalog.

## A. Data layer (`formats`)

**Where the metadata lives (decided during planning):** the look-at fields all
come from `items.xml`, so they go in **`ItemXmlAttrs`** — NOT on `ItemType`.
`ItemType` has ~30 struct-literal construction sites across the test suite;
adding fields there would force churn on every one and break the build mid-edit.
The only `ItemType` addition is an `is_pickupable()` **method** over the existing
`flags` word (no new field, no churn), mirroring `is_stackable()`.

### `ItemXmlAttrs` (items_xml.rs) gains

| Field | Source | Notes |
|---|---|---|
| `name: String` | `<item name=…>` element attr | empty → look falls back to `"an item of type <id>"` |
| `article: String` | `<item article=…>` element attr | `"a"` / `"an"`; omitted for stackable>1 |
| `plural: String` | `<item plural=…>` element attr | used when stackable & count>1 |
| `description: String` | `<attribute key="description">` child | shown only at lookDistance ≤ 1 |
| `weight: u32` | `<attribute key="weight">` child | **hundredths of an oz** (TFS stores ×100) |
| `show_count: bool` | `<attribute key="showcount">` child | default `true`; gold-style "N name" |

`name`/`article`/`plural` are **element attributes** on `<item>`; `description`/
`weight`/`showcount` are `<attribute key=… value=…>` children. `weight` in
`items.xml` is already in hundredths — store the raw integer.

### `ItemType::is_pickupable()` (otb.rs)

Add `FLAG_PICKUPABLE = 1 << 5` and `pub fn is_pickupable(&self) -> bool` (weight
is shown in look only for pickupable items). `is_stackable()` / `group` already
exist.

### `items_xml` loader (items_xml.rs)

Extend `parse_items_xml` to read the `<item>` element attributes `name`,
`article`, `plural` and the `<attribute>` children `description`, `weight`,
`showcount`, storing them per server-id alongside `floor_change`. No `merge`
change is needed — the catalog (§B) reads `ItemsXml` directly.

### OTBM count (otbm.rs)

`MapItem` gains `count: Option<u8>`. In `parse_item`, after reading the id,
walk the item node's remaining attribute bytes and capture `OTBM_ATTR_COUNT`
(`= 15`): `[u8 attr][u8 count]`. Unknown attrs keep being skipped. (Today
`parse_item` ignores all trailing attribute bytes — this adds a minimal attr
loop, matching the existing tile-attr pattern.)

## B. World layer (`world`)

### Tile identity + count threading (map.rs)

Each `TileStack` keeps a **`server_ids: Vec<u16>`** parallel to its `items`
(`Vec<WireItem>`), populated in `from_formats_with_spawn` — the build loop
already has each `MapItem.id` (= server id) in hand, so no signature change is
needed. Look-at indexes this to fetch metadata unambiguously (client_id is not
1:1 with server_id). Thread the parsed `MapItem.count` into `WireItem.subtype`
for stackable items (replacing the current hard-coded `1`); splash/fluid stay
`0` until M10/M15.

### `ItemMeta` catalog in `StaticMap` (map.rs)

`StaticMap` gains `item_meta: HashMap<u16, ItemMeta>` (empty by default).
`ItemMeta { name, article, plural, description, weight, show_count, stackable,
pickupable }` combines `items.xml` text with the `items.otb` flags. It is
populated by a **new chainable method** `load_item_metadata(&mut self, otb:
&ItemsOtb, xml: &ItemsXml)` so `from_formats_with_spawn`'s signature (84 call
sites) is untouched — only `main` calls it at boot; look-at tests call it
explicitly with a small fixture. The actor reads it via `self.map.item_meta(sid)`.

### `Command::LookAt` + `do_look` (game.rs)

`Command::LookAt { player_id, pos, stackpos }`:

1. **Resolve the thing** at `(pos, stackpos)` — STACKPOS_LOOK semantics under the
   ≤1-creature-per-tile invariant. Tile thing order is `ground(0) → top items →
   creature → down items`. With `pre_creature_len` known per tile:
   - `stackpos < pre_creature_len` → tile item at that index;
   - `stackpos == pre_creature_len` **and** a creature occupies the tile → the
     creature;
   - otherwise → a down item (index adjusted for the creature slot if present).
   Be lenient on out-of-range stackpos (TFS `getThing` returns the closest
   sensible thing); if nothing resolves, drop silently.
2. **`can_see`** — reuse the existing viewport visibility check; if the thing is
   not visible, drop (TFS sends a cancel; dropping is acceptable and simpler).
3. **`lookDistance`** = `max(|dx|, |dy|)`, `+15` if `playerPos.z != thingPos.z`.
   Self-look (the thing *is* the looker) → `-1`.
4. **Build text** (see below).
5. **GM debug** — if the looker is a gamemaster, append the faithful subset:
   `\nItem ID: <server_id>` (items only) and `\nPosition: x, y, z` (always).
6. **Push** `0xB4 MESSAGE_INFO_DESCR (22)` to the looker.

### Text assembly (world, ported from TFS)

**Item** (`item.cpp::getDescription` / `getNameDescription`, faithful subset):
```
"You see " + nameDescription
  nameDescription = (stackable && count>1 && show_count) ? "<count> <plural>"
                                                         : "<article> <name>"
                    (empty name → "an item of type <server_id>")
  if lookDistance <= 1 && pickupable && weight != 0:  "\nIt weighs <W.WW> oz."
  if lookDistance <= 1 && !description.empty():        "\n<description>"
```
Weight format: hundredths → `"%u.%02u"` (e.g. `weight=550` → `"5.50 oz."`;
plural form "They weigh …" when stackable & count>1). Punctuation (the trailing
`.` after the name) is ported exactly from `item.cpp` during TDD. Rune/weapon/
armor/wield branches are **out of scope** — plain ground-item shape only.

**Creature** (`player.cpp::getDescription`, faithful subset):
```
self (lookDistance == -1): "You see yourself. You have no vocation."
other player:              "You see <name> (Level 1). <He|She> has no vocation."
                           (He/She from M8 char sex; Level 1 until M14)
```
Party / IP / mana lines are out of scope (no party till M18, no mana till M15).

## C. Protocol layer (`protocol`)

New `look` module (pure wire, no game logic):

- `parse_look(body: &[u8]) -> Option<(Position, u8)>` — inbound `0x8C`:
  `[x u16][y u16][z u8][skip 2 spriteId][stackpos u8]`. (`protocolgame.cpp:908`.)
- `parse_look_battle(body: &[u8]) -> Option<u32>` — inbound `0x8D`:
  `[creatureId u32]`. (`protocolgame.cpp:916`.)
- `info_descr(text: &str) -> Vec<u8>` — outbound `[0xB4][22][u16 len][bytes]`.
  Falls through to a plain string (TFS `sendTextMessage` has no extra fields for
  `MESSAGE_INFO_DESCR`). Mirror the existing `push_status_message` shape but with
  type `22` instead of `21`; over-255 strings truncated (documented divergence,
  same as chat).

## D. Service wiring (`server`)

- `reader_loop` (game_service.rs): two new guards before the walk fallthrough —
  `0x8C` → `parse_look` → `world.look(player_id, pos, stackpos)`;
  `0x8D` → `parse_look_battle` → `world.look_battle(player_id, creature_id)`.
- **Gamemaster flag**: `game_login` already parses the gamemaster byte. Thread it
  through `Login` into `PlayerState.gamemaster: bool` so `do_look` can gate the
  debug block. (If `Login` does not currently carry it, add the field.)

## E. Testing (TDD, per the project's strict-red-then-green flow)

- **formats**: `items.xml` parses `name`/`article`/`plural`/`description`/`weight`/
  `show_count` and merges by id range; `items.otb` exposes `pickupable`; `.otbm`
  `parse_item` reads `OTBM_ATTR_COUNT`.
- **protocol**: `parse_look` round-trips position + stackpos (and skips the
  spriteId); `parse_look_battle` reads the id; `info_descr` emits
  `[B4 16 <len> …]` (0x16 = 22).
- **world `do_look`**: ground item → `"You see a <name>."`; stackable count>1 →
  `"You see <count> <plural>."`; weight + description appear only at distance ≤1
  and vanish at ≥2; pickupable=false / weight=0 → no weight line; other player →
  `"You see <name> (Level 1). He has no vocation."` (and `She` from sex);
  self-look → `"You see yourself. You have no vocation."`; gamemaster looker →
  `Item ID` + `Position` appended; non-gamemaster → not appended; out-of-view
  thing → no packet pushed.

Gate (matches prior milestones): `cargo test` green (workspace),
`cargo clippy --all-targets -- -D warnings` clean, `#![forbid(unsafe_code)]`
intact.

## Live acceptance (manual gate)

Real OTClient: right-click / look on a ground item near the temple → green
"You see …" with name, and weight+description when standing adjacent; look on a
gold pile (if present) shows its real count; look on a second player → "You see
<name> (Level 1). He has no vocation."; look on yourself → "yourself". With a
gamemaster character, the item id + position lines appear.

## Deferred

- Look in inventory / containers (M10), trade (M16), shop (M16).
- Action/Unique/Decay/Transform IDs and writable book text in GM debug.
- Rune/weapon/armor stat lines in item descriptions (need M10/M15 data).
- Real creature level / vocation (M14); mana, party, IP lines.
- Dynamic ground items (drop/pickup) and runtime stack counts (M10).
