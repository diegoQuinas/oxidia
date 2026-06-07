-- M10.2: equipped items per character. Flat by design; M10.3 adds a nullable
-- `parent` column for container contents.
CREATE TABLE player_inventory (
    player_name TEXT    NOT NULL,
    slot        INTEGER NOT NULL,   -- 1..=10 (TFS CONST_SLOT_HEAD..AMMO)
    server_id   INTEGER NOT NULL,
    count       INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (player_name, slot)
);
