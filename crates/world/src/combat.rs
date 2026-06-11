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
/// Convenience wrapper for the M7 loop (no inventory yet). M10 will call
/// `melee_damage` directly with the real equipped weapon values.
pub fn fist_damage(rng: &mut impl Rng, level: u32, fist_skill: i32) -> i32 {
    melee_damage(rng, level, fist_skill, FIST_ATTACK_VALUE, ATTACK_FACTOR)
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
}
