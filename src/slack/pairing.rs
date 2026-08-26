use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::discord::pairing::PairingStore;
use crate::error::OmonError;
use crate::Result;

const CODE_TTL_SECONDS: i64 = 600;
const RATE_LIMIT_SECONDS: i64 = 60;
const MAX_FAILED_ATTEMPTS: i64 = 5;

pub enum SlackPairingOutcome {
    Success { user_id: String },
    InvalidCode,
    Expired,
    LockedOut,
}

#[derive(Clone)]
pub struct SlackPairingStore {
    pool: SqlitePool,
    paired_cache: Arc<RwLock<HashSet<String>>>,
}

impl SlackPairingStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            paired_cache: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    fn storage_id(user_id: &str) -> String {
        format!("slack:{user_id}")
    }

    pub async fn init_cache(&self) -> Result<()> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT user_id FROM paired_users WHERE user_id LIKE 'slack:%'",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut cache = self.paired_cache.write().await;
        for (user_id,) in rows {
            if let Some(stripped) = user_id.strip_prefix("slack:") {
                cache.insert(stripped.to_string());
            }
        }
        Ok(())
    }

    pub fn is_user_paired_sync(&self, user_id: &str) -> bool {
        self.paired_cache
            .try_read()
            .map(|cache| cache.contains(user_id))
            .unwrap_or(false)
    }

    pub fn get_paired_user_ids_sync(&self) -> Vec<String> {
        self.paired_cache
            .try_read()
            .map(|cache| cache.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub async fn request_pairing_code(&self, user_id: &str) -> Result<String> {
        let now = Utc::now();
        let storage_id = Self::storage_id(user_id);

        let existing: Option<(String, DateTime<Utc>, DateTime<Utc>, i64)> = sqlx::query_as(
            "SELECT code, created_at, expires_at, attempts FROM pairing_codes WHERE user_id = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&storage_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some((code, created_at, expires_at, attempts)) = existing {
            if attempts < MAX_FAILED_ATTEMPTS
                && expires_at > now
                && (now - created_at).num_seconds() < RATE_LIMIT_SECONDS
            {
                return Ok(PairingStore::format_code(&code));
            }
        }

        let _ = sqlx::query("DELETE FROM pairing_codes WHERE user_id = ?")
            .bind(&storage_id)
            .execute(&self.pool)
            .await;

        let raw_code = PairingStore::generate_raw_code();
        let expires_at = now + chrono::Duration::seconds(CODE_TTL_SECONDS);
        sqlx::query(
            "INSERT INTO pairing_codes (code, user_id, created_at, expires_at, attempts) VALUES (?, ?, ?, ?, 0)",
        )
        .bind(&raw_code)
        .bind(&storage_id)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(|error| OmonError::Database(format!("failed to store pairing code: {error}")))?;

        Ok(PairingStore::format_code(&raw_code))
    }

    pub async fn approve_code(&self, input_code: &str) -> Result<SlackPairingOutcome> {
        let normalized = PairingStore::normalize_code(input_code);
        if normalized.is_empty() {
            return Ok(SlackPairingOutcome::InvalidCode);
        }

        let now = Utc::now();
        let record: Option<(String, String, DateTime<Utc>, i64)> = sqlx::query_as(
            "SELECT code, user_id, expires_at, attempts FROM pairing_codes WHERE code = ?",
        )
        .bind(&normalized)
        .fetch_optional(&self.pool)
        .await?;

        let Some((code, user_id, expires_at, attempts)) = record else {
            let _ =
                sqlx::query("UPDATE pairing_codes SET attempts = attempts + 1 WHERE attempts < ?")
                    .bind(MAX_FAILED_ATTEMPTS)
                    .execute(&self.pool)
                    .await;
            return Ok(SlackPairingOutcome::InvalidCode);
        };

        if attempts >= MAX_FAILED_ATTEMPTS {
            return Ok(SlackPairingOutcome::LockedOut);
        }
        if now > expires_at {
            let _ = sqlx::query("DELETE FROM pairing_codes WHERE code = ?")
                .bind(&code)
                .execute(&self.pool)
                .await;
            return Ok(SlackPairingOutcome::Expired);
        }
        let Some(slack_user_id) = user_id.strip_prefix("slack:").map(str::to_string) else {
            return Ok(SlackPairingOutcome::InvalidCode);
        };

        sqlx::query(
            "INSERT INTO paired_users (user_id, paired_at) VALUES (?, ?) ON CONFLICT(user_id) DO NOTHING",
        )
        .bind(&user_id)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|error| OmonError::Database(format!("failed to record paired user: {error}")))?;

        let _ = sqlx::query("DELETE FROM pairing_codes WHERE code = ?")
            .bind(&code)
            .execute(&self.pool)
            .await;

        self.paired_cache.write().await.insert(slack_user_id.clone());
        Ok(SlackPairingOutcome::Success {
            user_id: slack_user_id,
        })
    }
}
