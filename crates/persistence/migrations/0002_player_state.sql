-- Player restorable state — extends the `players` table in-place.
--
-- Design choice: columns added to `players` (not a separate `player_state`
-- table). There is a strict 1-to-1 relationship: every player row has exactly
-- one saved state and loading a character always needs both the identity fields
-- (account_id, name) and the state fields in a single fetch. A JOIN would add
-- no normalisation benefit while costing an extra round-trip. Keeping one table
-- also keeps `save_player` a single UPSERT with no cross-table coordination.
--
-- The existing `level` column is kept untouched; it is the character's
-- current level and is overwritten on save alongside the other stat columns.
--
-- Storage note: SQLite has no u16/u8 types — all integers are stored as
-- INTEGER (i64 internally). The Rust layer clamps values to the correct ranges
-- on load.

ALTER TABLE players ADD COLUMN pos_x      INTEGER NOT NULL DEFAULT 32369;
ALTER TABLE players ADD COLUMN pos_y      INTEGER NOT NULL DEFAULT 32241;
ALTER TABLE players ADD COLUMN pos_z      INTEGER NOT NULL DEFAULT 7;

ALTER TABLE players ADD COLUMN health     INTEGER NOT NULL DEFAULT 150;
ALTER TABLE players ADD COLUMN health_max INTEGER NOT NULL DEFAULT 150;
ALTER TABLE players ADD COLUMN mana       INTEGER NOT NULL DEFAULT 0;
ALTER TABLE players ADD COLUMN mana_max   INTEGER NOT NULL DEFAULT 0;

-- Direction: 0=North 1=East 2=South 3=West (wire encoding from world::Direction).
ALTER TABLE players ADD COLUMN direction  INTEGER NOT NULL DEFAULT 2;

-- Outfit fields.
ALTER TABLE players ADD COLUMN look_type   INTEGER NOT NULL DEFAULT 128;
ALTER TABLE players ADD COLUMN look_head   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE players ADD COLUMN look_body   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE players ADD COLUMN look_legs   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE players ADD COLUMN look_feet   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE players ADD COLUMN look_addons INTEGER NOT NULL DEFAULT 0;
ALTER TABLE players ADD COLUMN look_mount  INTEGER NOT NULL DEFAULT 0;
