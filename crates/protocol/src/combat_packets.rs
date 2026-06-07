//! Combat packets for protocol 10.98.
//!
//! Inbound:
//!   - `0xA1` attack target: `[u32 creatureId]` (body after opcode).
//!     `creatureId == 0` clears the current target.
//!   - `0xA2` follow (same body): consumed/ignored by the session reader.
//!     `OP_FOLLOW` is exported so `game_service.rs` can drain it cleanly.
//!
//! Outbound:
//!   - `0x8C` creature health-bar: `[0x8C][u32 id][u8 percent]`.
//!   - `0x28` death/relogin window: `[0x28][u8 0x00][u8 unfairFightReduction]`.
//!
//! Sources verified against `reference/tfs/src/protocolgame.cpp`:
//!   - `parseAttack`       line 972–977
//!   - `sendCreatureHealth` line 2339–2351
//!   - `sendReLoginWindow`  line 1376–1383

use crate::message::MessageWriter;

// ---------------------------------------------------------------------------
// Opcode constants
// ---------------------------------------------------------------------------

/// Inbound: client requests to attack a creature (`parseAttack`, TFS line 972).
pub const OP_ATTACK: u8 = 0xA1;

/// Inbound: client requests to follow a creature (`parseFollow`, TFS line 979).
/// M7 drains and ignores this opcode; the constant lives here so the session
/// reader never needs a magic literal.
pub const OP_FOLLOW: u8 = 0xA2;

/// Outbound: creature health-bar broadcast to spectators (`sendCreatureHealth`,
/// TFS line 2339).
pub const OP_CREATURE_HEALTH: u8 = 0x8C;

/// Outbound: death / relogin window sent to the dying player
/// (`sendReLoginWindow`, TFS line 1376).
pub const OP_DEATH_WINDOW: u8 = 0x28;

// ---------------------------------------------------------------------------
// Health-percent helper
// ---------------------------------------------------------------------------

/// Compute the health percentage for the `0x8C` packet.
///
/// Wire encoding: `ceil(health / max(max_health, 1) * 100)`, clamped to
/// `0..=100`. This mirrors `sendCreatureHealth` (TFS line 2348) exactly:
///
/// ```cpp
/// msg.addByte(std::ceil(
///     (static_cast<double>(creature->getHealth())
///      / std::max<int32_t>(creature->getMaxHealth(), 1)) * 100));
/// ```
///
/// Key edge cases:
/// - `max_health == 0` is guarded by `max(max_health, 1)` — avoids division by zero.
/// - `health == 0` → `0` (dead).
/// - `health == 1, max_health == 150` → `ceil(0.666…) == 1` (never 0 for a
///   living creature, preserving the TFS intent).
/// - Overheal (`health > max_health`) is clamped to `100`.
pub fn health_percent(health: i32, max_health: i32) -> u8 {
    if health <= 0 {
        return 0;
    }
    let denom = max_health.max(1) as f64;
    let raw = ((health as f64 / denom) * 100.0).ceil() as i32;
    raw.clamp(0, 100) as u8
}

// ---------------------------------------------------------------------------
// Outbound encoders
// ---------------------------------------------------------------------------

/// Encode a `0x8C` creature health-bar packet:
/// `[0x8C][creatureId u32 LE][healthPercent u8]`.
///
/// `percent` is the pre-computed value from [`health_percent`].
pub fn creature_health(creature_id: u32, percent: u8) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_CREATURE_HEALTH);
    w.write_u32(creature_id);
    w.write_u8(percent);
    w.into_bytes()
}

/// Encode a `0x28` death/relogin window packet:
/// `[0x28][0x00][unfairFightReduction u8]`.
///
/// M7 always passes `unfair_fight_reduction = 0` (no skull/PvP math until M23).
pub fn death_window(unfair_fight_reduction: u8) -> Vec<u8> {
    vec![OP_DEATH_WINDOW, 0x00, unfair_fight_reduction]
}

// ---------------------------------------------------------------------------
// Inbound parser
// ---------------------------------------------------------------------------

/// Parse the body of an inbound `0xA1` or `0xA2` packet (the bytes **after**
/// the opcode byte, which the reader loop has already consumed).
///
/// Returns `Some(creature_id)` on success — `0` means "clear current target".
/// Returns `None` if the body is shorter than 4 bytes.
pub fn parse_attack(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes([body[0], body[1], body[2], body[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // health_percent — the TFS ceil + max-1 guard
    // -------------------------------------------------------------------------

    #[test]
    fn health_percent_full() {
        assert_eq!(health_percent(100, 100), 100);
    }

    #[test]
    fn health_percent_zero() {
        assert_eq!(health_percent(0, 100), 0);
    }

    #[test]
    fn health_percent_half() {
        assert_eq!(health_percent(50, 100), 50);
    }

    #[test]
    fn health_percent_one_hp_does_not_round_to_zero() {
        // 1/100 * 100 = 1.0 exactly, ceil -> 1
        assert_eq!(health_percent(1, 100), 1);
    }

    #[test]
    fn health_percent_ceil_rounds_up() {
        // 1/3 * 100 = 33.33..., ceil -> 34
        assert_eq!(health_percent(1, 3), 34);
    }

    #[test]
    fn health_percent_one_in_150_rounds_up() {
        // 1/150 * 100 = 0.666..., ceil -> 1 (never 0 for living creature)
        assert_eq!(health_percent(1, 150), 1);
    }

    #[test]
    fn health_percent_zero_in_150() {
        assert_eq!(health_percent(0, 150), 0);
    }

    #[test]
    fn health_percent_maxhp_zero_guard() {
        // max(0,1) = 1, so 0/1*100 = 0
        assert_eq!(health_percent(0, 0), 0);
    }

    #[test]
    fn health_percent_clamped_to_100() {
        // Overheal: health > max_health should still cap at 100.
        assert_eq!(health_percent(200, 100), 100);
    }

    #[test]
    fn health_percent_odd_ratio_rounds_up() {
        // 2/3 * 100 = 66.66..., ceil -> 67
        assert_eq!(health_percent(2, 3), 67);
    }

    // -------------------------------------------------------------------------
    // creature_health (0x8C) — encoder + round-trip
    // -------------------------------------------------------------------------

    #[test]
    fn creature_health_exact_bytes() {
        // [0x8C][id u32 LE][percent u8]
        let p = creature_health(0x0102_0304, 75);
        assert_eq!(p.len(), 6);
        assert_eq!(p[0], OP_CREATURE_HEALTH);
        assert_eq!(u32::from_le_bytes([p[1], p[2], p[3], p[4]]), 0x0102_0304);
        assert_eq!(p[5], 75);
    }

    #[test]
    fn creature_health_round_trip() {
        let id = 0xDEAD_BEEF_u32;
        let pct = 42_u8;
        let p = creature_health(id, pct);
        // decode: opcode, then u32 LE, then u8
        assert_eq!(p[0], OP_CREATURE_HEALTH);
        let decoded_id = u32::from_le_bytes([p[1], p[2], p[3], p[4]]);
        let decoded_pct = p[5];
        assert_eq!(decoded_id, id);
        assert_eq!(decoded_pct, pct);
    }

    #[test]
    fn creature_health_zero_percent() {
        let p = creature_health(1, 0);
        assert_eq!(p[5], 0);
    }

    #[test]
    fn creature_health_max_percent() {
        let p = creature_health(1, 100);
        assert_eq!(p[5], 100);
    }

    // -------------------------------------------------------------------------
    // death_window (0x28) — encoder + round-trip
    // -------------------------------------------------------------------------

    #[test]
    fn death_window_m7_is_three_bytes() {
        // M7 always passes unfairFightReduction = 0
        assert_eq!(death_window(0), [0x28, 0x00, 0x00]);
    }

    #[test]
    fn death_window_exact_bytes() {
        // [0x28][u8 0x00][u8 reduction]
        let p = death_window(42);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], OP_DEATH_WINDOW);
        assert_eq!(p[1], 0x00);
        assert_eq!(p[2], 42);
    }

    #[test]
    fn death_window_round_trip() {
        for reduction in [0u8, 10, 100, 255] {
            let p = death_window(reduction);
            assert_eq!(p[0], OP_DEATH_WINDOW);
            assert_eq!(p[1], 0x00);
            assert_eq!(p[2], reduction, "round-trip failed for reduction={reduction}");
        }
    }

    // -------------------------------------------------------------------------
    // parse_attack (0xA1) — inbound parser
    // -------------------------------------------------------------------------

    #[test]
    fn parse_attack_returns_creature_id() {
        // body: u32 LE creature id (opcode has already been consumed by the reader loop)
        let id: u32 = 0x0100_0001;
        let body = id.to_le_bytes();
        assert_eq!(parse_attack(&body), Some(id));
    }

    #[test]
    fn parse_attack_zero_means_clear() {
        let body = 0u32.to_le_bytes();
        assert_eq!(parse_attack(&body), Some(0));
    }

    #[test]
    fn parse_attack_short_body_returns_none() {
        assert_eq!(parse_attack(&[]), None);
        assert_eq!(parse_attack(&[0x01]), None);
        assert_eq!(parse_attack(&[0x01, 0x00, 0x00]), None);
    }

    #[test]
    fn parse_attack_exact_four_bytes() {
        let body = [0x78, 0x56, 0x34, 0x12];
        assert_eq!(parse_attack(&body), Some(0x1234_5678));
    }

    #[test]
    fn parse_attack_opcode_constant() {
        assert_eq!(OP_ATTACK, 0xA1);
    }

    #[test]
    fn follow_opcode_constant() {
        assert_eq!(OP_FOLLOW, 0xA2);
    }
}
