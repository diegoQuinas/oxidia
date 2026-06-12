-- Teleport handler.
-- Maps item_id to teleport destination and calls do_teleport.
--
-- Called from the game actor's do_use_item when the item is registered
-- in actions.xml with script="teleport.onUse".

teleport = {}

function teleport.onUse(args)
    if args.item_id == 1386 then
        do_teleport(args.player_id, args.pos_x, args.pos_y - 1, args.pos_z - 1)
    elseif args.item_id == 1391 then
        do_teleport(args.player_id, args.pos_x, args.pos_y, args.pos_z + 1)
    elseif args.item_id == 384 then
        do_teleport(args.player_id, args.pos_x, args.pos_y, args.pos_z + 1)
    elseif args.item_id == 433 then
        do_teleport(args.player_id, args.pos_x, args.pos_y, args.pos_z + 1)
    end
end
