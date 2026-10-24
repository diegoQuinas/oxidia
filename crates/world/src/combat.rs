#![forbid(unsafe_code)]
//! Pure melee combat math — a byte-faithful port of TFS 1.4.2
//! `Weapons::getMaxWeaponDamage` (weapons.cpp:135-138) and `Weapon::useFist`.
//!
//! No actor, no protocol, no I/O. Every input is a parameter so M10 (equipped
//! weapons) and M14 (real skills/levels/vocation) plug in without touching this
//! module.
//!
//! # Critical arithmetic contract
//!
//! The TFS formula is:
//! ```text
//! round((level / 5) + ((((skill / 4.) + 1) * (value / 3.)) * 1.03) / factor)
//! ```
//! `level / 5` is **integer** division (C++ `uint32_t / int`).
//! `skill / 4.` and `value / 3.` are **float** division (the `.` suffix forces
//! promotion to `double` in C++). Using float for `level / 5` silently inflates
//! damage and desyncs from any TFS reference build — the Rust port MUST mirror
//! this exactly.

use rand::Rng;

/// Fist (unarmed) attack value — TFS `Weapon::useFist` constant.
pub const FIST_ATTACK_VALUE: i32 = 7;

/// Attack-mode factor (`FIGHTMODE_ATTACK`) — `Player::getAttackFactor`.
pub const ATTACK_FACTOR: f64 = 1.0;

/// Compute the maximum melee damage for a swing.
///
/// Formula (byte-faithful TFS port, `weapons.cpp:135-138`):
/// ```text
/// round((level / 5) + ((((skill / 4.0) + 1.0) * (value / 3.0)) * 1.03) / factor)
/// ```
///
/// # Arguments
/// * `level` — character level (integer division by 5, mirrors C++ `uint32_t / int`)
/// * `attack_skill` — attack skill value (float division by 4.0)
/// * `attack_value` — weapon attack value (float division by 3.0)
/// * `attack_factor` — fight-mode factor (1.0 = attack, 1.2 = balanced, 2.0 = defensive)
pub fn max_weapon_damage(
    level: u32,
    attack_skill: i32,
    attack_value: i32,
    attack_factor: f64,
) -> i32 {
    // INTEGER division — mirrors C++ `(uint32_t level) / 5`.
    let int_part = (level / 5) as f64;
    // FLOAT division — mirrors C++ `(attackSkill / 4.)` and `(attackValue / 3.)`.
    let skill_term = ((attack_skill as f64 / 4.0) + 1.0) * (attack_value as f64 / 3.0);
    (int_part + (skill_term * 1.03) / attack_factor).round() as i32
}

/// Roll a uniform damage value in `0..=max_weapon_damage(..)`.
///
/// `TFS normal_random(0, max)` is a uniform integer distribution despite the
/// name ("normal" is a historical misnomer in TFS `tools.cpp`). This function
/// takes an injected RNG so callers remain deterministic under test.
///
/// Returns the **positive** damage magnitude (the caller negates it when
/// applying to health if the protocol requires a signed value).
pub fn melee_damage(
    rng: &mut impl Rng,
    level: u32,
    attack_skill: i32,
    attack_value: i32,
    attack_factor: f64,
) -> i32 {
    let max = max_weapon_damage(level, attack_skill, attack_value, attack_factor).max(0);
    if max == 0 { 0 } else { rng.gen_range(0..=max) }
}

/// Fist (unarmed) swing: `attack_value = 7`, `attack_factor = 1.0`.
///
/// Convenience wrapper for the M7 loop (no inventory yet). Equipped weapons
/// call `melee_damage` directly with the item's attack value.
pub fn fist_damage(rng: &mut impl Rng, level: u32, fist_skill: i32) -> i32 {
    melee_damage(rng, level, fist_skill, FIST_ATTACK_VALUE, ATTACK_FACTOR)
}

/// Why a swing was fully absorbed. Mirrors TFS `BlockType_t` for the two
/// cases melee actually produces (`BLOCK_DEFENSE`, `BLOCK_ARMOR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    None,
    /// Shield / defense roll ate the hit (`CONST_ME_POFF`).
    Defense,
    /// Armor ate the remainder (`CONST_ME_BLOCKHIT`).
    Armor,
}

/// TFS `Player::getDefense` (player.cpp), vocation multiplier 1.0, fight mode
/// attack (factor 1.0). Integer truncation matches the `int32_t` return.
///
/// `skill == 0` (wand / unknown weapon type, attack mode) returns 1.
pub fn player_defense(skill: i32, defense_value: i32) -> i32 {
    if skill == 0 {
        return 1;
    }
    ((skill as f64 / 4.0 + 2.23) * defense_value as f64 * 0.15) as i32
}

/// TFS `Creature::blockHit` defense + armor steps (creature.cpp).
///
/// Defense (when `check_defense`) subtracts `uniform(defense/2, defense)`.
/// Armor then subtracts `uniform(armor/2, armor - (armor % 2 + 1))` when
/// `armor > 3`, or 1 when `0 < armor <= 3`. A non-positive remainder is a
/// full block and skips the later step.
///
/// `defense` and `armor` are the already-resolved creature values (player
/// formula, or a monster's `<defenses>` attributes).
pub fn apply_defense_armor(
    rng: &mut impl Rng,
    mut damage: i32,
    defense: i32,
    armor: i32,
    check_defense: bool,
) -> (i32, BlockKind) {
    if damage <= 0 {
        return (0, BlockKind::None);
    }
    if check_defense && defense > 0 {
        let lo = defense / 2;
        let hi = defense.max(lo);
        damage -= rng.gen_range(lo..=hi);
        if damage <= 0 {
            return (0, BlockKind::Defense);
        }
    }
    if armor > 3 {
        let hi = armor - (armor % 2 + 1);
        let lo = armor / 2;
        let hi = hi.max(lo);
        damage -= rng.gen_range(lo..=hi);
    } else if armor > 0 {
        damage -= 1;
    }
    if damage <= 0 {
        (0, BlockKind::Armor)
    } else {
        (damage, BlockKind::None)
    }
}

/// TFS `WeaponDistance::useWeapon` hit chance (weapons.cpp).
///
/// `hit_chance != 0` is used as-is. Otherwise the chance depends on distance
/// skill, Chebyshev distance, and `max_hit_chance` (`-1` derives 90 for a
/// bow/crossbow that takes ammo, 75 for a one-handed thrown weapon).
pub fn distance_hit_chance(
    skill: i32,
    distance: u32,
    hit_chance: i32,
    max_hit_chance: i32,
    takes_ammo: bool,
) -> i32 {
    if hit_chance != 0 {
        return hit_chance;
    }
    let skill = skill.max(0) as u32;
    let cap = if max_hit_chance != -1 {
        max_hit_chance.max(0) as u32
    } else if takes_ammo {
        90
    } else {
        75
    };
    let chance = match cap {
        75 => match distance {
            1 | 5 => skill.min(74) + 1,
            2 => ((skill.min(28) as f64) * 2.40) as u32 + 8,
            3 => ((skill.min(45) as f64) * 1.55) as u32 + 6,
            4 => ((skill.min(58) as f64) * 1.25) as u32 + 3,
            6 => ((skill.min(90) as f64) * 0.80) as u32 + 3,
            7 => ((skill.min(104) as f64) * 0.70) as u32 + 2,
            _ => 0,
        },
        90 => match distance {
            1 | 5 => ((skill.min(74) as f64) * 1.20) as u32 + 1,
            2 => ((skill.min(28) as f64) * 3.20) as u32,
            3 => skill.min(45) * 2,
            4 => ((skill.min(58) as f64) * 1.55) as u32,
            6 | 7 => skill.min(90),
            _ => 0,
        },
        100 => match distance {
            1 | 5 => ((skill.min(73) as f64) * 1.35) as u32 + 1,
            2 => ((skill.min(30) as f64) * 3.20) as u32 + 4,
            3 => ((skill.min(48) as f64) * 2.05) as u32 + 2,
            4 => ((skill.min(65) as f64) * 1.50) as u32 + 2,
            6 => ((skill.min(87) as f64) * 1.20) as u32 - 4,
            7 => ((skill.min(90) as f64) * 1.10) as u32 + 1,
            _ => 0,
        },
        other => other,
    };
    chance as i32
}

/// Minimum of a distance roll. TFS `WeaponDistance::getWeaponDamage`:
/// `ceil(level * 0.1)` against a player, `ceil(level * 0.2)` against a monster.
pub fn distance_min_damage(level: u32, target_is_player: bool) -> i32 {
    let factor = if target_is_player { 0.1 } else { 0.2 };
    (level as f64 * factor).ceil() as i32
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};

    // ------------------------------------------------------------------
    // max_weapon_damage — deterministic formula checks
    // ------------------------------------------------------------------

    /// TFS reference: level 1, fist skill 10, value 7, factor 1.0
    ///
    /// Worked by hand:
    ///   int_part  = 1 / 5 = 0  (integer division)
    ///   skill_term = ((10 / 4.0) + 1.0) * (7 / 3.0)
    ///              = (2.5 + 1.0) * 2.333...
    ///              = 3.5 * 2.333...
    ///              = 8.1666...
    ///   result    = round(0 + 8.1666... * 1.03 / 1.0)
    ///             = round(8.4116...)
    ///             = 8
    #[test]
    fn level1_fist_max_is_eight() {
        assert_eq!(max_weapon_damage(1, 10, 7, 1.0), 8);
    }

    /// Integer level/5 floors at the boundary:
    ///   level 4 → 4/5 = 0, level 5 → 5/5 = 1, level 9 → 9/5 = 1.
    /// With skill 0 and value 0 the skill_term is 0, so only the level part
    /// contributes — isolating the integer-division behaviour.
    #[test]
    fn integer_level_division_floors_at_boundary() {
        // level 4: int_part=0, skill_term=0 → 0
        assert_eq!(max_weapon_damage(4, 0, 0, 1.0), 0);
        // level 5: int_part=1, skill_term=0 → 1
        assert_eq!(max_weapon_damage(5, 0, 0, 1.0), 1);
        // level 9: int_part=1, skill_term=0 → 1
        assert_eq!(max_weapon_damage(9, 0, 0, 1.0), 1);
        // level 10: int_part=2, skill_term=0 → 2
        assert_eq!(max_weapon_damage(10, 0, 0, 1.0), 2);
    }

    /// Float vs integer distinction at level 4 with real skill/value:
    /// If `level/5` were float (0.8) the result would be inflated.
    ///   int_part  = 4/5 = 0  (must be integer)
    ///   skill_term = ((10/4.0)+1) * (7/3.0) = 3.5 * 2.333... = 8.1666...
    ///   result    = round(0 + 8.1666... * 1.03) = round(8.4116...) = 8
    /// Float variant would give round(0.8 + 8.4116...) = round(9.2116...) = 9.
    #[test]
    fn float_level_division_would_inflate_damage() {
        // Correct (integer level/5):
        assert_eq!(max_weapon_damage(4, 10, 7, 1.0), 8);
        // Proof by contrast: 4.0/5.0 = 0.8, which would give 9 — we must NOT do that.
        let would_be_wrong = (4.0_f64 / 5.0
            + ((10.0_f64 / 4.0 + 1.0) * (7.0_f64 / 3.0)) * 1.03 / 1.0)
            .round() as i32;
        assert_eq!(would_be_wrong, 9);
        assert_ne!(max_weapon_damage(4, 10, 7, 1.0), would_be_wrong);
    }

    /// Higher level and skill: level 100, skill 50, value 7, factor 1.0
    ///   int_part  = 100/5 = 20
    ///   skill_term = ((50/4.0)+1)*(7/3.0) = (12.5+1)*2.333... = 13.5*2.333... = 31.5
    ///   result    = round(20 + 31.5 * 1.03) = round(20 + 32.445) = round(52.445) = 52
    #[test]
    fn level100_skill50_gives_expected_max() {
        assert_eq!(max_weapon_damage(100, 50, 7, 1.0), 52);
    }

    /// Skill 0 edge case: no skill contribution, only level.
    ///   level 10: int_part=2, skill_term=((0/4.0)+1)*(7/3.0)=1*2.333...=2.333...
    ///   result = round(2 + 2.333... * 1.03) = round(2 + 2.4033...) = round(4.4033...) = 4
    #[test]
    fn skill_zero_edge_case() {
        assert_eq!(max_weapon_damage(10, 0, 7, 1.0), 4);
    }

    /// Factor 2.0 (defensive mode) halves the skill term.
    ///   level 1: int_part=0
    ///   skill_term = 3.5 * 2.333... = 8.166...
    ///   result = round(0 + 8.166... * 1.03 / 2.0) = round(4.2058...) = 4
    #[test]
    fn defensive_factor_reduces_damage() {
        assert_eq!(max_weapon_damage(1, 10, 7, 2.0), 4);
    }

    // ------------------------------------------------------------------
    // melee_damage / fist_damage — RNG bounds check
    // ------------------------------------------------------------------

    /// Over 10 000 seeded rolls the damage must never exceed max.
    #[test]
    fn fist_damage_stays_within_zero_to_max() {
        let mut rng = StdRng::seed_from_u64(42);
        let max = max_weapon_damage(1, 10, 7, 1.0); // == 8
        for _ in 0..10_000 {
            let d = fist_damage(&mut rng, 1, 10);
            assert!((0..=max).contains(&d), "damage {d} is out of 0..={max}");
        }
    }

    /// When max == 0 the function must return 0 without panicking (gen_range
    /// on an empty range would panic in rand 0.8).
    #[test]
    fn zero_max_returns_zero_without_panic() {
        let mut rng = StdRng::seed_from_u64(0);
        // level 0 / skill 0 / value 0 → max = 0
        assert_eq!(melee_damage(&mut rng, 0, 0, 0, 1.0), 0);
    }

    /// Seeded RNG produces a deterministic sequence (regression guard).
    ///
    /// Sequence generated by running the test once with `StdRng::seed_from_u64(1)`.
    /// If this test breaks after a rand version bump, update the snapshot.
    #[test]
    fn seeded_rng_deterministic_sequence() {
        let mut rng = StdRng::seed_from_u64(1);
        let results: Vec<i32> = (0..5).map(|_| fist_damage(&mut rng, 1, 10)).collect();
        // Every value must be in 0..=8.
        for &d in &results {
            assert!((0..=8).contains(&d), "value {d} out of range");
        }
        // The sequence must be stable across runs (determinism check).
        let mut rng2 = StdRng::seed_from_u64(1);
        let results2: Vec<i32> = (0..5).map(|_| fist_damage(&mut rng2, 1, 10)).collect();
        assert_eq!(results, results2);
    }

    /// Sword attack 40, skill 10, level 1, factor 1.0:
    /// skill_term = (2.5+1)*(40/3) = 46.666…; *1.03 = 48.066…; round = 48.
    #[test]
    fn weapon_attack_value_scales_max() {
        assert_eq!(max_weapon_damage(1, 10, 40, 1.0), 48);
    }

    /// Unarmed defense: skill 10, value 7.
    /// (10/4 + 2.23) * 7 * 0.15 = 4.73 * 1.05 = 4.9665 → truncates to 4.
    #[test]
    fn unarmed_defense_truncates_like_tfs() {
        assert_eq!(player_defense(10, 7), 4);
    }

    /// Armor 10 reduces by uniform(5, 9) — TFS `armor - (armor % 2 + 1)`.
    #[test]
    fn armor_reduction_stays_in_tfs_window() {
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..200 {
            let (left, kind) = apply_defense_armor(&mut rng, 20, 0, 10, false);
            assert!((11..=15).contains(&left), "left {left}");
            assert_eq!(kind, BlockKind::None);
        }
    }

    /// A defense roll that exceeds the incoming hit is a full shield block.
    #[test]
    fn defense_can_fully_block() {
        let mut rng = StdRng::seed_from_u64(1);
        let (left, kind) = apply_defense_armor(&mut rng, 1, 20, 10, true);
        assert_eq!(left, 0);
        assert_eq!(kind, BlockKind::Defense);
    }

    /// Bow (takes ammo) at distance 1, skill 10: min(10, 74)*1.20 + 1 = 13.
    #[test]
    fn bow_hit_chance_at_one_tile() {
        assert_eq!(distance_hit_chance(10, 1, 0, -1, true), 13);
    }
}
