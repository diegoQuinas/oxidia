-- Accounts and their characters.
--
-- M1 stores the password verbatim; the login flow only needs to match it and
-- list character names. Hashing (TFS uses SHA-1) is deferred to a later
-- milestone — see PROGRESS.md.

CREATE TABLE accounts (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT    NOT NULL UNIQUE,
    password        TEXT    NOT NULL,
    premium_ends_at INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE players (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name       TEXT    NOT NULL UNIQUE,
    level      INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX idx_players_account_id ON players(account_id);
