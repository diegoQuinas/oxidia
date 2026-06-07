-- Player sex / gender column.
--
-- Convention (matches TFS outfits.xml `type` attribute):
--   0 = female
--   1 = male
--
-- Default 1 (male) matches the default look_type 128 (male Citizen) that every
-- new character is assigned in `game_service.rs:knight_outfit()`. Existing rows
-- (from migrations 0001/0002) therefore keep a consistent value without a
-- data-backfill step.

ALTER TABLE players ADD COLUMN sex INTEGER NOT NULL DEFAULT 1;
