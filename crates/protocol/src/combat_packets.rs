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
//!   - `0xB4` text message (damage modes): floating damage numbers.
//!
//! Sources verified against `reference/tfs/src/protocolgame.cpp`:
//!   - `parseAttack`       line 972–977
//!   - `sendCreatureHealth` line 2339–2351
//!   - `sendReLoginWindow`  line 1376–1383
//!   - `sendTextMessage`    line 1411–1447
//!
//! The `0xB4` damage wire layout is cross-checked against the OTClient
//! Redemption parser (`protocolgameparse.cpp::parseTextMessage`, the
//! `MessageDamage*` arm) and its mode-byte table (`protocolcodes.cpp::
//! buildMessageModesMap`, `version >= 1055`).

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

/// Inbound: client cancels the current action — sent by the ESC / "Stop"
/// hotkey (`parseCancelMove` → `Game::playerCancelAttackAndFollow`: clear the
/// attacked creature, clear follow, stop walking). Body-less.
///
/// NOTE: `0xBE` is direction-overloaded — **outbound** it is the floor-change-up
/// map slice (`walk::OP_FLOOR_CHANGE_UP`). The reader only ever matches inbound
/// opcodes, so there is no conflict with that server→client writer.
pub const OP_CANCEL_MOVE: u8 = 0xBE;

/// Outbound: creature health-bar broadcast to spectators (`sendCreatureHealth`,
/// TFS line 2339).
pub const OP_CREATURE_HEALTH: u8 = 0x8C;

/// Outbound: death / relogin window sent to the dying player
/// (`sendReLoginWindow`, TFS line 1376).
pub const OP_DEATH_WINDOW: u8 = 0x28;

/// Outbound: text message (`sendTextMessage`, TFS line 1411). For the damage
/// modes it carries a tile position + value/color pairs that the client renders
/// as a floating "animated text" number. Replaces the pre-10.x `0x84`
/// `AddAnimatedText` opcode, which OTClient Redemption no longer parses.
pub const OP_TEXT_MESSAGE: u8 = 0xB4;

// ---------------------------------------------------------------------------
// Text-message mode bytes (wire values for protocol 10.98)
// ---------------------------------------------------------------------------
//
// These are the on-the-wire mode bytes the OTClient Redemption client maps in
// `buildMessageModesMap` for `version >= 1055` (protocolcodes.cpp). They are
// NOT the TFS internal `MessageClasses` enum values — TFS translates those per
// protocol version before sending; we emit the post-translation wire bytes
// directly.

/// Damage the local player dealt — floats on the victim's tile, shown to the
/// attacker (`MessageDamageDealed = 23`).
pub const MSG_DAMAGE_DEALT: u8 = 23;

/// Damage the local player received — shown to the victim
/// (`MessageDamageReceived = 24`).
pub const MSG_DAMAGE_RECEIVED: u8 = 24;

/// Damage between two other creatures — shown to bystanders
/// (`MessageDamageOthers = 27`).
pub const MSG_DAMAGE_OTHERS: u8 = 27;

/// Text colour for physical damage numbers (`TEXTCOLOR_RED`, TFS const.h:320).
pub const TEXTCOLOR_RED: u8 = 180;

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

/// Encode a `0xB4` damage text message (a floating damage number):
///
/// ```text
/// [0xB4][mode u8][x u16][y u16][z u8]
///       [primary.value u32][primary.color u8]
///       [secondary.value u32][secondary.color u8]
///       [text u16-len + bytes]
/// ```
///
/// `mode` is one of [`MSG_DAMAGE_DEALT`], [`MSG_DAMAGE_RECEIVED`], or
/// [`MSG_DAMAGE_OTHERS`]. The primary slot carries the physical damage
/// (`value`/`color`); the secondary slot (magic damage) is always zeroed here
/// because fist combat is purely physical. The client renders each non-zero
/// value as an animated number on the tile and skips a `0` value.
///
/// `text` MUST be non-empty: the 10.98 parser reads the trailing string for the
/// damage modes and, if it comes back empty, reads *another* string — an empty
/// payload would desync the stream. Callers always pass a console line.
pub fn damage_text(
    mode: u8,
    x: u16,
    y: u16,
    z: u8,
    value: u32,
    color: u8,
    text: &[u8],
) -> Vec<u8> {
    debug_assert!(!text.is_empty(), "damage_text requires a non-empty string");
    let mut w = MessageWriter::new();
    w.write_u8(OP_TEXT_MESSAGE);
    w.write_u8(mode);
    w.write_u16(x);
    w.write_u16(y);
    w.write_u8(z);
    // Primary slot: physical damage.
    w.write_u32(value);
    w.write_u8(color);
    // Secondary slot: magic damage — unused by fist combat.
    w.write_u32(0);
    w.write_u8(0);
    w.write_string(text);
    w.into_bytes()
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

    // -------------------------------------------------------------------------
    // damage_text (0xB4) — floating damage number encoder
    // -------------------------------------------------------------------------

    #[test]
    fn damage_text_exact_wire_layout() {
        // [0xB4][mode][x u16][y u16][z u8][value u32][color u8][0 u32][0 u8][str u16len+bytes]
        let text = b"You lose 5 hitpoints.";
        let p = damage_text(MSG_DAMAGE_RECEIVED, 0x0102, 0x0304, 7, 5, TEXTCOLOR_RED, text);

        let mut i = 0;
        assert_eq!(p[i], OP_TEXT_MESSAGE, "opcode"); i += 1;
        assert_eq!(p[i], MSG_DAMAGE_RECEIVED, "mode"); i += 1;
        assert_eq!(u16::from_le_bytes([p[i], p[i + 1]]), 0x0102, "x"); i += 2;
        assert_eq!(u16::from_le_bytes([p[i], p[i + 1]]), 0x0304, "y"); i += 2;
        assert_eq!(p[i], 7, "z"); i += 1;
        assert_eq!(u32::from_le_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]]), 5, "primary value"); i += 4;
        assert_eq!(p[i], TEXTCOLOR_RED, "primary color"); i += 1;
        assert_eq!(u32::from_le_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]]), 0, "secondary value"); i += 4;
        assert_eq!(p[i], 0, "secondary color"); i += 1;
        assert_eq!(u16::from_le_bytes([p[i], p[i + 1]]), text.len() as u16, "string length"); i += 2;
        assert_eq!(&p[i..i + text.len()], text, "string body"); i += text.len();
        assert_eq!(i, p.len(), "no trailing bytes after the string");
    }

    #[test]
    fn damage_text_mode_bytes_match_redemption_table() {
        // Wire mode bytes from OTClient Redemption `buildMessageModesMap`
        // (version >= 1055). These are protocol constants, not arbitrary — this
        // guards against an accidental renumbering that would desync the client.
        assert_eq!(OP_TEXT_MESSAGE, 0xB4);
        assert_eq!(MSG_DAMAGE_DEALT, 23);
        assert_eq!(MSG_DAMAGE_RECEIVED, 24);
        assert_eq!(MSG_DAMAGE_OTHERS, 27);
        assert_eq!(TEXTCOLOR_RED, 180);
    }

    #[test]
    fn damage_text_string_is_present_and_length_prefixed() {
        // The 10.98 parser re-reads a string if the first comes back empty
        // (protocolgameparse.cpp:3066), which would desync the stream. The
        // trailing string must be present and length-prefixed exactly once.
        let text = b"x";
        let p = damage_text(MSG_DAMAGE_DEALT, 1, 1, 0, 1, TEXTCOLOR_RED, text);
        let n = p.len();
        assert_eq!(u16::from_le_bytes([p[n - 3], p[n - 2]]), 1, "string length prefix");
        assert_eq!(p[n - 1], b'x', "string body");
    }

    #[test]
    fn damage_text_routes_each_mode() {
        for mode in [MSG_DAMAGE_DEALT, MSG_DAMAGE_RECEIVED, MSG_DAMAGE_OTHERS] {
            let p = damage_text(mode, 100, 200, 7, 42, TEXTCOLOR_RED, b"hit");
            assert_eq!(p[0], OP_TEXT_MESSAGE);
            assert_eq!(p[1], mode, "mode byte must round-trip into byte 1");
        }
    }
}
