//! # Authentication & users (platform slice)
//!
//! ## Почему такие библиотеки
//!
//! * **`tower-sessions`** — серверные сессии для axum: подписанная cookie хранит
//!   только id сессии, данные лежат на сервере. Для server-rendered htmx-приложения
//!   это правильнее JWT (logout = удаление записи, никаких «живучих» токенов на
//!   клиенте).
//! * **`argon2`** (RustCrypto) — Argon2id, рекомендованный алгоритм для паролей;
//!   в 0.6 API сам генерирует соль и отдаёт PHC-строку (`$argon2id$v=19$…`).
//! * **`sqlx` + SQLite** — хранилище пользователей. SQLite выбран, чтобы платформа
//!   запускалась одним процессом без внешней БД; переезд на Postgres — это смена
//!   URL и feature, запросы у нас простые.
//!
//! ## Что важно для безопасности
//!
//! * пароли сравниваются только через Argon2 (никаких `==` по хэшам);
//! * хэширование — дорогая операция (~десятки мс CPU), поэтому вызывается в
//!   `spawn_blocking`, чтобы не блокировать async-рантайм;
//! * сообщение об ошибке входа всегда одинаковое («invalid username or password»),
//!   чтобы не подсказывать, существует ли пользователь;
//! * в сессии хранится только `user_id`.

use sqlx::SqlitePool;
use tower_sessions::Session;

use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};

/// Ключ, под которым id пользователя лежит в сессии.
const SESSION_USER_KEY: &str = "user_id";

/// Пользователь платформы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// Первичный ключ (используется и как «тенант»: `workspaces/<id>/…`).
    pub id: i64,
    /// Логин (уникальный).
    pub username: String,
}

/// Ошибка слоя аутентификации (сообщение безопасно показывать пользователю).
#[derive(Debug)]
pub struct AuthError(pub String);

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<sqlx::Error> for AuthError {
    fn from(error: sqlx::Error) -> Self {
        AuthError(format!("database error: {error}"))
    }
}

/// Создать таблицу пользователей, если её ещё нет.
///
/// Мини-миграция вместо отдельного инструмента: у нас одна таблица и никакого
/// состояния, которое нужно накатывать постепенно.
pub async fn init_db(pool: &SqlitePool) -> Result<(), AuthError> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            username      TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Хэшировать пароль (Argon2id, соль генерируется внутри).
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|error| AuthError(format!("cannot hash password: {error}")))
}

/// Проверить пароль против PHC-строки из БД.
pub fn verify_password(password: &str, phc: &str) -> bool {
    PasswordHash::new(phc)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

/// Валидация логина/пароля на уровне платформы (до обращения к БД).
pub fn validate_credentials(username: &str, password: &str) -> Result<(), AuthError> {
    let username = username.trim();
    if username.len() < 3 || username.len() > 32 {
        return Err(AuthError("username must be 3..32 characters".into()));
    }
    if !username
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(AuthError(
            "username may contain letters, digits, '_', '-', '.'".into(),
        ));
    }
    if password.len() < 8 {
        return Err(AuthError("password must be at least 8 characters".into()));
    }
    Ok(())
}

/// Создать пользователя; возвращает его id.
pub async fn create_user(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
) -> Result<i64, AuthError> {
    let username = username.trim();
    let result = sqlx::query("INSERT INTO users (username, password_hash) VALUES (?, ?)")
        .bind(username)
        .bind(password_hash)
        .execute(pool)
        .await;

    match result {
        Ok(done) => Ok(done.last_insert_rowid()),
        Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
            Err(AuthError(format!("username '{username}' is already taken")))
        }
        Err(error) => Err(error.into()),
    }
}

/// Найти пользователя по логину: `(id, username, password_hash)`.
pub async fn find_user(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<(i64, String, String)>, AuthError> {
    let row: Option<(i64, String, String)> =
        sqlx::query_as("SELECT id, username, password_hash FROM users WHERE username = ?")
            .bind(username.trim())
            .fetch_optional(pool)
            .await?;
    Ok(row)
}

/// Положить пользователя в сессию (вход).
pub async fn login(session: &Session, user_id: i64) -> Result<(), AuthError> {
    session
        .insert(SESSION_USER_KEY, user_id)
        .await
        .map_err(|error| AuthError(format!("cannot start session: {error}")))?;
    // Защита от session fixation: после успешного входа меняем id сессии.
    session
        .cycle_id()
        .await
        .map_err(|error| AuthError(format!("cannot cycle session id: {error}")))?;
    Ok(())
}

/// Выйти: полностью очищаем серверную сессию.
pub async fn logout(session: &Session) {
    session.clear().await;
}

/// Кто сейчас в сессии (если кто-то есть).
pub async fn current_user_id(session: &Session) -> Option<i64> {
    session.get::<i64>(SESSION_USER_KEY).await.ok().flatten()
}

/// Загрузить пользователя из БД по id из сессии.
pub async fn current_user(pool: &SqlitePool, session: &Session) -> Option<User> {
    let id = current_user_id(session).await?;
    let row: Option<(i64, String)> = sqlx::query_as("SELECT id, username FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
    row.map(|(id, username)| User { id, username })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let phc = hash_password("correct horse battery").expect("hash");
        assert!(phc.starts_with("$argon2"), "PHC string expected, got {phc}");
        assert!(verify_password("correct horse battery", &phc));
        assert!(!verify_password("wrong password", &phc));
        // Same password, different salt → different hash.
        let other = hash_password("correct horse battery").expect("hash");
        assert_ne!(phc, other);
    }

    #[test]
    fn credential_validation_rules() {
        assert!(validate_credentials("ab", "longenough").is_err());
        assert!(validate_credentials("good_user", "short").is_err());
        assert!(validate_credentials("bad name", "longenough").is_err());
        assert!(validate_credentials("good_user", "longenough").is_ok());
    }
}
