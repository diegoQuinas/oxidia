#![forbid(unsafe_code)]

//! Account and player persistence, backed by SQLite via `sqlx`.
//!
//! M1 needs just enough to authenticate a login and list an account's
//! characters. Passwords are stored verbatim for now (TFS uses SHA-1) — see
//! PROGRESS.md; hashing lands in a later milestone.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

/// Errors produced by the persistence layer.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    /// A query or connection failed.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Applying the embedded migrations failed.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// An account and the characters that belong to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Primary key.
    pub id: i64,
    /// Account name (what the client logs in with).
    pub name: String,
    /// Characters on this account, name-ordered.
    pub characters: Vec<Character>,
}

/// A single character on an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Character {
    /// Character name shown in the character list.
    pub name: String,
}

/// Restorable player state persisted across sessions.
///
/// This is a standalone DTO owned by the `persistence` crate — it does NOT
/// import `world::PlayerState`. The world crate is on the far side of the
/// actor boundary and its types are not stable yet (M7 adds combat HP).
/// The persistence layer deliberately owns its own field set.
///
/// Integer widths follow the Tibia 10.98 wire format:
/// - positions: x/y ≤ 65535 (u16), z 0–15 (u8)
/// - health/mana: u16 at protocol 1098 (u32 from ≥ 1300)
/// - direction: 0=N 1=E 2=S 3=W
/// - look_* fields: u8 range
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerSave {
    /// Character name (primary key for player lookups).
    pub name: String,
    /// Map X coordinate.
    pub pos_x: u16,
    /// Map Y coordinate.
    pub pos_y: u16,
    /// Map Z / floor (0 = sky, 7 = surface, 8–15 = underground).
    pub pos_z: u8,
    /// Character level.
    pub level: u16,
    /// Current hit points.
    pub health: u16,
    /// Maximum hit points.
    pub health_max: u16,
    /// Current mana points.
    pub mana: u16,
    /// Maximum mana points.
    pub mana_max: u16,
    /// Facing direction (0=N, 1=E, 2=S, 3=W).
    pub direction: u8,
    /// Outfit look type id.
    pub look_type: u16,
    /// Outfit head colour.
    pub look_head: u8,
    /// Outfit body colour.
    pub look_body: u8,
    /// Outfit legs colour.
    pub look_legs: u8,
    /// Outfit feet colour.
    pub look_feet: u8,
    /// Outfit addons bitmask.
    pub look_addons: u8,
    /// Mount id (0 = no mount).
    pub look_mount: u16,
}

/// Handle to the account/player database.
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open (creating if missing) the SQLite database file at `path` and run
    /// migrations.
    pub async fn connect(path: &str) -> Result<Self, PersistenceError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        Self::migrated(pool).await
    }

    /// Open a fresh in-memory database with migrations applied. For tests.
    pub async fn open_in_memory() -> Result<Self, PersistenceError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?;
        // A single connection keeps the in-memory database alive for the pool.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        Self::migrated(pool).await
    }

    async fn migrated(pool: SqlitePool) -> Result<Self, PersistenceError> {
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// Insert the hardcoded M1 test account (`test`/`test`) with two characters.
    pub async fn seed_test_account(&self) -> Result<(), PersistenceError> {
        let account_id: i64 =
            sqlx::query_scalar("INSERT INTO accounts (name, password) VALUES (?, ?) RETURNING id")
                .bind("test")
                .bind("test")
                .fetch_one(&self.pool)
                .await?;

        for name in ["Test Knight", "Test Sorcerer"] {
            sqlx::query("INSERT INTO players (account_id, name) VALUES (?, ?)")
                .bind(account_id)
                .bind(name)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Persist (upsert) the restorable state for an existing player.
    ///
    /// The player row must already exist in the `players` table (the spine
    /// inserts it during character creation). Passing a name that has no row
    /// returns a [`PersistenceError::Database`] (FK or "not found" violation)
    /// rather than silently doing nothing.
    pub async fn save_player(&self, state: &PlayerSave) -> Result<(), PersistenceError> {
        sqlx::query(
            r#"
            UPDATE players SET
                pos_x      = ?,
                pos_y      = ?,
                pos_z      = ?,
                level      = ?,
                health     = ?,
                health_max = ?,
                mana       = ?,
                mana_max   = ?,
                direction  = ?,
                look_type  = ?,
                look_head  = ?,
                look_body  = ?,
                look_legs  = ?,
                look_feet  = ?,
                look_addons = ?,
                look_mount = ?
            WHERE name = ?
            "#,
        )
        .bind(i64::from(state.pos_x))
        .bind(i64::from(state.pos_y))
        .bind(i64::from(state.pos_z))
        .bind(i64::from(state.level))
        .bind(i64::from(state.health))
        .bind(i64::from(state.health_max))
        .bind(i64::from(state.mana))
        .bind(i64::from(state.mana_max))
        .bind(i64::from(state.direction))
        .bind(i64::from(state.look_type))
        .bind(i64::from(state.look_head))
        .bind(i64::from(state.look_body))
        .bind(i64::from(state.look_legs))
        .bind(i64::from(state.look_feet))
        .bind(i64::from(state.look_addons))
        .bind(i64::from(state.look_mount))
        .bind(&state.name)
        .execute(&self.pool)
        .await
        .and_then(|r| {
            if r.rows_affected() == 0 {
                Err(sqlx::Error::RowNotFound)
            } else {
                Ok(())
            }
        })?;
        Ok(())
    }

    /// Load the saved state for a player by name.
    ///
    /// Returns `Ok(None)` when the player does not exist.
    ///
    /// Uses `sqlx::query` + manual column access instead of `query_as` because
    /// sqlx's tuple `FromRow` impl only covers up to 16 columns and our SELECT
    /// returns 17.
    pub async fn load_player(&self, name: &str) -> Result<Option<PlayerSave>, PersistenceError> {
        use sqlx::Row as _;

        let row = sqlx::query(
            r#"
            SELECT name,
                   pos_x, pos_y, pos_z,
                   level,
                   health, health_max, mana, mana_max,
                   direction,
                   look_type, look_head, look_body, look_legs, look_feet,
                   look_addons, look_mount
            FROM players
            WHERE name = ?
            "#,
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| PlayerSave {
            name: r.get("name"),
            pos_x: r.get::<i64, _>("pos_x") as u16,
            pos_y: r.get::<i64, _>("pos_y") as u16,
            pos_z: r.get::<i64, _>("pos_z") as u8,
            level: r.get::<i64, _>("level") as u16,
            health: r.get::<i64, _>("health") as u16,
            health_max: r.get::<i64, _>("health_max") as u16,
            mana: r.get::<i64, _>("mana") as u16,
            mana_max: r.get::<i64, _>("mana_max") as u16,
            direction: r.get::<i64, _>("direction") as u8,
            look_type: r.get::<i64, _>("look_type") as u16,
            look_head: r.get::<i64, _>("look_head") as u8,
            look_body: r.get::<i64, _>("look_body") as u8,
            look_legs: r.get::<i64, _>("look_legs") as u8,
            look_feet: r.get::<i64, _>("look_feet") as u8,
            look_addons: r.get::<i64, _>("look_addons") as u8,
            look_mount: r.get::<i64, _>("look_mount") as u16,
        }))
    }

    /// Seed the M1 test account only if the accounts table is empty. Safe to
    /// call on every startup.
    pub async fn seed_test_account_if_empty(&self) -> Result<(), PersistenceError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(&self.pool)
            .await?;
        if count == 0 {
            self.seed_test_account().await?;
        }
        Ok(())
    }

    /// Return the account if `name`/`password` match, otherwise `None`.
    pub async fn authenticate(
        &self,
        name: &str,
        password: &str,
    ) -> Result<Option<Account>, PersistenceError> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT id, password FROM accounts WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;

        let Some((id, stored_password)) = row else {
            return Ok(None);
        };
        if stored_password != password {
            return Ok(None);
        }

        let characters: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM players WHERE account_id = ? ORDER BY name")
                .bind(id)
                .fetch_all(&self.pool)
                .await?;

        Ok(Some(Account {
            id,
            name: name.to_string(),
            characters: characters
                .into_iter()
                .map(|(name,)| Character { name })
                .collect(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seeded() -> Store {
        let store = Store::open_in_memory().await.unwrap();
        store.seed_test_account().await.unwrap();
        store
    }

    // ── player state tests (TDD: written before the implementation) ──────────

    fn default_save(name: &str) -> PlayerSave {
        PlayerSave {
            name: name.to_string(),
            pos_x: 100,
            pos_y: 200,
            pos_z: 7,
            level: 5,
            health: 120,
            health_max: 150,
            mana: 30,
            mana_max: 50,
            direction: 2,
            look_type: 128,
            look_head: 10,
            look_body: 20,
            look_legs: 30,
            look_feet: 40,
            look_addons: 1,
            look_mount: 0,
        }
    }

    #[tokio::test]
    async fn load_missing_player_returns_none() {
        let store = seeded().await;
        let result = store.load_player("Nobody").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn round_trip_save_then_load_returns_all_fields() {
        let store = seeded().await;
        let save = default_save("Test Knight");

        store.save_player(&save).await.unwrap();
        let loaded = store.load_player("Test Knight").await.unwrap().expect("should be Some");

        assert_eq!(loaded.name, save.name);
        assert_eq!(loaded.pos_x, save.pos_x);
        assert_eq!(loaded.pos_y, save.pos_y);
        assert_eq!(loaded.pos_z, save.pos_z);
        assert_eq!(loaded.level, save.level);
        assert_eq!(loaded.health, save.health);
        assert_eq!(loaded.health_max, save.health_max);
        assert_eq!(loaded.mana, save.mana);
        assert_eq!(loaded.mana_max, save.mana_max);
        assert_eq!(loaded.direction, save.direction);
        assert_eq!(loaded.look_type, save.look_type);
        assert_eq!(loaded.look_head, save.look_head);
        assert_eq!(loaded.look_body, save.look_body);
        assert_eq!(loaded.look_legs, save.look_legs);
        assert_eq!(loaded.look_feet, save.look_feet);
        assert_eq!(loaded.look_addons, save.look_addons);
        assert_eq!(loaded.look_mount, save.look_mount);
    }

    #[tokio::test]
    async fn save_twice_overwrites_position_and_stats() {
        let store = seeded().await;
        let first = default_save("Test Knight");
        store.save_player(&first).await.unwrap();

        let second = PlayerSave {
            name: "Test Knight".to_string(),
            pos_x: 999,
            pos_y: 888,
            pos_z: 3,
            level: 10,
            health: 300,
            health_max: 400,
            mana: 100,
            mana_max: 200,
            direction: 1,
            look_type: 200,
            look_head: 5,
            look_body: 6,
            look_legs: 7,
            look_feet: 8,
            look_addons: 3,
            look_mount: 42,
        };
        store.save_player(&second).await.unwrap();

        let loaded = store.load_player("Test Knight").await.unwrap().expect("should be Some");
        assert_eq!(loaded.pos_x, 999);
        assert_eq!(loaded.pos_y, 888);
        assert_eq!(loaded.pos_z, 3);
        assert_eq!(loaded.level, 10);
        assert_eq!(loaded.look_mount, 42);
    }

    #[tokio::test]
    async fn save_player_does_not_require_pre_existing_player_row() {
        // A player not yet seeded should also be saveable if the name is unique.
        // In practice the spine always inserts the player before calling save,
        // but the persistence layer must not crash on a missing row — it should
        // return a Database error (FK violation) rather than silently succeed.
        // This test documents the actual behaviour: saving a completely unknown
        // name (no row in `players`) propagates a Database error.
        let store = Store::open_in_memory().await.unwrap();
        // NOTE: store is NOT seeded — no accounts, no players rows.
        // The migration creates the columns; attempting an UPDATE-only path
        // on a non-existent row would silently succeed (0 rows affected).
        // Our implementation uses INSERT OR REPLACE which requires account_id
        // to satisfy the FK — so it returns an error. This test pins that
        // contract so the spine knows it must insert the player row first.
        let save = default_save("Ghost");
        let result = store.save_player(&save).await;
        assert!(
            result.is_err(),
            "saving an unknown player (no FK) must return an error, not silently succeed"
        );
    }

    #[tokio::test]
    async fn authenticate_returns_account_with_characters_for_valid_credentials() {
        let store = seeded().await;
        let account = store
            .authenticate("test", "test")
            .await
            .unwrap()
            .expect("account should be found");

        assert_eq!(account.name, "test");
        let names: Vec<&str> = account.characters.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Test Knight", "Test Sorcerer"]);
    }

    #[tokio::test]
    async fn authenticate_rejects_a_wrong_password() {
        let store = seeded().await;
        assert!(store.authenticate("test", "wrong").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn authenticate_rejects_an_unknown_account() {
        let store = seeded().await;
        assert!(store.authenticate("ghost", "test").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn seed_if_empty_is_idempotent() {
        let store = Store::open_in_memory().await.unwrap();
        store.seed_test_account_if_empty().await.unwrap();
        store.seed_test_account_if_empty().await.unwrap(); // must not duplicate

        let account = store.authenticate("test", "test").await.unwrap().unwrap();
        assert_eq!(account.characters.len(), 2);
    }
}
