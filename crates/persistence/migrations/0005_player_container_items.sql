-- Container contents for each player's inventory bags/sub-bags.
-- Each row is an item inside a container: either a top-level bag (inv_slot set,
-- parent_slot_tag NULL) or a nested item (parent_slot_tag identifies its position
-- inside the parent bag, e.g. "0", "1", "2").
CREATE TABLE IF NOT EXISTS player_container_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    player_name TEXT NOT NULL,
    -- Which inventory slot (1-10) the top-level bag occupies. NULL for nested items.
    inv_slot INTEGER,
    -- Dot-separated path from the top-level bag to this item.
    -- "" for items directly in the top-level bag, "N" for items in sub-bag at slot N, etc.
    slot_path TEXT NOT NULL DEFAULT '',
    server_id INTEGER NOT NULL,
    count INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS player_container_items_name_idx
    ON player_container_items(player_name);
