use anyhow::{Context, Result};
use rusqlite::{
    backup::{Backup, Progress},
    params, Connection, DatabaseName, OptionalExtension, TransactionBehavior,
};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use teloxide::types::UserId;
use url::Url;
use zeroize::Zeroizing;

pub struct AppState {
    pub owner_id: UserId,
    database: Mutex<Connection>,
    disk_space_path: PathBuf,
    support_username: Mutex<String>,
    setup: Mutex<HashMap<UserId, SetupState>>,
    session: Mutex<Option<PanelSession>>,
    bot_username: Mutex<Option<String>>,
}

#[derive(Clone)]
pub enum SetupState {
    AdminId,
    BroadcastAll,
    BroadcastSubscribers,
    DatabaseRestore,
    NewTariffVolume,
    NewTariffPrice {
        volume_gb: u16,
    },
    ReferralPercent,
    SupportId,
    RequiredChannelUsername,
    UserSearch,
    PaymentCardNumber,
    PaymentCardholder {
        card_number: String,
    },
    PanelUrl,
    Username {
        base_url: Url,
    },
    Password {
        base_url: Url,
        username: String,
    },
    TariffVolume {
        tariff_id: i64,
    },
    TariffPrice {
        tariff_id: i64,
    },
    WalletUserId,
    WalletBalance {
        user_id: u64,
    },
    WalletReason {
        user_id: u64,
        target_balance: i64,
    },
    WalletBulkAmount {
        action: WalletBulkAction,
    },
    WalletBulkReason {
        action: WalletBulkAction,
        amount_toman: i64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalletBulkAction {
    Increase,
    Decrease,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalletBulkAdjustmentSummary {
    pub targeted_users: u64,
    pub changed_users: u64,
    pub unchanged_users: u64,
    pub total_adjusted_toman: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionTariff {
    pub id: i64,
    pub volume_gb: u16,
    pub price_toman: u32,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotUserSearchResult {
    pub telegram_user_id: u64,
    pub username: Option<String>,
    pub first_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotUserDetails {
    pub telegram_user_id: u64,
    pub username: Option<String>,
    pub first_name: Option<String>,
    pub referrer_user_id: Option<u64>,
    pub referrer_username: Option<String>,
    pub referrer_first_name: Option<String>,
    pub referral_count: u64,
    pub commissions_paid_toman: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReferralCommissionCredit {
    pub referrer_user_id: u64,
    pub amount_toman: i64,
    pub balance_after_toman: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaymentCompletion {
    pub referral_credit: Option<ReferralCommissionCredit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinancialStatistics {
    pub total_sales_toman: i64,
    pub total_commissions_toman: i64,
    pub referred_users: u64,
    pub referral_orders: u64,
    pub average_referral_revenue_toman: i64,
    pub total_wallet_balance_toman: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskSpaceUsage {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequiredChannel {
    pub username: String,
}

pub const MAX_REQUIRED_CHANNELS: usize = 20;

pub fn normalize_required_channel_username(value: &str) -> Option<String> {
    let value = value.trim();
    let username = value.strip_prefix('@').unwrap_or(value);
    let mut characters = username.bytes();
    let first = characters.next()?;
    if !(first.is_ascii_alphabetic()
        && characters.all(|character| character.is_ascii_alphanumeric() || character == b'_')
        && (5..=32).contains(&username.len()))
    {
        return None;
    }
    Some(username.to_ascii_lowercase())
}

pub struct PaymentOrder {
    pub id: i64,
    pub telegram_user_id: u64,
    pub account_name: String,
    pub telegram_username: Option<String>,
    pub panel_username: String,
    pub plan_gb: u16,
    pub price_toman: u32,
    pub wallet_used_toman: u32,
    pub amount_due_toman: u32,
    pub is_wallet_payment: bool,
    pub is_renewal: bool,
    pub receipt_file_id: Option<String>,
}

pub struct CreatedPaymentOrder {
    pub id: i64,
    pub plan_gb: u16,
    pub price_toman: u32,
    pub wallet_used_toman: u32,
    pub amount_due_toman: u32,
    pub wallet_balance_after: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum CreateTariffPaymentOrderError {
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error("user already has an order awaiting admin review")]
    OrderAwaitingAdminReview,
}

pub struct PaymentReviewMessage {
    pub reviewer_user_id: u64,
    pub message_id: i32,
    pub caption: String,
    pub is_text: bool,
}

#[cfg(test)]
pub struct NewPaymentOrder<'a> {
    pub telegram_user_id: u64,
    pub account_name: &'a str,
    pub telegram_username: Option<&'a str>,
    pub panel_username: &'a str,
    pub plan_gb: u16,
    pub price_toman: u32,
    pub is_renewal: bool,
}

pub struct NewTariffPaymentOrder<'a> {
    pub telegram_user_id: u64,
    pub account_name: &'a str,
    pub telegram_username: Option<&'a str>,
    pub panel_username: &'a str,
    pub tariff_id: i64,
    pub use_wallet: bool,
    pub is_renewal: bool,
}

struct PanelSession {
    access_token: Zeroizing<String>,
}

impl AppState {
    pub fn new(owner_id: UserId, database_path: &str) -> Result<Self> {
        let path = Path::new(database_path);
        let disk_space_path = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        let database = Connection::open(path)?;
        database.execute_batch(
            "CREATE TABLE IF NOT EXISTS panel_settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                base_url TEXT NOT NULL,
                username TEXT NOT NULL,
                connected_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );
            CREATE TABLE IF NOT EXISTS panel_users (
                telegram_user_id TEXT NOT NULL,
                panel_username TEXT NOT NULL UNIQUE,
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')),
                PRIMARY KEY (telegram_user_id, panel_username)
            );
            CREATE TABLE IF NOT EXISTS admins (
                telegram_user_id TEXT PRIMARY KEY NOT NULL,
                added_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );
            CREATE TABLE IF NOT EXISTS bot_users (
                telegram_user_id TEXT PRIMARY KEY NOT NULL,
                referred_by_user_id TEXT,
                username TEXT,
                first_name TEXT,
                first_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')),
                last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );
            CREATE TABLE IF NOT EXISTS wallets (
                telegram_user_id TEXT PRIMARY KEY NOT NULL,
                balance_toman INTEGER NOT NULL DEFAULT 0,
                commission_debt_toman INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );
            CREATE TABLE IF NOT EXISTS wallet_transactions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                telegram_user_id TEXT NOT NULL,
                amount_toman INTEGER NOT NULL,
                balance_after_toman INTEGER NOT NULL,
                transaction_type TEXT NOT NULL,
                order_id INTEGER,
                description TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );
            CREATE TABLE IF NOT EXISTS referral_commissions (
                order_id INTEGER PRIMARY KEY NOT NULL,
                referrer_user_id TEXT NOT NULL,
                invited_user_id TEXT NOT NULL,
                amount_toman INTEGER NOT NULL CHECK (amount_toman > 0),
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );
            CREATE TABLE IF NOT EXISTS payment_orders (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                telegram_user_id TEXT NOT NULL,
                account_name TEXT NOT NULL,
                telegram_username TEXT,
                panel_username TEXT NOT NULL,
                plan_gb INTEGER NOT NULL,
                price_toman INTEGER NOT NULL,
                wallet_used_toman INTEGER NOT NULL DEFAULT 0,
                amount_due_toman INTEGER NOT NULL DEFAULT 0,
                payment_method TEXT NOT NULL DEFAULT 'card',
                is_renewal INTEGER NOT NULL,
                status TEXT NOT NULL,
                receipt_file_id TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );
            CREATE TABLE IF NOT EXISTS payment_review_messages (
                order_id INTEGER NOT NULL,
                reviewer_user_id TEXT NOT NULL,
                message_id INTEGER NOT NULL,
                caption TEXT NOT NULL,
                is_text INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (order_id, reviewer_user_id, message_id)
            );
            CREATE TABLE IF NOT EXISTS wallet_reservations (
                order_id INTEGER PRIMARY KEY NOT NULL,
                telegram_user_id TEXT NOT NULL,
                amount_toman INTEGER NOT NULL CHECK (amount_toman > 0)
            );
            CREATE TABLE IF NOT EXISTS payment_settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                card_number TEXT NOT NULL,
                cardholder_name TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );
            CREATE TABLE IF NOT EXISTS commerce_settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                purchases_enabled INTEGER NOT NULL DEFAULT 1,
                renewals_enabled INTEGER NOT NULL DEFAULT 1,
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );
            CREATE TABLE IF NOT EXISTS referral_settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
                percent INTEGER NOT NULL DEFAULT 15 CHECK (percent BETWEEN 0 AND 100),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );
            INSERT OR IGNORE INTO referral_settings (id) VALUES (1);
            CREATE TABLE IF NOT EXISTS subscription_tariffs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                volume_gb INTEGER NOT NULL UNIQUE CHECK (volume_gb > 0),
                price_toman INTEGER NOT NULL CHECK (price_toman > 0),
                enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );",
        )?;
        migrate_support_settings(&database)?;
        migrate_required_channels(&database)?;
        let support_username = read_support_username(&database)?;
        migrate_commerce_settings(&database)?;
        migrate_bot_user_referrals(&database)?;
        migrate_bot_user_profile(&database)?;
        migrate_wallet_transaction_descriptions(&database)?;
        migrate_financial_transaction_constraints(&database)?;
        migrate_payment_wallet_columns(&database)?;
        seed_subscription_tariffs(&database)?;
        database.execute_batch(
            "UPDATE panel_settings
             SET connected_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', connected_at, '+03:30')
             WHERE connected_at NOT LIKE '%+%';
             UPDATE panel_users
             SET updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', updated_at, '+03:30')
             WHERE updated_at NOT LIKE '%+%';
             UPDATE admins
             SET added_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', added_at, '+03:30')
             WHERE added_at NOT LIKE '%+%';
             UPDATE bot_users
             SET first_seen_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', first_seen_at, '+03:30')
             WHERE first_seen_at NOT LIKE '%+%';
             UPDATE bot_users
             SET last_seen_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', last_seen_at, '+03:30')
             WHERE last_seen_at NOT LIKE '%+%';
             UPDATE wallets
             SET updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', updated_at, '+03:30')
             WHERE updated_at NOT LIKE '%+%';
             UPDATE wallet_transactions
             SET created_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', created_at, '+03:30')
             WHERE created_at NOT LIKE '%+%';
             UPDATE referral_commissions
             SET created_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', created_at, '+03:30')
             WHERE created_at NOT LIKE '%+%';
             UPDATE payment_orders
             SET created_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', created_at, '+03:30')
             WHERE created_at NOT LIKE '%+%';
             UPDATE payment_settings
             SET updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', updated_at, '+03:30')
             WHERE updated_at NOT LIKE '%+%';
             UPDATE commerce_settings
             SET updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', updated_at, '+03:30')
             WHERE updated_at NOT LIKE '%+%';",
        )?;
        migrate_panel_user_mapping(&database)?;

        Ok(Self {
            owner_id,
            database: Mutex::new(database),
            disk_space_path,
            support_username: Mutex::new(support_username),
            setup: Mutex::new(HashMap::new()),
            session: Mutex::new(None),
            bot_username: Mutex::new(None),
        })
    }

    pub fn disk_space_usage(&self) -> std::io::Result<DiskSpaceUsage> {
        let total_bytes = fs2::total_space(&self.disk_space_path)?;
        let free_bytes = fs2::free_space(&self.disk_space_path)?;
        if total_bytes == 0 {
            return Err(std::io::Error::other(
                "filesystem reported zero total space",
            ));
        }
        Ok(DiskSpaceUsage {
            total_bytes,
            free_bytes,
        })
    }

    pub fn create_database_backup(&self, backup_path: &Path) -> rusqlite::Result<()> {
        let database = self.database.lock().expect("database lock poisoned");
        database.backup(DatabaseName::Main, backup_path, None)
    }

    pub fn restore_database_backup(
        &self,
        backup_path: &Path,
        rollback_path: &Path,
    ) -> rusqlite::Result<()> {
        let source = Connection::open(backup_path)?;
        let integrity: String = source.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let tables = source
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        const REQUIRED_TABLES: &[&str] = &[
            "admins",
            "bot_users",
            "commerce_settings",
            "panel_settings",
            "panel_users",
            "payment_orders",
            "payment_review_messages",
            "payment_settings",
            "referral_commissions",
            "referral_settings",
            "subscription_tariffs",
            "wallet_reservations",
            "wallet_transactions",
            "wallets",
        ];
        if REQUIRED_TABLES
            .iter()
            .any(|required| !tables.iter().any(|table| table == required))
        {
            return Err(rusqlite::Error::InvalidQuery);
        }

        let mut database = self.database.lock().expect("database lock poisoned");
        database.backup(DatabaseName::Main, rollback_path, None)?;
        let restore_result = {
            let backup = Backup::new(&source, &mut database)?;
            backup.run_to_completion(100, Duration::from_millis(10), None::<fn(Progress)>)
        };
        let restore_result = restore_result.and_then(|()| {
            migrate_support_settings(&database)?;
            migrate_required_channels(&database)?;
            migrate_commerce_settings(&database)?;
            migrate_bot_user_referrals(&database)?;
            migrate_bot_user_profile(&database)?;
            migrate_payment_wallet_columns(&database)?;
            migrate_wallet_transaction_descriptions(&database)?;
            migrate_panel_user_mapping(&database)?;
            seed_subscription_tariffs(&database)?;
            read_support_username(&database)
        });
        let support_username = match restore_result {
            Ok(support_username) => support_username,
            Err(error) => {
                if let Err(rollback_error) =
                    database.restore(DatabaseName::Main, rollback_path, None::<fn(Progress)>)
                {
                    log::error!(
                        "Database restore failed ({error}) and rollback failed ({rollback_error})"
                    );
                    return Err(rollback_error);
                }
                return Err(error);
            }
        };
        *self
            .support_username
            .lock()
            .expect("support username lock poisoned") = support_username;
        Ok(())
    }

    pub fn set_bot_username(&self, username: String) {
        *self
            .bot_username
            .lock()
            .expect("bot username lock poisoned") = Some(username);
    }

    pub fn bot_username(&self) -> Option<String> {
        self.bot_username
            .lock()
            .expect("bot username lock poisoned")
            .clone()
    }

    pub fn set_setup(&self, user_id: UserId, setup: SetupState) {
        self.setup
            .lock()
            .expect("setup lock poisoned")
            .insert(user_id, setup);
    }

    pub fn take_setup(&self, user_id: UserId) -> Option<SetupState> {
        self.setup
            .lock()
            .expect("setup lock poisoned")
            .remove(&user_id)
    }

    pub fn has_setup(&self, user_id: UserId) -> bool {
        self.setup
            .lock()
            .expect("setup lock poisoned")
            .contains_key(&user_id)
    }

    pub fn cancel_setup(&self, user_id: UserId) {
        self.setup
            .lock()
            .expect("setup lock poisoned")
            .remove(&user_id);
    }

    pub fn save_panel(&self, base_url: &Url, username: &str) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "INSERT INTO panel_settings (id, base_url, username)
                 VALUES (1, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET
                    base_url = excluded.base_url,
                    username = excluded.username,
                    connected_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
                params![base_url.as_str(), username],
            )?;
        Ok(())
    }

    pub fn panel_config(&self) -> rusqlite::Result<Option<(String, String)>> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT base_url, username FROM panel_settings WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    }

    pub fn reserved_panel_usernames(&self, telegram_user_id: u64) -> rusqlite::Result<Vec<String>> {
        let database = self.database.lock().expect("database lock poisoned");
        let mut statement = database.prepare(
            "SELECT panel_username FROM panel_users WHERE telegram_user_id = ?1
             UNION
             SELECT panel_username FROM payment_orders
             WHERE telegram_user_id = ?1
             ORDER BY panel_username",
        )?;
        let usernames = statement
            .query_map([telegram_user_id.to_string()], |row| row.get(0))?
            .collect();
        usernames
    }

    #[cfg(test)]
    pub fn record_bot_user(&self, telegram_user_id: u64) -> rusqlite::Result<()> {
        self.record_bot_user_with_referrer(telegram_user_id, None)
    }

    pub fn record_bot_user_with_referrer(
        &self,
        telegram_user_id: u64,
        referred_by_user_id: Option<u64>,
    ) -> rusqlite::Result<()> {
        let database = self.database.lock().expect("database lock poisoned");
        let transaction = database.unchecked_transaction()?;
        let referrals_enabled = transaction.query_row(
            "SELECT enabled FROM referral_settings WHERE id = 1",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if let Some(referrer_id) = referred_by_user_id
            .filter(|_| referrals_enabled)
            .filter(|id| *id != telegram_user_id && *id > 0)
        {
            transaction.execute(
                "INSERT INTO bot_users (telegram_user_id, referred_by_user_id)
                 SELECT ?1, telegram_user_id FROM bot_users WHERE telegram_user_id = ?2
                 ON CONFLICT(telegram_user_id) DO UPDATE SET
                    last_seen_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
                params![telegram_user_id.to_string(), referrer_id.to_string()],
            )?;
        }
        transaction.execute(
            "INSERT INTO bot_users (telegram_user_id) VALUES (?1)
             ON CONFLICT(telegram_user_id) DO UPDATE SET
                last_seen_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
            [telegram_user_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn update_bot_user_profile(
        &self,
        telegram_user_id: u64,
        username: Option<&str>,
        first_name: &str,
    ) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "UPDATE bot_users SET username = ?1, first_name = ?2,
                    last_seen_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')
                 WHERE telegram_user_id = ?3",
                params![username, first_name, telegram_user_id.to_string()],
            )?;
        Ok(())
    }

    pub fn bot_user_statistics(&self) -> rusqlite::Result<(u64, u64, i64)> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM bot_users),
                    (SELECT COUNT(*) FROM bot_users WHERE referred_by_user_id IS NOT NULL),
                    COALESCE((SELECT SUM(amount_toman) FROM referral_commissions), 0)",
                [],
                |row| {
                    let total_users: i64 = row.get(0)?;
                    let referred_users: i64 = row.get(1)?;
                    Ok((
                        total_users.max(0) as u64,
                        referred_users.max(0) as u64,
                        row.get(2)?,
                    ))
                },
            )
    }

    pub fn search_bot_users(&self, query: &str) -> rusqlite::Result<Vec<BotUserSearchResult>> {
        let query = query.trim().trim_start_matches('@');
        let database = self.database.lock().expect("database lock poisoned");
        let mut statement = database.prepare(
            "SELECT
                bu.telegram_user_id,
                COALESCE(bu.username, MAX(po.telegram_username)),
                bu.first_name
             FROM bot_users bu
             LEFT JOIN payment_orders po
                ON po.telegram_user_id = bu.telegram_user_id
             WHERE bu.telegram_user_id = ?1
                OR lower(bu.username) = lower(?2)
                OR lower(ltrim(po.telegram_username, '@')) = lower(?2)
             GROUP BY bu.telegram_user_id
             ORDER BY bu.telegram_user_id
             LIMIT 10",
        )?;
        let users = statement
            .query_map(params![query, query], |row| {
                let telegram_user_id: String = row.get(0)?;
                let telegram_user_id = telegram_user_id.parse::<u64>().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(BotUserSearchResult {
                    telegram_user_id,
                    username: row.get(1)?,
                    first_name: row.get(2)?,
                })
            })?
            .collect();
        users
    }

    pub fn bot_user_details(
        &self,
        telegram_user_id: u64,
    ) -> rusqlite::Result<Option<BotUserDetails>> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT
                    bu.username,
                    bu.first_name,
                    bu.referred_by_user_id,
                    referrer.username,
                    referrer.first_name,
                    (SELECT COUNT(*) FROM bot_users
                     WHERE referred_by_user_id = bu.telegram_user_id),
                    COALESCE((SELECT SUM(amount_toman) FROM referral_commissions
                              WHERE referrer_user_id = bu.telegram_user_id), 0)
                 FROM bot_users bu
                 LEFT JOIN bot_users referrer
                    ON referrer.telegram_user_id = bu.referred_by_user_id
                 WHERE bu.telegram_user_id = ?1",
                [telegram_user_id.to_string()],
                |row| {
                    let referrer_id: Option<String> = row.get(2)?;
                    let referrer_id = referrer_id
                        .map(|id| {
                            id.parse::<u64>().map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    2,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })
                        })
                        .transpose()?;
                    let referral_count: i64 = row.get(5)?;
                    Ok(BotUserDetails {
                        telegram_user_id,
                        username: row.get(0)?,
                        first_name: row.get(1)?,
                        referrer_user_id: referrer_id,
                        referrer_username: row.get(3)?,
                        referrer_first_name: row.get(4)?,
                        referral_count: referral_count.max(0) as u64,
                        commissions_paid_toman: row.get(6)?,
                    })
                },
            )
            .optional()
    }

    pub fn referral_summary(&self, telegram_user_id: u64) -> rusqlite::Result<(u64, i64)> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM bot_users WHERE referred_by_user_id = ?1),
                    COALESCE((SELECT SUM(amount_toman) FROM referral_commissions
                              WHERE referrer_user_id = ?1), 0)",
                [telegram_user_id.to_string()],
                |row| {
                    let count = row.get::<_, i64>(0)?;
                    let total = row.get::<_, i64>(1)?;
                    Ok((count.max(0) as u64, total))
                },
            )
    }

    pub fn wallet_summary(&self, telegram_user_id: u64) -> rusqlite::Result<(i64, i64)> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT balance_toman, commission_debt_toman FROM wallets
                 WHERE telegram_user_id = ?1",
                [telegram_user_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map(|summary| summary.unwrap_or((0, 0)))
    }

    pub fn set_wallet_balance(
        &self,
        telegram_user_id: u64,
        target_balance_toman: i64,
    ) -> rusqlite::Result<i64> {
        self.set_wallet_balance_with_reason(
            telegram_user_id,
            target_balance_toman,
            "تغییر دستی موجودی",
        )
    }

    pub fn set_wallet_balance_with_reason(
        &self,
        telegram_user_id: u64,
        target_balance_toman: i64,
        reason: &str,
    ) -> rusqlite::Result<i64> {
        let reason = reason.trim();
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(rusqlite::Error::InvalidParameterName(
                "wallet adjustment reason must contain 1 to 500 characters".to_owned(),
            ));
        }
        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction()?;
        let telegram_user_id = telegram_user_id.to_string();
        let previous_balance = transaction
            .query_row(
                "SELECT balance_toman FROM wallets WHERE telegram_user_id = ?1",
                [&telegram_user_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0);
        let reserved_balance = transaction.query_row(
            "SELECT COALESCE(SUM(amount_toman), 0) FROM wallet_reservations
             WHERE telegram_user_id = ?1",
            [&telegram_user_id],
            |row| row.get::<_, i64>(0),
        )?;
        if reserved_balance > 0 && target_balance_toman < reserved_balance {
            return Err(rusqlite::Error::InvalidParameterName(
                "wallet balance cannot be set below funds reserved by pending orders".to_owned(),
            ));
        }
        let adjustment = target_balance_toman.checked_sub(previous_balance).ok_or(
            rusqlite::Error::IntegralValueOutOfRange(0, target_balance_toman),
        )?;
        let debt =
            if target_balance_toman < 0 {
                target_balance_toman.checked_neg().ok_or(
                    rusqlite::Error::IntegralValueOutOfRange(0, target_balance_toman),
                )?
            } else {
                0
            };
        transaction.execute(
            "INSERT INTO wallets (telegram_user_id, balance_toman, commission_debt_toman)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(telegram_user_id) DO UPDATE SET
                balance_toman = excluded.balance_toman,
                commission_debt_toman = excluded.commission_debt_toman,
                updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
            params![telegram_user_id, target_balance_toman, debt],
        )?;
        if adjustment != 0 {
            transaction.execute(
                "INSERT INTO wallet_transactions
                    (telegram_user_id, amount_toman, balance_after_toman,
                     transaction_type, description)
                 VALUES (?1, ?2, ?3, 'owner_adjustment', ?4)",
                params![telegram_user_id, adjustment, target_balance_toman, reason],
            )?;
        }
        transaction.commit()?;
        Ok(previous_balance)
    }

    pub fn wallet_statistics(&self) -> rusqlite::Result<(i64, i64)> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT
                    COALESCE((SELECT SUM(balance_toman) FROM wallets), 0),
                    COALESCE((SELECT SUM(amount_toman) FROM referral_commissions), 0)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
    }

    pub fn financial_statistics(&self) -> rusqlite::Result<FinancialStatistics> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT
                    COALESCE((SELECT SUM(price_toman) FROM payment_orders
                              WHERE status = 'approved'), 0),
                    COALESCE((SELECT SUM(amount_toman) FROM referral_commissions), 0),
                    (SELECT COUNT(*) FROM bot_users
                     WHERE referred_by_user_id IS NOT NULL),
                    (SELECT COUNT(*) FROM payment_orders po
                     JOIN bot_users bu ON bu.telegram_user_id = po.telegram_user_id
                     WHERE bu.referred_by_user_id IS NOT NULL
                       AND po.status = 'approved'),
                    COALESCE((
                        SELECT SUM(po.price_toman) / NULLIF(COUNT(DISTINCT po.telegram_user_id), 0)
                        FROM payment_orders po
                        JOIN bot_users bu ON bu.telegram_user_id = po.telegram_user_id
                        WHERE bu.referred_by_user_id IS NOT NULL
                          AND po.status = 'approved'
                    ), 0),
                    COALESCE((SELECT SUM(balance_toman) FROM wallets), 0)",
                [],
                |row| {
                    let referred_users: i64 = row.get(2)?;
                    let referral_orders: i64 = row.get(3)?;
                    Ok(FinancialStatistics {
                        total_sales_toman: row.get(0)?,
                        total_commissions_toman: row.get(1)?,
                        referred_users: referred_users.max(0) as u64,
                        referral_orders: referral_orders.max(0) as u64,
                        average_referral_revenue_toman: row.get(4)?,
                        total_wallet_balance_toman: row.get(5)?,
                    })
                },
            )
    }

    pub fn adjust_all_wallets(
        &self,
        action: WalletBulkAction,
        amount_toman: i64,
        reason: &str,
    ) -> rusqlite::Result<WalletBulkAdjustmentSummary> {
        let reason = reason.trim();
        if amount_toman <= 0 || reason.is_empty() || reason.chars().count() > 500 {
            return Err(rusqlite::Error::InvalidParameterName(
                "bulk wallet adjustment requires a positive amount and a 1 to 500 character reason"
                    .to_owned(),
            ));
        }

        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction()?;
        let user_ids = {
            let mut statement = transaction.prepare(
                "SELECT telegram_user_id FROM bot_users
                 UNION
                 SELECT telegram_user_id FROM wallets
                 ORDER BY telegram_user_id",
            )?;
            let user_ids = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            user_ids
        };
        let mut summary = WalletBulkAdjustmentSummary {
            targeted_users: user_ids.len() as u64,
            changed_users: 0,
            unchanged_users: 0,
            total_adjusted_toman: 0,
        };

        for telegram_user_id in user_ids {
            let (balance, reserved_balance) = transaction.query_row(
                "SELECT
                    COALESCE((SELECT balance_toman FROM wallets WHERE telegram_user_id = ?1), 0),
                    COALESCE((SELECT SUM(amount_toman) FROM wallet_reservations
                              WHERE telegram_user_id = ?1), 0)",
                [&telegram_user_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )?;
            let (new_balance, adjusted_amount) = match action {
                WalletBulkAction::Increase => (
                    balance
                        .checked_add(amount_toman)
                        .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, balance))?,
                    amount_toman,
                ),
                WalletBulkAction::Decrease => {
                    let spendable_balance = balance.saturating_sub(reserved_balance).max(0);
                    let deduction = amount_toman.min(spendable_balance);
                    (
                        balance
                            .checked_sub(deduction)
                            .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, balance))?,
                        deduction,
                    )
                }
            };
            if adjusted_amount == 0 {
                summary.unchanged_users += 1;
                continue;
            }

            let debt = if new_balance < 0 {
                new_balance
                    .checked_neg()
                    .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, new_balance))?
            } else {
                0
            };
            transaction.execute(
                "INSERT INTO wallets (telegram_user_id, balance_toman, commission_debt_toman)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(telegram_user_id) DO UPDATE SET
                    balance_toman = excluded.balance_toman,
                    commission_debt_toman = excluded.commission_debt_toman,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
                params![telegram_user_id, new_balance, debt],
            )?;
            let signed_adjustment = match action {
                WalletBulkAction::Increase => adjusted_amount,
                WalletBulkAction::Decrease => -adjusted_amount,
            };
            transaction.execute(
                "INSERT INTO wallet_transactions
                    (telegram_user_id, amount_toman, balance_after_toman,
                     transaction_type, description)
                 VALUES (?1, ?2, ?3, 'owner_adjustment', ?4)",
                params![telegram_user_id, signed_adjustment, new_balance, reason],
            )?;
            summary.changed_users += 1;
            summary.total_adjusted_toman = summary
                .total_adjusted_toman
                .checked_add(adjusted_amount)
                .ok_or(rusqlite::Error::IntegralValueOutOfRange(
                    0,
                    summary.total_adjusted_toman,
                ))?;
        }

        transaction.commit()?;
        Ok(summary)
    }
    pub fn broadcast_targets(&self, subscribers_only: bool) -> Result<Vec<u64>> {
        let database = self.database.lock().expect("database lock poisoned");
        let query = if subscribers_only {
            "SELECT DISTINCT telegram_user_id FROM panel_users ORDER BY telegram_user_id"
        } else {
            "SELECT telegram_user_id FROM bot_users ORDER BY telegram_user_id"
        };
        let mut statement = database.prepare(query)?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|result| {
                result?
                    .parse::<u64>()
                    .context("Stored broadcast recipient Telegram ID is invalid")
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ids)
    }

    pub fn save_payment_details(
        &self,
        card_number: &str,
        cardholder_name: &str,
    ) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "INSERT INTO payment_settings (id, card_number, cardholder_name)
                 VALUES (1, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET
                    card_number = excluded.card_number,
                    cardholder_name = excluded.cardholder_name,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
                params![card_number, cardholder_name],
            )?;
        Ok(())
    }

    pub fn payment_details(&self) -> rusqlite::Result<Option<(String, String)>> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT card_number, cardholder_name FROM payment_settings WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    }

    pub fn support_username(&self) -> String {
        self.support_username
            .lock()
            .expect("support username lock poisoned")
            .clone()
    }

    pub fn set_support_username(&self, support_username: &str) -> rusqlite::Result<()> {
        if !is_valid_support_username(support_username) {
            return Err(rusqlite::Error::InvalidParameterName(
                "support username must contain 5 to 32 ASCII letters, digits, or underscores"
                    .to_owned(),
            ));
        }
        let support_username = support_username.to_ascii_lowercase();
        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction()?;
        transaction.execute(
            "INSERT INTO support_settings (id, support_username)
             VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET
                support_username = excluded.support_username,
                updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
            [&support_username],
        )?;
        transaction.commit()?;
        *self
            .support_username
            .lock()
            .expect("support username lock poisoned") = support_username;
        Ok(())
    }

    pub fn required_channels_enabled(&self) -> rusqlite::Result<bool> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT enabled FROM required_channel_settings WHERE id = 1",
                [],
                |row| row.get::<_, bool>(0),
            )
    }

    pub fn set_required_channels_enabled(&self, enabled: bool) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "UPDATE required_channel_settings
                 SET enabled = ?1,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')
                 WHERE id = 1",
                [enabled],
            )?;
        Ok(())
    }

    pub fn required_channels(&self) -> rusqlite::Result<Vec<RequiredChannel>> {
        let database = self.database.lock().expect("database lock poisoned");
        let mut statement =
            database.prepare("SELECT username FROM required_channels ORDER BY username")?;
        let channels = statement
            .query_map([], |row| {
                Ok(RequiredChannel {
                    username: row.get(0)?,
                })
            })?
            .collect();
        channels
    }

    pub fn add_required_channel(&self, username: &str) -> rusqlite::Result<bool> {
        let username = normalize_required_channel_username(username).ok_or_else(|| {
            rusqlite::Error::InvalidParameterName(
                "channel username must contain 5 to 32 ASCII letters, digits, or underscores"
                    .to_owned(),
            )
        })?;
        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM required_channels WHERE username = ?1)",
            [&username],
            |row| row.get(0),
        )?;
        if existing {
            return Ok(false);
        }
        let channel_count: usize =
            transaction.query_row("SELECT COUNT(*) FROM required_channels", [], |row| {
                row.get(0)
            })?;
        if channel_count >= MAX_REQUIRED_CHANNELS {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "at most {MAX_REQUIRED_CHANNELS} required channels may be configured"
            )));
        }
        transaction.execute(
            "INSERT INTO required_channels (username) VALUES (?1)",
            [&username],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn remove_required_channel(&self, username: &str) -> rusqlite::Result<bool> {
        let username = normalize_required_channel_username(username).ok_or_else(|| {
            rusqlite::Error::InvalidParameterName("invalid channel username".to_owned())
        })?;
        let changed = self
            .database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "DELETE FROM required_channels WHERE username = ?1",
                [&username],
            )?;
        Ok(changed != 0)
    }

    pub fn sales_status(&self) -> rusqlite::Result<(bool, bool)> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT purchases_enabled, renewals_enabled FROM commerce_settings WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map(|status| status.unwrap_or((true, true)))
    }

    pub fn set_purchases_enabled(&self, enabled: bool) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "INSERT INTO commerce_settings (id, purchases_enabled)
                 VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET
                    purchases_enabled = excluded.purchases_enabled,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
                [enabled],
            )?;
        Ok(())
    }

    pub fn set_renewals_enabled(&self, enabled: bool) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "INSERT INTO commerce_settings (id, renewals_enabled)
                 VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET
                    renewals_enabled = excluded.renewals_enabled,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
                [enabled],
            )?;
        Ok(())
    }

    pub fn referral_status(&self) -> rusqlite::Result<(bool, u8)> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT enabled, percent FROM referral_settings WHERE id = 1",
                [],
                |row| {
                    let percent: i64 = row.get(1)?;
                    Ok((row.get(0)?, percent as u8))
                },
            )
    }

    pub fn set_referrals_enabled(&self, enabled: bool) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "UPDATE referral_settings SET enabled = ?1,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')
                 WHERE id = 1",
                [enabled],
            )?;
        Ok(())
    }

    pub fn set_referral_percent(&self, percent: u8) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "UPDATE referral_settings SET percent = ?1,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')
                 WHERE id = 1",
                [percent],
            )?;
        Ok(())
    }

    pub fn subscription_tariffs(&self) -> rusqlite::Result<Vec<SubscriptionTariff>> {
        let database = self.database.lock().expect("database lock poisoned");
        let mut statement = database.prepare(
            "SELECT id, volume_gb, price_toman, enabled
             FROM subscription_tariffs ORDER BY volume_gb",
        )?;
        let tariffs = statement
            .query_map([], subscription_tariff_from_row)?
            .collect();
        tariffs
    }

    pub fn active_subscription_tariffs(&self) -> rusqlite::Result<Vec<SubscriptionTariff>> {
        let database = self.database.lock().expect("database lock poisoned");
        let mut statement = database.prepare(
            "SELECT id, volume_gb, price_toman, enabled
             FROM subscription_tariffs WHERE enabled = 1 ORDER BY volume_gb",
        )?;
        let tariffs = statement
            .query_map([], subscription_tariff_from_row)?
            .collect();
        tariffs
    }

    pub fn subscription_tariff(
        &self,
        tariff_id: i64,
    ) -> rusqlite::Result<Option<SubscriptionTariff>> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT id, volume_gb, price_toman, enabled
                 FROM subscription_tariffs WHERE id = ?1",
                [tariff_id],
                subscription_tariff_from_row,
            )
            .optional()
    }

    pub fn add_subscription_tariff(
        &self,
        volume_gb: u16,
        price_toman: u32,
    ) -> rusqlite::Result<i64> {
        let database = self.database.lock().expect("database lock poisoned");
        database.execute(
            "INSERT INTO subscription_tariffs (volume_gb, price_toman)
             VALUES (?1, ?2)",
            params![i64::from(volume_gb), i64::from(price_toman)],
        )?;
        Ok(database.last_insert_rowid())
    }

    pub fn update_subscription_tariff_volume(
        &self,
        tariff_id: i64,
        volume_gb: u16,
    ) -> rusqlite::Result<bool> {
        let changed = self
            .database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "UPDATE subscription_tariffs SET volume_gb = ?1,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')
                 WHERE id = ?2",
                params![i64::from(volume_gb), tariff_id],
            )?;
        Ok(changed == 1)
    }

    pub fn update_subscription_tariff_price(
        &self,
        tariff_id: i64,
        price_toman: u32,
    ) -> rusqlite::Result<bool> {
        let changed = self
            .database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "UPDATE subscription_tariffs SET price_toman = ?1,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')
                 WHERE id = ?2",
                params![i64::from(price_toman), tariff_id],
            )?;
        Ok(changed == 1)
    }

    pub fn set_subscription_tariff_enabled(
        &self,
        tariff_id: i64,
        enabled: bool,
    ) -> rusqlite::Result<bool> {
        let changed = self
            .database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "UPDATE subscription_tariffs SET enabled = ?1,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')
                 WHERE id = ?2",
                params![enabled, tariff_id],
            )?;
        Ok(changed == 1)
    }

    #[cfg(test)]
    pub fn create_payment_order(&self, order: NewPaymentOrder<'_>) -> rusqlite::Result<i64> {
        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction()?;
        transaction.execute(
            "UPDATE payment_orders SET status = 'cancelled'
             WHERE telegram_user_id = ?1 AND status = 'awaiting_receipt'",
            [order.telegram_user_id.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO payment_orders (
                telegram_user_id, account_name, telegram_username, panel_username,
                plan_gb, price_toman, amount_due_toman, is_renewal, status
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, 'awaiting_receipt')",
            params![
                order.telegram_user_id.to_string(),
                order.account_name,
                order.telegram_username,
                order.panel_username,
                i64::from(order.plan_gb),
                i64::from(order.price_toman),
                order.is_renewal
            ],
        )?;
        let order_id = transaction.last_insert_rowid();
        transaction.commit()?;
        Ok(order_id)
    }

    pub fn create_tariff_payment_order(
        &self,
        order: NewTariffPaymentOrder<'_>,
    ) -> Result<Option<CreatedPaymentOrder>, CreateTariffPaymentOrderError> {
        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let telegram_user_id = order.telegram_user_id.to_string();
        let has_order_awaiting_admin_review = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM payment_orders
                WHERE telegram_user_id = ?1
                  AND status IN ('pending_review', 'approving')
             )",
            [&telegram_user_id],
            |row| row.get::<_, bool>(0),
        )?;
        if has_order_awaiting_admin_review {
            return Err(CreateTariffPaymentOrderError::OrderAwaitingAdminReview);
        }
        let tariff = transaction
            .query_row(
                "SELECT volume_gb, price_toman FROM subscription_tariffs
                 WHERE id = ?1 AND enabled = 1",
                [order.tariff_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((volume_gb, price_toman)) = tariff else {
            return Ok(None);
        };
        let volume_gb = u16::try_from(volume_gb).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?;
        let price_toman = u32::try_from(price_toman).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?;

        transaction.execute(
            "UPDATE payment_orders SET status = 'cancelled'
             WHERE telegram_user_id = ?1 AND status = 'awaiting_receipt'",
            [&telegram_user_id],
        )?;
        transaction.execute(
            "DELETE FROM wallet_reservations
             WHERE order_id IN (
                 SELECT id FROM payment_orders
                 WHERE telegram_user_id = ?1 AND status = 'cancelled'
             )",
            [&telegram_user_id],
        )?;

        let balance = transaction
            .query_row(
                "SELECT balance_toman FROM wallets WHERE telegram_user_id = ?1",
                [&telegram_user_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0);
        let wallet_used_toman = if order.use_wallet {
            let reserved = transaction.query_row(
                "SELECT COALESCE(SUM(amount_toman), 0) FROM wallet_reservations
                 WHERE telegram_user_id = ?1",
                [&telegram_user_id],
                |row| row.get::<_, i64>(0),
            )?;
            let available = balance.saturating_sub(reserved).max(0);
            available.min(i64::from(price_toman))
        } else {
            0
        };
        let amount_due_toman = i64::from(price_toman) - wallet_used_toman;
        let balance_after_wallet_use = balance.saturating_sub(wallet_used_toman);
        let status = if amount_due_toman == 0 {
            "pending_review"
        } else {
            "awaiting_receipt"
        };
        transaction.execute(
            "INSERT INTO payment_orders (
                telegram_user_id, account_name, telegram_username, panel_username,
                plan_gb, price_toman, wallet_used_toman, amount_due_toman,
                payment_method, is_renewal, status
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                telegram_user_id,
                order.account_name,
                order.telegram_username,
                order.panel_username,
                i64::from(volume_gb),
                i64::from(price_toman),
                wallet_used_toman,
                amount_due_toman,
                if order.use_wallet { "wallet" } else { "card" },
                order.is_renewal,
                status
            ],
        )?;
        let order_id = transaction.last_insert_rowid();
        if wallet_used_toman > 0 {
            transaction.execute(
                "INSERT INTO wallet_reservations (order_id, telegram_user_id, amount_toman)
                 VALUES (?1, ?2, ?3)",
                params![order_id, telegram_user_id, wallet_used_toman],
            )?;
        }
        let wallet_used_toman = u32::try_from(wallet_used_toman).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?;
        let amount_due_toman = u32::try_from(amount_due_toman).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?;
        transaction.commit()?;
        Ok(Some(CreatedPaymentOrder {
            id: order_id,
            plan_gb: volume_gb,
            price_toman,
            wallet_used_toman,
            amount_due_toman,
            wallet_balance_after: balance_after_wallet_use,
        }))
    }

    pub fn awaiting_receipt_order(&self, telegram_user_id: u64) -> rusqlite::Result<Option<i64>> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT id FROM payment_orders
                 WHERE telegram_user_id = ?1 AND status = 'awaiting_receipt'
                 ORDER BY id DESC LIMIT 1",
                [telegram_user_id.to_string()],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn submit_order_receipt(
        &self,
        order_id: i64,
        telegram_user_id: u64,
        file_id: &str,
    ) -> rusqlite::Result<bool> {
        let changed = self
            .database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "UPDATE payment_orders SET status = 'pending_review', receipt_file_id = ?1
                 WHERE id = ?2 AND telegram_user_id = ?3 AND status = 'awaiting_receipt'",
                params![file_id, order_id, telegram_user_id.to_string()],
            )?;
        Ok(changed == 1)
    }

    pub fn payment_order(&self, order_id: i64) -> rusqlite::Result<Option<PaymentOrder>> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT id, telegram_user_id, account_name, telegram_username, panel_username,
                        plan_gb, price_toman, wallet_used_toman, amount_due_toman,
                        payment_method, is_renewal, receipt_file_id
                 FROM payment_orders WHERE id = ?1",
                [order_id],
                payment_order_from_row,
            )
            .optional()
    }

    pub fn record_payment_review_message(
        &self,
        order_id: i64,
        reviewer_user_id: u64,
        message_id: i32,
        caption: &str,
        is_text: bool,
    ) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "INSERT OR IGNORE INTO payment_review_messages
                    (order_id, reviewer_user_id, message_id, caption, is_text)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    order_id,
                    reviewer_user_id.to_string(),
                    message_id,
                    caption,
                    is_text
                ],
            )?;
        Ok(())
    }

    pub fn payment_review_messages(
        &self,
        order_id: i64,
    ) -> rusqlite::Result<Vec<PaymentReviewMessage>> {
        let database = self.database.lock().expect("database lock poisoned");
        let mut statement = database.prepare(
            "SELECT reviewer_user_id, message_id, caption, is_text
             FROM payment_review_messages
             WHERE order_id = ?1
             ORDER BY reviewer_user_id, message_id",
        )?;
        let messages = statement
            .query_map([order_id], |row| {
                let reviewer_id: String = row.get(0)?;
                let reviewer_user_id = reviewer_id.parse().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(PaymentReviewMessage {
                    reviewer_user_id,
                    message_id: row.get(1)?,
                    caption: row.get(2)?,
                    is_text: row.get(3)?,
                })
            })?
            .collect();
        messages
    }

    pub fn delete_payment_review_messages(&self, order_id: i64) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "DELETE FROM payment_review_messages WHERE order_id = ?1",
                [order_id],
            )?;
        Ok(())
    }

    pub fn reset_order_receipt(&self, order_id: i64) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "UPDATE payment_orders SET status = 'awaiting_receipt', receipt_file_id = NULL
                 WHERE id = ?1 AND status = 'pending_review'",
                [order_id],
            )?;
        Ok(())
    }

    pub fn claim_payment_order(&self, order_id: i64) -> Result<Option<PaymentOrder>> {
        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction()?;
        let changed = transaction.execute(
            "UPDATE payment_orders SET status = 'approving'
             WHERE id = ?1 AND status = 'pending_review'",
            [order_id],
        )?;
        if changed != 1 {
            transaction.commit()?;
            return Ok(None);
        }
        let order = transaction.query_row(
            "SELECT id, telegram_user_id, account_name, telegram_username, panel_username,
                    plan_gb, price_toman, wallet_used_toman, amount_due_toman,
                    payment_method, is_renewal, receipt_file_id
             FROM payment_orders WHERE id = ?1",
            [order_id],
            payment_order_from_row,
        )?;
        transaction.commit()?;
        Ok(Some(order))
    }

    pub fn reject_payment_order(&self, order_id: i64) -> rusqlite::Result<Option<u64>> {
        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction()?;
        let order = transaction
            .query_row(
                "SELECT telegram_user_id FROM payment_orders
                 WHERE id = ?1 AND status = 'pending_review'",
                [order_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(telegram_user_id) = order else {
            transaction.commit()?;
            return Ok(None);
        };
        let changed = transaction.execute(
            "UPDATE payment_orders SET status = 'rejected'
             WHERE id = ?1 AND status = 'pending_review'",
            [order_id],
        )?;
        if changed == 1 {
            transaction.execute(
                "DELETE FROM wallet_reservations WHERE order_id = ?1",
                [order_id],
            )?;
        }
        transaction.commit()?;
        if changed != 1 {
            return Ok(None);
        }
        let telegram_user_id = telegram_user_id.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        Ok(Some(telegram_user_id))
    }

    pub fn release_payment_order(&self, order_id: i64) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "UPDATE payment_orders SET status = 'pending_review'
                 WHERE id = ?1 AND status = 'approving'",
                [order_id],
            )?;
        Ok(())
    }

    pub fn complete_payment_order(
        &self,
        order_id: i64,
        telegram_user_id: u64,
        panel_username: &str,
    ) -> rusqlite::Result<PaymentCompletion> {
        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction()?;
        let mut referral_credit = None;
        let order = transaction
            .query_row(
                "SELECT telegram_user_id, price_toman, wallet_used_toman FROM payment_orders
                 WHERE id = ?1 AND status = 'approving'",
                [order_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((order_user_id, price_toman, wallet_used_toman)) = order else {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        };
        let telegram_user_id_text = telegram_user_id.to_string();
        if order_user_id != telegram_user_id_text {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        transaction.execute(
            "INSERT INTO panel_users (telegram_user_id, panel_username)
             VALUES (?1, ?2)
             ON CONFLICT(telegram_user_id, panel_username) DO UPDATE SET
                updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
            params![telegram_user_id_text, panel_username],
        )?;
        let changed = transaction.execute(
            "UPDATE payment_orders SET status = 'approved'
             WHERE id = ?1 AND status = 'approving'",
            [order_id],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        if wallet_used_toman > 0 {
            let reservation = transaction
                .query_row(
                    "SELECT amount_toman FROM wallet_reservations WHERE order_id = ?1",
                    [order_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if reservation != Some(wallet_used_toman) {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            transaction.execute(
                "DELETE FROM wallet_reservations WHERE order_id = ?1",
                [order_id],
            )?;
            let previous_balance = transaction.query_row(
                "SELECT balance_toman FROM wallets WHERE telegram_user_id = ?1",
                [telegram_user_id.to_string()],
                |row| row.get::<_, i64>(0),
            )?;
            let new_balance = previous_balance.checked_sub(wallet_used_toman).ok_or(
                rusqlite::Error::IntegralValueOutOfRange(0, previous_balance),
            )?;
            let debt = if new_balance < 0 {
                new_balance
                    .checked_neg()
                    .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, new_balance))?
            } else {
                0
            };
            transaction.execute(
                "UPDATE wallets SET balance_toman = ?1, commission_debt_toman = ?2,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')
                 WHERE telegram_user_id = ?3",
                params![new_balance, debt, telegram_user_id.to_string()],
            )?;
            transaction.execute(
                "INSERT INTO wallet_transactions
                    (telegram_user_id, amount_toman, balance_after_toman,
                     transaction_type, order_id)
                 VALUES (?1, ?2, ?3, 'wallet_purchase', ?4)",
                params![
                    telegram_user_id.to_string(),
                    -wallet_used_toman,
                    new_balance,
                    order_id
                ],
            )?;
        }
        let (referrals_enabled, referral_percent) = transaction.query_row(
            "SELECT enabled, percent FROM referral_settings WHERE id = 1",
            [],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let referrer_id = transaction
            .query_row(
                "SELECT referred_by_user_id FROM bot_users WHERE telegram_user_id = ?1",
                [telegram_user_id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        if referrals_enabled && referral_percent > 0 {
            if let Some(referrer_id) = referrer_id {
                let exact_commission = price_toman
                    .checked_mul(referral_percent)
                    .ok_or(rusqlite::Error::IntegralValueOutOfRange(1, price_toman))?
                    / 100;
                let commission = round_commission_to_thousand(exact_commission);
                if commission > 0 {
                    let inserted = transaction.execute(
                        "INSERT OR IGNORE INTO referral_commissions
                        (order_id, referrer_user_id, invited_user_id, amount_toman)
                     VALUES (?1, ?2, ?3, ?4)",
                        params![order_id, referrer_id, telegram_user_id, commission],
                    )?;
                    if inserted == 1 {
                        let referrer_user_id = referrer_id.parse::<u64>().map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?;
                        let previous_balance = transaction
                            .query_row(
                                "SELECT balance_toman FROM wallets WHERE telegram_user_id = ?1",
                                [&referrer_id],
                                |row| row.get::<_, i64>(0),
                            )
                            .optional()?
                            .unwrap_or(0);
                        let new_balance = previous_balance.checked_add(commission).ok_or(
                            rusqlite::Error::IntegralValueOutOfRange(0, previous_balance),
                        )?;
                        let debt = if new_balance < 0 {
                            new_balance
                                .checked_neg()
                                .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, new_balance))?
                        } else {
                            0
                        };
                        transaction.execute(
                            "INSERT INTO wallets
                            (telegram_user_id, balance_toman, commission_debt_toman)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(telegram_user_id) DO UPDATE SET
                            balance_toman = excluded.balance_toman,
                            commission_debt_toman = excluded.commission_debt_toman,
                            updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
                            params![referrer_id, new_balance, debt],
                        )?;
                        transaction.execute(
                            "INSERT INTO wallet_transactions
                            (telegram_user_id, amount_toman, balance_after_toman,
                             transaction_type, order_id)
                         VALUES (?1, ?2, ?3, 'referral_commission', ?4)",
                            params![referrer_id, commission, new_balance, order_id],
                        )?;
                        referral_credit = Some(ReferralCommissionCredit {
                            referrer_user_id,
                            amount_toman: commission,
                            balance_after_toman: new_balance,
                        });
                    }
                }
            }
        }
        transaction.commit()?;
        Ok(PaymentCompletion { referral_credit })
    }

    pub fn cancel_awaiting_order(&self, telegram_user_id: u64) -> rusqlite::Result<()> {
        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction()?;
        transaction.execute(
            "UPDATE payment_orders SET status = 'cancelled'
             WHERE telegram_user_id = ?1 AND status = 'awaiting_receipt'",
            [telegram_user_id.to_string()],
        )?;
        transaction.execute(
            "DELETE FROM wallet_reservations
             WHERE order_id IN (
                 SELECT id FROM payment_orders
                 WHERE telegram_user_id = ?1 AND status = 'cancelled'
             )",
            [telegram_user_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn cancel_payment_order(
        &self,
        order_id: i64,
        telegram_user_id: u64,
    ) -> rusqlite::Result<bool> {
        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction()?;
        let changed = transaction.execute(
            "UPDATE payment_orders SET status = 'cancelled'
             WHERE id = ?1 AND telegram_user_id = ?2 AND (
                 status = 'awaiting_receipt'
                 OR (status = 'pending_review' AND payment_method = 'wallet'
                     AND amount_due_toman = 0)
             )",
            params![order_id, telegram_user_id.to_string()],
        )?;
        if changed == 1 {
            transaction.execute(
                "DELETE FROM wallet_reservations WHERE order_id = ?1",
                [order_id],
            )?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    pub fn is_admin(&self, telegram_user_id: u64) -> rusqlite::Result<bool> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM admins WHERE telegram_user_id = ?1)",
                [telegram_user_id.to_string()],
                |row| row.get(0),
            )
    }

    pub fn add_admin(&self, telegram_user_id: u64) -> rusqlite::Result<bool> {
        if telegram_user_id == self.owner_id.0 {
            return Ok(false);
        }
        let changed = self
            .database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "INSERT OR IGNORE INTO admins (telegram_user_id) VALUES (?1)",
                [telegram_user_id.to_string()],
            )?;
        Ok(changed > 0)
    }

    pub fn remove_admin(&self, telegram_user_id: u64) -> rusqlite::Result<bool> {
        if telegram_user_id == self.owner_id.0 {
            return Ok(false);
        }
        let changed = self
            .database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "DELETE FROM admins WHERE telegram_user_id = ?1",
                [telegram_user_id.to_string()],
            )?;
        Ok(changed > 0)
    }

    pub fn admins(&self) -> Result<Vec<u64>> {
        let database = self.database.lock().expect("database lock poisoned");
        let mut statement =
            database.prepare("SELECT telegram_user_id FROM admins ORDER BY added_at")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|result| {
                result?
                    .parse::<u64>()
                    .context("Stored admin Telegram ID is invalid")
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ids)
    }

    pub fn save_panel_password(&self, password: &str) -> Result<()> {
        let entry = keyring::Entry::new("foxngbot-rust", &self.owner_id.0.to_string())
            .context("Could not access the operating-system credential store")?;
        entry
            .set_password(password)
            .context("Could not save the panel password in the operating-system credential store")
    }

    pub fn panel_password(&self) -> Result<Option<Zeroizing<String>>> {
        let entry = keyring::Entry::new("foxngbot-rust", &self.owner_id.0.to_string())
            .context("Could not access the operating-system credential store")?;
        match entry.get_password() {
            Ok(password) => Ok(Some(Zeroizing::new(password))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context(
                "Could not read the panel password from the operating-system credential store",
            ),
        }
    }

    pub fn set_session(&self, access_token: Zeroizing<String>) {
        *self.session.lock().expect("session lock poisoned") = Some(PanelSession { access_token });
    }

    pub fn panel_access_token(&self) -> Option<Zeroizing<String>> {
        self.session
            .lock()
            .expect("session lock poisoned")
            .as_ref()
            .map(|session| Zeroizing::new(session.access_token.to_string()))
    }

    pub fn panel_status(&self) -> rusqlite::Result<String> {
        let session = self.session.lock().expect("session lock poisoned");
        match self.panel_config()? {
            Some((url, username)) if session.is_some() => Ok(format!(
                "✅ ورود به پنل برقرار است.\n🌐 آدرس: {url}\n👤 نام کاربری: {username}"
            )),
            Some((url, username)) => Ok(format!(
                "⚠️ نشست پنل فعال نیست؛ مالک باید یک‌بار دوباره وارد شود.\n🌐 آدرس: {url}\n👤 نام کاربری: {username}"
            )),
            None => Ok("هنوز پنلی تنظیم نشده است.".into()),
        }
    }
}

fn migrate_panel_user_mapping(database: &Connection) -> rusqlite::Result<()> {
    let mut statement = database.prepare("PRAGMA table_info(panel_users)")?;
    let columns = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(5)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut primary_key_columns = columns
        .into_iter()
        .filter(|(position, _)| *position > 0)
        .collect::<Vec<_>>();
    drop(statement);
    primary_key_columns.sort_by_key(|(position, _)| *position);
    if primary_key_columns
        .iter()
        .map(|(_, name)| name.as_str())
        .eq(["telegram_user_id", "panel_username"])
    {
        return Ok(());
    }

    database.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE panel_users_multi (
            telegram_user_id TEXT NOT NULL,
            panel_username TEXT NOT NULL UNIQUE,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')),
            PRIMARY KEY (telegram_user_id, panel_username)
         );
         INSERT OR IGNORE INTO panel_users_multi
            (telegram_user_id, panel_username, updated_at)
         SELECT telegram_user_id, panel_username, updated_at FROM panel_users;
         DROP TABLE panel_users;
         ALTER TABLE panel_users_multi RENAME TO panel_users;
         COMMIT;",
    )
}

fn migrate_commerce_settings(database: &Connection) -> rusqlite::Result<()> {
    let has_renewals_column = database
        .prepare("PRAGMA table_info(commerce_settings)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|column| column == "renewals_enabled");

    if !has_renewals_column {
        database.execute_batch(
            "ALTER TABLE commerce_settings
                ADD COLUMN renewals_enabled INTEGER NOT NULL DEFAULT 1;
             UPDATE commerce_settings
                SET renewals_enabled = purchases_enabled;",
        )?;
    }
    Ok(())
}

fn migrate_support_settings(database: &Connection) -> rusqlite::Result<()> {
    database.execute_batch(
        "CREATE TABLE IF NOT EXISTS support_settings (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            support_username TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
        );
        INSERT OR IGNORE INTO support_settings (id, support_username)
        VALUES (1, 'ipublicsupport');",
    )
}

fn migrate_required_channels(database: &Connection) -> rusqlite::Result<()> {
    let had_required_channels_table: bool = database.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = 'required_channels'
         )",
        [],
        |row| row.get(0),
    )?;
    database.execute_batch(
        "CREATE TABLE IF NOT EXISTS required_channel_settings (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
        );
        INSERT OR IGNORE INTO required_channel_settings (id) VALUES (1);
        CREATE TABLE IF NOT EXISTS required_channels (
            username TEXT PRIMARY KEY NOT NULL,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
        );",
    )?;
    if !had_required_channels_table {
        database.execute(
            "INSERT OR IGNORE INTO required_channels (username) VALUES ('hakungvpn')",
            [],
        )?;
    }
    Ok(())
}

fn read_support_username(database: &Connection) -> rusqlite::Result<String> {
    let support_username: String = database.query_row(
        "SELECT support_username FROM support_settings WHERE id = 1",
        [],
        |row| row.get(0),
    )?;
    if !is_valid_support_username(&support_username) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(support_username.to_ascii_lowercase())
}

fn is_valid_support_username(support_username: &str) -> bool {
    (5..=32).contains(&support_username.len())
        && support_username
            .bytes()
            .all(|character| character.is_ascii_alphanumeric() || character == b'_')
}

fn migrate_bot_user_referrals(database: &Connection) -> rusqlite::Result<()> {
    let columns = database
        .prepare("PRAGMA table_info(bot_users)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|column| column == "referred_by_user_id") {
        database.execute(
            "ALTER TABLE bot_users ADD COLUMN referred_by_user_id TEXT",
            [],
        )?;
    }
    database.execute(
        "CREATE INDEX IF NOT EXISTS idx_bot_users_referred_by
         ON bot_users (referred_by_user_id)",
        [],
    )?;
    Ok(())
}

fn migrate_bot_user_profile(database: &Connection) -> rusqlite::Result<()> {
    let columns = database
        .prepare("PRAGMA table_info(bot_users)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for column in ["username", "first_name"] {
        if !columns.iter().any(|existing| existing == column) {
            database.execute(
                &format!("ALTER TABLE bot_users ADD COLUMN {column} TEXT"),
                [],
            )?;
        }
    }
    Ok(())
}

fn migrate_wallet_transaction_descriptions(database: &Connection) -> rusqlite::Result<()> {
    let columns = database
        .prepare("PRAGMA table_info(wallet_transactions)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|column| column == "description") {
        database.execute(
            "ALTER TABLE wallet_transactions ADD COLUMN description TEXT",
            [],
        )?;
    }
    Ok(())
}

fn migrate_financial_transaction_constraints(database: &Connection) -> rusqlite::Result<()> {
    database.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_wallet_transactions_unique_order_type
         ON wallet_transactions (order_id, transaction_type)
         WHERE order_id IS NOT NULL
           AND transaction_type IN ('wallet_purchase', 'referral_commission')",
        [],
    )?;
    Ok(())
}

fn migrate_payment_wallet_columns(database: &Connection) -> rusqlite::Result<()> {
    let order_columns = database
        .prepare("PRAGMA table_info(payment_orders)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (column, definition) in [
        ("wallet_used_toman", "INTEGER NOT NULL DEFAULT 0"),
        ("amount_due_toman", "INTEGER NOT NULL DEFAULT 0"),
        ("payment_method", "TEXT NOT NULL DEFAULT 'card'"),
    ] {
        if !order_columns.iter().any(|existing| existing == column) {
            database.execute(
                &format!("ALTER TABLE payment_orders ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }
    database.execute(
        "UPDATE payment_orders SET amount_due_toman = price_toman
         WHERE payment_method = 'card' AND amount_due_toman = 0",
        [],
    )?;

    let review_columns = database
        .prepare("PRAGMA table_info(payment_review_messages)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !review_columns.iter().any(|column| column == "is_text") {
        database.execute(
            "ALTER TABLE payment_review_messages ADD COLUMN is_text INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

fn round_commission_to_thousand(amount_toman: i64) -> i64 {
    let rounded_down = amount_toman / 1_000 * 1_000;
    if amount_toman % 1_000 >= 500 {
        rounded_down + 1_000
    } else {
        rounded_down
    }
}

fn seed_subscription_tariffs(database: &Connection) -> rusqlite::Result<()> {
    let has_tariffs: bool = database.query_row(
        "SELECT EXISTS(SELECT 1 FROM subscription_tariffs)",
        [],
        |row| row.get(0),
    )?;
    if has_tariffs {
        return Ok(());
    }

    let defaults = [
        (10, 60_000),
        (20, 115_000),
        (30, 165_000),
        (50, 255_000),
        (80, 370_000),
        (100, 450_000),
        (150, 675_000),
        (200, 850_000),
        (250, 1_025_000),
        (300, 1_200_000),
    ];
    let transaction = database.unchecked_transaction()?;
    for (volume_gb, price_toman) in defaults {
        transaction.execute(
            "INSERT INTO subscription_tariffs (volume_gb, price_toman)
             VALUES (?1, ?2)",
            params![volume_gb, price_toman],
        )?;
    }
    transaction.commit()
}

fn subscription_tariff_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SubscriptionTariff> {
    let volume_gb: i64 = row.get(1)?;
    let price_toman: i64 = row.get(2)?;
    Ok(SubscriptionTariff {
        id: row.get(0)?,
        volume_gb: u16::try_from(volume_gb).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        price_toman: u32::try_from(price_toman).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        enabled: row.get(3)?,
    })
}

fn payment_order_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PaymentOrder> {
    let telegram_user_id: String = row.get(1)?;
    let plan_gb: i64 = row.get(5)?;
    let price_toman: i64 = row.get(6)?;
    let wallet_used_toman: i64 = row.get(7)?;
    let amount_due_toman: i64 = row.get(8)?;
    let payment_method: String = row.get(9)?;
    Ok(PaymentOrder {
        id: row.get(0)?,
        telegram_user_id: telegram_user_id.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        account_name: row.get(2)?,
        telegram_username: row.get(3)?,
        panel_username: row.get(4)?,
        plan_gb: u16::try_from(plan_gb).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        price_toman: u32::try_from(price_toman).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        wallet_used_toman: u32::try_from(wallet_used_toman).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        amount_due_toman: u32::try_from(amount_due_toman).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        is_wallet_payment: payment_method == "wallet",
        is_renewal: row.get(10)?,
        receipt_file_id: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_backup_test_path() -> std::path::PathBuf {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "falconbot-test-backup-{}-{}.sqlite3",
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn database_backup_can_be_restored_and_invalid_backup_is_rejected() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        state.set_referral_percent(24).unwrap();
        let backup_path = temporary_backup_test_path();
        let rollback_path = temporary_backup_test_path();
        state.create_database_backup(&backup_path).unwrap();

        state.set_referral_percent(12).unwrap();
        state.set_referrals_enabled(false).unwrap();
        state
            .restore_database_backup(&backup_path, &rollback_path)
            .unwrap();
        assert_eq!(state.referral_status().unwrap(), (true, 24));

        let invalid_path = temporary_backup_test_path();
        fs::write(&invalid_path, b"not a sqlite database").unwrap();
        assert!(state
            .restore_database_backup(&invalid_path, &rollback_path)
            .is_err());
        assert_eq!(state.referral_status().unwrap(), (true, 24));

        fs::remove_file(backup_path).unwrap();
        fs::remove_file(rollback_path).unwrap();
        fs::remove_file(invalid_path).unwrap();
    }

    #[test]
    fn disk_space_usage_reports_nonzero_filesystem_capacity() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let usage = state.disk_space_usage().unwrap();

        assert!(usage.total_bytes > 0);
        assert!(usage.free_bytes <= usage.total_bytes);
    }

    #[test]
    fn subscription_tariffs_are_seeded_and_read_from_the_database() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let tariffs = state.subscription_tariffs().unwrap();

        assert_eq!(
            tariffs
                .iter()
                .map(|tariff| (tariff.volume_gb, tariff.price_toman))
                .collect::<Vec<_>>(),
            vec![
                (10, 60_000),
                (20, 115_000),
                (30, 165_000),
                (50, 255_000),
                (80, 370_000),
                (100, 450_000),
                (150, 675_000),
                (200, 850_000),
                (250, 1_025_000),
                (300, 1_200_000),
            ]
        );
    }

    #[test]
    fn tariffs_can_be_added_edited_and_hidden_when_disabled() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let tariff_id = state.add_subscription_tariff(35, 185_000).unwrap();
        assert_eq!(
            state.subscription_tariff(tariff_id).unwrap(),
            Some(SubscriptionTariff {
                id: tariff_id,
                volume_gb: 35,
                price_toman: 185_000,
                enabled: true,
            })
        );

        assert!(state
            .update_subscription_tariff_volume(tariff_id, 40)
            .unwrap());
        assert!(state
            .update_subscription_tariff_price(tariff_id, 200_000)
            .unwrap());
        assert!(state
            .set_subscription_tariff_enabled(tariff_id, false)
            .unwrap());
        assert_eq!(
            state
                .active_subscription_tariffs()
                .unwrap()
                .iter()
                .map(|tariff| tariff.id)
                .find(|id| *id == tariff_id),
            None
        );
        assert_eq!(
            state.subscription_tariff(tariff_id).unwrap(),
            Some(SubscriptionTariff {
                id: tariff_id,
                volume_gb: 40,
                price_toman: 200_000,
                enabled: false,
            })
        );
    }

    #[test]
    fn tariff_edits_do_not_change_price_or_volume_of_existing_orders() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let tariff_id = state
            .active_subscription_tariffs()
            .unwrap()
            .into_iter()
            .find(|tariff| tariff.volume_gb == 10)
            .unwrap()
            .id;
        state
            .update_subscription_tariff_price(tariff_id, 65_000)
            .unwrap();
        let order_snapshot = state
            .create_tariff_payment_order(NewTariffPaymentOrder {
                telegram_user_id: 42,
                account_name: "Test User",
                telegram_username: None,
                panel_username: "haku-420001",
                tariff_id,
                use_wallet: false,
                is_renewal: false,
            })
            .unwrap()
            .unwrap();
        let order_id = order_snapshot.id;
        assert_eq!(
            (order_snapshot.plan_gb, order_snapshot.price_toman),
            (10, 65_000)
        );
        assert_eq!(
            (
                order_snapshot.wallet_used_toman,
                order_snapshot.amount_due_toman
            ),
            (0, 65_000)
        );

        state
            .update_subscription_tariff_volume(tariff_id, 12)
            .unwrap();
        state
            .update_subscription_tariff_price(tariff_id, 75_000)
            .unwrap();
        {
            let database = state.database.lock().unwrap();
            seed_subscription_tariffs(&database).unwrap();
        }

        let order = state.payment_order(order_id).unwrap().unwrap();
        assert_eq!(order.plan_gb, 10);
        assert_eq!(order.price_toman, 65_000);
        assert!(state
            .subscription_tariffs()
            .unwrap()
            .iter()
            .any(|tariff| tariff.volume_gb == 12 && tariff.price_toman == 75_000));
        assert!(state
            .subscription_tariffs()
            .unwrap()
            .iter()
            .all(|tariff| tariff.volume_gb != 10));

        state
            .set_subscription_tariff_enabled(tariff_id, false)
            .unwrap();
        assert!(state
            .create_tariff_payment_order(NewTariffPaymentOrder {
                telegram_user_id: 43,
                account_name: "Another User",
                telegram_username: None,
                panel_username: "haku-430001",
                tariff_id,
                use_wallet: false,
                is_renewal: false,
            })
            .unwrap()
            .is_none());
    }

    #[test]
    fn stored_timestamps_use_the_iran_offset() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        state.record_bot_user(42).unwrap();
        let timestamp: String = state
            .database
            .lock()
            .unwrap()
            .query_row(
                "SELECT last_seen_at FROM bot_users WHERE telegram_user_id = '42'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let parsed = chrono::DateTime::parse_from_rfc3339(&timestamp).unwrap();

        assert_eq!(parsed.offset().local_minus_utc(), 3 * 60 * 60 + 30 * 60);
    }

    #[test]
    fn referral_is_assigned_only_on_first_registration_and_never_to_self() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        state.record_bot_user(10).unwrap();
        state.record_bot_user_with_referrer(20, Some(10)).unwrap();
        state.record_bot_user_with_referrer(20, Some(30)).unwrap();
        state.record_bot_user_with_referrer(30, Some(30)).unwrap();
        state.record_bot_user_with_referrer(40, Some(999)).unwrap();

        let database = state.database.lock().unwrap();
        let referrer: Option<String> = database
            .query_row(
                "SELECT referred_by_user_id FROM bot_users WHERE telegram_user_id = '20'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let self_referrer: Option<String> = database
            .query_row(
                "SELECT referred_by_user_id FROM bot_users WHERE telegram_user_id = '30'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let invalid_referrer: Option<String> = database
            .query_row(
                "SELECT referred_by_user_id FROM bot_users WHERE telegram_user_id = '40'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(referrer.as_deref(), Some("10"));
        assert_eq!(self_referrer, None);
        assert_eq!(invalid_referrer, None);
    }

    #[test]
    fn users_can_be_searched_by_id_or_username_with_referral_details() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        state.record_bot_user(10).unwrap();
        state.record_bot_user_with_referrer(20, Some(10)).unwrap();
        state
            .update_bot_user_profile(10, Some("inviter"), "Inviter")
            .unwrap();
        state
            .update_bot_user_profile(20, Some("invitee"), "Invitee")
            .unwrap();

        assert_eq!(
            state.search_bot_users("@invitee").unwrap(),
            vec![BotUserSearchResult {
                telegram_user_id: 20,
                username: Some("invitee".into()),
                first_name: Some("Invitee".into()),
            }]
        );
        assert_eq!(
            state.search_bot_users("20").unwrap()[0].telegram_user_id,
            20
        );
        assert_eq!(
            state.bot_user_details(20).unwrap(),
            Some(BotUserDetails {
                telegram_user_id: 20,
                username: Some("invitee".into()),
                first_name: Some("Invitee".into()),
                referrer_user_id: Some(10),
                referrer_username: Some("inviter".into()),
                referrer_first_name: Some("Inviter".into()),
                referral_count: 0,
                commissions_paid_toman: 0,
            })
        );
        assert_eq!(
            state.bot_user_details(10).unwrap().unwrap().referral_count,
            1
        );
    }

    #[test]
    fn referral_migration_preserves_existing_bot_users() {
        let database = Connection::open_in_memory().unwrap();
        database
            .execute_batch(
                "CREATE TABLE bot_users (
                    telegram_user_id TEXT PRIMARY KEY NOT NULL,
                    first_seen_at TEXT NOT NULL,
                    last_seen_at TEXT NOT NULL
                 );
                 INSERT INTO bot_users VALUES
                    ('42', '2026-01-01T00:00:00+03:30', '2026-01-01T00:00:00+03:30');",
            )
            .unwrap();

        migrate_bot_user_referrals(&database).unwrap();
        migrate_bot_user_profile(&database).unwrap();

        let (user_id, referrer_id): (String, Option<String>) = database
            .query_row(
                "SELECT telegram_user_id, referred_by_user_id FROM bot_users",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(user_id, "42");
        assert_eq!(referrer_id, None);
        let profile_columns = database
            .prepare("PRAGMA table_info(bot_users)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(profile_columns.iter().any(|column| column == "username"));
        assert!(profile_columns.iter().any(|column| column == "first_name"));
    }

    #[test]
    fn payment_wallet_migration_preserves_existing_order_amounts() {
        let database = Connection::open_in_memory().unwrap();
        database
            .execute_batch(
                "CREATE TABLE payment_orders (
                id INTEGER PRIMARY KEY,
                telegram_user_id TEXT NOT NULL,
                price_toman INTEGER NOT NULL,
                status TEXT NOT NULL
             );
             INSERT INTO payment_orders VALUES (1, '42', 60_000, 'pending_review');
             CREATE TABLE payment_review_messages (
                order_id INTEGER NOT NULL,
                reviewer_user_id TEXT NOT NULL,
                message_id INTEGER NOT NULL,
                caption TEXT NOT NULL,
                PRIMARY KEY (order_id, reviewer_user_id, message_id)
             );
             INSERT INTO payment_review_messages VALUES (1, '1', 1, 'caption');",
            )
            .unwrap();

        migrate_payment_wallet_columns(&database).unwrap();
        let migrated_order = database
            .query_row(
                "SELECT amount_due_toman, wallet_used_toman, payment_method
             FROM payment_orders WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap();
        let review_is_text = database
            .query_row("SELECT is_text FROM payment_review_messages", [], |row| {
                row.get::<_, bool>(0)
            })
            .optional()
            .unwrap()
            .unwrap_or(false);
        assert_eq!(migrated_order, (60_000, 0, "card".to_owned()));
        assert!(!review_is_text);
    }

    #[test]
    fn wallet_transaction_description_migration_preserves_existing_rows() {
        let database = Connection::open_in_memory().unwrap();
        database
            .execute_batch(
                "CREATE TABLE wallet_transactions (
                    id INTEGER PRIMARY KEY,
                    telegram_user_id TEXT NOT NULL,
                    amount_toman INTEGER NOT NULL,
                    balance_after_toman INTEGER NOT NULL,
                    transaction_type TEXT NOT NULL,
                    order_id INTEGER,
                    created_at TEXT NOT NULL
                 );
                 INSERT INTO wallet_transactions
                    (id, telegram_user_id, amount_toman, balance_after_toman,
                     transaction_type, order_id, created_at)
                 VALUES (1, '42', 1000, 1000, 'owner_adjustment', NULL, '2026-01-01');",
            )
            .unwrap();

        migrate_wallet_transaction_descriptions(&database).unwrap();
        let (amount, description): (i64, Option<String>) = database
            .query_row(
                "SELECT amount_toman, description FROM wallet_transactions WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(amount, 1_000);
        assert_eq!(description, None);
    }

    #[test]
    fn successful_payment_credits_referrer_once_at_fifteen_percent() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        state.record_bot_user(10).unwrap();
        state.record_bot_user_with_referrer(20, Some(10)).unwrap();
        let order_id = state
            .create_payment_order(NewPaymentOrder {
                telegram_user_id: 20,
                account_name: "Invited User",
                telegram_username: None,
                panel_username: "haku-200001",
                plan_gb: 10,
                price_toman: 115_000,
                is_renewal: false,
            })
            .unwrap();
        state.submit_order_receipt(order_id, 20, "receipt").unwrap();
        state.claim_payment_order(order_id).unwrap().unwrap();

        let referral_credit = state
            .complete_payment_order(order_id, 20, "haku-200001")
            .unwrap()
            .referral_credit
            .unwrap();
        assert_eq!(
            referral_credit,
            ReferralCommissionCredit {
                referrer_user_id: 10,
                amount_toman: 17_000,
                balance_after_toman: 17_000,
            }
        );
        assert!(state
            .complete_payment_order(order_id, 20, "haku-200001")
            .is_err());
        assert_eq!(state.wallet_summary(10).unwrap(), (17_000, 0));
        assert_eq!(state.referral_summary(10).unwrap(), (1, 17_000));
        assert_eq!(state.bot_user_statistics().unwrap(), (2, 1, 17_000));
        assert_eq!(state.wallet_statistics().unwrap(), (17_000, 17_000));
    }

    #[test]
    fn referral_ledger_is_unique_per_order_and_payment_completion_is_atomic() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        state.record_bot_user(10).unwrap();
        state.record_bot_user_with_referrer(20, Some(10)).unwrap();
        let order_id = state
            .create_payment_order(NewPaymentOrder {
                telegram_user_id: 20,
                account_name: "Invited User",
                telegram_username: None,
                panel_username: "haku-200001",
                plan_gb: 10,
                price_toman: 115_000,
                is_renewal: false,
            })
            .unwrap();
        state.submit_order_receipt(order_id, 20, "receipt").unwrap();
        state.claim_payment_order(order_id).unwrap().unwrap();

        {
            let database = state.database.lock().unwrap();
            database
                .execute(
                    "INSERT INTO wallet_transactions
                        (telegram_user_id, amount_toman, balance_after_toman,
                         transaction_type, order_id)
                     VALUES ('10', 1000, 1000, 'referral_commission', ?1)",
                    [order_id],
                )
                .unwrap();
            assert!(database
                .execute(
                    "INSERT INTO wallet_transactions
                        (telegram_user_id, amount_toman, balance_after_toman,
                         transaction_type, order_id)
                     VALUES ('10', 1000, 2000, 'referral_commission', ?1)",
                    [order_id],
                )
                .is_err());
        }

        assert!(state
            .complete_payment_order(order_id, 20, "haku-200001")
            .is_err());
        let database = state.database.lock().unwrap();
        let (status, commission_count): (String, i64) = database
            .query_row(
                "SELECT status,
                        (SELECT COUNT(*) FROM referral_commissions WHERE order_id = ?1)
                 FROM payment_orders WHERE id = ?1",
                [order_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "approving");
        assert_eq!(commission_count, 0);
        drop(database);
        assert_eq!(state.wallet_summary(10).unwrap(), (0, 0));

        state
            .database
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM wallet_transactions
                 WHERE order_id = ?1 AND transaction_type = 'referral_commission'",
                [order_id],
            )
            .unwrap();
        state.release_payment_order(order_id).unwrap();
        state.claim_payment_order(order_id).unwrap().unwrap();
        state
            .complete_payment_order(order_id, 20, "haku-200001")
            .unwrap();
        assert_eq!(state.wallet_summary(10).unwrap(), (17_000, 0));
    }

    #[test]
    fn referral_settings_persist_and_apply_custom_commission_rate() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        assert_eq!(state.referral_status().unwrap(), (true, 15));
        state.set_referral_percent(20).unwrap();
        state.record_bot_user(10).unwrap();
        state.record_bot_user_with_referrer(20, Some(10)).unwrap();
        let order_id = state
            .create_payment_order(NewPaymentOrder {
                telegram_user_id: 20,
                account_name: "Invited User",
                telegram_username: None,
                panel_username: "haku-200001",
                plan_gb: 10,
                price_toman: 115_000,
                is_renewal: false,
            })
            .unwrap();
        state.submit_order_receipt(order_id, 20, "receipt").unwrap();
        state.claim_payment_order(order_id).unwrap().unwrap();
        state
            .complete_payment_order(order_id, 20, "haku-200001")
            .unwrap();

        assert_eq!(state.referral_status().unwrap(), (true, 20));
        assert_eq!(state.wallet_summary(10).unwrap(), (23_000, 0));
    }

    #[test]
    fn disabled_referrals_skip_new_attribution_and_commissions() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        state.record_bot_user(10).unwrap();
        state.set_referrals_enabled(false).unwrap();
        state.record_bot_user_with_referrer(20, Some(10)).unwrap();
        assert_eq!(state.referral_summary(10).unwrap(), (0, 0));

        state.set_referrals_enabled(true).unwrap();
        state.record_bot_user_with_referrer(30, Some(10)).unwrap();
        state.set_referrals_enabled(false).unwrap();
        let order_id = state
            .create_payment_order(NewPaymentOrder {
                telegram_user_id: 30,
                account_name: "Invited User",
                telegram_username: None,
                panel_username: "haku-300001",
                plan_gb: 10,
                price_toman: 115_000,
                is_renewal: false,
            })
            .unwrap();
        state.submit_order_receipt(order_id, 30, "receipt").unwrap();
        state.claim_payment_order(order_id).unwrap().unwrap();
        let completion = state
            .complete_payment_order(order_id, 30, "haku-300001")
            .unwrap();
        assert_eq!(completion.referral_credit, None);

        assert_eq!(state.referral_status().unwrap(), (false, 15));
        assert_eq!(state.referral_summary(10).unwrap(), (1, 0));
        assert_eq!(state.wallet_summary(10).unwrap(), (0, 0));
    }

    #[test]
    fn wallet_order_reserves_price_and_preserves_surplus_until_approval() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let tariff_id = state
            .active_subscription_tariffs()
            .unwrap()
            .into_iter()
            .find(|tariff| tariff.volume_gb == 10)
            .unwrap()
            .id;
        state.set_wallet_balance(42, 100_000).unwrap();
        let order_snapshot = state
            .create_tariff_payment_order(NewTariffPaymentOrder {
                telegram_user_id: 42,
                account_name: "Test User",
                telegram_username: None,
                panel_username: "haku-420001",
                tariff_id,
                use_wallet: true,
                is_renewal: false,
            })
            .unwrap()
            .unwrap();

        let order_id = order_snapshot.id;
        assert_eq!(
            (
                order_snapshot.price_toman,
                order_snapshot.wallet_used_toman,
                order_snapshot.amount_due_toman
            ),
            (60_000, 60_000, 0)
        );
        assert_eq!(order_snapshot.wallet_balance_after, 40_000);
        assert_eq!(state.wallet_summary(42).unwrap(), (100_000, 0));
        assert_eq!(state.awaiting_receipt_order(42).unwrap(), None);
        let order = state.payment_order(order_id).unwrap().unwrap();
        assert!(order.is_wallet_payment);
        assert_eq!(order.receipt_file_id, None);
        assert!(state.claim_payment_order(order_id).unwrap().is_some());

        state
            .complete_payment_order(order_id, 42, "haku-420001")
            .unwrap();
        assert_eq!(state.wallet_summary(42).unwrap(), (40_000, 0));
    }

    #[test]
    fn rejecting_full_wallet_order_releases_reservation_without_deduction() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let tariff_id = state
            .active_subscription_tariffs()
            .unwrap()
            .into_iter()
            .find(|tariff| tariff.volume_gb == 10)
            .unwrap()
            .id;
        state.set_wallet_balance(42, 60_000).unwrap();
        let order_snapshot = state
            .create_tariff_payment_order(NewTariffPaymentOrder {
                telegram_user_id: 42,
                account_name: "Test User",
                telegram_username: None,
                panel_username: "haku-420001",
                tariff_id,
                use_wallet: true,
                is_renewal: false,
            })
            .unwrap()
            .unwrap();
        let order_id = order_snapshot.id;
        assert_eq!(
            (
                order_snapshot.wallet_used_toman,
                order_snapshot.amount_due_toman
            ),
            (60_000, 0)
        );

        assert_eq!(state.reject_payment_order(order_id).unwrap(), Some(42));
        assert_eq!(state.wallet_summary(42).unwrap(), (60_000, 0));
        let reservations: i64 = state
            .database
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM wallet_reservations WHERE order_id = ?1",
                [order_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reservations, 0);
    }

    #[test]
    fn partial_wallet_payment_releases_reservation_when_order_is_rejected() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let tariff_id = state
            .active_subscription_tariffs()
            .unwrap()
            .into_iter()
            .find(|tariff| tariff.volume_gb == 10)
            .unwrap()
            .id;
        state.set_wallet_balance(42, 20_000).unwrap();
        let order_snapshot = state
            .create_tariff_payment_order(NewTariffPaymentOrder {
                telegram_user_id: 42,
                account_name: "Test User",
                telegram_username: None,
                panel_username: "haku-420001",
                tariff_id,
                use_wallet: true,
                is_renewal: false,
            })
            .unwrap()
            .unwrap();
        let order_id = order_snapshot.id;
        assert_eq!(
            (
                order_snapshot.wallet_used_toman,
                order_snapshot.amount_due_toman
            ),
            (20_000, 40_000)
        );
        assert_eq!(order_snapshot.wallet_balance_after, 0);
        assert_eq!(state.awaiting_receipt_order(42).unwrap(), Some(order_id));
        assert_eq!(state.wallet_summary(42).unwrap(), (20_000, 0));
        state.submit_order_receipt(order_id, 42, "receipt").unwrap();

        assert_eq!(state.reject_payment_order(order_id).unwrap(), Some(42));
        assert_eq!(state.wallet_summary(42).unwrap(), (20_000, 0));
        let reservations: i64 = state
            .database
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM wallet_reservations WHERE order_id = ?1",
                [order_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reservations, 0);
    }

    #[test]
    fn partial_wallet_payment_deducts_only_reserved_amount_after_approval() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let tariff_id = state
            .active_subscription_tariffs()
            .unwrap()
            .into_iter()
            .find(|tariff| tariff.volume_gb == 10)
            .unwrap()
            .id;
        state.set_wallet_balance(42, 25_000).unwrap();
        let order_snapshot = state
            .create_tariff_payment_order(NewTariffPaymentOrder {
                telegram_user_id: 42,
                account_name: "Test User",
                telegram_username: None,
                panel_username: "haku-420001",
                tariff_id,
                use_wallet: true,
                is_renewal: false,
            })
            .unwrap()
            .unwrap();
        let order_id = order_snapshot.id;
        assert_eq!(
            (
                order_snapshot.wallet_used_toman,
                order_snapshot.amount_due_toman,
                order_snapshot.wallet_balance_after
            ),
            (25_000, 35_000, 0)
        );
        state.submit_order_receipt(order_id, 42, "receipt").unwrap();
        state.claim_payment_order(order_id).unwrap().unwrap();
        state
            .complete_payment_order(order_id, 42, "haku-420001")
            .unwrap();
        assert_eq!(state.wallet_summary(42).unwrap(), (0, 0));
    }

    #[test]
    fn commission_rounding_uses_nearest_thousand_and_rounds_half_up() {
        assert_eq!(round_commission_to_thousand(17_250), 17_000);
        assert_eq!(round_commission_to_thousand(24_499), 24_000);
        assert_eq!(round_commission_to_thousand(24_500), 25_000);
        assert_eq!(round_commission_to_thousand(24_750), 25_000);
    }

    #[test]
    fn owner_wallet_adjustment_sets_exact_signed_balance_and_is_idempotent() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();

        assert_eq!(
            state
                .set_wallet_balance_with_reason(42, -25_000, "اصلاح شارژ اشتباه")
                .unwrap(),
            0
        );
        assert_eq!(state.wallet_summary(42).unwrap(), (-25_000, 25_000));
        assert_eq!(state.set_wallet_balance(42, -25_000).unwrap(), -25_000);
        assert_eq!(state.wallet_summary(42).unwrap(), (-25_000, 25_000));

        let transactions: i64 = state
            .database
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM wallet_transactions
                 WHERE telegram_user_id = '42' AND transaction_type = 'owner_adjustment'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(transactions, 1);
        let description: String = state
            .database
            .lock()
            .unwrap()
            .query_row(
                "SELECT description FROM wallet_transactions
                 WHERE telegram_user_id = '42' AND transaction_type = 'owner_adjustment'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(description, "اصلاح شارژ اشتباه");
    }

    #[test]
    fn wallet_statistics_sum_signed_balances_and_paid_commissions() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        state.set_wallet_balance(10, 12_000).unwrap();
        state.set_wallet_balance(20, -2_000).unwrap();
        state
            .database
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO referral_commissions
                    (order_id, referrer_user_id, invited_user_id, amount_toman)
                 VALUES (1, '10', '20', 5_000)",
                [],
            )
            .unwrap();

        assert_eq!(state.wallet_statistics().unwrap(), (10_000, 5_000));
    }

    #[test]
    fn financial_statistics_count_only_approved_orders_and_average_referral_revenue() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        state.record_bot_user(10).unwrap();
        for user_id in [20, 30, 40] {
            state
                .record_bot_user_with_referrer(user_id, Some(10))
                .unwrap();
        }
        state.record_bot_user(50).unwrap();

        let approve_order = |user_id, price_toman, panel_username: &str| {
            let order_id = state
                .create_payment_order(NewPaymentOrder {
                    telegram_user_id: user_id,
                    account_name: "Test User",
                    telegram_username: None,
                    panel_username,
                    plan_gb: 10,
                    price_toman,
                    is_renewal: false,
                })
                .unwrap();
            state
                .submit_order_receipt(order_id, user_id, "receipt")
                .unwrap();
            state.claim_payment_order(order_id).unwrap().unwrap();
            state
                .complete_payment_order(order_id, user_id, panel_username)
                .unwrap();
        };
        approve_order(20, 20_000, "haku-200001");
        approve_order(20, 40_000, "haku-200002");
        approve_order(30, 60_000, "haku-300001");
        approve_order(50, 70_000, "haku-500001");

        state
            .create_payment_order(NewPaymentOrder {
                telegram_user_id: 40,
                account_name: "Test User",
                telegram_username: None,
                panel_username: "haku-400001",
                plan_gb: 10,
                price_toman: 500_000,
                is_renewal: false,
            })
            .unwrap();
        state.set_wallet_balance(50, 5_000).unwrap();

        assert_eq!(
            state.financial_statistics().unwrap(),
            FinancialStatistics {
                total_sales_toman: 190_000,
                total_commissions_toman: 18_000,
                referred_users: 3,
                referral_orders: 3,
                average_referral_revenue_toman: 60_000,
                total_wallet_balance_toman: 23_000,
            }
        );
    }

    #[test]
    fn bulk_wallet_credit_and_debit_records_each_users_reason() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        for user_id in [10, 20, 30, 40] {
            state.record_bot_user(user_id).unwrap();
        }
        state.set_wallet_balance(10, 10_000).unwrap();
        state.set_wallet_balance(20, 3_000).unwrap();

        let credit = state
            .adjust_all_wallets(WalletBulkAction::Increase, 5_000, "هدیه مناسبتی")
            .unwrap();
        assert_eq!(
            credit,
            WalletBulkAdjustmentSummary {
                targeted_users: 4,
                changed_users: 4,
                unchanged_users: 0,
                total_adjusted_toman: 20_000,
            }
        );
        assert_eq!(state.wallet_summary(30).unwrap(), (5_000, 0));

        state.record_bot_user(50).unwrap();
        let debit = state
            .adjust_all_wallets(WalletBulkAction::Decrease, 6_000, "اصلاح هدیه")
            .unwrap();
        assert_eq!(
            debit,
            WalletBulkAdjustmentSummary {
                targeted_users: 5,
                changed_users: 4,
                unchanged_users: 1,
                total_adjusted_toman: 22_000,
            }
        );
        assert_eq!(state.wallet_summary(10).unwrap(), (9_000, 0));
        assert_eq!(state.wallet_summary(20).unwrap(), (2_000, 0));
        assert_eq!(state.wallet_summary(30).unwrap(), (0, 0));
        assert_eq!(state.wallet_summary(50).unwrap(), (0, 0));

        let reasons: i64 = state
            .database
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM wallet_transactions
                 WHERE description IN ('هدیه مناسبتی', 'اصلاح هدیه')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reasons, 8);
    }

    #[test]
    fn bulk_wallet_debit_preserves_pending_order_reservations() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let tariff_id = state
            .active_subscription_tariffs()
            .unwrap()
            .into_iter()
            .find(|tariff| tariff.volume_gb == 10)
            .unwrap()
            .id;
        state.set_wallet_balance(42, 100_000).unwrap();
        let order = state
            .create_tariff_payment_order(NewTariffPaymentOrder {
                telegram_user_id: 42,
                account_name: "Test User",
                telegram_username: None,
                panel_username: "haku-420001",
                tariff_id,
                use_wallet: true,
                is_renewal: false,
            })
            .unwrap()
            .unwrap();

        let summary = state
            .adjust_all_wallets(WalletBulkAction::Decrease, 100_000, "کاهش گروهی")
            .unwrap();

        assert_eq!(state.wallet_summary(42).unwrap(), (60_000, 0));
        assert_eq!(summary.total_adjusted_toman, 40_000);
        assert!(state.claim_payment_order(order.id).unwrap().is_some());
    }

    #[test]
    fn owner_cannot_reduce_wallet_below_pending_order_reservations() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let tariff_id = state
            .active_subscription_tariffs()
            .unwrap()
            .into_iter()
            .find(|tariff| tariff.volume_gb == 10)
            .unwrap()
            .id;
        state.set_wallet_balance(42, 100_000).unwrap();
        let order = state
            .create_tariff_payment_order(NewTariffPaymentOrder {
                telegram_user_id: 42,
                account_name: "Test User",
                telegram_username: None,
                panel_username: "haku-420001",
                tariff_id,
                use_wallet: true,
                is_renewal: false,
            })
            .unwrap()
            .unwrap();

        assert!(state.set_wallet_balance(42, 59_999).is_err());
        assert_eq!(state.wallet_summary(42).unwrap(), (100_000, 0));
        assert_eq!(state.set_wallet_balance(42, 60_000).unwrap(), 100_000);
        assert_eq!(state.wallet_summary(42).unwrap(), (60_000, 0));
        assert!(state.claim_payment_order(order.id).unwrap().is_some());
        state
            .complete_payment_order(order.id, 42, "haku-420001")
            .unwrap();
        assert_eq!(state.wallet_summary(42).unwrap(), (0, 0));
    }

    #[test]
    fn panel_username_mappings_allow_multiple_clients_per_user() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        assert!(state
            .reserved_panel_usernames(123_456_789)
            .unwrap()
            .is_empty());

        for (panel_username, order_id) in
            ["haku-123456", "haku-123789"]
                .into_iter()
                .map(|panel_username| {
                    let order_id = state
                        .create_payment_order(NewPaymentOrder {
                            telegram_user_id: 123_456_789,
                            account_name: "Test User",
                            telegram_username: None,
                            panel_username,
                            plan_gb: 10,
                            price_toman: 40_000,
                            is_renewal: false,
                        })
                        .unwrap();
                    assert!(state
                        .reserved_panel_usernames(123_456_789)
                        .unwrap()
                        .contains(&panel_username.to_owned()));
                    (panel_username, order_id)
                })
        {
            state
                .submit_order_receipt(order_id, 123_456_789, "photo-id")
                .unwrap();
            state.claim_payment_order(order_id).unwrap().unwrap();
            state
                .complete_payment_order(order_id, 123_456_789, panel_username)
                .unwrap();
        }
        assert_eq!(
            state.reserved_panel_usernames(123_456_789).unwrap(),
            vec!["haku-123456", "haku-123789"]
        );
        assert_eq!(state.broadcast_targets(true).unwrap(), vec![123_456_789]);
    }

    #[test]
    fn migrates_the_legacy_single_username_mapping() {
        let database = Connection::open_in_memory().unwrap();
        database
            .execute_batch(
                "CREATE TABLE panel_users (
                    telegram_user_id TEXT PRIMARY KEY NOT NULL,
                    panel_username TEXT NOT NULL UNIQUE,
                    updated_at TEXT NOT NULL
                 );
                 INSERT INTO panel_users VALUES ('42', 'haku-420001', '2026-01-01T00:00:00+03:30');",
            )
            .unwrap();

        migrate_panel_user_mapping(&database).unwrap();
        assert_eq!(
            database
                .query_row(
                    "SELECT telegram_user_id || ':' || panel_username FROM panel_users",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "42:haku-420001"
        );
        database
            .execute(
                "INSERT INTO panel_users (telegram_user_id, panel_username) VALUES ('42', 'haku-420002')",
                [],
            )
            .unwrap();
    }

    #[test]
    fn admin_ids_persist_and_owner_cannot_be_added_or_removed() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();

        assert!(!state.add_admin(1).unwrap());
        assert!(state.add_admin(42).unwrap());
        assert!(!state.add_admin(42).unwrap());
        assert!(state.is_admin(42).unwrap());
        assert_eq!(state.admins().unwrap(), vec![42]);
        assert!(state.remove_admin(42).unwrap());
        assert!(!state.is_admin(42).unwrap());
    }

    #[test]
    fn broadcast_targets_include_started_users_and_subscribers() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        state.record_bot_user(10).unwrap();
        state.record_bot_user(20).unwrap();
        let order_id = state
            .create_payment_order(NewPaymentOrder {
                telegram_user_id: 20,
                account_name: "Test User",
                telegram_username: None,
                panel_username: "haku-200001",
                plan_gb: 10,
                price_toman: 40_000,
                is_renewal: false,
            })
            .unwrap();
        state
            .submit_order_receipt(order_id, 20, "photo-id")
            .unwrap();
        state.claim_payment_order(order_id).unwrap().unwrap();
        state
            .complete_payment_order(order_id, 20, "haku-200001")
            .unwrap();

        assert_eq!(state.broadcast_targets(false).unwrap(), vec![10, 20]);
        assert_eq!(state.broadcast_targets(true).unwrap(), vec![20]);
    }

    #[test]
    fn payment_orders_accept_one_receipt_and_claim_review_once() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let order_id = state
            .create_payment_order(NewPaymentOrder {
                telegram_user_id: 42,
                account_name: "Test User",
                telegram_username: Some("test"),
                panel_username: "haku-420001",
                plan_gb: 20,
                price_toman: 70_000,
                is_renewal: false,
            })
            .unwrap();
        assert_eq!(state.awaiting_receipt_order(42).unwrap(), Some(order_id));
        assert!(state
            .submit_order_receipt(order_id, 42, "photo-id")
            .unwrap());
        assert!(!state
            .submit_order_receipt(order_id, 42, "photo-id-2")
            .unwrap());

        let order = state.claim_payment_order(order_id).unwrap().unwrap();
        assert_eq!(order.plan_gb, 20);
        assert_eq!(order.receipt_file_id.as_deref(), Some("photo-id"));
        assert!(state.claim_payment_order(order_id).unwrap().is_none());
    }

    #[test]
    fn new_orders_are_blocked_while_existing_order_waits_for_admin() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let tariff_id = state
            .active_subscription_tariffs()
            .unwrap()
            .into_iter()
            .find(|tariff| tariff.volume_gb == 10)
            .unwrap()
            .id;
        let new_order = || NewTariffPaymentOrder {
            telegram_user_id: 42,
            account_name: "Test User",
            telegram_username: Some("test"),
            panel_username: "Falcon🦅-420001",
            tariff_id,
            use_wallet: false,
            is_renewal: false,
        };
        let order = state
            .create_tariff_payment_order(new_order())
            .unwrap()
            .unwrap();
        assert!(state.submit_order_receipt(order.id, 42, "receipt").unwrap());

        assert!(matches!(
            state.create_tariff_payment_order(new_order()),
            Err(CreateTariffPaymentOrderError::OrderAwaitingAdminReview)
        ));
        let order_count: i64 = state
            .database
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM payment_orders WHERE telegram_user_id = '42'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(order_count, 1);

        state.claim_payment_order(order.id).unwrap().unwrap();
        assert!(matches!(
            state.create_tariff_payment_order(new_order()),
            Err(CreateTariffPaymentOrderError::OrderAwaitingAdminReview)
        ));
    }

    #[test]
    fn payment_review_messages_are_tracked_for_each_reviewer() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let order_id = state
            .create_payment_order(NewPaymentOrder {
                telegram_user_id: 42,
                account_name: "Test User",
                telegram_username: None,
                panel_username: "haku-420001",
                plan_gb: 10,
                price_toman: 40_000,
                is_renewal: false,
            })
            .unwrap();

        state
            .record_payment_review_message(order_id, 1, 101, "pending caption", false)
            .unwrap();
        state
            .record_payment_review_message(order_id, 2, 202, "pending caption", false)
            .unwrap();

        let messages = state.payment_review_messages(order_id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].reviewer_user_id, 1);
        assert_eq!(messages[0].message_id, 101);
        assert_eq!(messages[0].caption, "pending caption");
        assert!(!messages[0].is_text);
        assert_eq!(messages[1].reviewer_user_id, 2);
        assert_eq!(messages[1].message_id, 202);
        assert!(!messages[1].is_text);

        let other_order_id = state
            .create_payment_order(NewPaymentOrder {
                telegram_user_id: 43,
                account_name: "Another User",
                telegram_username: None,
                panel_username: "haku-430001",
                plan_gb: 10,
                price_toman: 40_000,
                is_renewal: false,
            })
            .unwrap();
        state
            .record_payment_review_message(other_order_id, 1, 303, "another order", true)
            .unwrap();

        state.delete_payment_review_messages(order_id).unwrap();
        assert!(state.payment_review_messages(order_id).unwrap().is_empty());
        assert_eq!(
            state.payment_review_messages(other_order_id).unwrap().len(),
            1
        );
    }

    #[test]
    fn payment_details_persist_for_owner_configuration() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        assert_eq!(state.payment_details().unwrap(), None);

        state
            .save_payment_details("6037997512345678", "Test Account Owner")
            .unwrap();

        assert_eq!(
            state.payment_details().unwrap(),
            Some(("6037997512345678".into(), "Test Account Owner".into()))
        );
    }

    #[test]
    fn support_username_defaults_and_persists_owner_changes() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();

        assert_eq!(state.support_username(), "ipublicsupport");
        state.set_support_username("falcon_help").unwrap();
        assert_eq!(state.support_username(), "falcon_help");
        let stored_username: String = state
            .database
            .lock()
            .unwrap()
            .query_row(
                "SELECT support_username FROM support_settings WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_username, "falcon_help");
    }

    #[test]
    fn required_channel_settings_default_to_legacy_channel_and_support_management() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();

        assert!(state.required_channels_enabled().unwrap());
        assert_eq!(
            state.required_channels().unwrap(),
            vec![RequiredChannel {
                username: "hakungvpn".into()
            }]
        );
        assert!(state.add_required_channel("@Falcon_News").unwrap());
        assert!(!state.add_required_channel("falcon_news").unwrap());
        assert_eq!(
            state.required_channels().unwrap(),
            vec![
                RequiredChannel {
                    username: "falcon_news".into()
                },
                RequiredChannel {
                    username: "hakungvpn".into()
                }
            ]
        );

        state.set_required_channels_enabled(false).unwrap();
        assert!(!state.required_channels_enabled().unwrap());
        assert!(state.remove_required_channel("@FALCON_NEWS").unwrap());
        assert!(!state.remove_required_channel("falcon_news").unwrap());
        assert_eq!(
            state.required_channels().unwrap(),
            vec![RequiredChannel {
                username: "hakungvpn".into()
            }]
        );
    }

    #[test]
    fn legacy_channel_migration_does_not_restore_removed_channels() {
        let database = Connection::open_in_memory().unwrap();
        migrate_required_channels(&database).unwrap();
        database
            .execute("DELETE FROM required_channels", [])
            .unwrap();

        migrate_required_channels(&database).unwrap();

        let channel_count: usize = database
            .query_row("SELECT COUNT(*) FROM required_channels", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(channel_count, 0);
    }

    #[test]
    fn restoring_legacy_backup_migrates_required_channels() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let backup_path = temporary_backup_test_path();
        let rollback_path = temporary_backup_test_path();
        state.create_database_backup(&backup_path).unwrap();
        Connection::open(&backup_path)
            .unwrap()
            .execute_batch(
                "DROP TABLE required_channels;
                 DROP TABLE required_channel_settings;",
            )
            .unwrap();

        state
            .restore_database_backup(&backup_path, &rollback_path)
            .unwrap();

        assert!(state.required_channels_enabled().unwrap());
        assert_eq!(
            state.required_channels().unwrap(),
            vec![RequiredChannel {
                username: "hakungvpn".into()
            }]
        );
        fs::remove_file(backup_path).unwrap();
        fs::remove_file(rollback_path).unwrap();
    }

    #[test]
    fn restoring_legacy_backup_migrates_support_username_setting() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        state.set_support_username("backup_support").unwrap();
        let backup_path = temporary_backup_test_path();
        let rollback_path = temporary_backup_test_path();
        state.create_database_backup(&backup_path).unwrap();
        Connection::open(&backup_path)
            .unwrap()
            .execute("DROP TABLE support_settings", [])
            .unwrap();

        state.set_support_username("current_support").unwrap();
        state
            .restore_database_backup(&backup_path, &rollback_path)
            .unwrap();

        assert_eq!(state.support_username(), "ipublicsupport");
        fs::remove_file(backup_path).unwrap();
        fs::remove_file(rollback_path).unwrap();
    }

    #[test]
    fn purchase_and_renewal_availability_are_independent_and_persisted() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();

        assert_eq!(state.sales_status().unwrap(), (true, true));
        state.set_purchases_enabled(false).unwrap();
        assert_eq!(state.sales_status().unwrap(), (false, true));
        state.set_renewals_enabled(false).unwrap();
        assert_eq!(state.sales_status().unwrap(), (false, false));
        state.set_purchases_enabled(true).unwrap();
        assert_eq!(state.sales_status().unwrap(), (true, false));
        state.set_renewals_enabled(true).unwrap();
        assert_eq!(state.sales_status().unwrap(), (true, true));
    }

    #[test]
    fn legacy_sales_setting_is_migrated_to_both_features() {
        let database = Connection::open_in_memory().unwrap();
        database
            .execute_batch(
                "CREATE TABLE commerce_settings (
                    id INTEGER PRIMARY KEY,
                    purchases_enabled INTEGER NOT NULL DEFAULT 1
                );
                INSERT INTO commerce_settings (id, purchases_enabled) VALUES (1, 0);",
            )
            .unwrap();

        migrate_commerce_settings(&database).unwrap();

        let status = database
            .query_row(
                "SELECT purchases_enabled, renewals_enabled
                 FROM commerce_settings WHERE id = 1",
                [],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
            )
            .unwrap();
        assert_eq!(status, (false, false));
    }

    #[test]
    fn rejected_payment_order_cannot_be_approved() {
        let state = AppState::new(UserId(1), ":memory:").unwrap();
        let order_id = state
            .create_payment_order(NewPaymentOrder {
                telegram_user_id: 42,
                account_name: "Test User",
                telegram_username: None,
                panel_username: "haku-420001",
                plan_gb: 10,
                price_toman: 40_000,
                is_renewal: false,
            })
            .unwrap();
        state
            .submit_order_receipt(order_id, 42, "photo-id")
            .unwrap();

        assert_eq!(state.reject_payment_order(order_id).unwrap(), Some(42));
        assert!(state.reject_payment_order(order_id).unwrap().is_none());
        assert!(state.claim_payment_order(order_id).unwrap().is_none());
    }
}
