-- Food consumption handler.
-- Maps item_id to health/heal-over-time parameters and calls do_feed.
-- Cooldown: 2 seconds between eating (checked per-player via last_eat_ms).
--
-- Called from the game actor's do_use_item when the item is registered
-- in actions.xml with script="food.onUse".
--
-- Server IDs matching reference/tfs/data/items/items.xml

food = {}

-- Per-player cooldown tracking: last eat time by player_id.
local last_eat_ms = {}

-- Per-item food stats: { health_gain, interval_ms, duration_ms, total_heal_cap, message }
-- Messages by food family: meat → "Glup!", fish → "Chomp!", produce → "Munch!"
local foods = {
    [2666] = { 8,  2000, 60000,  240, "Glup!" },   -- meat
    [2667] = { 5,  2000, 30000,  120, "Chomp!" },  -- fish
    [2668] = { 7,  2000, 40000,  160, "Glup!" },   -- salmon
    [2669] = { 6,  2000, 30000,  120, "Glup!" },   -- northern pike
    [2670] = { 4,  2000, 20000,  80,  "Chomp!" },  -- shrimp
    [2671] = { 7,  2000, 40000,  160, "Glup!" },   -- ham
    [2672] = { 4,  2000, 20000,  80,  "Glup!" },   -- dragon ham
    [2673] = { 2,  2000, 10000,  40,  "Munch!" },  -- pear
    [2674] = { 2,  2000, 10000,  40,  "Munch!" },  -- red apple
    [2675] = { 2,  2000, 10000,  40,  "Munch!" },  -- orange
    [2676] = { 2,  2000, 10000,  40,  "Munch!" },  -- banana
    [2677] = { 2,  2000, 10000,  40,  "Munch!" },  -- blueberry
    [2678] = { 2,  2000, 10000,  40,  "Munch!" },  -- coconut
    [2679] = { 1,  2000, 5000,   20,  "Munch!" },  -- cherry
    [2680] = { 2,  2000, 10000,  40,  "Munch!" },  -- strawberry
    [2681] = { 3,  2000, 15000,  60,  "Munch!" },  -- grapes
    [2682] = { 2,  2000, 10000,  40,  "Munch!" },  -- melon
    [2683] = { 3,  2000, 15000,  60,  "Munch!" },  -- pumpkin
    [2684] = { 2,  2000, 10000,  40,  "Munch!" },  -- carrot
    [2685] = { 2,  2000, 10000,  40,  "Munch!" },  -- tomato
    [2686] = { 2,  2000, 10000,  40,  "Munch!" },  -- corncob
    [2687] = { 8,  2000, 60000,  240, "Glup!" },   -- cookie
    [2688] = { 3,  2000, 15000,  60,  "Glup!" },   -- candy cane
    [2689] = { 2,  2000, 10000,  40,  "Glup!" },   -- bread
    [2690] = { 3,  2000, 15000,  60,  "Glup!" },   -- roll
    [2691] = { 6,  2000, 40000,  160, "Glup!" },   -- brown bread
    [2696] = { 4,  2000, 20000,  80,  "Munch!" },  -- cheese
}

function food.onUse(args)
    local item = foods[args.item_id]
    if item == nil then
        return false
    end

    -- Cooldown: 2 seconds between eating (per-player).
    -- Uses os.time() for second-granularity cooldown.
    local now = os.time()
    local last = last_eat_ms[args.player_id] or 0
    if now - last < 2 then
        return false
    end
    last_eat_ms[args.player_id] = now

    do_feed(args.player_id, item[1], item[2], item[3], item[4])
    do_send_text_message(args.player_id, 36, item[5])
    return true
end
