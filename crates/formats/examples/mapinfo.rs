//! Acceptance demo for the M2 format parsers.
//!
//! Loads an `items.otb` and an `.otbm` map and prints a summary: versions,
//! dimensions, file references, tile/item counts, per-floor distribution, and
//! the town/waypoint lists.
//!
//! ```text
//! cargo run -p formats --example mapinfo [items.otb] [map.otbm]
//! ```
//!
//! With no arguments it falls back to the bundled TFS reference data.

use std::collections::BTreeMap;
use std::process::ExitCode;

const DEFAULT_OTB: &str = "reference/tfs/data/items/items.otb";
const DEFAULT_OTBM: &str = "reference/tfs/data/world/forgotten.otbm";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let otb_path = args.next().unwrap_or_else(|| DEFAULT_OTB.to_string());
    let otbm_path = args.next().unwrap_or_else(|| DEFAULT_OTBM.to_string());

    match run(&otb_path, &otbm_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(otb_path: &str, otbm_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let otb_bytes = std::fs::read(otb_path)?;
    let items = formats::otb::parse(&otb_bytes)?;

    println!("items.otb  ({otb_path})");
    println!("  version : {}.{} build {}", items.major_version, items.minor_version, items.build_number);
    println!("  items   : {}", items.items.len());
    println!();

    let map_bytes = std::fs::read(otbm_path)?;
    let map = formats::otbm::parse(&map_bytes)?;

    let total_items: usize = map.tiles.iter().map(|t| count_items(&t.items)).sum();
    let mut per_floor: BTreeMap<u8, usize> = BTreeMap::new();
    for tile in &map.tiles {
        *per_floor.entry(tile.z).or_default() += 1;
    }

    println!("map  ({otbm_path})");
    println!("  size       : {}x{}", map.width, map.height);
    println!("  items.otb  : {}.{}", map.major_items, map.minor_items);
    if !map.description.is_empty() {
        println!("  description: {}", map.description.replace('\n', " | "));
    }
    if let Some(spawn) = &map.spawn_file {
        println!("  spawn file : {spawn}");
    }
    if let Some(house) = &map.house_file {
        println!("  house file : {house}");
    }
    println!("  tiles      : {}", map.tiles.len());
    println!("  items      : {total_items}");
    println!("  towns      : {}", map.towns.len());
    println!("  waypoints  : {}", map.waypoints.len());

    println!("  floors     :");
    for (z, count) in &per_floor {
        println!("    z={z:<2} {count} tiles");
    }

    if !map.towns.is_empty() {
        println!("  town list  :");
        for town in &map.towns {
            println!("    #{:<3} {:<20} temple ({}, {}, {})", town.id, town.name, town.x, town.y, town.z);
        }
    }

    Ok(())
}

/// Total item count including items nested inside containers.
fn count_items(items: &[formats::otbm::MapItem]) -> usize {
    items.iter().map(|i| 1 + count_items(&i.contents)).sum()
}
