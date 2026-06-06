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
