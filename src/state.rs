use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::{collections::HashMap, fs, path::Path, sync::Mutex};
use teloxide::types::UserId;
use url::Url;
use zeroize::Zeroizing;

pub struct AppState {
    pub owner_id: UserId,
    database: Mutex<Connection>,
    setup: Mutex<HashMap<UserId, SetupState>>,
    session: Mutex<Option<PanelSession>>,
}

#[derive(Clone)]
pub enum SetupState {
    AdminId,
    BroadcastAll,
    BroadcastSubscribers,
    PaymentCardNumber,
    PaymentCardholder { card_number: String },
    PanelUrl,
    Username { base_url: Url },
    Password { base_url: Url, username: String },
}

pub struct PaymentOrder {
    pub id: i64,
    pub telegram_user_id: u64,
    pub account_name: String,
    pub telegram_username: Option<String>,
    pub panel_username: String,
    pub plan_gb: u16,
    pub price_toman: u32,
    pub is_renewal: bool,
    pub receipt_file_id: Option<String>,
}

pub struct PaymentReviewMessage {
    pub reviewer_user_id: u64,
    pub message_id: i32,
    pub caption: String,
}

pub struct NewPaymentOrder<'a> {
    pub telegram_user_id: u64,
    pub account_name: &'a str,
    pub telegram_username: Option<&'a str>,
    pub panel_username: &'a str,
    pub plan_gb: u16,
    pub price_toman: u32,
    pub is_renewal: bool,
}

struct PanelSession {
    access_token: Zeroizing<String>,
}

impl AppState {
    pub fn new(owner_id: UserId, database_path: &str) -> Result<Self> {
        let path = Path::new(database_path);
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
                first_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')),
                last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30'))
            );
            CREATE TABLE IF NOT EXISTS payment_orders (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                telegram_user_id TEXT NOT NULL,
                account_name TEXT NOT NULL,
                telegram_username TEXT,
                panel_username TEXT NOT NULL,
                plan_gb INTEGER NOT NULL,
                price_toman INTEGER NOT NULL,
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
                PRIMARY KEY (order_id, reviewer_user_id, message_id)
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
            );",
        )?;
        migrate_commerce_settings(&database)?;
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
            setup: Mutex::new(HashMap::new()),
            session: Mutex::new(None),
        })
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

    pub fn record_bot_user(&self, telegram_user_id: u64) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "INSERT INTO bot_users (telegram_user_id) VALUES (?1)
                 ON CONFLICT(telegram_user_id) DO UPDATE SET
                    last_seen_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
                [telegram_user_id.to_string()],
            )?;
        Ok(())
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
                plan_gb, price_toman, is_renewal, status
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'awaiting_receipt')",
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
                        plan_gb, price_toman, is_renewal, receipt_file_id
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
    ) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "INSERT OR IGNORE INTO payment_review_messages
                    (order_id, reviewer_user_id, message_id, caption)
                 VALUES (?1, ?2, ?3, ?4)",
                params![order_id, reviewer_user_id.to_string(), message_id, caption],
            )?;
        Ok(())
    }

    pub fn payment_review_messages(
        &self,
        order_id: i64,
    ) -> rusqlite::Result<Vec<PaymentReviewMessage>> {
        let database = self.database.lock().expect("database lock poisoned");
        let mut statement = database.prepare(
            "SELECT reviewer_user_id, message_id, caption
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
                })
            })?
            .collect();
        messages
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
                    plan_gb, price_toman, is_renewal, receipt_file_id
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
    ) -> rusqlite::Result<()> {
        let mut database = self.database.lock().expect("database lock poisoned");
        let transaction = database.transaction()?;
        transaction.execute(
            "INSERT INTO panel_users (telegram_user_id, panel_username)
             VALUES (?1, ?2)
             ON CONFLICT(telegram_user_id, panel_username) DO UPDATE SET
                updated_at = strftime('%Y-%m-%dT%H:%M:%f+03:30', 'now', '+03:30')",
            params![telegram_user_id.to_string(), panel_username],
        )?;
        let changed = transaction.execute(
            "UPDATE payment_orders SET status = 'approved'
             WHERE id = ?1 AND status = 'approving'",
            [order_id],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn cancel_awaiting_order(&self, telegram_user_id: u64) -> rusqlite::Result<()> {
        self.database
            .lock()
            .expect("database lock poisoned")
            .execute(
                "UPDATE payment_orders SET status = 'cancelled'
                 WHERE telegram_user_id = ?1 AND status = 'awaiting_receipt'",
                [telegram_user_id.to_string()],
            )?;
        Ok(())
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

fn payment_order_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PaymentOrder> {
    let telegram_user_id: String = row.get(1)?;
    let plan_gb: i64 = row.get(5)?;
    let price_toman: i64 = row.get(6)?;
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
        is_renewal: row.get(7)?,
        receipt_file_id: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .record_payment_review_message(order_id, 1, 101, "pending caption")
            .unwrap();
        state
            .record_payment_review_message(order_id, 2, 202, "pending caption")
            .unwrap();

        let messages = state.payment_review_messages(order_id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].reviewer_user_id, 1);
        assert_eq!(messages[0].message_id, 101);
        assert_eq!(messages[0].caption, "pending caption");
        assert_eq!(messages[1].reviewer_user_id, 2);
        assert_eq!(messages[1].message_id, 202);
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
