#![forbid(unsafe_code)]

//! Build-time tool that reads an OTBM map + `items.otb`, splits the world into
//! 256×256-tile chunks, serializes each via `bincode`, and writes them to
//! `data/chunks/{z}/{x}_{y}.chunk`.
//!
//! Also writes a SHA-256 fingerprint of the input files to
//! `data/chunks/fingerprint` and the world metadata (spawn position + towns)
//! to `data/chunks/meta.bin` so the server binary can boot without parsing OTBM.
//!
//! Usage:
//!   cargo run --bin prechunk -- <map.otbm> <items.otb> [items.xml]
//!
//! When `items.xml` is omitted the default `reference/tfs/data/items/items.xml`
//! is used.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use world::{CHUNK_DIM, ChunkId, Position};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("Usage: {} <map.otbm> <items.otb> [items.xml]", args[0]);
        std::process::exit(1);
    }
    let otbm_path = &args[1];
    let items_otb_path = &args[2];
    let items_xml_path = if args.len() >= 4 {
        args[3].clone()
    } else {
        "reference/tfs/data/items/items.xml".to_string()
    };

    let output_dir = Path::new("data/chunks");
    std::fs::create_dir_all(output_dir).context("creating data/chunks/")?;

    let otbm_bytes = std::fs::read(otbm_path).with_context(|| format!("reading {otbm_path}"))?;
    let items_otb_bytes =
        std::fs::read(items_otb_path).with_context(|| format!("reading {items_otb_path}"))?;
    let items_xml_bytes = std::fs::read_to_string(&items_xml_path)
        .with_context(|| format!("reading {items_xml_path}"))?;

    // --- Parse inputs ---
    let map = formats::otbm::parse(&otbm_bytes).with_context(|| format!("parsing {otbm_path}"))?;
    let mut items = formats::otb::parse(&items_otb_bytes)
        .with_context(|| format!("parsing {items_otb_path}"))?;
    let items_xml = formats::items_xml::parse_items_xml(&items_xml_bytes)
        .with_context(|| format!("parsing {items_xml_path}"))?;

    // FIX: merge items.xml into items.otb so ItemType.floor_change is populated.
    // Without this, every tile's floor_change is NONE (the OTB default).
    formats::items_xml::merge_items_xml(&mut items, &items_xml);

    // Rebuild by_id after merge_items_xml mutated items.items. The old by_id
    // held &ItemType refs into the pre-mutation vec, which are invalidated.
    let by_id: HashMap<u16, &formats::otb::ItemType> =
        items.items.iter().map(|it| (it.server_id, it)).collect();

    // --- Group tiles by ChunkId ---
    let mut chunk_tiles: HashMap<ChunkId, Vec<&formats::otbm::MapTile>> = HashMap::new();
    for tile in &map.tiles {
        let cid: ChunkId = (
            (tile.x as i32 / CHUNK_DIM) as i16,
            (tile.y as i32 / CHUNK_DIM) as i16,
            tile.z,
        );
        chunk_tiles.entry(cid).or_default().push(tile);
    }

    // --- Build and write each chunk ---
    let mut chunks_written = 0usize;
    for (cid, tiles) in &chunk_tiles {
        let chunk = world::Chunk::from_otbm_tiles(tiles, &by_id);
        let data = bincode::serialize(&chunk).context("serializing chunk")?;
        let floor_dir = output_dir.join(cid.2.to_string());
        std::fs::create_dir_all(&floor_dir)
            .with_context(|| format!("creating {}", floor_dir.display()))?;
        let path = file_path(output_dir, *cid);
        std::fs::write(&path, &data).with_context(|| format!("writing {}", path.display()))?;
        chunks_written += 1;
    }

    // --- Fingerprint: SHA-256(otbm ‖ items_otb ‖ items_xml) ---
    // Include items.xml so chunk cache invalidates on floor-change edits.
    let mut hasher = Sha256::new();
    hasher.update(&otbm_bytes);
    hasher.update(&items_otb_bytes);
    hasher.update(items_xml_bytes.as_bytes());
    let fingerprint = format!("{:x}", hasher.finalize());
    let fp_path = output_dir.join("fingerprint");
    std::fs::write(&fp_path, &fingerprint).context("writing fingerprint")?;

    // --- World meta: spawn position + towns ---
    let spawn = map
        .towns
        .first()
        .map(|t| Position::new(t.x, t.y, t.z))
        .unwrap_or_else(|| {
            // Fallback to default spawn (same as FALLBACK_SPAWN in map.rs)
            Position::new(1000, 1000, 7)
        });
    let meta = (spawn, map.towns.clone());
    let meta_bytes = bincode::serialize(&meta).context("serializing world meta")?;
    std::fs::write(output_dir.join("meta.bin"), &meta_bytes).context("writing world meta")?;

    eprintln!(
        "Wrote {chunks_written} chunks, fingerprint {fingerprint} to {}",
        output_dir.display()
    );

    Ok(())
}

/// Path for a chunk file: `data/chunks/{z}/{x}_{y}.chunk`.
fn file_path(dir: &Path, cid: ChunkId) -> PathBuf {
    dir.join(cid.2.to_string())
        .join(format!("{}_{}.chunk", cid.0, cid.1))
}

#[cfg(test)]
mod tests {
    use formats::items_xml::{FloorChange, parse_items_xml};
    use formats::otb::{ItemType, ItemsOtb};
    use formats::otbm::{MapItem, MapTile, OtbmMap, Town};
    use world::map::StaticMap;

    /// The CORRECTED prechunker flow: parse OTB, merge items.xml, rebuild by_id,
    /// then build StaticMap. This is what the fix adds to the prechunker binary.
    fn fixed_prechunk_flow() -> StaticMap {
        let mut items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![
                ItemType {
                    group: 1, // ground
                    flags: 0,
                    server_id: 1,
                    client_id: 100,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: FloorChange::NONE,
                },
                ItemType {
                    group: 5, // stairs
                    flags: 0,
                    server_id: 100,
                    client_id: 200,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: FloorChange::NONE,
                },
            ],
        };

        // FIX: merge items.xml to populate floor_change (was missing before fix).
        let xml_str =
            r#"<items><item id="100"><attribute key="floorchange" value="down"/></item></items>"#;
        let items_xml = parse_items_xml(xml_str).unwrap();
        formats::items_xml::merge_items_xml(&mut items, &items_xml);

        let map = OtbmMap {
            width: 200,
            height: 200,
            major_items: 3,
            minor_items: 57,
            description: String::new(),
            spawn_file: None,
            house_file: None,
            tiles: vec![MapTile {
                x: 100,
                y: 100,
                z: 7,
                flags: 0,
                house_id: None,
                items: vec![
                    MapItem {
                        id: 1,
                        count: None,
                        contents: vec![],
                    },
                    MapItem {
                        id: 100,
                        count: None,
                        contents: vec![],
                    },
                ],
            }],
            towns: vec![Town {
                id: 1,
                name: "Test".into(),
                x: 100,
                y: 100,
                z: 7,
            }],
            waypoints: vec![],
        };

        StaticMap::from_formats(&map, &items)
    }

    #[test]
    fn prechunk_flow_populates_floor_change_from_items_xml() {
        let sm = fixed_prechunk_flow();

        let fc = sm.floor_change_at(100, 100, 7);
        assert!(
            fc.contains(FloorChange::DOWN),
            "stairs tile (sid 100) must have FloorChange::DOWN after merge_items_xml, got {fc:?}"
        );
    }

    #[test]
    fn prechunk_flow_without_xml_merge_yields_none_floor_change() {
        // Triangulation: without merge_items_xml, floor_change stays NONE.
        // This documents the bug behavior before the fix.
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![
                ItemType {
                    group: 1,
                    flags: 0,
                    server_id: 1,
                    client_id: 100,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: FloorChange::NONE,
                },
                ItemType {
                    group: 5,
                    flags: 0,
                    server_id: 100,
                    client_id: 200,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: FloorChange::NONE,
                },
            ],
        };
        let map = OtbmMap {
            width: 200,
            height: 200,
            major_items: 3,
            minor_items: 57,
            description: String::new(),
            spawn_file: None,
            house_file: None,
            tiles: vec![MapTile {
                x: 100,
                y: 100,
                z: 7,
                flags: 0,
                house_id: None,
                items: vec![
                    MapItem {
                        id: 1,
                        count: None,
                        contents: vec![],
                    },
                    MapItem {
                        id: 100,
                        count: None,
                        contents: vec![],
                    },
                ],
            }],
            towns: vec![Town {
                id: 1,
                name: "Test".into(),
                x: 100,
                y: 100,
                z: 7,
            }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        assert_eq!(
            sm.floor_change_at(100, 100, 7),
            FloorChange::NONE,
            "without merge_items_xml, floor_change must be NONE"
        );
    }
}
