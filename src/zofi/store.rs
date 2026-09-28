//! Storage for ZofiDesk accounts, sessions, address books and the device inventory.
//!
//! Every query lives in this file and sticks to standard SQL, so moving to another engine
//! (e.g. PostgreSQL, for running several servers) only has to touch this module.

use hbb_common::{bail, log, tokio, ResultType};
use once_cell::sync::Lazy;
use sodiumoxide::crypto::hash::sha256;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    ConnectOptions, Executor, Row, SqlitePool,
};
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

pub const DEFAULT_PATH: &str = "zofidesk.sqlite3";
const TOKEN_TTL_SECS: i64 = 30 * 24 * 3600;
const STATUS_ACTIVE: i64 = 1;
const STATUS_DISABLED: i64 = 0;

/// Schema upgrades, one entry per version. Never edit an entry once released; append a new one.
const MIGRATIONS: &[&[&str]] = &[&[
    "create table users (
        id integer primary key,
        username varchar(100) not null unique,
        display_name varchar(100) not null default '',
        password_hash varchar(100) not null,
        is_admin smallint not null default 0,
        status smallint not null default 1,
        created_at bigint not null
    )",
    "create table tokens (
        token_hash varchar(64) primary key,
        user_id integer not null references users(id),
        device_id varchar(100) not null default '',
        device_uuid varchar(100) not null default '',
        created_at bigint not null,
        expires_at bigint not null
    )",
    "create index index_tokens_user_id on tokens (user_id)",
    "create table address_books (
        user_id integer primary key references users(id),
        data text not null,
        updated_at bigint not null
    )",
    "create table devices (
        id varchar(100) primary key,
        info text not null default '',
        last_seen_at bigint not null default 0,
        updated_at bigint not null default 0
    )",
]];

// Verified against when the username does not exist, so a failed login takes the same time
// whether or not the account exists.
static DUMMY_HASH: Lazy<String> =
    Lazy::new(|| bcrypt::hash("zofidesk", bcrypt::DEFAULT_COST).unwrap_or_default());

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    pub active: bool,
}

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
    // Heartbeats arrive every 15s from every device, so last-seen times are kept in memory
    // and written in batches by `flush_last_seen`.
    last_seen: Arc<Mutex<HashMap<String, i64>>>,
    // Devices whose system info is already stored, to avoid a query per heartbeat.
    known_devices: Arc<Mutex<HashSet<String>>>,
}

impl Db {
    pub async fn open(path: &str) -> ResultType<Self> {
        let mut options = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        options.log_statements(log::LevelFilter::Debug);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        let db = Db {
            pool,
            last_seen: Default::default(),
            known_devices: Default::default(),
        };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> ResultType<()> {
        self.pool
            .execute("create table if not exists schema_version (version integer not null)")
            .await?;
        let current: Option<i64> = sqlx::query("select max(version) from schema_version")
            .fetch_one(&self.pool)
            .await?
            .try_get(0)?;
        let current = current.unwrap_or(0) as usize;
        for (index, statements) in MIGRATIONS.iter().enumerate().skip(current) {
            let mut tx = self.pool.begin().await?;
            for statement in statements.iter() {
                tx.execute(*statement).await?;
            }
            sqlx::query("insert into schema_version (version) values (?)")
                .bind((index + 1) as i64)
                .execute(&mut tx)
                .await?;
            tx.commit().await?;
        }
        Ok(())
    }

    pub async fn add_user(&self, username: &str, password: &str, is_admin: bool) -> ResultType<()> {
        let username = normalize_username(username)?;
        check_password_strength(password)?;
        let hash = hash_password(password).await?;
        let exists = sqlx::query("select 1 from users where username = ?")
            .bind(&username)
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        if exists {
            bail!("User {username} already exists");
        }
        sqlx::query(
            "insert into users (username, password_hash, is_admin, status, created_at)
             values (?, ?, ?, ?, ?)",
        )
        .bind(&username)
        .bind(hash)
        .bind(is_admin as i64)
        .bind(STATUS_ACTIVE)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_users(&self) -> ResultType<Vec<User>> {
        let rows = sqlx::query(
            "select id, username, display_name, is_admin, status from users order by username",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(user_from_row).collect()
    }

    /// Changes the password and signs the user out everywhere.
    pub async fn set_password(&self, username: &str, password: &str) -> ResultType<()> {
        check_password_strength(password)?;
        let user = self.find_user(username).await?;
        let hash = hash_password(password).await?;
        sqlx::query("update users set password_hash = ? where id = ?")
            .bind(hash)
            .bind(user.id)
            .execute(&self.pool)
            .await?;
        self.revoke_user_tokens(user.id).await
    }

    /// Enables or disables an account. Disabling also signs the user out everywhere.
    pub async fn set_active(&self, username: &str, active: bool) -> ResultType<()> {
        let user = self.find_user(username).await?;
        let status = if active { STATUS_ACTIVE } else { STATUS_DISABLED };
        sqlx::query("update users set status = ? where id = ?")
            .bind(status)
            .bind(user.id)
            .execute(&self.pool)
            .await?;
        if !active {
            self.revoke_user_tokens(user.id).await?;
        }
        Ok(())
    }

    /// Returns the user when the password matches, whether or not the account is active.
    pub async fn check_password(&self, username: &str, password: &str) -> ResultType<Option<User>> {
        let username = username.trim().to_lowercase();
        let row = sqlx::query(
            "select id, username, display_name, is_admin, status, password_hash
             from users where username = ?",
        )
        .bind(&username)
        .fetch_optional(&self.pool)
        .await?;
        let (user, hash) = match &row {
            Some(row) => (Some(user_from_row(row)?), row.try_get::<String, _>("password_hash")?),
            None => (None, DUMMY_HASH.clone()),
        };
        let password = password.to_owned();
        let matches =
            tokio::task::spawn_blocking(move || bcrypt::verify(password, &hash).unwrap_or(false))
                .await?;
        Ok(if matches { user } else { None })
    }

    pub async fn create_token(&self, user_id: i64, device_id: &str, device_uuid: &str) -> ResultType<String> {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let now = now();
        sqlx::query(
            "insert into tokens (token_hash, user_id, device_id, device_uuid, created_at, expires_at)
             values (?, ?, ?, ?, ?, ?)",
        )
        .bind(hash_token(&token))
        .bind(user_id)
        .bind(device_id)
        .bind(device_uuid)
        .bind(now)
        .bind(now + TOKEN_TTL_SECS)
        .execute(&self.pool)
        .await?;
        Ok(token)
    }

    /// Returns the owner of a token if the token is unexpired and the account active.
    pub async fn user_for_token(&self, token: &str) -> ResultType<Option<User>> {
        let row = sqlx::query(
            "select u.id, u.username, u.display_name, u.is_admin, u.status
             from tokens t join users u on u.id = t.user_id
             where t.token_hash = ? and t.expires_at > ? and u.status = ?",
        )
        .bind(hash_token(token))
        .bind(now())
        .bind(STATUS_ACTIVE)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(user_from_row).transpose()
    }

    pub async fn revoke_token(&self, token: &str) -> ResultType<()> {
        sqlx::query("delete from tokens where token_hash = ?")
            .bind(hash_token(token))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn revoke_user_tokens(&self, user_id: i64) -> ResultType<()> {
        sqlx::query("delete from tokens where user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn load_address_book(&self, user_id: i64) -> ResultType<Option<String>> {
        let row = sqlx::query("select data from address_books where user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| row.try_get("data")).transpose().map_err(Into::into)
    }

    pub async fn save_address_book(&self, user_id: i64, data: &str) -> ResultType<()> {
        sqlx::query(
            "insert into address_books (user_id, data, updated_at) values (?, ?, ?)
             on conflict (user_id) do update set data = excluded.data, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(data)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records a heartbeat and tells whether the device's system info still has to be uploaded.
    pub async fn touch_device(&self, id: &str) -> ResultType<bool> {
        self.last_seen.lock().unwrap().insert(id.to_owned(), now());
        if self.known_devices.lock().unwrap().contains(id) {
            return Ok(false);
        }
        let known = sqlx::query("select 1 from devices where id = ? and info <> ''")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        if known {
            self.known_devices.lock().unwrap().insert(id.to_owned());
        }
        Ok(!known)
    }

    pub async fn save_device_info(&self, id: &str, info: &str) -> ResultType<()> {
        let now = now();
        sqlx::query(
            "insert into devices (id, info, last_seen_at, updated_at) values (?, ?, ?, ?)
             on conflict (id) do update set info = excluded.info, updated_at = excluded.updated_at",
        )
        .bind(id)
        .bind(info)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.known_devices.lock().unwrap().insert(id.to_owned());
        Ok(())
    }

    pub async fn flush_last_seen(&self) -> ResultType<()> {
        let pending = std::mem::take(&mut *self.last_seen.lock().unwrap());
        if pending.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for (id, seen) in &pending {
            sqlx::query(
                "insert into devices (id, last_seen_at) values (?, ?)
                 on conflict (id) do update set last_seen_at = excluded.last_seen_at",
            )
            .bind(id)
            .bind(seen)
            .execute(&mut tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    #[cfg(test)]
    async fn device_last_seen(&self, id: &str) -> ResultType<Option<i64>> {
        let row = sqlx::query("select last_seen_at from devices where id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| row.try_get(0)).transpose().map_err(Into::into)
    }

    async fn find_user(&self, username: &str) -> ResultType<User> {
        let username = username.trim().to_lowercase();
        let row = sqlx::query(
            "select id, username, display_name, is_admin, status from users where username = ?",
        )
        .bind(&username)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => user_from_row(&row),
            None => bail!("User {username} not found"),
        }
    }
}

fn user_from_row(row: &sqlx::sqlite::SqliteRow) -> ResultType<User> {
    Ok(User {
        id: row.try_get("id")?,
        username: row.try_get("username")?,
        display_name: row.try_get("display_name")?,
        is_admin: row.try_get::<i64, _>("is_admin")? != 0,
        active: row.try_get::<i64, _>("status")? == STATUS_ACTIVE,
    })
}

fn normalize_username(username: &str) -> ResultType<String> {
    let username = username.trim().to_lowercase();
    let valid = !username.is_empty()
        && username.len() <= 100
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'));
    if !valid {
        bail!("Invalid username: use letters, digits, '.', '_', '-' or '@'");
    }
    Ok(username)
}

fn check_password_strength(password: &str) -> ResultType<()> {
    if password.chars().count() < 8 {
        bail!("The password must have at least 8 characters");
    }
    Ok(())
}

async fn hash_password(password: &str) -> ResultType<String> {
    let password = password.to_owned();
    Ok(tokio::task::spawn_blocking(move || bcrypt::hash(password, bcrypt::DEFAULT_COST)).await??)
}

// Only a hash of each token is stored, so a copy of the database cannot be used to sign in.
fn hash_token(token: &str) -> String {
    sha256::hash(token.as_bytes())
        .0
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now() -> i64 {
    crate::common::now() as i64
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) struct TempDb {
        pub db: Db,
        path: String,
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                std::fs::remove_file(format!("{}{suffix}", self.path)).ok();
            }
        }
    }

    pub(crate) async fn temp_db() -> TempDb {
        let path = std::env::temp_dir()
            .join(format!("zofidesk-test-{}.sqlite3", uuid::Uuid::new_v4().simple()))
            .to_string_lossy()
            .replace('\\', "/");
        let db = Db::open(&path).await.unwrap();
        TempDb { db, path }
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let temp = temp_db().await;
        temp.db.migrate().await.unwrap();
        let versions: i64 = sqlx::query("select count(*) from schema_version")
            .fetch_one(&temp.db.pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(versions as usize, MIGRATIONS.len());
    }

    #[tokio::test]
    async fn login_with_correct_password_only() {
        let temp = temp_db().await;
        let db = &temp.db;
        db.add_user("Luis", "correct-horse", true).await.unwrap();
        let user = db.check_password("luis", "correct-horse").await.unwrap().unwrap();
        assert_eq!(user.username, "luis");
        assert!(user.is_admin && user.active);
        assert!(db.check_password("luis", "wrong-password").await.unwrap().is_none());
        assert!(db.check_password("nobody", "correct-horse").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_duplicates_and_weak_input() {
        let temp = temp_db().await;
        let db = &temp.db;
        db.add_user("ana", "password1", false).await.unwrap();
        assert!(db.add_user("ANA", "password2", false).await.is_err());
        assert!(db.add_user("bob", "short", false).await.is_err());
        assert!(db.add_user("bad name", "password1", false).await.is_err());
    }

    #[tokio::test]
    async fn tokens_follow_account_state() {
        let temp = temp_db().await;
        let db = &temp.db;
        db.add_user("ana", "password1", false).await.unwrap();
        let user = db.check_password("ana", "password1").await.unwrap().unwrap();
        let token = db.create_token(user.id, "123456789", "uuid").await.unwrap();
        assert_eq!(db.user_for_token(&token).await.unwrap(), Some(user.clone()));
        assert!(db.user_for_token("not-a-token").await.unwrap().is_none());

        db.set_active("ana", false).await.unwrap();
        assert!(db.user_for_token(&token).await.unwrap().is_none());

        db.set_active("ana", true).await.unwrap();
        let token = db.create_token(user.id, "", "").await.unwrap();
        db.set_password("ana", "password2").await.unwrap();
        assert!(db.user_for_token(&token).await.unwrap().is_none());

        let token = db.create_token(user.id, "", "").await.unwrap();
        db.revoke_token(&token).await.unwrap();
        assert!(db.user_for_token(&token).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn address_book_round_trip() {
        let temp = temp_db().await;
        let db = &temp.db;
        db.add_user("ana", "password1", false).await.unwrap();
        assert_eq!(db.load_address_book(1).await.unwrap(), None);
        db.save_address_book(1, "{\"peers\":[]}").await.unwrap();
        db.save_address_book(1, "{\"peers\":[1]}").await.unwrap();
        assert_eq!(db.load_address_book(1).await.unwrap().as_deref(), Some("{\"peers\":[1]}"));
    }

    #[tokio::test]
    async fn device_inventory_and_last_seen() {
        let temp = temp_db().await;
        let db = &temp.db;
        assert!(db.touch_device("111").await.unwrap());
        db.save_device_info("111", "{\"os\":\"Windows\"}").await.unwrap();
        assert!(!db.touch_device("111").await.unwrap());
        db.flush_last_seen().await.unwrap();
        assert!(db.device_last_seen("111").await.unwrap().unwrap() > 0);
        db.touch_device("222").await.unwrap();
        db.flush_last_seen().await.unwrap();
        assert!(db.device_last_seen("222").await.unwrap().is_some());
    }
}
