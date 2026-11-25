mod panel;
mod state;

use anyhow::{anyhow, Context, Result};
use chrono::{Datelike, TimeZone, Timelike, Utc};
use image::{DynamicImage, ImageFormat, Luma};
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use state::{
    AppState, BotUserDetails, BotUserSearchResult, CreateTariffPaymentOrderError, DiskSpaceUsage,
    FinancialStatistics, NewTariffPaymentOrder, PaymentOrder, RequiredChannel, SetupState,
    SubscriptionTariff, WalletBulkAction, WalletBulkAdjustmentSummary, MAX_REQUIRED_CHANNELS,
};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Cursor, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use teloxide::{
    dispatching::UpdateFilterExt,
    dptree,
    net::Download,
    prelude::*,
    types::{
        CopyTextButton, FileId, InlineKeyboardButton, InlineKeyboardButtonKind,
        InlineKeyboardMarkup, InputFile, MessageId, ParseMode, Update, User,
    },
};
use tokio::io::AsyncWriteExt;
use url::Url;
use zeroize::Zeroizing;

const USER_MENU_PROMPT: &str = "از منوی زیر انتخاب کن 👇🏻";
const MAX_DATABASE_BACKUP_SIZE: u64 = 20 * 1024 * 1024;
const PANEL_RECONNECT_ATTEMPTS: usize = 3;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

macro_rules! retry_panel_request {
    ($state:expr, $base_url:ident, $access_token:ident, $operation:expr, $request:expr) => {{
        let mut result = ($request).await;
        for attempt in 1..=PANEL_RECONNECT_ATTEMPTS {
            let should_reconnect = result
                .as_ref()
                .err()
                .is_some_and(should_reconnect_after_panel_error);
            if !should_reconnect {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
            match connect_panel_once($state).await {
                Ok((connected_url, token)) => {
                    $base_url = connected_url;
                    $access_token = token;
                    result = ($request).await;
                    if result.is_ok() {
                        log::info!("Panel connection recovered during {}", $operation);
                        break;
                    }
                }
                Err(error) => {
                    log::warn!(
                        "Panel reconnection attempt {attempt}/{PANEL_RECONNECT_ATTEMPTS} failed during {}: {error}",
                        $operation
                    );
                    if !is_transient_panel_error(&error) {
                        break;
                    }
                }
            }
        }
        result
    }};
}
fn start_user_menu_prompt(first_name: &str) -> String {
    let name = first_name.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = if name.is_empty() { "دوست" } else { &name };
    format!(
        "🌐 سلام {name} عزیز!\n\n⚡️ سرعت بالا، بدون قطعی\n🚗 ترافیک نیم بها\n💎 کانفیگ بدون تبلیغ\n👨🏼‍💻 پشتیبانی تا آخرین روز سرویس\n💸 ضمانت بازگشت وجه\n\nحالا انتخاب کن🔻"
    )
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UserMenuBlock {
    Paragraph { text: UserMenuRichText },
    BlockQuotation { blocks: Vec<UserMenuBlock> },
    Buttons { buttons: Vec<UserMenuButton> },
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
enum UserMenuRichText {
    Plain(String),
    Entity(UserMenuRichTextEntity),
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UserMenuRichTextEntity {
    Bold { text: Box<UserMenuRichText> },
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct UserMenuButton {
    text: String,
    style: UserMenuButtonStyle,
    #[serde(skip_serializing_if = "Option::is_none")]
    callback_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    copy_text: Option<CopyTextButton>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum UserMenuButtonStyle {
    Primary,
    Danger,
}

#[derive(Serialize)]
struct UserMenuRichMessage {
    blocks: Vec<UserMenuBlock>,
    is_rtl: bool,
}

#[derive(Serialize)]
struct SendRichMessageRequest {
    chat_id: i64,
    rich_message: UserMenuRichMessage,
}

#[derive(Serialize)]
struct EditRichMessageRequest {
    chat_id: i64,
    message_id: i32,
    rich_message: UserMenuRichMessage,
}

#[derive(Serialize)]
struct EditTextMessageRequest {
    chat_id: i64,
    message_id: i32,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<InlineKeyboardMarkup>,
}

#[derive(Serialize)]
struct EditInputMediaPhoto<'a> {
    #[serde(rename = "type")]
    media_type: &'static str,
    media: &'static str,
    caption: &'a str,
    parse_mode: &'static str,
}

#[derive(Deserialize)]
struct TelegramApiResponse {
    ok: bool,
    description: Option<String>,
}

#[derive(Clone, Copy)]
struct CallbackMessageTarget {
    chat_id: ChatId,
    message_id: MessageId,
}

fn callback_message_target(query: &CallbackQuery) -> Option<CallbackMessageTarget> {
    query
        .regular_message()
        .map(|message| CallbackMessageTarget {
            chat_id: message.chat.id,
            message_id: message.id,
        })
}

fn require_callback_message_target(
    target: Option<CallbackMessageTarget>,
) -> Result<CallbackMessageTarget> {
    target.ok_or_else(|| anyhow!("Callback query does not contain an editable message"))
}

fn user_menu_rich_message(prompt: &str) -> UserMenuRichMessage {
    let button = |text: &str, callback_data: &str, style| UserMenuButton {
        text: text.to_owned(),
        callback_data: Some(callback_data.to_owned()),
        url: None,
        copy_text: None,
        style,
    };

    UserMenuRichMessage {
        blocks: vec![
            UserMenuBlock::Paragraph {
                text: UserMenuRichText::Plain(prompt.to_owned()),
            },
            UserMenuBlock::Buttons {
                buttons: vec![button(
                    "🚀 خرید و تمدید اشتراک 💰",
                    "user:purchase-menu",
                    UserMenuButtonStyle::Primary,
                )],
            },
            UserMenuBlock::Buttons {
                buttons: vec![button(
                    "⚡️ مدیریت اشتراک ⚙️",
                    "user:management-menu",
                    UserMenuButtonStyle::Primary,
                )],
            },
            UserMenuBlock::Buttons {
                buttons: vec![button(
                    "🎁 زیر مجموعه گیری ‼️",
                    "user:referral",
                    UserMenuButtonStyle::Primary,
                )],
            },
            UserMenuBlock::Buttons {
                buttons: vec![button(
                    "〽️ کیف پول من 💵",
                    "user:wallet",
                    UserMenuButtonStyle::Primary,
                )],
            },
            UserMenuBlock::Buttons {
                buttons: vec![button(
                    "👨🏻‍💻 پشتیبانی",
                    "user:support",
                    UserMenuButtonStyle::Danger,
                )],
            },
        ],
        is_rtl: true,
    }
}

fn text_message_rich_message(
    text: &str,
    keyboard: &InlineKeyboardMarkup,
) -> Result<UserMenuRichMessage> {
    text_message_rich_message_with_bold_lines(text, keyboard, &[])
}

fn text_message_rich_message_with_bold_lines(
    text: &str,
    keyboard: &InlineKeyboardMarkup,
    bold_line_prefixes: &[&str],
) -> Result<UserMenuRichMessage> {
    let mut blocks = if bold_line_prefixes.is_empty() {
        vec![UserMenuBlock::Paragraph {
            text: UserMenuRichText::Plain(text.to_owned()),
        }]
    } else {
        text.split('\n')
            .map(|line| UserMenuBlock::Paragraph {
                text: if bold_line_prefixes
                    .iter()
                    .any(|prefix| line.starts_with(prefix))
                {
                    UserMenuRichText::Entity(UserMenuRichTextEntity::Bold {
                        text: Box::new(UserMenuRichText::Plain(line.to_owned())),
                    })
                } else {
                    UserMenuRichText::Plain(line.to_owned())
                },
            })
            .collect()
    };
    for button in keyboard.inline_keyboard.iter().flatten() {
        let (callback_data, url, copy_text) = match &button.kind {
            InlineKeyboardButtonKind::CallbackData(data) => (Some(data.clone()), None, None),
            InlineKeyboardButtonKind::Url(url) => (None, Some(url.to_string()), None),
            InlineKeyboardButtonKind::CopyText(copy_text) => (None, None, Some(copy_text.clone())),
            _ => {
                return Err(anyhow!(
                    "Unsupported inline keyboard button in user rich message"
                ))
            }
        };
        let is_back_button = button.text.contains("بازگشت")
            || matches!(
                callback_data.as_deref(),
                Some("user:main" | "user:menu" | "owner:main")
            );
        blocks.push(UserMenuBlock::Buttons {
            buttons: vec![UserMenuButton {
                text: button.text.clone(),
                style: if is_back_button || button.text.contains("پشتیبانی") {
                    UserMenuButtonStyle::Danger
                } else {
                    UserMenuButtonStyle::Primary
                },
                callback_data,
                url,
                copy_text,
            }],
        });
    }

    Ok(UserMenuRichMessage {
        blocks,
        is_rtl: true,
    })
}

async fn send_rich_message(
    bot: &Bot,
    chat_id: ChatId,
    rich_message: UserMenuRichMessage,
) -> Result<()> {
    let request = SendRichMessageRequest {
        chat_id: chat_id.0,
        rich_message,
    };
    call_telegram_api(bot, "sendRichMessage", &request).await
}

async fn edit_rich_message(
    bot: &Bot,
    target: CallbackMessageTarget,
    rich_message: UserMenuRichMessage,
) -> Result<()> {
    let request = EditRichMessageRequest {
        chat_id: target.chat_id.0,
        message_id: target.message_id.0,
        rich_message,
    };
    call_telegram_api(bot, "editMessageText", &request).await
}

async fn edit_text_message(
    bot: &Bot,
    target: CallbackMessageTarget,
    text: impl Into<String>,
    parse_mode: Option<&'static str>,
    reply_markup: Option<InlineKeyboardMarkup>,
) -> Result<()> {
    let request = EditTextMessageRequest {
        chat_id: target.chat_id.0,
        message_id: target.message_id.0,
        text: text.into(),
        parse_mode,
        reply_markup,
    };
    call_telegram_api(bot, "editMessageText", &request).await
}

async fn edit_photo_message(
    bot: &Bot,
    target: CallbackMessageTarget,
    image: Vec<u8>,
    caption: &str,
    keyboard: &InlineKeyboardMarkup,
) -> Result<()> {
    let media = EditInputMediaPhoto {
        media_type: "photo",
        media: "attach://photo",
        caption,
        parse_mode: "HTML",
    };
    let form = reqwest::multipart::Form::new()
        .text("chat_id", target.chat_id.0.to_string())
        .text("message_id", target.message_id.0.to_string())
        .text("media", serde_json::to_string(&media)?)
        .text("reply_markup", serde_json::to_string(keyboard)?)
        .part(
            "photo",
            reqwest::multipart::Part::bytes(image)
                .file_name("subscription-qr.png")
                .mime_str("image/png")?,
        );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("Could not create Telegram media-edit client")?;
    let response = client
        .post(format!(
            "https://api.telegram.org/bot{}/editMessageMedia",
            bot.token()
        ))
        .multipart(form)
        .send()
        .await
        .map_err(|error| {
            anyhow!(
                "Telegram editMessageMedia request failed: {}",
                error.without_url()
            )
        })?;
    check_telegram_api_response(response, "editMessageMedia").await
}

async fn call_telegram_api<T: Serialize>(bot: &Bot, method: &str, request: &T) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("Could not create Telegram rich-message client")?;
    let response = client
        .post(format!(
            "https://api.telegram.org/bot{}/{method}",
            bot.token(),
        ))
        .json(request)
        .send()
        .await
        .map_err(|error| anyhow!("Telegram {method} request failed: {}", error.without_url()))?;
    check_telegram_api_response(response, method).await
}

async fn check_telegram_api_response(response: reqwest::Response, method: &str) -> Result<()> {
    let response = response
        .json::<TelegramApiResponse>()
        .await
        .map_err(|error| {
            anyhow!(
                "Could not read Telegram {method} response: {}",
                error.without_url()
            )
        })?;
    if !response.ok {
        return Err(anyhow!(
            "Telegram {method} was rejected: {}",
            response
                .description
                .unwrap_or_else(|| "no error description".to_owned())
        ));
    }
    Ok(())
}

async fn edit_user_message_with_keyboard(
    bot: &Bot,
    target: impl Into<Option<CallbackMessageTarget>>,
    text: &str,
    keyboard: InlineKeyboardMarkup,
) -> Result<()> {
    let rich_message = text_message_rich_message(text, &keyboard)?;
    edit_rich_message(
        bot,
        require_callback_message_target(target.into())?,
        rich_message,
    )
    .await
}

async fn edit_user_message_with_bold_lines(
    bot: &Bot,
    target: impl Into<Option<CallbackMessageTarget>>,
    text: &str,
    keyboard: InlineKeyboardMarkup,
    bold_line_prefixes: &[&str],
) -> Result<()> {
    let rich_message =
        text_message_rich_message_with_bold_lines(text, &keyboard, bold_line_prefixes)?;
    edit_rich_message(
        bot,
        require_callback_message_target(target.into())?,
        rich_message,
    )
    .await
}

async fn edit_user_menu(
    bot: &Bot,
    target: Option<CallbackMessageTarget>,
    prompt: &str,
) -> Result<()> {
    edit_rich_message(
        bot,
        require_callback_message_target(target)?,
        user_menu_rich_message(prompt),
    )
    .await
}

async fn send_text_message_with_keyboard(
    bot: &Bot,
    chat_id: impl Into<ChatId>,
    text: impl Into<String>,
    keyboard: InlineKeyboardMarkup,
) -> ResponseResult<()> {
    let chat_id = chat_id.into();
    let text = text.into();
    let rich_result = match text_message_rich_message(&text, &keyboard) {
        Ok(rich_message) => send_rich_message(bot, chat_id, rich_message).await,
        Err(error) => Err(error),
    };
    if let Err(error) = rich_result {
        log::error!("Could not send rich text message to chat {chat_id}: {error:#}");
        bot.send_message(chat_id, text)
            .reply_markup(keyboard)
            .await?;
    }
    Ok(())
}

async fn send_user_message_with_keyboard(
    bot: &Bot,
    chat_id: impl Into<ChatId>,
    text: impl Into<String>,
    keyboard: InlineKeyboardMarkup,
) -> ResponseResult<()> {
    send_text_message_with_keyboard(bot, chat_id, text, keyboard).await
}

async fn send_user_menu(bot: &Bot, chat_id: impl Into<ChatId>, prompt: &str) -> ResponseResult<()> {
    let chat_id = chat_id.into();
    if let Err(error) = send_rich_message(bot, chat_id, user_menu_rich_message(prompt)).await {
        log::error!("Could not send colored user menu to chat {chat_id}: {error:#}");
        bot.send_message(
            chat_id,
            format!("{prompt}\n\n⚠️ منوی رنگی در دسترس نبود؛ از دکمه‌های جایگزین استفاده کنید."),
        )
        .reply_markup(user_keyboard())
        .await?;
    }
    Ok(())
}

struct TerminalFileLogger {
    file: Mutex<fs::File>,
    level: log::LevelFilter,
}

impl log::Log for TerminalFileLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let timestamp = chrono::Utc::now()
            .with_timezone(&chrono_tz::Asia::Tehran)
            .format("%Y-%m-%d %H:%M:%S%.3f %:z");
        let line = format!("[{timestamp}] [{:<5}] {}", record.level(), record.args());

        let mut terminal = io::stdout().lock();
        let _ = writeln!(terminal, "{line}");
        let _ = terminal.flush();

        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }

    fn flush(&self) {}
}

fn init_terminal_file_logger() -> Result<()> {
    let log_path = match env::var("BOT_LOG_PATH") {
        Ok(path) if !path.trim().is_empty() => PathBuf::from(path),
        Ok(_) => return Err(anyhow!("BOT_LOG_PATH must not be empty")),
        Err(env::VarError::NotPresent) => PathBuf::from("logs/foxngbot.log"),
        Err(error) => return Err(anyhow!("Could not read BOT_LOG_PATH: {error}")),
    };
    if let Some(parent) = log_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("Could not create log directory {}", parent.display()))?;
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Could not open log file {}", log_path.display()))?;

    let level = env::var("RUST_LOG")
        .ok()
        .and_then(|value| log::LevelFilter::from_str(value.trim()).ok())
        .unwrap_or(log::LevelFilter::Info);

    log::set_boxed_logger(Box::new(TerminalFileLogger {
        file: Mutex::new(file),
        level,
    }))
    .map_err(|_| anyhow!("Could not initialize logger"))?;
    log::set_max_level(level);

    log::info!("Terminal and file logging started: {}", log_path.display());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    if Path::new(".env").exists() {
        dotenvy::from_path(".env").context("Could not read .env")?;
    }
    init_terminal_file_logger()?;

    let token = required_env("BOT_TOKEN")?;
    if token.is_empty() {
        return Err(anyhow!("BOT_TOKEN must not be empty"));
    }
    let owner_id = required_env("OWNER_ID")?
        .parse::<u64>()
        .context("OWNER_ID must be a numeric Telegram user ID")?;
    if owner_id == 0 {
        return Err(anyhow!("OWNER_ID must be greater than zero"));
    }
    let database_path = match env::var("BOT_DB_PATH") {
        Ok(path) if !path.trim().is_empty() => path,
        Ok(_) => return Err(anyhow!("BOT_DB_PATH must not be empty")),
        Err(env::VarError::NotPresent) => "foxngbot.sqlite3".into(),
        Err(error) => return Err(anyhow!("Could not read BOT_DB_PATH: {error}")),
    };
    let state = Arc::new(AppState::new(UserId(owner_id), &database_path)?);
    let bot = Bot::new(token);
    restore_panel_session(&bot, &state).await;

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback_query));

    let bot_info = bot
        .get_me()
        .await
        .context("Could not connect to Telegram. Check network, firewall, and proxy settings")?;
    if let Some(username) = bot_info.username.clone() {
        state.set_bot_username(username);
    } else {
        log::warn!("Telegram did not return a bot username; referral links will be unavailable");
    }

    tokio::spawn(schedule_daily_database_backups(
        bot.clone(),
        Arc::clone(&state),
    ));
    tokio::spawn(schedule_daily_disk_space_checks(
        bot.clone(),
        Arc::clone(&state),
    ));
    log::info!("FOXNG Rust bot started");
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

fn required_env(name: &str) -> Result<String> {
    let value = env::var(name).with_context(|| format!("Set {name} in the environment or .env"))?;
    Ok(value.trim().to_owned())
}

fn duration_until_next_iran_time(
    now: chrono::DateTime<Utc>,
    hour: u32,
    minute: u32,
) -> Result<std::time::Duration> {
    if hour > 23 || minute > 59 {
        return Err(anyhow!("Iran scheduler time is out of range"));
    }
    let iran_now = now.with_timezone(&chrono_tz::Asia::Tehran);
    let target_today = iran_now
        .date_naive()
        .and_hms_opt(hour, minute, 0)
        .context("Could not construct scheduled time in the Iran timezone")?;
    let mut next_run = chrono_tz::Asia::Tehran
        .from_local_datetime(&target_today)
        .single()
        .context("Scheduled time is ambiguous or invalid in the Iran timezone")?;
    if next_run <= iran_now {
        let next_date = iran_now
            .date_naive()
            .succ_opt()
            .context("Could not calculate the next Iran calendar date")?;
        let target_tomorrow = next_date
            .and_hms_opt(hour, minute, 0)
            .context("Could not construct next scheduled time in the Iran timezone")?;
        next_run = chrono_tz::Asia::Tehran
            .from_local_datetime(&target_tomorrow)
            .single()
            .context("Next scheduled time is ambiguous or invalid in the Iran timezone")?;
    }
    next_run
        .with_timezone(&Utc)
        .signed_duration_since(now)
        .to_std()
        .context("Next Iran scheduled time is not in the future")
}

fn duration_until_next_iran_midnight(now: chrono::DateTime<Utc>) -> Result<std::time::Duration> {
    duration_until_next_iran_time(now, 0, 0)
}

async fn schedule_daily_database_backups(bot: Bot, state: Arc<AppState>) {
    loop {
        let delay = match duration_until_next_iran_midnight(Utc::now()) {
            Ok(delay) => delay,
            Err(error) => {
                log::error!("Could not schedule the next daily database backup: {error}");
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }
        };
        tokio::time::sleep(delay).await;

        if let Err(error) = send_database_backup(
            &bot,
            state.owner_id,
            &state,
            "🌙 بکاپ روزانه دیتابیس — ساعت ۰۰:۰۰ به وقت ایران",
        )
        .await
        {
            log::error!("Could not send scheduled database backup: {error}");
            if let Err(notification_error) = bot
                .send_message(
                    state.owner_id,
                    "⚠️ ارسال بکاپ شبانه‌ی دیتابیس انجام نشد؛ لاگ برنامه را بررسی کنید.",
                )
                .await
            {
                log::error!(
                    "Could not notify the owner about the failed database backup: {notification_error}"
                );
            }
        }
    }
}

async fn schedule_daily_disk_space_checks(bot: Bot, state: Arc<AppState>) {
    loop {
        let delay = match duration_until_next_iran_time(Utc::now(), 15, 0) {
            Ok(delay) => delay,
            Err(error) => {
                log::error!("Could not schedule the next daily disk-space check: {error}");
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }
        };
        tokio::time::sleep(delay).await;

        match state.disk_space_usage() {
            Ok(usage) if disk_space_below_alert_threshold(usage) => {
                let free_percent = usage.free_bytes as f64 * 100.0 / usage.total_bytes as f64;
                let message = format!(
                    "⚠️ هشدار کمبود فضای دیسک سرور\n\n🟢 فضای خالی: {}\n💽 ظرفیت کل: {}\n📊 درصد فضای خالی: {}٪\n⛔ آستانه هشدار: کمتر از ۱۵٪",
                    format_disk_space(usage.free_bytes),
                    format_disk_space(usage.total_bytes),
                    persian_digits(format!("{free_percent:.2}"))
                );
                let mut recipients = vec![state.owner_id.0];
                match state.admins() {
                    Ok(admins) => recipients.extend(admins),
                    Err(error) => {
                        log::error!("Could not load admins for disk-space warning: {error}");
                    }
                }
                recipients.sort_unstable();
                recipients.dedup();
                for recipient in recipients {
                    if let Err(error) = bot.send_message(UserId(recipient), message.clone()).await {
                        log::error!(
                            "Could not send low disk-space warning to user {recipient}: {error}"
                        );
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                log::error!("Could not read server disk-space usage: {error}");
            }
        }
    }
}

fn is_start_command(text: &str) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|command| command == "/start" || command.starts_with("/start@"))
}

fn start_referrer_id(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?;
    if command != "/start" && !command.starts_with("/start@") {
        return None;
    }
    parts
        .next()?
        .strip_prefix("ref_")?
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
}

fn parse_subscription_checkout_callback(data: &str) -> Option<(i64, Option<&str>, bool)> {
    if let Some(tariff_id) = data.strip_prefix("user:pc:") {
        return Some((tariff_id.parse().ok()?, None, false));
    }
    if let Some(tariff_id) = data.strip_prefix("user:pw:") {
        return Some((tariff_id.parse().ok()?, None, true));
    }
    let (payload, use_wallet) = if let Some(payload) = data.strip_prefix("user:rc:") {
        (payload, false)
    } else {
        (data.strip_prefix("user:rw:")?, true)
    };
    let (tariff_id, username) = payload.split_once(':')?;
    let tariff_id = tariff_id.parse().ok()?;
    (!username.is_empty()).then_some((tariff_id, Some(username), use_wallet))
}

async fn notify_panel_issue(bot: &Bot, state: &AppState, details: &str) {
    let message = format!(
        "⚠️ هشدار پنل\nربات نتوانست از پنل استفاده کند.\n\nعلت: {details}\n\nلطفاً وضعیت پنل، اتصال شبکه و اطلاعات ورود را بررسی کنید."
    );
    let mut recipients = vec![state.owner_id.0];
    match state.admins() {
        Ok(admins) => recipients.extend(admins),
        Err(error) => log::error!("Could not load admins for panel warning: {error}"),
    }
    recipients.sort_unstable();
    recipients.dedup();
    for recipient in recipients {
        if let Err(error) = bot.send_message(UserId(recipient), message.clone()).await {
            log::error!("Could not send panel warning to user {recipient}: {error}");
        }
    }
}

fn is_transient_panel_error(error: &panel::PanelError) -> bool {
    matches!(
        error,
        panel::PanelError::Request
            | panel::PanelError::InvalidResponse
            | panel::PanelError::HttpStatus(408 | 425 | 429 | 500..=599)
    )
}

fn should_reconnect_after_panel_error(error: &panel::PanelError) -> bool {
    matches!(
        error,
        panel::PanelError::Unauthorized
            | panel::PanelError::HttpStatus(401)
            | panel::PanelError::Request
            | panel::PanelError::InvalidResponse
            | panel::PanelError::HttpStatus(408 | 425 | 429 | 500..=599)
    )
}

async fn connect_panel_once(
    state: &AppState,
) -> std::result::Result<(Url, Zeroizing<String>), panel::PanelError> {
    let panel_config = state.panel_config().map_err(|error| {
        log::error!("Could not read panel configuration while reconnecting: {error}");
        panel::PanelError::Request
    })?;
    let Some((saved_url, username)) = panel_config else {
        return Err(panel::PanelError::Request);
    };
    let password = state.panel_password().map_err(|error| {
        log::error!("Could not load panel password while reconnecting: {error}");
        panel::PanelError::Request
    })?;
    let Some(password) = password else {
        return Err(panel::PanelError::Unauthorized);
    };
    let base_url = panel::normalize_panel_url(&saved_url)?;
    let access_token = panel::authenticate(&base_url, &username, &password).await?;
    state.set_session(Zeroizing::new(access_token.to_string()));
    Ok((base_url, access_token))
}

async fn connect_panel_with_retries(
    state: &AppState,
    operation: &str,
) -> std::result::Result<(Url, Zeroizing<String>), panel::PanelError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match connect_panel_once(state).await {
            Ok(context) => return Ok(context),
            Err(error) => {
                log::warn!(
                    "Panel connection attempt {attempt}/{PANEL_RECONNECT_ATTEMPTS} failed during {operation}: {error}"
                );
                if attempt >= PANEL_RECONNECT_ATTEMPTS || !is_transient_panel_error(&error) {
                    return Err(error);
                }
                tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
            }
        }
    }
}

async fn authenticate_panel_with_retries(
    base_url: &Url,
    username: &str,
    password: &str,
    operation: &str,
) -> std::result::Result<Zeroizing<String>, panel::PanelError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match panel::authenticate(base_url, username, password).await {
            Ok(access_token) => return Ok(access_token),
            Err(error) => {
                log::warn!(
                    "Panel authentication attempt {attempt}/{PANEL_RECONNECT_ATTEMPTS} failed during {operation}: {error}"
                );
                if attempt >= PANEL_RECONNECT_ATTEMPTS || !is_transient_panel_error(&error) {
                    return Err(error);
                }
                tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
            }
        }
    }
}

async fn restore_panel_session(bot: &Bot, state: &AppState) {
    let panel_config = match state.panel_config() {
        Ok(config) => config,
        Err(error) => {
            log::error!("Could not read panel configuration during startup: {error}");
            notify_panel_issue(
                bot,
                state,
                &format!("خواندن تنظیمات پنل هنگام راه‌اندازی ناموفق بود: {error}"),
            )
            .await;
            return;
        }
    };
    if panel_config.is_none() {
        return;
    }

    match connect_panel_with_retries(state, "startup").await {
        Ok(_) => {
            log::info!("Panel session restored automatically");
        }
        Err(error) => {
            log::warn!("Could not restore the panel session automatically: {error}");
            notify_panel_issue(bot, state, &error.user_message()).await;
        }
    }
}

async fn handle_message(bot: Bot, msg: Message, state: Arc<AppState>) -> Result<()> {
    let Some(sender) = msg.from.as_ref() else {
        return Ok(());
    };
    let is_start_message = msg.text().is_some_and(is_start_command);
    if msg.chat.is_private() && !is_start_message {
        if let Err(error) = state.update_bot_user_profile(
            sender.id.0,
            sender.username.as_deref(),
            &sender.first_name,
        ) {
            log::error!(
                "Could not update profile for bot user {}: {error}",
                sender.id
            );
        }
    }
    let is_owner = sender.id == state.owner_id;
    let is_admin = if is_owner {
        false
    } else {
        match state.is_admin(sender.id.0) {
            Ok(is_admin) => is_admin,
            Err(error) => {
                log::error!(
                    "Could not check admin access for user {}: {error}",
                    sender.id
                );
                false
            }
        }
    };
    let Some(text) = msg.text() else {
        if is_owner {
            if let Some(document) = msg.document() {
                match state.take_setup(sender.id) {
                    Some(SetupState::DatabaseRestore) => {
                        if !msg.chat.is_private() {
                            state.set_setup(sender.id, SetupState::DatabaseRestore);
                            bot.send_message(
                                msg.chat.id,
                                "برای حفظ امنیت، فایل بکاپ را فقط در گفت‌وگوی خصوصی با ربات ارسال کنید.",
                            )
                            .await?;
                            return Ok(());
                        }
                        restore_database_backup(&bot, sender.id, &msg, &state, document).await?;
                        return Ok(());
                    }
                    Some(setup) => state.set_setup(sender.id, setup),
                    None => {}
                }
            }
        }
        if !is_owner && !is_admin {
            match state.awaiting_receipt_order(sender.id.0) {
                Ok(Some(order_id)) => {
                    receive_payment_receipt(&bot, &msg, &state, order_id).await?;
                    return Ok(());
                }
                Ok(None) => {}
                Err(error) => {
                    log::error!(
                        "Could not check pending order for user {}: {error}",
                        sender.id
                    );
                    bot.send_message(
                        msg.chat.id,
                        "بررسی سفارش در انتظار رسید با خطا روبه‌رو شد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    )
                    .await?;
                    return Ok(());
                }
            }
        }
        if is_owner && state.has_setup(sender.id) {
            match state.take_setup(sender.id) {
                Some(SetupState::BroadcastAll) => {
                    broadcast_message(&bot, &msg, &state, false).await?;
                }
                Some(SetupState::BroadcastSubscribers) => {
                    broadcast_message(&bot, &msg, &state, true).await?;
                }
                Some(setup) => {
                    state.set_setup(sender.id, setup);
                    bot.send_message(msg.chat.id, "لطفاً پاسخ این مرحله را به‌صورت متن ارسال کنید.")
                        .await?;
                }
                None => {}
            }
        }
        return Ok(());
    };

    if is_start_message {
        if msg.chat.is_private() {
            let referrer_id = start_referrer_id(text);
            match state.record_bot_user_with_referrer(sender.id.0, referrer_id) {
                Ok(()) => {
                    if let Err(error) = state.update_bot_user_profile(
                        sender.id.0,
                        sender.username.as_deref(),
                        &sender.first_name,
                    ) {
                        log::error!(
                            "Could not update profile for bot user {}: {error}",
                            sender.id
                        );
                    }
                }
                Err(error) => {
                    log::error!("Could not record bot user {}: {error}", sender.id);
                }
            }
        }
        state.cancel_setup(sender.id);
        if is_owner {
            log::info!("Owner {} opened the start menu", sender.id);
            bot.send_message(
                msg.chat.id,
                "سلام مالک عزیز! از منوی زیر برای مدیریت ربات استفاده کنید.",
            )
            .reply_markup(owner_keyboard())
            .await?;
        } else if is_admin {
            log::info!("Admin {} opened the start menu", sender.id);
            bot.send_message(
                msg.chat.id,
                "دسترسی ادمین فقط برای تأیید یا رد رسیدهای پرداخت است. دکمه‌های بررسی روی پیام هر رسید نمایش داده می‌شوند.",
            )
            .await?;
        } else {
            log::info!("User {} opened the start menu", sender.id);
            match is_required_channel_member(&bot, &state, sender.id).await {
                Ok(true) => {
                    let prompt = start_user_menu_prompt(&sender.first_name);
                    send_user_menu(&bot, msg.chat.id, &prompt).await?;
                }
                Ok(false) => {
                    send_join_prompt(&bot, msg.chat.id, &state).await?;
                }
                Err(error) => {
                    log::error!(
                        "Could not verify channel membership for user {}: {error}",
                        sender.id
                    );
                    send_user_message_with_keyboard(
                        &bot,
                        msg.chat.id,
                        "برای بررسی عضویت، ربات باید در کانال دسترسی مدیر داشته باشد. لطفاً بعداً دوباره تلاش کنید.",
                        required_channel_join_keyboard(&state)?,
                    )
                    .await?;
                }
            }
        }
        return Ok(());
    }

    if is_admin {
        return Ok(());
    }

    if !is_owner && text == "/cancel" {
        match state.cancel_awaiting_order(sender.id.0) {
            Ok(()) => {
                send_user_menu(&bot, msg.chat.id, "سفارش در انتظار رسید لغو شد.").await?;
            }
            Err(error) => {
                log::error!(
                    "Could not cancel awaiting order for user {}: {error}",
                    sender.id
                );
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "لغو سفارش انجام نشد؛ لطفاً دوباره تلاش کنید یا با پشتیبانی @{} تماس بگیرید.",
                        state.support_username()
                    ),
                )
                .await?;
            }
        }
        return Ok(());
    }

    if !is_owner {
        match state.awaiting_receipt_order(sender.id.0) {
            Ok(Some(order_id)) => {
                receive_payment_receipt(&bot, &msg, &state, order_id).await?;
            }
            Ok(None) => {}
            Err(error) => {
                log::error!(
                    "Could not check pending order for user {}: {error}",
                    sender.id
                );
                bot.send_message(
                    msg.chat.id,
                    "بررسی سفارش در انتظار رسید با خطا روبه‌رو شد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                )
                .await?;
            }
        }
        return Ok(());
    }

    if text == "/cancel" {
        state.cancel_setup(sender.id);
        log::info!("Privileged user {} cancelled setup", sender.id);
        bot.send_message(msg.chat.id, "فرآیند ورود لغو شد.")
            .reply_markup(owner_keyboard())
            .await?;
        return Ok(());
    }

    let Some(step) = state.take_setup(sender.id) else {
        return Ok(());
    };
    if is_owner
        && !msg.chat.is_private()
        && matches!(
            &step,
            SetupState::WalletUserId
                | SetupState::WalletBalance { .. }
                | SetupState::WalletReason { .. }
                | SetupState::WalletBulkAmount { .. }
                | SetupState::WalletBulkReason { .. }
        )
    {
        state.set_setup(sender.id, step);
        bot.send_message(
            msg.chat.id,
            "برای حفظ حریم خصوصی، مدیریت کیف پول را فقط در گفت‌وگوی خصوصی با ربات انجام دهید.",
        )
        .await?;
        return Ok(());
    }
    match step {
        SetupState::AdminId => {
            let admin_id = text.trim().parse::<u64>().ok().filter(|id| *id > 0);
            match admin_id {
                Some(admin_id) => match state.add_admin(admin_id) {
                    Ok(true) => {
                        log::info!("Owner {} added Telegram admin {}", sender.id, admin_id);
                        send_text_message_with_keyboard(
                            &bot,
                            msg.chat.id,
                            format!(
                                "✅ ادمین با شناسه‌ی {} اضافه شد. او می‌تواند با /start وارد منوی مدیریت شود.",
                                persian_digits(admin_id)
                            ),
                            admin_management_keyboard(),
                        )
                        .await?;
                    }
                    Ok(false) => {
                        send_text_message_with_keyboard(
                            &bot,
                            msg.chat.id,
                            "این شناسه مالک است یا از قبل در فهرست ادمین‌ها وجود دارد.",
                            admin_management_keyboard(),
                        )
                        .await?;
                    }
                    Err(error) => {
                        log::error!("Could not add Telegram admin {admin_id}: {error}");
                        bot.send_message(
                            msg.chat.id,
                            "ذخیره‌ی ادمین در دیتابیس انجام نشد. لطفاً لاگ برنامه را بررسی کنید.",
                        )
                        .await?;
                    }
                },
                None => {
                    state.set_setup(sender.id, SetupState::AdminId);
                    bot.send_message(
                        msg.chat.id,
                        "شناسه باید یک عدد تلگرامیِ بزرگ‌تر از صفر باشد؛ دوباره بفرستید یا /cancel را بزنید.",
                    )
                    .await?;
                }
            }
        }
        SetupState::WalletUserId => {
            let user_id = parse_management_number(text).filter(|user_id| *user_id > 0);
            match user_id {
                Some(user_id) => match state.wallet_summary(user_id) {
                    Ok((balance, _)) => {
                        state.set_setup(sender.id, SetupState::WalletBalance { user_id });
                        bot.send_message(
                            msg.chat.id,
                            format!(
                                "موجودی فعلی کاربر {}: {}\nموجودی نهایی دلخواه را به تومان وارد کنید. مبلغ منفی هم پذیرفته می‌شود؛ برای لغو /cancel را بزنید.",
                                persian_digits(user_id),
                                format_signed_toman(balance)
                            ),
                        )
                        .await?;
                    }
                    Err(error) => {
                        log::error!("Could not read wallet for Telegram user {user_id}: {error}");
                        state.set_setup(sender.id, SetupState::WalletUserId);
                        bot.send_message(
                            msg.chat.id,
                            "خواندن کیف پول انجام نشد؛ شناسه‌ی عددی کاربر را دوباره بفرستید یا /cancel را بزنید.",
                        )
                        .await?;
                    }
                },
                None => {
                    state.set_setup(sender.id, SetupState::WalletUserId);
                    bot.send_message(
                        msg.chat.id,
                        "شناسه‌ی کاربر باید عددی بزرگ‌تر از صفر باشد؛ دوباره بفرستید یا /cancel را بزنید.",
                    )
                    .await?;
                }
            }
        }
        SetupState::WalletBalance { user_id } => match parse_management_signed_number(text) {
            Some(target_balance) => {
                state.set_setup(
                    sender.id,
                    SetupState::WalletReason {
                        user_id,
                        target_balance,
                    },
                );
                bot.send_message(
                    msg.chat.id,
                    "دلیل تغییر موجودی را بنویسید (حداکثر ۵۰۰ نویسه). برای لغو /cancel را بزنید.",
                )
                .await?;
            }
            None => {
                state.set_setup(sender.id, SetupState::WalletBalance { user_id });
                bot.send_message(
                        msg.chat.id,
                        "مبلغ باید یک عدد صحیح معتبر به تومان باشد؛ عدد منفی هم مجاز است. دوباره بفرستید یا /cancel را بزنید.",
                    )
                    .await?;
            }
        },
        SetupState::WalletReason {
            user_id,
            target_balance,
        } => {
            let reason = text.trim();
            if reason.is_empty() || reason.chars().count() > 500 {
                state.set_setup(
                    sender.id,
                    SetupState::WalletReason {
                        user_id,
                        target_balance,
                    },
                );
                bot.send_message(
                    msg.chat.id,
                    "دلیل نباید خالی و باید حداکثر ۵۰۰ نویسه باشد؛ دوباره بفرستید یا /cancel را بزنید.",
                )
                .await?;
            } else {
                match state.set_wallet_balance_with_reason(user_id, target_balance, reason) {
                    Ok(previous_balance) => {
                        log::info!(
                            "Owner {} set wallet balance for user {} from {} to {} toman",
                            sender.id,
                            user_id,
                            previous_balance,
                            target_balance
                        );
                        send_text_message_with_keyboard(
                            &bot,
                            msg.chat.id,
                            format!(
                                "✅ موجودی کاربر {} تغییر کرد.\nموجودی قبلی: {}\nموجودی جدید: {}\n📝 دلیل: {}",
                                persian_digits(user_id),
                                format_signed_toman(previous_balance),
                                format_signed_toman(target_balance),
                                reason
                            ),
                            wallet_management_keyboard(),
                        )
                        .await?;
                    }
                    Err(error) => {
                        log::error!("Could not set wallet balance for user {user_id}: {error}");
                        state.set_setup(
                            sender.id,
                            SetupState::WalletReason {
                                user_id,
                                target_balance,
                            },
                        );
                        bot.send_message(
                            msg.chat.id,
                            "ذخیره‌ی تغییر موجودی انجام نشد؛ دلیل را دوباره وارد کنید یا /cancel را بزنید.",
                        )
                        .await?;
                    }
                }
            }
        }
        SetupState::WalletBulkAmount { action } => {
            match parse_management_number(text).and_then(|amount| i64::try_from(amount).ok()) {
                Some(amount_toman) if amount_toman > 0 => {
                    state.set_setup(
                        sender.id,
                        SetupState::WalletBulkReason {
                            action,
                            amount_toman,
                        },
                    );
                    bot.send_message(
                            msg.chat.id,
                            "دلیل این تغییر گروهی را وارد کنید (حداکثر ۵۰۰ نویسه). برای لغو /cancel را بزنید.",
                        )
                        .await?;
                }
                _ => {
                    state.set_setup(sender.id, SetupState::WalletBulkAmount { action });
                    bot.send_message(
                            msg.chat.id,
                            "مبلغ باید عدد صحیحی بزرگ‌تر از صفر باشد؛ دوباره وارد کنید یا /cancel را بزنید.",
                        )
                        .await?;
                }
            }
        }
        SetupState::WalletBulkReason {
            action,
            amount_toman,
        } => {
            let reason = text.trim();
            if reason.is_empty() || reason.chars().count() > 500 {
                state.set_setup(
                    sender.id,
                    SetupState::WalletBulkReason {
                        action,
                        amount_toman,
                    },
                );
                bot.send_message(
                        msg.chat.id,
                        "دلیل نباید خالی و باید حداکثر ۵۰۰ نویسه باشد؛ دوباره بفرستید یا /cancel را بزنید.",
                    )
                    .await?;
            } else {
                match state.adjust_all_wallets(action, amount_toman, reason) {
                    Ok(summary) => {
                        log::info!(
                                "Owner {} performed a bulk {:?} wallet adjustment of {} toman for {} users",
                                sender.id,
                                action,
                                amount_toman,
                                summary.targeted_users
                            );
                        send_text_message_with_keyboard(
                            &bot,
                            msg.chat.id,
                            bulk_wallet_adjustment_message(action, amount_toman, reason, summary),
                            wallet_management_keyboard(),
                        )
                        .await?;
                    }
                    Err(error) => {
                        log::error!("Could not apply bulk wallet adjustment: {error}");
                        state.set_setup(
                            sender.id,
                            SetupState::WalletBulkReason {
                                action,
                                amount_toman,
                            },
                        );
                        bot.send_message(
                                msg.chat.id,
                                "تغییر گروهی موجودی انجام نشد و هیچ کیف پولی تغییر نکرد؛ دلیل را دوباره بفرستید یا /cancel را بزنید.",
                            )
                            .await?;
                    }
                }
            }
        }
        SetupState::ReferralPercent => {
            match parse_management_number(text).and_then(|percent| u8::try_from(percent).ok()) {
                Some(percent) if percent <= 100 => match state.set_referral_percent(percent) {
                    Ok(()) => {
                        log::info!(
                            "Owner {} set referral commission rate to {}%",
                            sender.id,
                            percent
                        );
                        send_referral_management(&bot, sender.id, &state).await?;
                    }
                    Err(error) => {
                        log::error!("Could not update referral commission rate: {error}");
                        state.set_setup(sender.id, SetupState::ReferralPercent);
                        bot.send_message(
                                    msg.chat.id,
                                    "ذخیره‌ی درصد پورسانت انجام نشد؛ دوباره عددی بین ۰ تا ۱۰۰ وارد کنید یا /cancel را بزنید.",
                                )
                                .await?;
                    }
                },
                _ => {
                    state.set_setup(sender.id, SetupState::ReferralPercent);
                    bot.send_message(
                            msg.chat.id,
                            "درصد پورسانت باید عدد صحیحی بین ۰ تا ۱۰۰ باشد؛ دوباره وارد کنید یا /cancel را بزنید.",
                        )
                        .await?;
                }
            }
        }
        SetupState::SupportId => {
            if !msg.chat.is_private() {
                state.set_setup(sender.id, SetupState::SupportId);
                bot.send_message(
                    msg.chat.id,
                    "برای حفظ امنیت، تغییر آیدی پشتیبان را فقط در گفت‌وگوی خصوصی با ربات انجام دهید.",
                )
                .await?;
            } else if let Some(support_username) = normalize_support_username(text) {
                match state.set_support_username(&support_username) {
                    Ok(()) => {
                        log::info!(
                            "Owner {} changed the support username to @{}",
                            sender.id,
                            support_username
                        );
                        send_text_message_with_keyboard(
                            &bot,
                            msg.chat.id,
                            format!("✅ آیدی پشتیبان به @{support_username} تغییر کرد."),
                            owner_keyboard(),
                        )
                        .await?;
                    }
                    Err(error) => {
                        log::error!("Could not update the support username: {error}");
                        state.set_setup(sender.id, SetupState::SupportId);
                        bot.send_message(
                            msg.chat.id,
                            "ذخیره‌ی آیدی پشتیبان انجام نشد؛ دوباره یوزرنیم را بفرستید یا /cancel را بزنید.",
                        )
                        .await?;
                    }
                }
            } else {
                state.set_setup(sender.id, SetupState::SupportId);
                bot.send_message(
                    msg.chat.id,
                    "یوزرنیم تلگرام باید ۵ تا ۳۲ نویسه و شامل حروف انگلیسی، عدد یا _ باشد. با یا بدون @ بفرستید یا /cancel را بزنید.",
                )
                .await?;
            }
        }
        SetupState::RequiredChannelUsername => {
            if !msg.chat.is_private() {
                state.set_setup(sender.id, SetupState::RequiredChannelUsername);
                bot.send_message(
                    msg.chat.id,
                    "برای حفظ امنیت، مدیریت کانال‌های اجباری را فقط در گفت‌وگوی خصوصی با ربات انجام دهید.",
                )
                .await?;
            } else if let Some(username) = state::normalize_required_channel_username(text) {
                match state.add_required_channel(&username) {
                    Ok(true) => {
                        log::info!("Owner {} added required channel @{}", sender.id, username);
                        send_required_channel_management(&bot, sender.id, &state).await?;
                    }
                    Ok(false) => {
                        bot.send_message(
                            msg.chat.id,
                            format!("کانال @{username} از قبل در فهرست است."),
                        )
                        .await?;
                        send_required_channel_management(&bot, sender.id, &state).await?;
                    }
                    Err(error) => {
                        log::error!("Could not add required channel @{username}: {error}");
                        state.set_setup(sender.id, SetupState::RequiredChannelUsername);
                        bot.send_message(
                            msg.chat.id,
                            format!(
                                "افزودن کانال انجام نشد. حداکثر {MAX_REQUIRED_CHANNELS} کانال قابل ثبت است؛ یوزرنیم را دوباره بفرستید یا /cancel را بزنید."
                            ),
                        )
                        .await?;
                    }
                }
            } else {
                state.set_setup(sender.id, SetupState::RequiredChannelUsername);
                bot.send_message(
                    msg.chat.id,
                    "یوزرنیم عمومی کانال را با یا بدون @ بفرستید؛ باید ۵ تا ۳۲ نویسه و شامل حروف انگلیسی، عدد یا _ باشد. برای لغو /cancel را بزنید.",
                )
                .await?;
            }
        }
        SetupState::DatabaseRestore => {
            state.set_setup(sender.id, SetupState::DatabaseRestore);
            bot.send_message(
                msg.chat.id,
                "لطفاً فایل بکاپی را که همین ربات برای شما فرستاده، به‌صورت فایل (Document) ارسال کنید یا /cancel را بزنید.",
            )
            .await?;
        }
        SetupState::UserSearch => {
            if !msg.chat.is_private() {
                state.set_setup(sender.id, SetupState::UserSearch);
                bot.send_message(
                    msg.chat.id,
                    "برای حفظ حریم خصوصی، جستجوی کاربران را فقط در گفت‌وگوی خصوصی با ربات انجام دهید.",
                )
                .await?;
            } else {
                send_user_search_results(&bot, sender.id, &state, text).await?;
            }
        }
        SetupState::BroadcastAll => {
            broadcast_message(&bot, &msg, &state, false).await?;
        }
        SetupState::BroadcastSubscribers => {
            broadcast_message(&bot, &msg, &state, true).await?;
        }
        SetupState::NewTariffVolume => {
            match parse_management_number(text).and_then(|value| u16::try_from(value).ok()) {
                Some(volume_gb) if volume_gb > 0 => {
                    state.set_setup(sender.id, SetupState::NewTariffPrice { volume_gb });
                    bot.send_message(
                        msg.chat.id,
                        "قیمت این تعرفه را به تومان وارد کنید (فقط عدد بزرگ‌تر از صفر).",
                    )
                    .await?;
                }
                _ => {
                    state.set_setup(sender.id, SetupState::NewTariffVolume);
                    bot.send_message(
                        msg.chat.id,
                        "حجم باید عددی بزرگ‌تر از صفر باشد؛ دوباره وارد کنید یا /cancel را بزنید.",
                    )
                    .await?;
                }
            }
        }
        SetupState::NewTariffPrice { volume_gb } => {
            match parse_management_number(text).and_then(|value| u32::try_from(value).ok()) {
                Some(price_toman) if price_toman > 0 => {
                    match state.add_subscription_tariff(volume_gb, price_toman) {
                        Ok(tariff_id) => {
                            log::info!(
                                "Owner {} added subscription tariff {tariff_id} ({volume_gb} GB, {price_toman} toman)",
                                sender.id
                            );
                            send_subscription_tariffs(&bot, sender.id, &state).await?;
                        }
                        Err(error) => {
                            log::error!("Could not add subscription tariff: {error}");
                            bot.send_message(
                                msg.chat.id,
                                "ذخیره تعرفه انجام نشد؛ ممکن است این حجم از قبل ثبت شده باشد. فهرست تعرفه‌ها را بررسی کنید.",
                            )
                            .await?;
                            send_subscription_tariffs(&bot, sender.id, &state).await?;
                        }
                    }
                }
                _ => {
                    state.set_setup(sender.id, SetupState::NewTariffPrice { volume_gb });
                    bot.send_message(
                        msg.chat.id,
                        "قیمت باید عددی بزرگ‌تر از صفر و به تومان باشد؛ دوباره وارد کنید یا /cancel را بزنید.",
                    )
                    .await?;
                }
            }
        }
        SetupState::PaymentCardNumber => match normalize_card_number(text) {
            Some(card_number) => {
                state.set_setup(sender.id, SetupState::PaymentCardholder { card_number });
                bot.send_message(
                    msg.chat.id,
                    "حالا نام و نام خانوادگی صاحب حساب را بفرستید. برای لغو /cancel را بزنید.",
                )
                .await?;
            }
            None => {
                state.set_setup(sender.id, SetupState::PaymentCardNumber);
                bot.send_message(
                    msg.chat.id,
                    "شماره کارت باید ۱۶ رقم باشد. فاصله و خط تیره پذیرفته می‌شود؛ دوباره بفرستید یا /cancel را بزنید.",
                )
                .await?;
            }
        },
        SetupState::PaymentCardholder { card_number } => {
            let cardholder_name = text.trim();
            if cardholder_name.is_empty() || cardholder_name.chars().count() > 100 {
                state.set_setup(sender.id, SetupState::PaymentCardholder { card_number });
                bot.send_message(
                    msg.chat.id,
                    "نام صاحب حساب معتبر نیست؛ دوباره بفرستید یا /cancel را بزنید.",
                )
                .await?;
            } else {
                match state.save_payment_details(&card_number, cardholder_name) {
                    Ok(()) => {
                        log::info!("Owner {} updated payment card details", sender.id);
                        bot.send_message(
                            msg.chat.id,
                            "✅ شماره کارت و نام صاحب حساب ذخیره شد و از این پس برای سفارش‌های جدید نمایش داده می‌شود.",
                        )
                        .reply_markup(owner_keyboard())
                        .await?;
                    }
                    Err(error) => {
                        log::error!("Could not save payment card details: {error}");
                        bot.send_message(
                            msg.chat.id,
                            "ذخیره‌ی اطلاعات کارت انجام نشد؛ لطفاً لاگ برنامه را بررسی کنید.",
                        )
                        .await?;
                    }
                }
            }
        }
        SetupState::PanelUrl => match panel::normalize_panel_url(text) {
            Ok(base_url) => {
                state.set_setup(sender.id, SetupState::Username { base_url });
                bot.send_message(msg.chat.id, "نام کاربری مدیر پنل را ارسال کنید.")
                    .await?;
            }
            Err(error) => {
                state.set_setup(sender.id, SetupState::PanelUrl);
                bot.send_message(
                    msg.chat.id,
                    format!("{error}\nآدرس پنل را دوباره بفرستید یا /cancel را بزنید."),
                )
                .await?;
            }
        },
        SetupState::Username { base_url } => {
            let username = text.trim();
            if username.is_empty() || username.len() > 128 {
                state.set_setup(sender.id, SetupState::Username { base_url });
                bot.send_message(
                    msg.chat.id,
                    "نام کاربری معتبر نیست؛ دوباره ارسال کنید یا /cancel را بزنید.",
                )
                .await?;
            } else {
                state.set_setup(
                    sender.id,
                    SetupState::Password {
                        base_url,
                        username: username.to_owned(),
                    },
                );
                bot.send_message(
                    msg.chat.id,
                    "گذرواژه‌ی پنل را ارسال کنید. ربات برای حفظ امنیت، پیام گذرواژه را پیش از ورود حذف می‌کند.",
                )
                .await?;
            }
        }
        SetupState::Password { base_url, username } => {
            let password = Zeroizing::new(text.to_owned());
            if password.is_empty() {
                state.set_setup(sender.id, SetupState::Password { base_url, username });
                bot.send_message(
                    msg.chat.id,
                    "گذرواژه خالی است؛ دوباره ارسال کنید یا /cancel را بزنید.",
                )
                .await?;
                return Ok(());
            }

            if bot.delete_message(msg.chat.id, msg.id).await.is_err() {
                bot.send_message(
                    msg.chat.id,
                    "برای محافظت از حساب، ورود انجام نشد چون حذف پیام گذرواژه ممکن نبود. پیام را دستی حذف کنید و دوباره تلاش کنید.",
                )
                .await?;
                return Ok(());
            }

            bot.send_message(msg.chat.id, "در حال بررسی اطلاعات ورود پنل…")
                .await?;
            let authentication =
                authenticate_panel_with_retries(&base_url, &username, &password, "owner login")
                    .await;

            match authentication {
                Ok(access_token) => {
                    log::info!("Panel authentication succeeded for user {}", sender.id);
                    if let Err(error) = state.save_panel(&base_url, &username) {
                        log::error!("Could not save panel settings: {error}");
                        bot.send_message(
                            msg.chat.id,
                            "ورود به پنل موفق بود، اما ذخیره‌ی تنظیمات انجام نشد. لطفاً دوباره تلاش کنید.",
                        )
                        .await?;
                        return Ok(());
                    }
                    let credential_warning = match state.save_panel_password(&password) {
                        Ok(()) => "",
                        Err(error) => {
                            log::error!("Could not save panel credentials in the operating-system credential store: {error}");
                            "\n⚠️ ذخیره‌ی امن گذرواژه انجام نشد؛ ورود خودکار پس از راه‌اندازی مجدد فعال نخواهد بود."
                        }
                    };
                    state.set_session(access_token);
                    bot.send_message(
                        msg.chat.id,
                        format!(
                            "✅ ورود به پنل پاسارگارد موفق بود. تنظیمات پنل ذخیره شد.{}",
                            credential_warning
                        ),
                    )
                    .reply_markup(owner_keyboard())
                    .await?;
                }
                Err(error) => {
                    log::warn!(
                        "Panel authentication failed for user {}: {}",
                        sender.id,
                        error
                    );
                    notify_panel_issue(&bot, &state, &error.user_message()).await;
                    bot.send_message(msg.chat.id, error.user_message())
                        .reply_markup(owner_keyboard())
                        .await?;
                }
            }
        }
        SetupState::TariffVolume { tariff_id } => {
            match parse_management_number(text).and_then(|value| u16::try_from(value).ok()) {
                Some(volume_gb) if volume_gb > 0 => {
                    match state.update_subscription_tariff_volume(tariff_id, volume_gb) {
                        Ok(true) => send_subscription_tariffs(&bot, sender.id, &state).await?,
                        Ok(false) => {
                            bot.send_message(msg.chat.id, "این تعرفه دیگر وجود ندارد.")
                                .await?;
                            send_subscription_tariffs(&bot, sender.id, &state).await?;
                        }
                        Err(error) => {
                            log::error!("Could not update subscription tariff volume: {error}");
                            state.set_setup(sender.id, SetupState::TariffVolume { tariff_id });
                            bot.send_message(
                                msg.chat.id,
                                "ذخیره حجم انجام نشد؛ ممکن است حجم تکراری باشد. مقدار دیگری وارد کنید یا /cancel را بزنید.",
                            )
                            .await?;
                        }
                    }
                }
                _ => {
                    state.set_setup(sender.id, SetupState::TariffVolume { tariff_id });
                    bot.send_message(
                        msg.chat.id,
                        "حجم باید عددی بزرگ‌تر از صفر باشد؛ دوباره وارد کنید یا /cancel را بزنید.",
                    )
                    .await?;
                }
            }
        }
        SetupState::TariffPrice { tariff_id } => {
            match parse_management_number(text).and_then(|value| u32::try_from(value).ok()) {
                Some(price_toman) if price_toman > 0 => {
                    match state.update_subscription_tariff_price(tariff_id, price_toman) {
                        Ok(true) => send_subscription_tariffs(&bot, sender.id, &state).await?,
                        Ok(false) => {
                            bot.send_message(msg.chat.id, "این تعرفه دیگر وجود ندارد.")
                                .await?;
                            send_subscription_tariffs(&bot, sender.id, &state).await?;
                        }
                        Err(error) => {
                            log::error!("Could not update subscription tariff price: {error}");
                            state.set_setup(sender.id, SetupState::TariffPrice { tariff_id });
                            bot.send_message(
                                msg.chat.id,
                                "ذخیره قیمت انجام نشد؛ قیمت دیگری وارد کنید یا /cancel را بزنید.",
                            )
                            .await?;
                        }
                    }
                }
                _ => {
                    state.set_setup(sender.id, SetupState::TariffPrice { tariff_id });
                    bot.send_message(
                        msg.chat.id,
                        "قیمت باید عددی بزرگ‌تر از صفر و به تومان باشد؛ دوباره وارد کنید یا /cancel را بزنید.",
                    )
                    .await?;
                }
            }
        }
    }

    Ok(())
}

async fn broadcast_message(
    bot: &Bot,
    source: &Message,
    state: &AppState,
    subscribers_only: bool,
) -> ResponseResult<()> {
    let recipients = match state.broadcast_targets(subscribers_only) {
        Ok(recipients) => recipients
            .into_iter()
            .filter(|recipient| *recipient != state.owner_id.0)
            .collect::<Vec<_>>(),
        Err(error) => {
            log::error!("Could not load broadcast recipients: {error}");
            bot.send_message(
                source.chat.id,
                "خواندن فهرست گیرندگان با خطا روبه‌رو شد؛ پیام ارسال نشد.",
            )
            .await?;
            return Ok(());
        }
    };

    if recipients.is_empty() {
        bot.send_message(
            source.chat.id,
            if subscribers_only {
                "هنوز مشترکی برای ارسال پیام ثبت نشده است."
            } else {
                "هنوز کاربری که ربات را در گفت‌وگوی خصوصی شروع کرده باشد ثبت نشده است."
            },
        )
        .await?;
        return Ok(());
    }

    let total = recipients.len();
    bot.send_message(
        source.chat.id,
        format!("📨 ارسال پیام به {} نفر آغاز شد…", persian_digits(total)),
    )
    .await?;

    let mut sent = 0usize;
    let mut failed = 0usize;
    for recipient in recipients {
        match bot
            .copy_message(UserId(recipient), source.chat.id, source.id)
            .await
        {
            Ok(_) => sent += 1,
            Err(error) => {
                failed += 1;
                log::warn!("Broadcast copy to Telegram user {recipient} failed: {error}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    log::info!(
        "Broadcast finished: sent {sent}, failed {failed}, subscribers_only={subscribers_only}"
    );
    bot.send_message(
        source.chat.id,
        format!(
            "✅ ارسال همگانی پایان یافت.\n📬 موفق: {}\n⚠️ ناموفق: {}",
            persian_digits(sent),
            persian_digits(failed)
        ),
    )
    .await?;
    Ok(())
}

async fn handle_callback_query(bot: Bot, query: CallbackQuery, state: Arc<AppState>) -> Result<()> {
    bot.answer_callback_query(query.id.clone()).await?;
    let target = callback_message_target(&query);
    let data = query.data.as_deref().unwrap_or_default();
    if (data.starts_with("user:") || data.starts_with("payment:cancel:"))
        && target.is_some_and(|target| target.chat_id.0 != query.from.id.0 as i64)
    {
        send_user_menu(
            &bot,
            query.from.id,
            "برای حفظ حریم خصوصی، لطفاً ربات را در گفت‌وگوی خصوصی باز کنید.",
        )
        .await?;
        return Ok(());
    }
    let is_owner = query.from.id == state.owner_id;
    let is_admin = if is_owner {
        false
    } else {
        match state.is_admin(query.from.id.0) {
            Ok(is_admin) => is_admin,
            Err(error) => {
                log::error!(
                    "Could not check admin access for user {}: {error}",
                    query.from.id
                );
                false
            }
        }
    };
    if is_admin && !is_owner && !is_admin_allowed_callback(data) {
        bot.send_message(
            query.from.id,
            "دسترسی ادمین فقط برای تأیید یا رد رسیدهای پرداخت است.",
        )
        .await?;
        return Ok(());
    }
    if is_owner
        && (data.starts_with("owner:database:")
            || data.starts_with("owner:users:")
            || data.starts_with("owner:wallet:")
            || data.starts_with("owner:finance:")
            || data.starts_with("owner:disk:")
            || data.starts_with("owner:support:")
            || data.starts_with("owner:channels:"))
        && !target.is_some_and(|target| target.chat_id.0 == query.from.id.0 as i64)
    {
        bot.send_message(
            query.from.id,
            "برای حفظ حریم خصوصی، این بخش فقط در گفت‌وگوی خصوصی با ربات در دسترس است.",
        )
        .await?;
        return Ok(());
    }

    let is_purchase_action = data == "user:buy"
        || data == "user:plans:month"
        || data.starts_with("user:plan:")
        || data.starts_with("user:pc:")
        || data.starts_with("user:pw:");
    let is_renewal_action = data == "user:renew"
        || data.starts_with("user:renew:")
        || data.starts_with("user:rc:")
        || data.starts_with("user:rw:");
    if is_purchase_action || is_renewal_action {
        match state.sales_status() {
            Ok((purchases_enabled, renewals_enabled))
                if (!is_purchase_action || purchases_enabled)
                    && (!is_renewal_action || renewals_enabled) => {}
            Ok((purchases_enabled, renewals_enabled)) => {
                let message = if is_purchase_action && !purchases_enabled {
                    "فعلاً امکان خرید اشتراک جدید توسط مالک غیرفعال شده است."
                } else if is_renewal_action && !renewals_enabled {
                    "فعلاً امکان تمدید اشتراک توسط مالک غیرفعال شده است."
                } else {
                    "این عملیات فعلاً در دسترس نیست."
                };
                edit_user_message_with_keyboard(
                    &bot,
                    target,
                    message,
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
            Err(error) => {
                log::error!("Could not read purchase/renewal availability: {error}");
                edit_user_message_with_keyboard(
                    &bot,
                    target,
                    "بررسی وضعیت خرید یا تمدید ممکن نشد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
        }
    }

    match data {
        "owner:panel:manage" if is_owner => {
            log::info!("Owner {} opened panel management", query.from.id);
            let body = match state.panel_status() {
                Ok(status) => status,
                Err(error) => {
                    log::error!("Could not read panel settings: {error}");
                    "خواندن وضعیت پنل با خطا روبه‌رو شد. لطفاً لاگ برنامه را بررسی کنید.".into()
                }
            };
            send_text_message_with_keyboard(&bot, query.from.id, body, panel_keyboard()).await?;
        }
        "owner:panel:login" if is_owner => {
            state.set_setup(query.from.id, SetupState::PanelUrl);
            bot.send_message(
                query.from.id,
                "آدرس کامل پنل پاسارگارد را بفرستید (مثلاً https://panel.example.com). برای لغو /cancel را بزنید.",
            )
            .await?;
        }
        "owner:broadcast:all" if is_owner => {
            state.set_setup(query.from.id, SetupState::BroadcastAll);
            bot.send_message(
                query.from.id,
                "پیام یا رسانه‌ای را که می‌خواهید برای همه‌ی کاربرانی که ربات را در گفت‌وگوی خصوصی شروع کرده‌اند بفرستید، ارسال کنید.\nبرای لغو /cancel را بزنید.",
            )
            .await?;
        }
        "owner:broadcast:subscribers" if is_owner => {
            state.set_setup(query.from.id, SetupState::BroadcastSubscribers);
            bot.send_message(
                query.from.id,
                "پیام یا رسانه‌ای را که می‌خواهید برای مشترکین ثبت‌شده بفرستید، ارسال کنید.\nبرای لغو /cancel را بزنید.",
            )
            .await?;
        }
        "owner:payment:manage" if is_owner => {
            state.set_setup(query.from.id, SetupState::PaymentCardNumber);
            bot.send_message(
                query.from.id,
                "شماره کارت ۱۶ رقمی نمایشی به کاربر را ارسال کنید. بعد از آن نام و نام خانوادگی صاحب حساب را می‌پرسم.\nبرای لغو /cancel را بزنید.",
            )
            .await?;
        }
        "owner:support:set" if is_owner => {
            state.set_setup(query.from.id, SetupState::SupportId);
            bot.send_message(
                query.from.id,
                format!(
                    "آیدی فعلی پشتیبان: @{}\nیوزرنیم جدید تلگرام را با یا بدون @ بفرستید. برای لغو /cancel را بزنید.",
                    state.support_username()
                ),
            )
            .await?;
        }
        "owner:channels:manage" if is_owner => {
            send_required_channel_management(&bot, query.from.id, &state).await?;
        }
        "owner:channels:add" if is_owner => {
            state.set_setup(query.from.id, SetupState::RequiredChannelUsername);
            bot.send_message(
                query.from.id,
                format!(
                    "یوزرنیم عمومی کانال را با یا بدون @ بفرستید. ربات باید در کانال ادمین باشد تا عضویت کاربران را بررسی کند. حداکثر {MAX_REQUIRED_CHANNELS} کانال قابل ثبت است. برای لغو /cancel را بزنید."
                ),
            )
            .await?;
        }
        "owner:channels:toggle" if is_owner => match state.required_channels_enabled() {
            Ok(enabled) => match state.set_required_channels_enabled(!enabled) {
                Ok(()) => {
                    log::info!(
                        "Owner {} {} required channel membership",
                        query.from.id,
                        if enabled { "disabled" } else { "enabled" }
                    );
                    send_required_channel_management(&bot, query.from.id, &state).await?;
                }
                Err(error) => {
                    log::error!("Could not toggle required channel membership: {error}");
                    bot.send_message(
                        query.from.id,
                        "ذخیره وضعیت عضویت اجباری انجام نشد؛ لاگ برنامه را بررسی کنید.",
                    )
                    .await?;
                }
            },
            Err(error) => {
                log::error!("Could not read required channel status: {error}");
                bot.send_message(
                    query.from.id,
                    "خواندن وضعیت عضویت اجباری انجام نشد؛ لاگ برنامه را بررسی کنید.",
                )
                .await?;
            }
        },
        data if data.starts_with("owner:channels:remove:") && is_owner => {
            let username = data
                .strip_prefix("owner:channels:remove:")
                .unwrap_or_default();
            match state.remove_required_channel(username) {
                Ok(true) => {
                    log::info!(
                        "Owner {} removed required channel @{}",
                        query.from.id,
                        username
                    );
                    send_required_channel_management(&bot, query.from.id, &state).await?;
                }
                Ok(false) => {
                    bot.send_message(query.from.id, "این کانال در فهرست پیدا نشد.")
                        .await?;
                    send_required_channel_management(&bot, query.from.id, &state).await?;
                }
                Err(error) => {
                    log::error!("Could not remove required channel @{username}: {error}");
                    bot.send_message(
                        query.from.id,
                        "حذف کانال انجام نشد؛ لاگ برنامه را بررسی کنید.",
                    )
                    .await?;
                }
            }
        }
        "owner:wallet:manage" if is_owner => {
            send_wallet_management(&bot, query.from.id, &state).await?;
        }
        "owner:disk:manage" if is_owner => {
            send_disk_space_management(&bot, query.from.id, &state).await?;
        }
        "owner:finance:manage" if is_owner => {
            send_financial_statistics_menu(&bot, query.from.id).await?;
        }
        data if data.starts_with("owner:finance:") && is_owner => {
            match financial_metric_from_callback(data) {
                Some(metric) => {
                    send_financial_statistic(&bot, query.from.id, &state, metric).await?;
                }
                None => {
                    bot.send_message(query.from.id, "گزینه‌ی آمار مالی معتبر نیست.")
                        .await?;
                }
            }
        }
        "owner:wallet:adjust" if is_owner => {
            state.set_setup(query.from.id, SetupState::WalletUserId);
            bot.send_message(
                query.from.id,
                "شناسه‌ی عددی تلگرام کاربر را ارسال کنید تا موجودی کیف پول او را تنظیم کنید. برای لغو /cancel را بزنید.",
            )
            .await?;
        }
        "owner:wallet:increase_all" if is_owner => {
            state.set_setup(
                query.from.id,
                SetupState::WalletBulkAmount {
                    action: WalletBulkAction::Increase,
                },
            );
            bot.send_message(
                query.from.id,
                "مبلغ هدیه برای هر کاربر را به تومان وارد کنید (عدد صحیح بزرگ‌تر از صفر). سپس دلیل تغییر ثبت می‌شود. برای لغو /cancel را بزنید.",
            )
            .await?;
        }
        "owner:wallet:decrease_all" if is_owner => {
            state.set_setup(
                query.from.id,
                SetupState::WalletBulkAmount {
                    action: WalletBulkAction::Decrease,
                },
            );
            bot.send_message(
                query.from.id,
                "مبلغ کسر از هر کاربر را به تومان وارد کنید. اگر موجودی کمتر باشد، فقط تا صفر کسر می‌شود و موجودی رزروشده‌ی سفارش‌های باز حفظ می‌شود. سپس دلیل تغییر ثبت می‌شود. برای لغو /cancel را بزنید.",
            )
            .await?;
        }
        "owner:sales:manage" if is_owner => {
            send_sales_management(&bot, query.from.id, &state).await?;
        }
        "owner:sales:purchase:disable" if is_owner => {
            update_sales_feature(&bot, query.from.id, &state, SalesFeature::Purchase, false)
                .await?;
        }
        "owner:sales:purchase:enable" if is_owner => {
            update_sales_feature(&bot, query.from.id, &state, SalesFeature::Purchase, true).await?;
        }
        "owner:sales:renewal:disable" if is_owner => {
            update_sales_feature(&bot, query.from.id, &state, SalesFeature::Renewal, false).await?;
        }
        "owner:sales:renewal:enable" if is_owner => {
            update_sales_feature(&bot, query.from.id, &state, SalesFeature::Renewal, true).await?;
        }
        "owner:referral:manage" if is_owner => {
            send_referral_management(&bot, query.from.id, &state).await?;
        }
        "owner:referral:disable" if is_owner => {
            update_referrals_enabled(&bot, query.from.id, &state, false).await?;
        }
        "owner:referral:enable" if is_owner => {
            update_referrals_enabled(&bot, query.from.id, &state, true).await?;
        }
        "owner:referral:percent" if is_owner => {
            state.set_setup(query.from.id, SetupState::ReferralPercent);
            bot.send_message(
                query.from.id,
                "درصد پورسانت را به‌صورت عدد صحیح بین ۰ تا ۱۰۰ ارسال کنید. برای لغو /cancel را بزنید.",
            )
            .await?;
        }
        "owner:database:manage" if is_owner => {
            send_database_management(&bot, query.from.id).await?;
        }
        "owner:database:backup" if is_owner => {
            send_database_backup(&bot, query.from.id, &state, "✅ بکاپ دیتابیس ربات").await?;
        }
        "owner:database:restore" if is_owner => {
            state.set_setup(query.from.id, SetupState::DatabaseRestore);
            bot.send_message(
                query.from.id,
                "برای ریستور، همان فایل بکاپی را که قبلاً همین ربات برای شما فرستاده، بدون تغییر به‌صورت فایل (Document) در همین گفت‌وگو ارسال کنید. برای لغو /cancel را بزنید.",
            )
            .await?;
        }
        "owner:users:manage" if is_owner => {
            send_user_management_menu(&bot, query.from.id, &state).await?;
        }
        "owner:users:search" if is_owner => {
            state.set_setup(query.from.id, SetupState::UserSearch);
            bot.send_message(
                query.from.id,
                "شناسه‌ی عددی تلگرام یا نام کاربری کاربر را (با یا بدون @) ارسال کنید. برای لغو /cancel را بزنید.",
            )
            .await?;
        }
        data if data.starts_with("owner:users:item:") && is_owner => {
            let user_id = data
                .strip_prefix("owner:users:item:")
                .and_then(|value| value.parse::<u64>().ok());
            match user_id {
                Some(user_id) => {
                    send_managed_user_details(&bot, query.from.id, &state, user_id).await?;
                }
                None => {
                    bot.send_message(query.from.id, "شناسه‌ی کاربر معتبر نیست.")
                        .await?;
                }
            }
        }
        "owner:tariffs:manage" if is_owner => {
            send_subscription_tariffs(&bot, query.from.id, &state).await?;
        }
        "owner:tariffs:add" if is_owner => {
            state.set_setup(query.from.id, SetupState::NewTariffVolume);
            bot.send_message(
                query.from.id,
                "حجم تعرفه جدید را به گیگابایت وارد کنید (عدد بزرگ‌تر از صفر). برای لغو /cancel را بزنید.",
            )
            .await?;
        }
        data if data.starts_with("owner:tariffs:item:") && is_owner => {
            let tariff_id = data
                .strip_prefix("owner:tariffs:item:")
                .and_then(|value| value.parse::<i64>().ok());
            match tariff_id {
                Some(tariff_id) => match state.subscription_tariff(tariff_id) {
                    Ok(Some(tariff)) => {
                        send_text_message_with_keyboard(
                            &bot,
                            query.from.id,
                            subscription_tariff_message(&tariff),
                            subscription_tariff_detail_keyboard(&tariff),
                        )
                        .await?;
                    }
                    Ok(None) => {
                        bot.send_message(query.from.id, "تعرفه پیدا نشد.").await?;
                        send_subscription_tariffs(&bot, query.from.id, &state).await?;
                    }
                    Err(error) => {
                        log::error!("Could not read tariff {tariff_id}: {error}");
                        bot.send_message(
                            query.from.id,
                            "خواندن تعرفه انجام نشد؛ لاگ برنامه را بررسی کنید.",
                        )
                        .await?;
                    }
                },
                None => {
                    bot.send_message(query.from.id, "شناسه تعرفه معتبر نیست.")
                        .await?;
                }
            }
        }
        data if data.starts_with("owner:tariffs:toggle:") && is_owner => {
            let tariff_id = data
                .strip_prefix("owner:tariffs:toggle:")
                .and_then(|value| value.parse::<i64>().ok());
            match tariff_id {
                Some(tariff_id) => match state.subscription_tariff(tariff_id) {
                    Ok(Some(tariff)) => {
                        match state.set_subscription_tariff_enabled(tariff_id, !tariff.enabled) {
                            Ok(true) => {
                                send_subscription_tariffs(&bot, query.from.id, &state).await?
                            }
                            Ok(false) => {
                                bot.send_message(query.from.id, "تعرفه پیدا نشد.").await?;
                            }
                            Err(error) => {
                                log::error!("Could not toggle tariff {tariff_id}: {error}");
                                bot.send_message(
                                    query.from.id,
                                    "ذخیره وضعیت تعرفه انجام نشد؛ لاگ برنامه را بررسی کنید.",
                                )
                                .await?;
                            }
                        }
                    }
                    Ok(None) => {
                        bot.send_message(query.from.id, "تعرفه پیدا نشد.").await?;
                    }
                    Err(error) => {
                        log::error!("Could not read tariff {tariff_id}: {error}");
                        bot.send_message(
                            query.from.id,
                            "خواندن تعرفه انجام نشد؛ لاگ برنامه را بررسی کنید.",
                        )
                        .await?;
                    }
                },
                None => {
                    bot.send_message(query.from.id, "شناسه تعرفه معتبر نیست.")
                        .await?;
                }
            }
        }
        data if data.starts_with("owner:tariffs:volume:") && is_owner => {
            if let Some(tariff_id) = data
                .strip_prefix("owner:tariffs:volume:")
                .and_then(|value| value.parse::<i64>().ok())
            {
                state.set_setup(query.from.id, SetupState::TariffVolume { tariff_id });
                bot.send_message(
                    query.from.id,
                    "حجم جدید تعرفه را به گیگابایت وارد کنید. برای لغو /cancel را بزنید.",
                )
                .await?;
            } else {
                bot.send_message(query.from.id, "شناسه تعرفه معتبر نیست.")
                    .await?;
            }
        }
        data if data.starts_with("owner:tariffs:price:") && is_owner => {
            if let Some(tariff_id) = data
                .strip_prefix("owner:tariffs:price:")
                .and_then(|value| value.parse::<i64>().ok())
            {
                state.set_setup(query.from.id, SetupState::TariffPrice { tariff_id });
                bot.send_message(
                    query.from.id,
                    "قیمت جدید تعرفه را به تومان وارد کنید. برای لغو /cancel را بزنید.",
                )
                .await?;
            } else {
                bot.send_message(query.from.id, "شناسه تعرفه معتبر نیست.")
                    .await?;
            }
        }
        "owner:admins:manage" if is_owner => match state.admins() {
            Ok(admins) => {
                let body = if admins.is_empty() {
                    "فهرست ادمین‌ها خالی است.".to_owned()
                } else {
                    format!(
                            "👥 ادمین‌های ربات:\n{}\n\nبرای حذف هر ادمین، دکمه‌ی مربوط به شناسه‌اش را بزنید.",
                            admins
                                .iter()
                                .map(|id| format!("• {}", persian_digits(id)))
                                .collect::<Vec<_>>()
                                .join("\n")
                        )
                };
                send_text_message_with_keyboard(
                    &bot,
                    query.from.id,
                    body,
                    admin_list_keyboard(&admins),
                )
                .await?;
            }
            Err(error) => {
                log::error!("Could not list Telegram admins: {error}");
                send_text_message_with_keyboard(
                    &bot,
                    query.from.id,
                    "خواندن فهرست ادمین‌ها با خطا روبه‌رو شد.",
                    admin_management_keyboard(),
                )
                .await?;
            }
        },
        "owner:admins:add" if is_owner => {
            state.set_setup(query.from.id, SetupState::AdminId);
            bot.send_message(
                query.from.id,
                "شناسه‌ی عددی تلگرام ادمین جدید را ارسال کنید. برای لغو /cancel را بزنید.",
            )
            .await?;
        }
        data if data.starts_with("owner:admins:remove:") && is_owner => {
            let removed_id = data
                .strip_prefix("owner:admins:remove:")
                .and_then(|id| id.parse::<u64>().ok());
            match removed_id {
                Some(id) => match state.remove_admin(id) {
                    Ok(true) => {
                        log::info!("Owner {} removed Telegram admin {}", query.from.id, id);
                        send_text_message_with_keyboard(
                            &bot,
                            query.from.id,
                            format!("✅ ادمین {} حذف شد.", persian_digits(id)),
                            admin_management_keyboard(),
                        )
                        .await?;
                    }
                    Ok(false) => {
                        send_text_message_with_keyboard(
                            &bot,
                            query.from.id,
                            "این شناسه در فهرست ادمین‌ها نیست.",
                            admin_management_keyboard(),
                        )
                        .await?;
                    }
                    Err(error) => {
                        log::error!("Could not remove Telegram admin {id}: {error}");
                        bot.send_message(
                            query.from.id,
                            "حذف ادمین انجام نشد؛ لاگ برنامه را بررسی کنید.",
                        )
                        .await?;
                    }
                },
                None => {
                    bot.send_message(query.from.id, "شناسه‌ی ادمین معتبر نیست.")
                        .await?;
                }
            }
        }
        "owner:main" if is_owner => {
            bot.send_message(query.from.id, "از منوی زیر برای مدیریت ربات استفاده کنید.")
                .reply_markup(owner_keyboard())
                .await?;
        }
        "user:purchase-menu" => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            edit_user_message_with_keyboard(
                &bot,
                target,
                "خرید یا تمدید اشتراک را انتخاب کنید:",
                purchase_menu_keyboard(),
            )
            .await?;
        }
        "user:management-menu" => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            edit_user_message_with_keyboard(
                &bot,
                target,
                "بخش مدیریت اشتراک را انتخاب کنید:",
                subscription_management_keyboard(),
            )
            .await?;
        }
        "user:referral" => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            let referral_percent = match state.referral_status() {
                Ok((true, percent)) => percent,
                Ok((false, _)) => {
                    edit_user_message_with_keyboard(
                        &bot,
                        target,
                        "قابلیت زیرمجموعه‌گیری در حال حاضر غیرفعال است.",
                        subscription_back_keyboard(),
                    )
                    .await?;
                    return Ok(());
                }
                Err(error) => {
                    log::error!("Could not read referral settings: {error}");
                    edit_user_message_with_keyboard(
                        &bot,
                        target,
                        "خواندن وضعیت زیرمجموعه‌گیری ممکن نشد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                        subscription_back_keyboard(),
                    )
                    .await?;
                    return Ok(());
                }
            };
            let Some(username) = state.bot_username() else {
                log::error!("Bot username is unavailable for referral-link generation");
                edit_user_message_with_keyboard(
                    &bot,
                    target,
                    "ساخت لینک دعوت ممکن نشد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            };
            match state.referral_summary(query.from.id.0) {
                Ok((referrals, commissions)) => {
                    let link = format!("https://t.me/{username}?start=ref_{}", query.from.id.0);
                    let body = referral_message(&link, referrals, commissions, referral_percent);
                    edit_user_message_with_keyboard(&bot, target, &body, referral_keyboard(&link))
                        .await?;
                }
                Err(error) => {
                    log::error!(
                        "Could not read referral summary for user {}: {error}",
                        query.from.id
                    );
                    edit_user_message_with_keyboard(
                        &bot,
                        target,
                        "خواندن اطلاعات زیرمجموعه‌ها انجام نشد؛ لطفاً دوباره تلاش کنید.",
                        subscription_back_keyboard(),
                    )
                    .await?;
                }
            }
        }
        "user:wallet" => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            match state.wallet_summary(query.from.id.0) {
                Ok((balance, debt)) => {
                    edit_user_message_with_keyboard(
                        &bot,
                        target,
                        &wallet_message(balance, debt),
                        subscription_back_keyboard(),
                    )
                    .await?;
                }
                Err(error) => {
                    log::error!("Could not read wallet for user {}: {error}", query.from.id);
                    edit_user_message_with_keyboard(
                        &bot,
                        target,
                        "خواندن موجودی کیف پول انجام نشد؛ لطفاً دوباره تلاش کنید.",
                        subscription_back_keyboard(),
                    )
                    .await?;
                }
            }
        }
        data if data.starts_with("payment:cancel:") => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            let order_id = data
                .strip_prefix("payment:cancel:")
                .and_then(|value| value.parse::<i64>().ok());
            match order_id
                .map(|order_id| state.cancel_payment_order(order_id, query.from.id.0))
                .transpose()
            {
                Ok(Some(true)) => {
                    send_user_menu(&bot, query.from.id, "سفارش در انتظار رسید لغو شد.").await?;
                }
                Ok(Some(false)) | Ok(None) => {
                    send_user_menu(
                        &bot,
                        query.from.id,
                        "به منوی اصلی برگشتید؛ این سفارش دیگر در انتظار رسید نیست.",
                    )
                    .await?;
                }
                Err(error) => {
                    log::error!(
                        "Could not cancel payment order for user {}: {error}",
                        query.from.id
                    );
                    send_user_menu(
                        &bot,
                        query.from.id,
                        &format!(
                            "به صفحه‌ی اصلی برگشتید؛ لغو سفارش در انتظار رسید انجام نشد. برای پیگیری با پشتیبانی @{} تماس بگیرید.",
                            state.support_username()
                        ),
                    )
                    .await?;
                }
            }
        }
        "user:buy" => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            edit_user_message_with_keyboard(
                &bot,
                target,
                "مدت اشتراک را انتخاب کنید:",
                subscription_period_keyboard(),
            )
            .await?;
        }
        "user:renew" => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            send_panel_client_choices(
                &bot,
                target,
                &query.from,
                &state,
                ClientListPurpose::Renewal,
            )
            .await?;
        }
        data if data.starts_with("user:renew:select:") => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            let username = data.strip_prefix("user:renew:select:").unwrap_or_default();
            send_renewal_plans(&bot, target, &query.from, &state, username).await?;
        }
        data if data.starts_with("user:renew:plan:id:") => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            let selection = data.strip_prefix("user:renew:plan:id:").unwrap_or_default();
            let (tariff_id, username) = selection
                .split_once(':')
                .and_then(|(id, username)| id.parse::<i64>().ok().map(|id| (id, username)))
                .unwrap_or((0, ""));
            if tariff_id <= 0 || username.is_empty() {
                edit_user_message_with_keyboard(
                    &bot,
                    target,
                    "تعرفه یا کلاینت انتخاب‌شده معتبر نیست؛ دوباره از منو انتخاب کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
            } else {
                match state.subscription_tariff(tariff_id) {
                    Ok(Some(tariff)) if tariff.enabled => {
                        process_subscription_selection(
                            &bot,
                            target,
                            &query.from,
                            &tariff,
                            Some(username),
                            &state,
                        )
                        .await?;
                    }
                    Ok(_) => {
                        send_renewal_plans(&bot, target, &query.from, &state, username).await?;
                    }
                    Err(error) => {
                        log::error!("Could not load subscription tariff {tariff_id}: {error}");
                        edit_user_message_with_keyboard(
                            &bot,
                            target,
                            "خواندن تعرفه انجام نشد؛ لطفاً دوباره تلاش کنید.",
                            subscription_back_keyboard(),
                        )
                        .await?;
                    }
                }
            }
        }
        "user:plans:month" => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            match state.active_subscription_tariffs() {
                Ok(tariffs) if tariffs.is_empty() => {
                    edit_user_message_with_keyboard(
                        &bot,
                        target,
                        "در حال حاضر تعرفه فعالی برای خرید وجود ندارد.",
                        subscription_back_keyboard(),
                    )
                    .await?;
                }
                Ok(tariffs) => {
                    edit_user_message_with_keyboard(
                        &bot,
                        target,
                        "✨ تمام سرویس‌ها بدون محدودیت کاربر و دارای تانل‌های نیم‌بها هستند! 🚀\n\nپلن خریدتو انتخاب کن 👇",
                        monthly_plans_keyboard(&tariffs),
                    )
                    .await?;
                }
                Err(error) => {
                    log::error!("Could not list active subscription tariffs: {error}");
                    edit_user_message_with_keyboard(
                        &bot,
                        target,
                        "خواندن فهرست تعرفه‌ها انجام نشد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                        subscription_back_keyboard(),
                    )
                    .await?;
                }
            }
        }
        "user:main" => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            if let Err(error) = state.cancel_awaiting_order(query.from.id.0) {
                log::error!(
                    "Could not cancel awaiting payment order for user {}: {error}",
                    query.from.id
                );
            }
            send_user_menu(&bot, query.from.id, USER_MENU_PROMPT).await?;
        }
        "user:menu" => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            send_user_menu(&bot, query.from.id, USER_MENU_PROMPT).await?;
        }
        "user:usage" => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            send_panel_client_choices(&bot, target, &query.from, &state, ClientListPurpose::Usage)
                .await?;
        }
        "user:link" => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            send_panel_client_choices(&bot, target, &query.from, &state, ClientListPurpose::Link)
                .await?;
        }
        data if data.starts_with("user:usage:select:") => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            let username = data.strip_prefix("user:usage:select:").unwrap_or_default();
            send_subscription_usage(&bot, target, &query.from, &state, username).await?;
        }
        data if data.starts_with("user:link:select:") => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            let username = data.strip_prefix("user:link:select:").unwrap_or_default();
            send_client_subscription_link(&bot, target, &query.from, &state, username).await?;
        }
        "user:support" => {
            let support_username = state.support_username();
            edit_user_message_with_keyboard(
                &bot,
                target,
                &format!("برای ارتباط با پشتیبانی @{support_username} از دکمه زیر استفاده کن 👇🏼"),
                support_keyboard(&support_username),
            )
            .await?;
        }
        data if data.starts_with("user:pc:")
            || data.starts_with("user:pw:")
            || data.starts_with("user:rc:")
            || data.starts_with("user:rw:") =>
        {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            let Some((tariff_id, renewal_username, use_wallet)) =
                parse_subscription_checkout_callback(data)
            else {
                edit_user_message_with_keyboard(
                    &bot,
                    target,
                    "اطلاعات سفارش معتبر نیست؛ لطفاً دوباره تعرفه را انتخاب کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            };
            match state.subscription_tariff(tariff_id) {
                Ok(Some(tariff)) if tariff.enabled => {
                    create_subscription_order(
                        &bot,
                        target,
                        &query.from,
                        &tariff,
                        renewal_username,
                        use_wallet,
                        &state,
                    )
                    .await?;
                }
                Ok(_) => {
                    edit_user_message_with_keyboard(
                        &bot,
                        target,
                        "این تعرفه دیگر فعال نیست؛ لطفاً تعرفه‌ی دیگری انتخاب کنید.",
                        subscription_back_keyboard(),
                    )
                    .await?;
                }
                Err(error) => {
                    log::error!("Could not read tariff {tariff_id} during checkout: {error}");
                    edit_user_message_with_keyboard(
                        &bot,
                        target,
                        "خواندن تعرفه انجام نشد؛ لطفاً دوباره تلاش کنید.",
                        subscription_back_keyboard(),
                    )
                    .await?;
                }
            }
        }
        data if data.starts_with("payment:approve:") && (is_owner || is_admin) => {
            if let Some(order_id) = data
                .strip_prefix("payment:approve:")
                .and_then(|value| value.parse::<i64>().ok())
            {
                let source_message = query.regular_message().and_then(|message| {
                    message
                        .caption()
                        .or_else(|| message.text())
                        .map(|text| (message.id, text.to_owned(), message.text().is_some()))
                });
                approve_payment_order(&bot, query.from.id, &state, order_id, source_message)
                    .await?;
            } else {
                bot.send_message(query.from.id, "شناسه‌ی سفارش معتبر نیست.")
                    .await?;
            }
        }
        data if data.starts_with("payment:reject:") && (is_owner || is_admin) => {
            if let Some(order_id) = data
                .strip_prefix("payment:reject:")
                .and_then(|value| value.parse::<i64>().ok())
            {
                let source_message = query.regular_message().and_then(|message| {
                    message
                        .caption()
                        .or_else(|| message.text())
                        .map(|text| (message.id, text.to_owned(), message.text().is_some()))
                });
                reject_payment_order(&bot, query.from.id, &state, order_id, source_message).await?;
            } else {
                bot.send_message(query.from.id, "شناسه‌ی سفارش معتبر نیست.")
                    .await?;
            }
        }
        data if data.starts_with("user:plan:id:") => {
            if !is_owner
                && !ensure_required_channel_member(&bot, &state, target, query.from.id).await?
            {
                return Ok(());
            }
            let tariff_id = data
                .strip_prefix("user:plan:id:")
                .and_then(|value| value.parse::<i64>().ok());
            match tariff_id {
                Some(tariff_id) => match state.subscription_tariff(tariff_id) {
                    Ok(Some(tariff)) if tariff.enabled => {
                        process_subscription_selection(
                            &bot,
                            target,
                            &query.from,
                            &tariff,
                            None,
                            &state,
                        )
                        .await?
                    }
                    Ok(_) => {
                        let tariffs = match state.active_subscription_tariffs() {
                            Ok(tariffs) => tariffs,
                            Err(error) => {
                                log::error!("Could not list active subscription tariffs: {error}");
                                edit_user_message_with_keyboard(
                                    &bot,
                                    target,
                                    "خواندن تعرفه‌ها انجام نشد؛ لطفاً دوباره تلاش کنید.",
                                    subscription_back_keyboard(),
                                )
                                .await?;
                                return Ok(());
                            }
                        };
                        edit_user_message_with_keyboard(
                            &bot,
                            target,
                            "این تعرفه دیگر فعال نیست؛ یکی از تعرفه‌های فعال را انتخاب کنید.",
                            monthly_plans_keyboard(&tariffs),
                        )
                        .await?;
                    }
                    Err(error) => {
                        log::error!("Could not read subscription tariff {tariff_id}: {error}");
                        edit_user_message_with_keyboard(
                            &bot,
                            target,
                            "خواندن تعرفه انجام نشد؛ لطفاً دوباره تلاش کنید.",
                            subscription_back_keyboard(),
                        )
                        .await?;
                    }
                },
                None => match state.active_subscription_tariffs() {
                    Ok(tariffs) => {
                        edit_user_message_with_keyboard(
                            &bot,
                            target,
                            "تعرفه‌ی انتخاب‌شده معتبر نیست؛ دوباره از منو انتخاب کنید.",
                            monthly_plans_keyboard(&tariffs),
                        )
                        .await?;
                    }
                    Err(error) => {
                        log::error!("Could not list active subscription tariffs: {error}");
                        edit_user_message_with_keyboard(
                            &bot,
                            target,
                            "خواندن تعرفه‌ها انجام نشد؛ لطفاً دوباره تلاش کنید.",
                            subscription_back_keyboard(),
                        )
                        .await?;
                    }
                },
            }
        }
        "user:join:check" => match is_required_channel_member(&bot, &state, query.from.id).await {
            Ok(true) => {
                log::info!("User {} joined the required channel", query.from.id);
                let prompt = start_user_menu_prompt(&query.from.first_name);
                edit_user_menu(&bot, target, &prompt).await?;
            }
            Ok(false) => {
                edit_user_message_with_keyboard(
                    &bot,
                    target,
                    "هنوز عضویت شما در همه‌ی کانال‌ها تأیید نشده است. ابتدا عضو شوید و دوباره بررسی کنید.",
                    required_channel_join_keyboard(&state)?,
                )
                .await?;
            }
            Err(error) => {
                log::error!(
                    "Could not verify channel membership for user {}: {error}",
                    query.from.id
                );
                edit_user_message_with_keyboard(
                    &bot,
                    target,
                    "بررسی عضویت ممکن نشد. لطفاً بعداً دوباره تلاش کنید.",
                    required_channel_join_keyboard(&state)?,
                )
                .await?;
            }
        },
        _ => {}
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum ClientListPurpose {
    Renewal,
    Usage,
    Link,
}

async fn panel_api_context(
    bot: &Bot,
    state: &AppState,
    operation: &str,
) -> Result<Option<(Url, Zeroizing<String>)>> {
    let panel_config = match state.panel_config() {
        Ok(config) => config,
        Err(error) => {
            notify_panel_issue(
                bot,
                state,
                &format!("{operation}: خواندن تنظیمات پنل ناموفق بود: {error}"),
            )
            .await;
            return Err(error).context("Could not read panel configuration");
        }
    };
    let Some((saved_url, _)) = panel_config else {
        notify_panel_issue(
            bot,
            state,
            &format!("{operation}: تنظیمات پنل ثبت نشده است."),
        )
        .await;
        return Ok(None);
    };
    let base_url = match panel::normalize_panel_url(&saved_url) {
        Ok(url) => url,
        Err(error) => {
            notify_panel_issue(bot, state, &format!("{operation}: {error}")).await;
            return Err(anyhow::Error::new(error));
        }
    };
    let (mut base_url, mut access_token) = match state.panel_access_token() {
        Some(access_token) => (base_url, access_token),
        None => match connect_panel_with_retries(state, operation).await {
            Ok(context) => context,
            Err(error) => {
                notify_panel_issue(
                    bot,
                    state,
                    &format!("{operation}: {}", error.user_message()),
                )
                .await;
                return Ok(None);
            }
        },
    };
    match retry_panel_request!(
        state,
        base_url,
        access_token,
        operation,
        panel::check_connection(&base_url, access_token.as_str())
    ) {
        Ok(()) => Ok(Some((base_url, access_token))),
        Err(error) => {
            notify_panel_issue(
                bot,
                state,
                &format!("{operation}: {}", error.user_message()),
            )
            .await;
            Ok(None)
        }
    }
}

async fn send_panel_client_choices(
    bot: &Bot,
    target: Option<CallbackMessageTarget>,
    telegram_user: &User,
    state: &AppState,
    purpose: ClientListPurpose,
) -> Result<()> {
    let (mut base_url, mut access_token) =
        match panel_api_context(bot, state, "خواندن فهرست کلاینت‌ها").await {
            Ok(Some(context)) => context,
            Ok(None) => {
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "بررسی اشتراک فعلاً در دسترس نیست؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
            Err(error) => {
                log::error!("Could not prepare panel connection for client listing: {error:#}");
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "خواندن اطلاعات پنل انجام نشد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
        };
    let clients = match retry_panel_request!(
        state,
        base_url,
        access_token,
        "list panel clients",
        panel::panel_clients_for_telegram_user(
            &base_url,
            access_token.as_str(),
            telegram_user.id.0
        )
    ) {
        Ok(clients) => clients,
        Err(error) => {
            log::warn!(
                "Could not list panel clients for user {}: {error}",
                telegram_user.id
            );
            notify_panel_issue(bot, state, &error.user_message()).await;
            edit_user_message_with_keyboard(
                bot,
                target,
                &error.user_message(),
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
    };
    if clients.is_empty() {
        let message = match purpose {
            ClientListPurpose::Renewal => {
                "برای تمدید، هنوز کلاینتی در پنل ندارید. برای ساخت اشتراک جدید از دکمه‌ی خرید استفاده کنید."
            }
            ClientListPurpose::Usage => "برای شناسه‌ی تلگرام شما کلاینتی در پنل پیدا نشد.",
            ClientListPurpose::Link => "برای شناسه‌ی تلگرام شما کلاینتی در پنل پیدا نشد.",
        };
        edit_user_menu(bot, target, message).await?;
        return Ok(());
    }

    let prompt = match purpose {
        ClientListPurpose::Renewal => {
            "کلاینتی را که می‌خواهید تمدید کنید انتخاب کنید:\n🟢 فعال  🔴 منقضی"
        }
        ClientListPurpose::Usage => {
            "کلاینتی را انتخاب کنید تا حجم و زمان باقی‌مانده‌ی همان اشتراک را ببینید:\n🟢 فعال  🔴 منقضی"
        }
        ClientListPurpose::Link => {
            "کلاینتی را انتخاب کنید تا لینک سابسکریپشن همان کلاینت برایتان ارسال شود:\n🟢 فعال  🔴 منقضی"
        }
    };
    edit_user_message_with_keyboard(
        bot,
        target,
        prompt,
        panel_client_keyboard(&clients, purpose),
    )
    .await?;
    Ok(())
}

async fn send_renewal_plans(
    bot: &Bot,
    target: Option<CallbackMessageTarget>,
    telegram_user: &User,
    state: &AppState,
    selected_username: &str,
) -> Result<()> {
    let (mut base_url, mut access_token) =
        match panel_api_context(bot, state, "بررسی کلاینت برای تمدید").await {
            Ok(Some(context)) => context,
            Ok(None) => {
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "بررسی اشتراک فعلاً در دسترس نیست؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
            Err(error) => {
                log::error!("Could not prepare panel connection for renewal: {error:#}");
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "خواندن اطلاعات پنل انجام نشد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
        };
    let clients = match retry_panel_request!(
        state,
        base_url,
        access_token,
        "verify renewal client",
        panel::panel_clients_for_telegram_user(
            &base_url,
            access_token.as_str(),
            telegram_user.id.0
        )
    ) {
        Ok(clients) => clients,
        Err(error) => {
            log::warn!(
                "Could not verify renewal client for user {}: {error}",
                telegram_user.id
            );
            notify_panel_issue(bot, state, &error.user_message()).await;
            edit_user_message_with_keyboard(
                bot,
                target,
                &error.user_message(),
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
    };
    let Some(client) = clients
        .iter()
        .find(|client| client.username == selected_username)
    else {
        edit_user_message_with_keyboard(
            bot,
            target,
            "این کلاینت در حساب شما پیدا نشد؛ فهرست را دوباره باز کنید.",
            subscription_back_keyboard(),
        )
        .await?;
        return Ok(());
    };
    let tariffs = match state.active_subscription_tariffs() {
        Ok(tariffs) => tariffs,
        Err(error) => {
            log::error!("Could not list active subscription tariffs: {error}");
            edit_user_message_with_keyboard(
                bot,
                target,
                "خواندن فهرست تعرفه‌ها انجام نشد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
    };
    if tariffs.is_empty() {
        edit_user_message_with_keyboard(
            bot,
            target,
            "در حال حاضر تعرفه فعالی برای تمدید وجود ندارد.",
            subscription_back_keyboard(),
        )
        .await?;
        return Ok(());
    }
    edit_user_message_with_keyboard(
        bot,
        target,
        &format!("بسته‌ی تمدید برای «{}» را انتخاب کنید:", client.username),
        monthly_renewal_plans_keyboard(&tariffs, &client.username),
    )
    .await?;
    Ok(())
}

async fn send_subscription_usage(
    bot: &Bot,
    target: Option<CallbackMessageTarget>,
    telegram_user: &User,
    state: &AppState,
    username: &str,
) -> Result<()> {
    let (mut base_url, mut access_token) =
        match panel_api_context(bot, state, "خواندن وضعیت اشتراک").await {
            Ok(Some(context)) => context,
            Ok(None) => {
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "بررسی موجودی اشتراک فعلاً ممکن نیست؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
            Err(error) => {
                log::error!("Could not prepare panel connection for usage lookup: {error:#}");
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "خواندن اطلاعات پنل انجام نشد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
        };

    match retry_panel_request!(
        state,
        base_url,
        access_token,
        "read subscription usage",
        panel::subscription_usage(
            &base_url,
            access_token.as_str(),
            telegram_user.id.0,
            username
        )
    ) {
        Ok(Some(usage)) => {
            edit_user_message_with_keyboard(
                bot,
                target,
                &subscription_usage_message(&usage),
                subscription_back_keyboard(),
            )
            .await?;
        }
        Ok(None) => {
            edit_user_message_with_keyboard(
            bot,
            target,
            &format!(
                "برای حساب شما اشتراکی در پنل پیدا نشد. اگر قبلاً پرداخت کرده‌اید، لطفاً با پشتیبانی @{} تماس بگیرید.",
                state.support_username()
            ),
            subscription_back_keyboard(),
        )
            .await?;
        }
        Err(error) => {
            log::warn!(
                "Could not look up subscription usage for user {}: {error}",
                telegram_user.id
            );
            notify_panel_issue(bot, state, &error.user_message()).await;
            edit_user_message_with_keyboard(
                bot,
                target,
                &error.user_message(),
                subscription_back_keyboard(),
            )
            .await?;
        }
    }

    Ok(())
}

async fn send_client_subscription_link(
    bot: &Bot,
    target: Option<CallbackMessageTarget>,
    telegram_user: &User,
    state: &AppState,
    username: &str,
) -> Result<()> {
    let (mut base_url, mut access_token) =
        match panel_api_context(bot, state, "دریافت لینک اشتراک").await {
            Ok(Some(context)) => context,
            Ok(None) => {
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "دریافت لینک کانفیگ فعلاً ممکن نیست؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
            Err(error) => {
                log::error!("Could not prepare panel connection for link retrieval: {error:#}");
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "خواندن اطلاعات پنل انجام نشد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
        };

    match retry_panel_request!(
        state,
        base_url,
        access_token,
        "retrieve subscription link",
        panel::subscription_link(
            &base_url,
            access_token.as_str(),
            telegram_user.id.0,
            username
        )
    ) {
        Ok(Some(url)) => {
            let caption = subscription_link_caption(username, &url);
            if let Err(error) = edit_subscription_qr(bot, target, &url, &caption).await {
                log::error!(
                    "Could not deliver subscription link for user {}: {error}",
                    telegram_user.id
                );
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    &format!(
                        "ارسال لینک اشتراک با خطا روبه‌رو شد؛ لطفاً دوباره تلاش کنید یا با پشتیبانی @{} تماس بگیرید.",
                        state.support_username()
                    ),
                    subscription_back_keyboard(),
                )
                .await?;
            }
        }
        Ok(None) => {
            edit_user_message_with_keyboard(
                bot,
                target,
                "این کلاینت در پنل پیدا نشد؛ فهرست را دوباره باز کنید.",
                subscription_back_keyboard(),
            )
            .await?;
        }
        Err(error) => {
            log::warn!(
                "Could not retrieve subscription link for user {}: {error}",
                telegram_user.id
            );
            notify_panel_issue(bot, state, &error.user_message()).await;
            edit_user_message_with_keyboard(
                bot,
                target,
                &error.user_message(),
                subscription_back_keyboard(),
            )
            .await?;
        }
    }

    Ok(())
}

fn subscription_qr_png(subscription_url: &str) -> Result<Vec<u8>> {
    let code = QrCode::new(subscription_url.as_bytes())
        .context("Could not encode the subscription URL as a QR code")?;
    let image = code.render::<Luma<u8>>().min_dimensions(512, 512).build();
    let mut buffer = Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(image)
        .write_to(&mut buffer, ImageFormat::Png)
        .context("Could not encode the subscription QR code as PNG")?;
    Ok(buffer.into_inner())
}

fn subscription_link_caption(username: &str, subscription_url: &str) -> String {
    format!(
        "📲 لینک کانفیگ کلاینت {}\n(برای کپی لینک، کافی است یک‌بار روی آن ضربه بزنید)\n\n<code>{}</code>",
        html_escape(username),
        html_escape(subscription_url)
    )
}

async fn send_subscription_qr(
    bot: &Bot,
    chat_id: UserId,
    subscription_url: &str,
    caption: &str,
) -> ResponseResult<()> {
    let qr_error = match subscription_qr_png(subscription_url) {
        Ok(image) => match bot
            .send_photo(
                chat_id,
                InputFile::memory(image).file_name("subscription-qr.png"),
            )
            .caption(caption)
            .parse_mode(ParseMode::Html)
            .reply_markup(subscription_back_keyboard())
            .await
        {
            Ok(_) => None,
            Err(error) => {
                log::error!("Could not send subscription QR photo to {chat_id}: {error}");
                Some("ارسال تصویر QR ناموفق بود.")
            }
        },
        Err(error) => {
            log::error!("Could not generate subscription QR photo for {chat_id}: {error:#}");
            Some("ساخت تصویر QR ناموفق بود.")
        }
    };

    if let Some(error) = qr_error {
        bot.send_message(
            chat_id,
            format!("{caption}\n\n⚠️ {error} لینک اشتراک در همین پیام به‌صورت متن ارسال شده است."),
        )
        .parse_mode(ParseMode::Html)
        .reply_markup(subscription_back_keyboard())
        .await?;
    }
    Ok(())
}

async fn edit_subscription_qr(
    bot: &Bot,
    target: Option<CallbackMessageTarget>,
    subscription_url: &str,
    caption: &str,
) -> Result<()> {
    let target = require_callback_message_target(target)?;
    let keyboard = subscription_back_keyboard();
    let fallback_text = |error: &str| {
        format!("{caption}\n\n⚠️ {error} لینک اشتراک در همین پیام به‌صورت متن ارسال شده است.")
    };
    match subscription_qr_png(subscription_url) {
        Ok(image) => {
            if let Err(error) = edit_photo_message(bot, target, image, caption, &keyboard).await {
                log::error!(
                    "Could not edit message to subscription QR for {}: {error:#}",
                    target.chat_id
                );
                edit_text_message(
                    bot,
                    target,
                    fallback_text("ارسال تصویر QR ناموفق بود."),
                    Some("HTML"),
                    Some(keyboard),
                )
                .await?;
            }
        }
        Err(error) => {
            log::error!(
                "Could not generate subscription QR photo for {}: {error:#}",
                target.chat_id
            );
            edit_text_message(
                bot,
                target,
                fallback_text("ساخت تصویر QR ناموفق بود."),
                Some("HTML"),
                Some(keyboard),
            )
            .await?;
        }
    }
    Ok(())
}

fn subscription_back_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "🏠 بازگشت به منوی اصلی",
        "user:menu",
    )]])
}

async fn process_subscription_selection(
    bot: &Bot,
    target: Option<CallbackMessageTarget>,
    telegram_user: &User,
    tariff: &SubscriptionTariff,
    renewal_username: Option<&str>,
    state: &AppState,
) -> Result<()> {
    let (wallet_balance, _) = match state.wallet_summary(telegram_user.id.0) {
        Ok(summary) => summary,
        Err(error) => {
            log::error!(
                "Could not read wallet for user {} before checkout: {error}",
                telegram_user.id
            );
            edit_user_message_with_keyboard(
                bot,
                target,
                "خواندن موجودی کیف پول ممکن نشد؛ لطفاً دوباره تلاش کنید.",
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
    };
    let subscription_kind = if renewal_username.is_some() {
        "♻️ تمدید اشتراک"
    } else {
        "🆕 خرید اشتراک جدید"
    };
    let message = format!(
        "🧾 جزئیات اشتراک\n{subscription_kind}\n📦 بسته: {} گیگابایت | ⏳ ۳۰ روز\n💰 مبلغ اشتراک: {}\n👛 موجودی دقیق کیف پول: {}\n\nℹ️ اگر موجودی کیف پول کمتر از مبلغ اشتراک است، با دکمه‌ی «پرداخت با کیف پول» مبلغ نهایی فاکتور را کاهش دهید.",
        persian_digits(tariff.volume_gb),
        format_toman(tariff.price_toman),
        format_signed_toman(wallet_balance)
    );
    edit_user_message_with_bold_lines(
        bot,
        target,
        &message,
        subscription_payment_options_keyboard(tariff.id, renewal_username),
        &["💰 مبلغ اشتراک:"],
    )
    .await
}

async fn create_subscription_order(
    bot: &Bot,
    target: Option<CallbackMessageTarget>,
    telegram_user: &User,
    tariff: &SubscriptionTariff,
    renewal_username: Option<&str>,
    use_wallet: bool,
    state: &AppState,
) -> Result<()> {
    let target = require_callback_message_target(target)?;
    let telegram_user_id = telegram_user.id;

    let (mut base_url, mut access_token) =
        match panel_api_context(bot, state, "بررسی پنل برای ثبت سفارش").await {
            Ok(Some(context)) => context,
            Ok(None) => {
                edit_user_message_with_keyboard(
                bot,
                target,
                "اتصال پنل برقرار نیست و امکان ثبت سفارش وجود ندارد. لطفاً کمی بعد دوباره تلاش کنید.",
                subscription_back_keyboard(),
            )
            .await?;
                return Ok(());
            }
            Err(error) => {
                log::error!("Could not prepare panel connection for order: {error:#}");
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "خواندن تنظیمات پنل انجام نشد. لطفاً بعداً دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
        };
    let clients = match retry_panel_request!(
        state,
        base_url,
        access_token,
        "verify client before order",
        panel::panel_clients_for_telegram_user(
            &base_url,
            access_token.as_str(),
            telegram_user_id.0
        )
    ) {
        Ok(clients) => clients,
        Err(error) => {
            log::warn!(
                "Could not verify panel clients for user {}: {error}",
                telegram_user_id
            );
            notify_panel_issue(bot, state, &error.user_message()).await;
            edit_user_message_with_keyboard(
                bot,
                target,
                &error.user_message(),
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
    };

    let (panel_username, is_renewal) = if let Some(selected_username) = renewal_username {
        if !clients
            .iter()
            .any(|client| client.username == selected_username)
        {
            edit_user_message_with_keyboard(
                bot,
                target,
                "کلاینت انتخاب‌شده متعلق به حساب شما نیست یا دیگر در پنل وجود ندارد.",
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
        (selected_username.to_owned(), true)
    } else {
        let mut reserved = match state.reserved_panel_usernames(telegram_user_id.0) {
            Ok(usernames) => usernames,
            Err(error) => {
                log::error!(
                    "Could not read reserved panel usernames for user {}: {error}",
                    telegram_user_id
                );
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "ثبت سفارش ممکن نشد؛ لطفاً کمی بعد دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
        };
        for client in &clients {
            if !reserved.contains(&client.username) {
                reserved.push(client.username.clone());
            }
        }
        let username = match retry_panel_request!(
            state,
            base_url,
            access_token,
            "suggest panel username",
            panel::suggest_available_panel_username(
                &base_url,
                access_token.as_str(),
                telegram_user_id.0,
                &reserved
            )
        ) {
            Ok(Some(username)) => username,
            Ok(None) => {
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    &format!(
                        "ظرفیت ساخت نام کاربری جدید برای حساب شما تکمیل شده است؛ لطفاً با پشتیبانی @{} تماس بگیرید.",
                        state.support_username()
                    ),
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
            Err(error) => {
                log::warn!(
                    "Could not choose an available username for user {}: {error}",
                    telegram_user_id
                );
                notify_panel_issue(bot, state, &error.user_message()).await;
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    &error.user_message(),
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
        };
        (username, false)
    };
    let account_name = std::iter::once(telegram_user.first_name.as_str())
        .chain(telegram_user.last_name.as_deref())
        .collect::<Vec<_>>()
        .join(" ");
    let order_snapshot = match state.create_tariff_payment_order(NewTariffPaymentOrder {
        telegram_user_id: telegram_user_id.0,
        account_name: &account_name,
        telegram_username: telegram_user.username.as_deref(),
        panel_username: &panel_username,
        tariff_id: tariff.id,
        use_wallet,
        is_renewal,
    }) {
        Ok(Some(order)) => order,
        Ok(None) => {
            match state.active_subscription_tariffs() {
                Ok(tariffs) if !tariffs.is_empty() => {
                    edit_user_message_with_keyboard(
                        bot,
                        target,
                        "این تعرفه در همین لحظه غیرفعال شده است؛ لطفاً یکی از تعرفه‌های فعال را انتخاب کنید.",
                        monthly_plans_keyboard(&tariffs),
                    )
                    .await?;
                }
                Ok(_) => {
                    edit_user_message_with_keyboard(
                        bot,
                        target,
                        "در حال حاضر تعرفه فعالی وجود ندارد.",
                        subscription_back_keyboard(),
                    )
                    .await?;
                }
                Err(error) => {
                    log::error!("Could not list active subscription tariffs: {error}");
                    edit_user_message_with_keyboard(
                        bot,
                        target,
                        "خواندن تعرفه‌ها انجام نشد؛ لطفاً دوباره تلاش کنید.",
                        subscription_back_keyboard(),
                    )
                    .await?;
                }
            }
            return Ok(());
        }
        Err(CreateTariffPaymentOrderError::OrderAwaitingAdminReview) => {
            edit_user_message_with_keyboard(
                bot,
                target,
                &format!(
                    "⏳ در حال حاضر شما یک سفارش باز دارید.\nلطفاً منتظر تأیید ادمین باشید.\nدر صورت بروز مشکل، با پشتیبانی @{} در ارتباط باشید.",
                    state.support_username()
                ),
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
        Err(CreateTariffPaymentOrderError::Database(error)) => {
            log::error!(
                "Could not create payment order for Telegram user {}: {error}",
                telegram_user_id
            );
            edit_user_message_with_keyboard(
                bot,
                target,
                &format!(
                    "ثبت سفارش ممکن نشد. لطفاً دوباره تلاش کنید یا با پشتیبانی @{} تماس بگیرید.",
                    state.support_username()
                ),
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
    };
    let order_id = order_snapshot.id;
    let plan_gb = order_snapshot.plan_gb;
    let price_toman = order_snapshot.price_toman;
    let wallet_used_toman = order_snapshot.wallet_used_toman;
    let amount_due_toman = order_snapshot.amount_due_toman;
    let wallet_balance_after = order_snapshot.wallet_balance_after;
    if use_wallet && wallet_used_toman == 0 {
        state.cancel_payment_order(order_id, telegram_user_id.0)?;
        edit_user_message_with_keyboard(
            bot,
            target,
            "⚠️ موجودی قابل استفاده‌ای در کیف پول ندارید. برای ادامه، گزینه‌ی «کارت‌به‌کارت» را انتخاب کنید.",
            subscription_payment_options_keyboard(tariff.id, renewal_username),
        )
        .await?;
        return Ok(());
    }

    if amount_due_toman == 0 {
        let order = match state.payment_order(order_id) {
            Ok(Some(order)) => order,
            Ok(None) => {
                log::error!("Wallet payment order {order_id} disappeared after creation");
                state.cancel_payment_order(order_id, telegram_user_id.0)?;
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "خواندن سفارش کیف پول ممکن نشد؛ لطفاً دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
            Err(error) => {
                log::error!("Could not read wallet payment order {order_id}: {error}");
                state.cancel_payment_order(order_id, telegram_user_id.0)?;
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "خواندن سفارش کیف پول انجام نشد؛ لطفاً دوباره تلاش کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
                return Ok(());
            }
        };
        if !send_wallet_order_for_review(bot, state, &order).await? {
            state.cancel_payment_order(order_id, telegram_user_id.0)?;
            edit_user_message_with_keyboard(
                bot,
                target,
                "❌ درخواست برای مالک یا ادمین‌ها ارسال نشد و موجودی شما کسر نشد. لطفاً بعداً دوباره تلاش کنید.",
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
        edit_user_message_with_bold_lines(
            bot,
            target,
            &format!(
                "✅ درخواست خرید با کیف پول ثبت شد.\n📦 بسته: {} گیگابایت\n💰 مبلغ اصلی اشتراک: {}\n👛 مبلغ استفاده‌شده: {}\n💳 مبلغ نهایی پرداخت: {}\n💵 مانده‌ی کیف پول پس از تأیید: {}\n\n⏳ مبلغ استفاده‌شده پس از تأیید کسر می‌شود و اشتراک فعال خواهد شد.",
                persian_digits(plan_gb),
                format_toman(price_toman),
                format_toman(wallet_used_toman),
                format_toman(amount_due_toman),
                format_signed_toman(wallet_balance_after)
            ),
            subscription_back_keyboard(),
            &["💳 مبلغ نهایی پرداخت:"],
        )
        .await?;
        return Ok(());
    }

    let payment_details = match state.payment_details() {
        Ok(Some(details)) => Some(details),
        Ok(None) => {
            let card_number = env::var("PAYMENT_CARD_NUMBER")
                .ok()
                .and_then(|value| normalize_card_number(value.trim()));
            let cardholder_name = env::var("PAYMENT_CARDHOLDER_NAME")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty());
            card_number.zip(cardholder_name)
        }
        Err(error) => {
            log::error!("Could not read payment card details: {error}");
            state.cancel_payment_order(order_id, telegram_user_id.0)?;
            edit_user_message_with_keyboard(
                bot,
                target,
                "خواندن اطلاعات کارت انجام نشد و رزرو کیف پول لغو شد؛ لطفاً بعداً دوباره تلاش کنید.",
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
    };
    let Some((card_number, cardholder_name)) = payment_details else {
        log::error!("Payment card configuration is missing");
        state.cancel_payment_order(order_id, telegram_user_id.0)?;
        edit_user_message_with_keyboard(
            bot,
            target,
            &format!(
                "اطلاعات کارت پرداخت تنظیم نشده است و رزرو کیف پول لغو شد. لطفاً با پشتیبانی @{} تماس بگیرید.",
                state.support_username()
            ),
            subscription_back_keyboard(),
        )
        .await?;
        return Ok(());
    };

    let kind = if is_renewal {
        "♻️ تمدید اشتراک"
    } else {
        "🆕 خرید جدید"
    };
    let support_username = state.support_username();
    edit_text_message(
        bot,
        target,
        format!(
            "🧾 جزئیات سفارش\n{kind}\n📦 بسته: {} گیگابایت | ⏳ مدت: ۳۰ روز\n💰 مبلغ اصلی اشتراک: {}\n👛 کسر از کیف پول: {}\n<b>💳 مبلغ نهایی قابل پرداخت: {}</b>\n👛 موجودی پس از تأیید پرداخت: {}\n\n💳 شماره کارت:\n(برای کپی، یک‌بار روی شماره کارت ضربه بزنید)\n\n<blockquote><code>{}</code></blockquote>\n👤 به نام: {}\n\n🧾 پس از واریز مبلغ نهایی، عکس رسید را همین‌جا ارسال کنید.\n⚠️ فقط عکس رسید پذیرفته می‌شود.\n🆘 پشتیبانی: @{}",
            persian_digits(plan_gb),
            format_toman(price_toman),
            format_toman(wallet_used_toman),
            format_toman(amount_due_toman),
            format_signed_toman(wallet_balance_after),
            html_escape(&card_number),
            html_escape(&cardholder_name),
            html_escape(&support_username)
        ),
        Some("HTML"),
        Some(payment_order_keyboard(order_id)),
    )
    .await?;

    Ok(())
}

async fn send_wallet_order_for_review(
    bot: &Bot,
    state: &AppState,
    order: &PaymentOrder,
) -> Result<bool> {
    let caption = payment_review_caption(order);
    let mut reviewers = vec![state.owner_id.0];
    match state.admins() {
        Ok(admins) => reviewers.extend(admins),
        Err(error) => log::error!(
            "Could not load reviewers for wallet order {}: {error}",
            order.id
        ),
    }
    reviewers.sort_unstable();
    reviewers.dedup();

    let mut delivered = 0usize;
    for reviewer_id in reviewers {
        match bot
            .send_message(UserId(reviewer_id), caption.clone())
            .parse_mode(ParseMode::Html)
            .reply_markup(payment_review_keyboard(order.id))
            .await
        {
            Ok(message) => {
                delivered += 1;
                if let Err(error) = state.record_payment_review_message(
                    order.id,
                    reviewer_id,
                    message.id.0,
                    &caption,
                    true,
                ) {
                    log::error!(
                        "Could not track wallet review message {} for order {} and reviewer {reviewer_id}: {error}",
                        message.id.0,
                        order.id
                    );
                }
            }
            Err(error) => log::warn!(
                "Could not deliver wallet order {} to reviewer {reviewer_id}: {error}",
                order.id
            ),
        }
    }
    Ok(delivered > 0)
}

async fn receive_payment_receipt(
    bot: &Bot,
    msg: &Message,
    state: &AppState,
    order_id: i64,
) -> ResponseResult<()> {
    let Some(sender) = msg.from.as_ref() else {
        return Ok(());
    };
    let support_username = state.support_username();
    let Some(photo) = msg.photo().and_then(|photos| photos.last()) else {
        bot.send_message(
            msg.chat.id,
            format!("فقط عکس رسید تراکنش قابل ارسال است؛ لطفاً عکس رسید را همین‌جا بفرستید.\nاگر به عکس رسید دسترسی ندارید، با پشتیبانی @{support_username} در ارتباط باشید."),
        )
        .await?;
        return Ok(());
    };

    let file_id = photo.file.id.to_string();
    match state.submit_order_receipt(order_id, sender.id.0, &file_id) {
        Ok(true) => {}
        Ok(false) => {
            bot.send_message(
                msg.chat.id,
                "این سفارش دیگر در انتظار رسید نیست. برای ثبت سفارش جدید از منوی خرید اقدام کنید.",
            )
            .await?;
            return Ok(());
        }
        Err(error) => {
            log::error!("Could not store receipt for order {order_id}: {error}");
            bot.send_message(
                msg.chat.id,
                "ذخیره‌ی رسید انجام نشد؛ لطفاً دوباره عکس را ارسال کنید.",
            )
            .await?;
            return Ok(());
        }
    }

    let order = match state.payment_order(order_id) {
        Ok(Some(order)) => order,
        Ok(None) => {
            log::error!("Payment order {order_id} disappeared after receipt submission");
            if let Err(error) = state.reset_order_receipt(order_id) {
                log::error!(
                    "Could not restore order {order_id} to receipt-waiting status: {error}"
                );
            }
            bot.send_message(
                msg.chat.id,
                format!("خواندن سفارش پس از دریافت رسید انجام نشد. لطفاً عکس رسید را دوباره ارسال کنید یا با پشتیبانی @{support_username} تماس بگیرید."),
            )
            .await?;
            return Ok(());
        }
        Err(error) => {
            log::error!("Could not load payment order {order_id}: {error}");
            if let Err(db_error) = state.reset_order_receipt(order_id) {
                log::error!(
                    "Could not restore order {order_id} to receipt-waiting status: {db_error}"
                );
            }
            bot.send_message(
                msg.chat.id,
                format!("خواندن سفارش پس از دریافت رسید انجام نشد. لطفاً کمی بعد عکس را دوباره ارسال کنید یا با پشتیبانی @{support_username} تماس بگیرید."),
            )
            .await?;
            return Ok(());
        }
    };
    let caption = payment_review_caption(&order);
    let mut reviewers = vec![state.owner_id.0];
    match state.admins() {
        Ok(admins) => reviewers.extend(admins),
        Err(error) => log::error!("Could not load payment reviewers: {error}"),
    }
    reviewers.sort_unstable();
    reviewers.dedup();

    let mut delivered = 0usize;
    for reviewer_id in reviewers {
        match bot
            .send_photo(
                UserId(reviewer_id),
                InputFile::file_id(FileId(file_id.clone())),
            )
            .caption(caption.clone())
            .parse_mode(ParseMode::Html)
            .reply_markup(payment_review_keyboard(order_id))
            .await
        {
            Ok(message) => {
                delivered += 1;
                if let Err(error) = state.record_payment_review_message(
                    order_id,
                    reviewer_id,
                    message.id.0,
                    &caption,
                    false,
                ) {
                    log::error!(
                        "Could not track review message {} for order {order_id} and reviewer {reviewer_id}: {error}",
                        message.id.0
                    );
                }
            }
            Err(error) => {
                log::warn!("Could not deliver payment receipt for order {order_id} to reviewer {reviewer_id}: {error}");
            }
        }
    }

    if delivered == 0 {
        if let Err(error) = state.reset_order_receipt(order_id) {
            log::error!("Could not reset undelivered receipt for order {order_id}: {error}");
        }
        bot.send_message(
            msg.chat.id,
            format!("رسید به مالک یا ادمین‌ها تحویل نشد؛ لطفاً کمی بعد دوباره عکس رسید را ارسال کنید یا با پشتیبانی @{support_username} تماس بگیرید."),
        )
        .await?;
        return Ok(());
    }

    if let Err(error) =
        send_rich_message(bot, msg.chat.id, receipt_confirmation_rich_message()).await
    {
        log::error!(
            "Could not send rich receipt confirmation to chat {}: {error:#}",
            msg.chat.id
        );
        bot.send_message(
            msg.chat.id,
            "🟢 <b>تراکنش شما با موفقیت ثبت شد.</b>\n\nپس از تایید رسید شما توسط ادمین، اشتراک شما شارژ می‌شود 🚀\n\n<blockquote>لطفاً تا زمان تأیید فیش صبور باشید و از ارسال پیام به پشتیبانی خودداری کنید.</blockquote>\n\n⚡️ از انتخاب شما سپاسگزاریم.",
        )
        .parse_mode(ParseMode::Html)
        .reply_markup(subscription_back_keyboard())
        .await?;
    }
    Ok(())
}

fn receipt_confirmation_rich_message() -> UserMenuRichMessage {
    UserMenuRichMessage {
        blocks: vec![
            UserMenuBlock::Paragraph {
                text: UserMenuRichText::Entity(UserMenuRichTextEntity::Bold {
                    text: Box::new(UserMenuRichText::Plain(
                        "🟢 تراکنش شما با موفقیت ثبت شد.".to_owned(),
                    )),
                }),
            },
            UserMenuBlock::Paragraph {
                text: UserMenuRichText::Plain(
                    "پس از تایید رسید شما توسط ادمین، اشتراک شما شارژ می‌شود 🚀".to_owned(),
                ),
            },
            UserMenuBlock::BlockQuotation {
                blocks: vec![UserMenuBlock::Paragraph {
                    text: UserMenuRichText::Plain(
                        "لطفاً تا زمان تأیید فیش صبور باشید و از ارسال پیام به پشتیبانی خودداری کنید."
                            .to_owned(),
                    ),
                }],
            },
            UserMenuBlock::Paragraph {
                text: UserMenuRichText::Plain("⚡️ از انتخاب شما سپاسگزاریم.".to_owned()),
            },
            UserMenuBlock::Buttons {
                buttons: vec![UserMenuButton {
                    text: "🏠 بازگشت به منوی اصلی".to_owned(),
                    style: UserMenuButtonStyle::Danger,
                    callback_data: Some("user:menu".to_owned()),
                    url: None,
                    copy_text: None,
                }],
            },
        ],
        is_rtl: true,
    }
}

async fn approve_payment_order(
    bot: &Bot,
    reviewer_id: UserId,
    state: &AppState,
    order_id: i64,
    source_message: Option<(MessageId, String, bool)>,
) -> ResponseResult<()> {
    let order = match state.claim_payment_order(order_id) {
        Ok(Some(order)) => order,
        Ok(None) => {
            bot.send_message(
                reviewer_id,
                format!(
                    "سفارش #{} قبلاً بررسی شده یا در دست بررسی است.",
                    persian_digits(order_id)
                ),
            )
            .await?;
            return Ok(());
        }
        Err(error) => {
            log::error!("Could not claim payment order {order_id}: {error}");
            bot.send_message(
                reviewer_id,
                "خواندن سفارش انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
            return Ok(());
        }
    };
    let (base_url, access_token) = match panel_api_context(bot, state, "تأیید سفارش").await
    {
        Ok(Some(context)) => context,
        Ok(None) | Err(_) => {
            let released = release_order_for_retry(state, order_id);
            bot.send_message(
                    reviewer_id,
                    if released {
                        "اتصال پنل برقرار نشد؛ پس از بررسی پنل، سفارش را دوباره تأیید کنید."
                    } else {
                        "اتصال پنل برقرار نشد و وضعیت سفارش به صف بررسی برنگشت؛ برای جلوگیری از فعال‌سازی تکراری با پشتیبانی فنی تماس بگیرید."
                    },
                )
                .await?;
            return Ok(());
        }
    };
    let mapped_username = order.is_renewal.then_some(order.panel_username.as_str());

    match panel::provision_subscription(
        &base_url,
        access_token.as_str(),
        order.telegram_user_id,
        mapped_username,
        &order.panel_username,
        order.plan_gb,
    )
    .await
    {
        Ok(subscription) => {
            let referral_credit = match state.complete_payment_order(
                order_id,
                order.telegram_user_id,
                &subscription.username,
            ) {
                Ok(completion) => completion.referral_credit,
                Err(error) => {
                    log::error!(
                        "Panel provisioning succeeded for order {order_id}, but saving its status failed: {error}"
                    );
                    bot.send_message(
                        reviewer_id,
                        format!(
                            "⚠️ اشتراک سفارش #{} در پنل فعال شد، اما ثبت نتیجه در دیتابیس خطا داشت. این سفارش را دوباره تأیید نکنید و با پشتیبانی فنی تماس بگیرید.",
                            persian_digits(order_id)
                        ),
                    )
                    .await?;
                    return Ok(());
                }
            };
            if let Some(credit) = referral_credit {
                let message = referral_commission_notification(&credit, order.is_renewal);
                if let Err(error) = bot
                    .send_message(UserId(credit.referrer_user_id), message)
                    .await
                {
                    log::warn!(
                        "Could not notify referrer {} about commission for order {order_id}: {error}",
                        credit.referrer_user_id
                    );
                }
            }
            finalize_payment_review_messages(
                bot,
                state,
                order_id,
                PaymentReviewStatus::Approved,
                reviewer_id,
                source_message.as_ref(),
            )
            .await;

            let action = if subscription.renewed {
                "اشتراک شما تمدید شد"
            } else {
                "اشتراک شما فعال شد"
            };
            let caption = format!(
                "✅ {action}\n👤 نام کاربری: {}\n📦 حجم: {} گیگ{}\n📅 انقضا: {}\n\n🔗 لینک کانفیگ (برای کپی، یک‌بار روی لینک ضربه بزنید):\n\n<code>{}</code>",
                html_escape(&subscription.username),
                persian_digits(order.plan_gb),
                payment_amount_details_html(&order),
                persian_datetime(
                    subscription
                        .expires_at
                        .with_timezone(&chrono_tz::Asia::Tehran)
                ),
                html_escape(&subscription.subscription_url)
            );
            if let Err(error) = send_subscription_qr(
                bot,
                UserId(order.telegram_user_id),
                &subscription.subscription_url,
                &caption,
            )
            .await
            {
                log::warn!(
                    "Could not notify user {} about approved order {order_id}: {error}",
                    order.telegram_user_id
                );
            }
            bot.send_message(
                reviewer_id,
                format!(
                    "✅ سفارش #{} تأیید و اشتراک فعال شد.",
                    persian_digits(order_id)
                ),
            )
            .await?;
        }
        Err(error) => {
            log::error!("Could not provision approved order {order_id}: {error}");
            notify_panel_issue(bot, state, &error.user_message()).await;
            let released = release_order_for_retry(state, order_id);
            bot.send_message(
                reviewer_id,
                format!(
                    "فعال‌سازی سفارش #{} انجام نشد. {}\n{}",
                    persian_digits(order_id),
                    if released {
                        "سفارش برای تلاش دوباره در انتظار بررسی ماند."
                    } else {
                        "وضعیت سفارش به صف بررسی برنگشت؛ آن را دوباره تأیید نکنید و با پشتیبانی فنی تماس بگیرید."
                    },
                    error.user_message()
                ),
            )
            .await?;
        }
    }
    Ok(())
}

fn release_order_for_retry(state: &AppState, order_id: i64) -> bool {
    match state.release_payment_order(order_id) {
        Ok(()) => true,
        Err(error) => {
            log::error!("Could not release payment order {order_id} for retry: {error}");
            false
        }
    }
}

async fn reject_payment_order(
    bot: &Bot,
    reviewer_id: UserId,
    state: &AppState,
    order_id: i64,
    source_message: Option<(MessageId, String, bool)>,
) -> ResponseResult<()> {
    match state.reject_payment_order(order_id) {
        Ok(Some(telegram_user_id)) => {
            finalize_payment_review_messages(
                bot,
                state,
                order_id,
                PaymentReviewStatus::Rejected,
                reviewer_id,
                source_message.as_ref(),
            )
            .await;
            let support_username = state.support_username();
            if let Err(error) = bot
                .send_message(
                    UserId(telegram_user_id),
                    format!("❌ درخواست پرداخت شما تأیید نشد و اشتراکی فعال نشد. برای پیگیری با پشتیبانی @{support_username} در ارتباط باشید."),
                )
                .await
            {
                log::warn!(
                    "Could not notify user {telegram_user_id} about rejected order {order_id}: {error}"
                );
            }
            bot.send_message(
                reviewer_id,
                format!("سفارش #{} رد شد.", persian_digits(order_id)),
            )
            .await?;
        }
        Ok(None) => {
            bot.send_message(
                reviewer_id,
                format!(
                    "سفارش #{} قبلاً بررسی شده یا در دست بررسی است.",
                    persian_digits(order_id)
                ),
            )
            .await?;
        }
        Err(error) => {
            log::error!("Could not reject payment order {order_id}: {error}");
            bot.send_message(reviewer_id, "رد سفارش انجام نشد؛ لاگ برنامه را بررسی کنید.")
                .await?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum PaymentReviewStatus {
    Approved,
    Rejected,
}

fn final_payment_review_caption(caption: &str, status: PaymentReviewStatus) -> String {
    let status = match status {
        PaymentReviewStatus::Approved => "✅ تأیید شد",
        PaymentReviewStatus::Rejected => "❌ رد شد",
    };
    caption
        .replace("— ⏳ در انتظار بررسی", &format!("— {status}"))
        .replace("وضعیت: ⏳ منتظر تأیید", &format!("وضعیت: {status}"))
}

async fn edit_payment_review_message(
    bot: &Bot,
    order_id: i64,
    reviewer_id: u64,
    message_id: MessageId,
    caption: &str,
    is_text: bool,
    status: PaymentReviewStatus,
) {
    let result = if is_text {
        bot.edit_message_text(
            UserId(reviewer_id),
            message_id,
            final_payment_review_caption(caption, status),
        )
        .parse_mode(ParseMode::Html)
        .reply_markup(InlineKeyboardMarkup::default())
        .await
    } else {
        bot.edit_message_caption(UserId(reviewer_id), message_id)
            .caption(final_payment_review_caption(caption, status))
            .parse_mode(ParseMode::Html)
            .reply_markup(InlineKeyboardMarkup::default())
            .await
    };
    if let Err(error) = result {
        log::warn!(
            "Could not update review message {} for order {order_id} and reviewer {reviewer_id}: {error}",
            message_id.0
        );
    }
}

async fn finalize_payment_review_messages(
    bot: &Bot,
    state: &AppState,
    order_id: i64,
    status: PaymentReviewStatus,
    reviewer_id: UserId,
    source_message: Option<&(MessageId, String, bool)>,
) {
    let messages = match state.payment_review_messages(order_id) {
        Ok(messages) => messages,
        Err(error) => {
            log::error!("Could not load review messages for order {order_id}: {error}");
            Vec::new()
        }
    };
    let source_message_id = source_message.map(|(message_id, _, _)| message_id.0);
    let source_is_tracked = messages.iter().any(|message| {
        message.reviewer_user_id == reviewer_id.0 && Some(message.message_id) == source_message_id
    });

    for message in messages {
        edit_payment_review_message(
            bot,
            order_id,
            message.reviewer_user_id,
            MessageId(message.message_id),
            &message.caption,
            message.is_text,
            status,
        )
        .await;
    }

    if !source_is_tracked {
        if let Some((message_id, caption, is_text)) = source_message {
            edit_payment_review_message(
                bot,
                order_id,
                reviewer_id.0,
                *message_id,
                caption,
                *is_text,
                status,
            )
            .await;
        }
    }
}

fn payment_review_keyboard(order_id: i64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback(
            "✅ تأیید و فعال‌سازی",
            format!("payment:approve:{order_id}"),
        ),
        InlineKeyboardButton::callback("❌ رد سفارش", format!("payment:reject:{order_id}")),
    ]])
}

fn payment_order_keyboard(order_id: i64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "🏠 بازگشت به صفحه‌ی اول",
        format!("payment:cancel:{order_id}"),
    )]])
}

fn payment_review_caption(order: &PaymentOrder) -> String {
    let order_kind = if order.is_renewal {
        "تمدید"
    } else {
        "جدید"
    };
    let telegram_username = order
        .telegram_username
        .as_ref()
        .map(|username| format!("@{username}"))
        .unwrap_or_else(|| "ندارد".to_owned());
    let receipt_status = if order.amount_due_toman == 0 && order.is_wallet_payment {
        "نیازی نیست؛ پرداخت کامل با کیف پول"
    } else if order.receipt_file_id.is_some() {
        "دریافت شد ✅"
    } else {
        "دریافت نشده"
    };

    let payment_details = payment_amount_details_html(order);
    let full_wallet_alert = if order.is_wallet_payment
        && order.amount_due_toman == 0
        && order.wallet_used_toman == order.price_toman
    {
        "\n‼️‼️‼️‼️‼️ این اشتراک به‌طور کامل با کیف پول پرداخت شده است؛ لطفاً درخواست را بررسی و تأیید یا رد کنید."
    } else {
        ""
    };

    format!(
        "🧾 سفارش #{} ({order_kind}) — ⏳ در انتظار بررسی\n👤 کاربر: {} ({})\n🔗 یوزرنیم: {}\n📧 ایمیل کانفیگ: {}\n📦 پلن: {} گیگ | ⏳ ۳۰ روزه{payment_details}{full_wallet_alert}\n🧾 رسید: {}\n🕓 {}\nوضعیت: ⏳ منتظر تأیید",
        persian_digits(order.id),
        html_escape(&order.account_name),
        persian_digits(order.telegram_user_id),
        html_escape(&telegram_username),
        html_escape(&order.panel_username),
        persian_digits(order.plan_gb),
        receipt_status,
        persian_datetime(chrono::Utc::now().with_timezone(&chrono_tz::Asia::Tehran))
    )
}

fn payment_amount_details_html(order: &PaymentOrder) -> String {
    if order.wallet_used_toman > 0 {
        format!(
            "\n💰 مبلغ اصلی اشتراک: {}\n👛 کسر از کیف پول: {}\n<b>💳 مبلغ نهایی پرداخت: {}</b>",
            format_toman(order.price_toman),
            format_toman(order.wallet_used_toman),
            format_toman(order.amount_due_toman)
        )
    } else {
        format!(
            "\n💰 مبلغ اصلی اشتراک: {}\n<b>💳 مبلغ نهایی پرداخت: {}</b>",
            format_toman(order.price_toman),
            format_toman(order.amount_due_toman)
        )
    }
}

fn persian_datetime(datetime: chrono::DateTime<chrono_tz::Tz>) -> String {
    let year = datetime.year();
    let month = datetime.month() as i32;
    let day = datetime.day() as i32;
    let gregorian_month_days = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let mut gy = year;
    if month > 2 {
        gy += 1;
    }
    let mut days = 355_666 + 365 * year + (gy + 3).div_euclid(4) - (gy + 99).div_euclid(100)
        + (gy + 399).div_euclid(400)
        + day
        + gregorian_month_days[(month - 1) as usize];
    let mut jy = -1_595 + 33 * days.div_euclid(12_053);
    days = days.rem_euclid(12_053);
    jy += 4 * days.div_euclid(1_461);
    days = days.rem_euclid(1_461);
    if days > 365 {
        jy += (days - 1).div_euclid(365);
        days = (days - 1).rem_euclid(365);
    }
    let (jm, jd) = if days < 186 {
        (1 + days / 31, 1 + days % 31)
    } else {
        (7 + (days - 186) / 30, 1 + (days - 186) % 30)
    };
    let result = format!(
        "{jy:04}/{jm:02}/{jd:02}, {:02}:{:02}:{:02}",
        datetime.hour(),
        datetime.minute(),
        datetime.second()
    );
    persian_digits(result)
}

async fn ensure_required_channel_member(
    bot: &Bot,
    state: &AppState,
    target: Option<CallbackMessageTarget>,
    user_id: UserId,
) -> Result<bool> {
    match is_required_channel_member(bot, state, user_id).await {
        Ok(true) => Ok(true),
        Ok(false) => {
            edit_user_message_with_keyboard(
                bot,
                target,
                "برای استفاده از ربات، ابتدا در همه‌ی کانال‌های زیر عضو شوید و سپس «عضو شدم» را بزنید.",
                required_channel_join_keyboard(state)?,
            )
            .await?;
            Ok(false)
        }
        Err(error) => {
            log::error!("Could not verify channel membership for user {user_id}: {error}");
            edit_user_message_with_keyboard(
                bot,
                target,
                "بررسی عضویت ممکن نشد. لطفاً بعداً دوباره تلاش کنید.",
                required_channel_join_keyboard(state)?,
            )
            .await?;
            Ok(false)
        }
    }
}

async fn is_required_channel_member(bot: &Bot, state: &AppState, user_id: UserId) -> Result<bool> {
    if !state.required_channels_enabled()? {
        return Ok(true);
    }
    for channel in state.required_channels()? {
        let member = bot
            .get_chat_member(format!("@{}", channel.username), user_id)
            .await?;
        if !member.kind.is_present() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn required_channel_join_keyboard(state: &AppState) -> rusqlite::Result<InlineKeyboardMarkup> {
    Ok(join_channel_keyboard(&state.required_channels()?))
}

async fn send_join_prompt(bot: &Bot, chat_id: ChatId, state: &AppState) -> Result<()> {
    send_user_message_with_keyboard(
        bot,
        chat_id,
        "برای استفاده از ربات، ابتدا در همه‌ی کانال‌های زیر عضو شوید و سپس «عضو شدم» را بزنید.",
        required_channel_join_keyboard(state)?,
    )
    .await?;
    Ok(())
}

fn owner_keyboard() -> InlineKeyboardMarkup {
    let rows = vec![
        vec![InlineKeyboardButton::callback(
            "🖥 مدیریت پنل",
            "owner:panel:manage",
        )],
        vec![InlineKeyboardButton::callback(
            "👥 مدیریت ادمین‌ها",
            "owner:admins:manage",
        )],
        vec![InlineKeyboardButton::callback(
            "💳 مدیریت شماره کارت",
            "owner:payment:manage",
        )],
        vec![InlineKeyboardButton::callback(
            "🆘 تغییر آیدی پشتیبان",
            "owner:support:set",
        )],
        vec![InlineKeyboardButton::callback(
            "📢 تنظیم کانال اجباری",
            "owner:channels:manage",
        )],
        vec![
            InlineKeyboardButton::callback("📢 پیام همگانی", "owner:broadcast:all"),
            InlineKeyboardButton::callback("💳 پیام به مشترکین", "owner:broadcast:subscribers"),
        ],
        vec![InlineKeyboardButton::callback(
            "🛒 وضعیت خرید و تمدید",
            "owner:sales:manage",
        )],
        vec![InlineKeyboardButton::callback(
            "🎁 تنظیمات رفرال",
            "owner:referral:manage",
        )],
        vec![InlineKeyboardButton::callback(
            "🗄 مدیریت دیتابیس",
            "owner:database:manage",
        )],
        vec![InlineKeyboardButton::callback(
            "👤 مدیریت کاربران",
            "owner:users:manage",
        )],
        vec![InlineKeyboardButton::callback(
            "📦 مدیریت تعرفه‌ها",
            "owner:tariffs:manage",
        )],
        vec![InlineKeyboardButton::callback(
            "〽️ مدیریت کیف پول کاربران",
            "owner:wallet:manage",
        )],
        vec![InlineKeyboardButton::callback(
            "📊 آمار مالی",
            "owner:finance:manage",
        )],
        vec![InlineKeyboardButton::callback(
            "💽 مدیریت فضای دیسک",
            "owner:disk:manage",
        )],
    ];
    InlineKeyboardMarkup::new(rows)
}

#[derive(Clone, Copy)]
enum FinancialMetric {
    Revenue,
    Commissions,
    ReferredUsers,
    ReferralOrders,
    AverageReferralRevenue,
    WalletBalances,
}

fn financial_metric_from_callback(data: &str) -> Option<FinancialMetric> {
    match data {
        "owner:finance:revenue" => Some(FinancialMetric::Revenue),
        "owner:finance:commissions" => Some(FinancialMetric::Commissions),
        "owner:finance:referred_users" => Some(FinancialMetric::ReferredUsers),
        "owner:finance:referral_orders" => Some(FinancialMetric::ReferralOrders),
        "owner:finance:average_referral_revenue" => Some(FinancialMetric::AverageReferralRevenue),
        "owner:finance:wallet_balances" => Some(FinancialMetric::WalletBalances),
        _ => None,
    }
}

async fn send_financial_statistics_menu(bot: &Bot, owner_id: UserId) -> ResponseResult<()> {
    send_text_message_with_keyboard(
        bot,
        owner_id,
        "📊 آمار مالی\n\nبرای مشاهده‌ی هر آمار، دکمه‌ی مربوط به آن را انتخاب کنید.",
        financial_statistics_keyboard(),
    )
    .await
}

fn financial_statistics_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "💰 کل درآمد فروش",
            "owner:finance:revenue",
        )],
        vec![InlineKeyboardButton::callback(
            "🎁 کل پورسانت پرداخت‌شده",
            "owner:finance:commissions",
        )],
        vec![InlineKeyboardButton::callback(
            "👥 تعداد کاربران جذب‌شده از رفرال",
            "owner:finance:referred_users",
        )],
        vec![InlineKeyboardButton::callback(
            "🛒 تعداد خریدهای انجام‌شده توسط زیرمجموعه‌ها",
            "owner:finance:referral_orders",
        )],
        vec![InlineKeyboardButton::callback(
            "📈 میانگین خرید هر زیرمجموعه",
            "owner:finance:average_referral_revenue",
        )],
        vec![InlineKeyboardButton::callback(
            "〽️ مجموع موجودی کیف پول کاربران",
            "owner:finance:wallet_balances",
        )],
        vec![InlineKeyboardButton::callback(
            "🔙 بازگشت به منوی مالک",
            "owner:main",
        )],
    ])
}

async fn send_financial_statistic(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
    metric: FinancialMetric,
) -> ResponseResult<()> {
    let statistics = match state.financial_statistics() {
        Ok(statistics) => statistics,
        Err(error) => {
            log::error!("Could not read financial statistics: {error}");
            bot.send_message(
                owner_id,
                "خواندن آمار مالی انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
            return Ok(());
        }
    };
    let (label, value) = financial_statistic_value(statistics, metric);
    send_text_message_with_keyboard(
        bot,
        owner_id,
        format!("{label}\n\n{value}"),
        financial_statistic_result_keyboard(),
    )
    .await
}

fn financial_statistic_value(
    statistics: FinancialStatistics,
    metric: FinancialMetric,
) -> (&'static str, String) {
    match metric {
        FinancialMetric::Revenue => (
            "💰 کل درآمد فروش (سفارش‌های تأییدشده)",
            format_signed_toman(statistics.total_sales_toman),
        ),
        FinancialMetric::Commissions => (
            "🎁 کل پورسانت پرداخت‌شده",
            format_signed_toman(statistics.total_commissions_toman),
        ),
        FinancialMetric::ReferredUsers => (
            "👥 تعداد کاربران جذب‌شده از رفرال",
            persian_digits(statistics.referred_users),
        ),
        FinancialMetric::ReferralOrders => (
            "🛒 تعداد خریدهای انجام‌شده توسط زیرمجموعه‌ها",
            persian_digits(statistics.referral_orders),
        ),
        FinancialMetric::AverageReferralRevenue => (
            "📈 میانگین مبلغ خرید هر زیرمجموعه‌ی خریدار",
            format_signed_toman(statistics.average_referral_revenue_toman),
        ),
        FinancialMetric::WalletBalances => (
            "〽️ مجموع موجودی کیف پول کاربران",
            format_signed_toman(statistics.total_wallet_balance_toman),
        ),
    }
}

fn financial_statistic_result_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "🔙 بازگشت به آمار مالی",
            "owner:finance:manage",
        )],
        vec![InlineKeyboardButton::callback("🏠 منوی مالک", "owner:main")],
    ])
}

async fn send_disk_space_management(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
) -> ResponseResult<()> {
    match state.disk_space_usage() {
        Ok(usage) => {
            let free_percent = usage.free_bytes as f64 * 100.0 / usage.total_bytes as f64;
            let body = format!(
                "💽 فضای دیسک سرور (دیسک محل دیتابیس)\n\n💽 ظرفیت کل: {}\n🟢 فضای خالی: {}\n📊 درصد فضای خالی: {}٪\n⛔ آستانه‌ی هشدار روزانه: کمتر از ۱۵٪",
                format_disk_space(usage.total_bytes),
                format_disk_space(usage.free_bytes),
                persian_digits(format!("{free_percent:.2}"))
            );
            send_text_message_with_keyboard(bot, owner_id, body, disk_space_management_keyboard())
                .await?;
        }
        Err(error) => {
            log::error!("Could not read disk-space usage for owner view: {error}");
            bot.send_message(
                owner_id,
                "خواندن فضای دیسک انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
        }
    }
    Ok(())
}

fn disk_space_management_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "🔙 بازگشت به منوی مالک",
        "owner:main",
    )]])
}

fn disk_space_below_alert_threshold(usage: DiskSpaceUsage) -> bool {
    u128::from(usage.free_bytes) * 100 < u128::from(usage.total_bytes) * 15
}

fn format_disk_space(bytes: u64) -> String {
    let gibibytes = bytes as f64 / 1024_f64.powi(3);
    format!(
        "{} گیگابایت",
        persian_digits(format!("{gibibytes:.2}").replace('.', "٫"))
    )
}

async fn send_wallet_management(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
) -> ResponseResult<()> {
    match state.wallet_statistics() {
        Ok((total_balance, total_commissions)) => {
            let body = format!(
                "〽️ مدیریت کیف پول\n\n💰 مجموع موجودی تمام کیف پول‌ها: {}\n🎁 مجموع پورسانت‌های پرداخت‌شده: {}\n\nبرای تغییر گروهی یا دستی موجودی از دکمه‌ها استفاده کنید؛ ثبت دلیل تغییر الزامی است. در کاهش گروهی، موجودی تا صفر کم می‌شود و مبالغ رزروشده‌ی سفارش‌های باز حفظ می‌شوند.",
                format_signed_toman(total_balance),
                format_signed_toman(total_commissions)
            );
            send_text_message_with_keyboard(bot, owner_id, body, wallet_management_keyboard())
                .await?;
        }
        Err(error) => {
            log::error!("Could not read wallet statistics: {error}");
            bot.send_message(
                owner_id,
                "خواندن آمار کیف پول‌ها انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
        }
    }
    Ok(())
}

fn wallet_management_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "🎁 افزایش موجودی همه‌ی کاربران",
            "owner:wallet:increase_all",
        )],
        vec![InlineKeyboardButton::callback(
            "📉 کاهش موجودی همه‌ی کاربران",
            "owner:wallet:decrease_all",
        )],
        vec![InlineKeyboardButton::callback(
            "✏️ تغییر دستی موجودی کاربر",
            "owner:wallet:adjust",
        )],
        vec![InlineKeyboardButton::callback("🔙 بازگشت", "owner:main")],
    ])
}

fn bulk_wallet_adjustment_message(
    action: WalletBulkAction,
    amount_per_user: i64,
    reason: &str,
    summary: WalletBulkAdjustmentSummary,
) -> String {
    let action_name = match action {
        WalletBulkAction::Increase => "افزایش",
        WalletBulkAction::Decrease => "کاهش",
    };
    format!(
        "✅ تغییر گروهی کیف پول‌ها انجام شد.\n📊 کاربران بررسی‌شده: {}\n✏️ کیف پول‌های تغییرکرده: {}\n⏭ بدون تغییر: {}\n💵 مبلغ {action_name} برای هر کاربر: {}\n🧮 مجموع مبلغ {action_name}: {}\n📝 دلیل: {}",
        persian_digits(summary.targeted_users),
        persian_digits(summary.changed_users),
        persian_digits(summary.unchanged_users),
        format_signed_toman(amount_per_user),
        format_signed_toman(summary.total_adjusted_toman),
        reason
    )
}

async fn send_user_management_menu(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
) -> ResponseResult<()> {
    match state.bot_user_statistics() {
        Ok((total_users, referred_users, commissions_paid)) => {
            let body = format!(
                "👤 آمار کاربران\n\n👥 تعداد کل کاربران: {}\n🔗 کاربران عضو‌شده با لینک رفرال: {}\n💰 مجموع پورسانت پرداخت‌شده: {}",
                persian_digits(total_users),
                persian_digits(referred_users),
                format_signed_toman(commissions_paid)
            );
            send_text_message_with_keyboard(bot, owner_id, body, user_management_keyboard())
                .await?;
        }
        Err(error) => {
            log::error!("Could not read bot user statistics: {error}");
            bot.send_message(
                owner_id,
                "خواندن آمار کاربران انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
        }
    }
    Ok(())
}

fn user_management_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "🔍 جستجوی کاربر",
            "owner:users:search",
        )],
        vec![InlineKeyboardButton::callback("🔙 بازگشت", "owner:main")],
    ])
}

async fn send_user_search_results(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
    query: &str,
) -> ResponseResult<()> {
    match state.search_bot_users(query) {
        Ok(users) if users.is_empty() => {
            state.set_setup(owner_id, SetupState::UserSearch);
            send_text_message_with_keyboard(
                bot,
                owner_id,
                "کاربری با این شناسه یا نام کاربری پیدا نشد. دوباره جستجو کنید:",
                user_search_results_keyboard(&[]),
            )
            .await?;
        }
        Ok(users) => {
            let body = format!(
                "🔍 {} کاربر پیدا شد. برای مشاهده‌ی اطلاعات، کاربر را انتخاب کنید:",
                persian_digits(users.len())
            );
            send_text_message_with_keyboard(
                bot,
                owner_id,
                body,
                user_search_results_keyboard(&users),
            )
            .await?;
        }
        Err(error) => {
            log::error!("Could not search bot users: {error}");
            state.set_setup(owner_id, SetupState::UserSearch);
            bot.send_message(
                owner_id,
                "جستجوی کاربر انجام نشد؛ دوباره شناسه یا نام کاربری را بفرستید یا /cancel را بزنید.",
            )
            .await?;
        }
    }
    Ok(())
}

fn user_search_results_keyboard(users: &[BotUserSearchResult]) -> InlineKeyboardMarkup {
    let mut rows = users
        .iter()
        .map(|user| {
            let mut label = user
                .first_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("کاربر")
                .to_owned();
            if let Some(username) = &user.username {
                label.push_str(&format!(" (@{})", username.trim_start_matches('@')));
            }
            label.push_str(&format!(" | {}", persian_digits(user.telegram_user_id)));
            vec![InlineKeyboardButton::callback(
                label,
                format!("owner:users:item:{}", user.telegram_user_id),
            )]
        })
        .collect::<Vec<_>>();
    rows.push(vec![InlineKeyboardButton::callback(
        "🔍 جستجوی دوباره",
        "owner:users:search",
    )]);
    rows.push(vec![InlineKeyboardButton::callback(
        "🔙 بازگشت به مدیریت کاربران",
        "owner:users:manage",
    )]);
    InlineKeyboardMarkup::new(rows)
}

async fn send_managed_user_details(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
    telegram_user_id: u64,
) -> ResponseResult<()> {
    match state.bot_user_details(telegram_user_id) {
        Ok(Some(user)) => {
            send_text_message_with_keyboard(
                bot,
                owner_id,
                managed_user_details_message(&user),
                managed_user_details_keyboard(),
            )
            .await?;
        }
        Ok(None) => {
            bot.send_message(owner_id, "کاربر پیدا نشد یا اطلاعات او در دسترس نیست.")
                .await?;
        }
        Err(error) => {
            log::error!("Could not read details for bot user {telegram_user_id}: {error}");
            bot.send_message(
                owner_id,
                "خواندن اطلاعات کاربر انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
        }
    }
    Ok(())
}

fn managed_user_details_message(user: &BotUserDetails) -> String {
    let name = user
        .first_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("ثبت نشده");
    let username = user
        .username
        .as_deref()
        .map(|username| format!("@{}", username.trim_start_matches('@')))
        .unwrap_or_else(|| "ندارد".to_owned());
    let referrer = match user.referrer_user_id {
        Some(referrer_id) => {
            let mut label = format!("شناسه‌ی {}", persian_digits(referrer_id));
            if let Some(name) = user
                .referrer_first_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
            {
                label.push_str(&format!(" | {name}"));
            }
            if let Some(username) = &user.referrer_username {
                label.push_str(&format!(" | @{}", username.trim_start_matches('@')));
            }
            label
        }
        None => "ندارد".to_owned(),
    };

    format!(
        "👤 اطلاعات کاربر\n\n🆔 شناسه: {}\n📝 نام: {}\n🔗 نام کاربری: {}\n👥 معرف: {}\n👥 تعداد زیرمجموعه‌ها: {}\n💰 پورسانت پرداخت‌شده به این کاربر: {}",
        persian_digits(user.telegram_user_id),
        name,
        username,
        referrer,
        persian_digits(user.referral_count),
        format_signed_toman(user.commissions_paid_toman)
    )
}

fn managed_user_details_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "🔍 جستجوی کاربر دیگر",
            "owner:users:search",
        )],
        vec![InlineKeyboardButton::callback(
            "🔙 بازگشت به مدیریت کاربران",
            "owner:users:manage",
        )],
    ])
}

async fn send_database_management(bot: &Bot, owner_id: UserId) -> ResponseResult<()> {
    send_text_message_with_keyboard(
        bot,
        owner_id,
        "🗄 مدیریت دیتابیس\n\nاز دیتابیس فعلی ربات می‌توانید بکاپ بگیرید. برای ریستور، همان فایل بکاپی را که ربات قبلاً برای شما فرستاده است ارسال کنید.",
        database_management_keyboard(),
    )
    .await
}

fn database_management_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "📥 گرفتن بکاپ",
            "owner:database:backup",
        )],
        vec![InlineKeyboardButton::callback(
            "📤 ریستور بکاپ",
            "owner:database:restore",
        )],
        vec![InlineKeyboardButton::callback("🔙 بازگشت", "owner:main")],
    ])
}

fn reserve_temporary_database_file() -> io::Result<PathBuf> {
    loop {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "falconbot-db-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                drop(file);
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn remove_temporary_database_file(path: &Path) {
    if let Err(error) = fs::remove_file(path) {
        if error.kind() != io::ErrorKind::NotFound {
            log::warn!("Could not remove temporary database file: {error}");
        }
    }
}

async fn send_database_backup(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
    caption: &str,
) -> Result<()> {
    let backup_path = reserve_temporary_database_file()?;
    let result = state
        .create_database_backup(&backup_path)
        .context("Could not create database backup");
    if let Err(error) = result {
        remove_temporary_database_file(&backup_path);
        log::error!("Could not create database backup for owner {owner_id}: {error}");
        bot.send_message(
            owner_id,
            "گرفتن بکاپ دیتابیس انجام نشد؛ لاگ برنامه را بررسی کنید.",
        )
        .await?;
        return Ok(());
    }

    let size = fs::metadata(&backup_path)?.len();
    if size > MAX_DATABASE_BACKUP_SIZE {
        remove_temporary_database_file(&backup_path);
        bot.send_message(
            owner_id,
            "حجم بکاپ از محدودیت ارسال تلگرام بیشتر است و فایل ارسال نشد.",
        )
        .await?;
        return Ok(());
    }
    let file_name = format!(
        "falconbot-backup-{}.sqlite3",
        chrono::Utc::now()
            .with_timezone(&chrono_tz::Asia::Tehran)
            .format("%Y-%m-%d")
    );
    let result = bot
        .send_document(
            owner_id,
            InputFile::file(backup_path.clone()).file_name(file_name),
        )
        .caption(caption)
        .await;
    remove_temporary_database_file(&backup_path);
    result?;
    Ok(())
}

async fn restore_database_backup(
    bot: &Bot,
    owner_id: UserId,
    msg: &Message,
    state: &AppState,
    document: &teloxide::types::Document,
) -> Result<()> {
    if u64::from(document.file.size) > MAX_DATABASE_BACKUP_SIZE {
        state.set_setup(owner_id, SetupState::DatabaseRestore);
        bot.send_message(
            msg.chat.id,
            "حجم فایل از محدودیت ۲۰ مگابایت بیشتر است. فایل بکاپ کوچک‌تری ارسال کنید یا /cancel را بزنید.",
        )
        .await?;
        return Ok(());
    }

    let backup_path = reserve_temporary_database_file()?;
    let rollback_path = match reserve_temporary_database_file() {
        Ok(path) => path,
        Err(error) => {
            remove_temporary_database_file(&backup_path);
            return Err(error.into());
        }
    };
    let restore_result: Result<()> = async {
        let file = bot.get_file(document.file.id.clone()).await?;
        let mut destination = tokio::fs::File::create(&backup_path).await?;
        bot.download_file(&file.path, &mut destination).await?;
        destination.flush().await?;
        drop(destination);

        if fs::metadata(&backup_path)?.len() > MAX_DATABASE_BACKUP_SIZE {
            return Err(anyhow!("database backup exceeds the upload size limit"));
        }
        state
            .restore_database_backup(&backup_path, &rollback_path)
            .context("Uploaded file is not a valid FalconBOT database backup")?;
        Ok(())
    }
    .await;
    remove_temporary_database_file(&backup_path);
    remove_temporary_database_file(&rollback_path);

    match restore_result {
        Ok(()) => {
            log::info!("Owner {owner_id} restored the database from an uploaded backup");
            bot.send_message(msg.chat.id, "✅ بکاپ با موفقیت ریستور شد.")
                .reply_markup(owner_keyboard())
                .await?;
        }
        Err(error) => {
            log::error!("Could not restore database backup for owner {owner_id}: {error}");
            state.set_setup(owner_id, SetupState::DatabaseRestore);
            bot.send_message(
                msg.chat.id,
                "ریستور انجام نشد؛ فایل معتبرِ بکاپ همین ربات را بفرستید. اگر مشکل ادامه داشت، لاگ برنامه را بررسی کنید. برای لغو /cancel را بزنید.",
            )
            .await?;
        }
    }
    Ok(())
}

fn is_admin_review_action(data: &str) -> bool {
    data.starts_with("payment:approve:") || data.starts_with("payment:reject:")
}

fn is_admin_allowed_callback(data: &str) -> bool {
    is_admin_review_action(data) || data == "user:menu"
}

#[derive(Clone, Copy)]
enum SalesFeature {
    Purchase,
    Renewal,
}

impl SalesFeature {
    fn label(self) -> &'static str {
        match self {
            Self::Purchase => "خرید اشتراک",
            Self::Renewal => "تمدید اشتراک",
        }
    }

    fn set_enabled(self, state: &AppState, enabled: bool) -> rusqlite::Result<()> {
        match self {
            Self::Purchase => state.set_purchases_enabled(enabled),
            Self::Renewal => state.set_renewals_enabled(enabled),
        }
    }
}

async fn send_sales_management(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
) -> ResponseResult<()> {
    match state.sales_status() {
        Ok((purchases_enabled, renewals_enabled)) => {
            send_text_message_with_keyboard(
                bot,
                owner_id,
                purchase_status_message(purchases_enabled, renewals_enabled),
                purchase_management_keyboard(purchases_enabled, renewals_enabled),
            )
            .await?;
        }
        Err(error) => {
            log::error!("Could not read purchase and renewal availability: {error}");
            bot.send_message(
                owner_id,
                "خواندن وضعیت خرید و تمدید انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
        }
    }
    Ok(())
}

async fn send_required_channel_management(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
) -> ResponseResult<()> {
    match (state.required_channels_enabled(), state.required_channels()) {
        (Ok(enabled), Ok(channels)) => {
            send_text_message_with_keyboard(
                bot,
                owner_id,
                required_channel_management_message(enabled, &channels),
                required_channel_management_keyboard(enabled, &channels),
            )
            .await?;
        }
        (Err(error), _) | (_, Err(error)) => {
            log::error!("Could not read required channel settings: {error}");
            bot.send_message(
                owner_id,
                "خواندن تنظیمات کانال اجباری انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
        }
    }
    Ok(())
}

async fn send_referral_management(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
) -> ResponseResult<()> {
    match state.referral_status() {
        Ok((enabled, percent)) => {
            send_text_message_with_keyboard(
                bot,
                owner_id,
                referral_status_message(enabled, percent),
                referral_management_keyboard(enabled),
            )
            .await?;
        }
        Err(error) => {
            log::error!("Could not read referral settings: {error}");
            bot.send_message(
                owner_id,
                "خواندن تنظیمات رفرال انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
        }
    }
    Ok(())
}

async fn update_referrals_enabled(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
    enabled: bool,
) -> ResponseResult<()> {
    match state.set_referrals_enabled(enabled) {
        Ok(()) => {
            log::info!(
                "Owner {owner_id} {} referral acquisition",
                if enabled { "enabled" } else { "disabled" }
            );
            send_referral_management(bot, owner_id, state).await?;
        }
        Err(error) => {
            log::error!("Could not update referral availability: {error}");
            bot.send_message(
                owner_id,
                "ذخیره‌ی وضعیت رفرال انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
        }
    }
    Ok(())
}

fn referral_status_message(enabled: bool, percent: u8) -> String {
    format!(
        "🎁 وضعیت رفرال: {}\n💰 درصد پورسانت: {}٪\n\nبا خاموش‌کردن رفرال، ثبت زیرمجموعه‌های جدید و پرداخت پورسانت متوقف می‌شود. برای تغییر درصد، دکمه‌ی مربوط را بزنید.",
        if enabled { "✅ فعال" } else { "⛔ غیرفعال" },
        persian_digits(percent)
    )
}

fn referral_management_keyboard(enabled: bool) -> InlineKeyboardMarkup {
    let toggle_button = if enabled {
        InlineKeyboardButton::callback("⛔ غیرفعال‌کردن رفرال", "owner:referral:disable")
    } else {
        InlineKeyboardButton::callback("✅ فعال‌کردن رفرال", "owner:referral:enable")
    };
    InlineKeyboardMarkup::new(vec![
        vec![toggle_button],
        vec![InlineKeyboardButton::callback(
            "✏️ تغییر درصد پورسانت",
            "owner:referral:percent",
        )],
        vec![InlineKeyboardButton::callback("🔙 بازگشت", "owner:main")],
    ])
}

async fn send_subscription_tariffs(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
) -> ResponseResult<()> {
    match state.subscription_tariffs() {
        Ok(tariffs) => {
            let body = if tariffs.is_empty() {
                "هنوز تعرفه‌ای ثبت نشده است.".to_owned()
            } else {
                format!(
                    "📦 تعرفه‌های اشتراک\n\nبرای ویرایش حجم، قیمت یا وضعیت، تعرفه موردنظر را انتخاب کنید.\n\n{}",
                    tariffs
                        .iter()
                        .map(subscription_tariff_message)
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            send_text_message_with_keyboard(
                bot,
                owner_id,
                body,
                subscription_tariffs_keyboard(&tariffs),
            )
            .await?;
        }
        Err(error) => {
            log::error!("Could not list subscription tariffs: {error}");
            bot.send_message(
                owner_id,
                "خواندن تعرفه‌ها انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
        }
    }
    Ok(())
}

fn subscription_tariff_message(tariff: &SubscriptionTariff) -> String {
    format!(
        "{} {} گیگ | {} | {}",
        if tariff.enabled { "✅" } else { "⛔" },
        persian_digits(tariff.volume_gb),
        format_toman(tariff.price_toman),
        if tariff.enabled {
            "فعال"
        } else {
            "غیرفعال"
        }
    )
}

fn subscription_tariffs_keyboard(tariffs: &[SubscriptionTariff]) -> InlineKeyboardMarkup {
    let mut rows = tariffs
        .iter()
        .map(|tariff| {
            vec![InlineKeyboardButton::callback(
                format!(
                    "{} {} گیگ | {}",
                    if tariff.enabled { "✅" } else { "⛔" },
                    persian_digits(tariff.volume_gb),
                    format_toman(tariff.price_toman)
                ),
                format!("owner:tariffs:item:{}", tariff.id),
            )]
        })
        .collect::<Vec<_>>();
    rows.push(vec![InlineKeyboardButton::callback(
        "➕ افزودن تعرفه",
        "owner:tariffs:add",
    )]);
    rows.push(vec![InlineKeyboardButton::callback(
        "🔙 بازگشت",
        "owner:main",
    )]);
    InlineKeyboardMarkup::new(rows)
}

fn subscription_tariff_detail_keyboard(tariff: &SubscriptionTariff) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "✏️ تغییر حجم",
            format!("owner:tariffs:volume:{}", tariff.id),
        )],
        vec![InlineKeyboardButton::callback(
            "💰 تغییر قیمت",
            format!("owner:tariffs:price:{}", tariff.id),
        )],
        vec![InlineKeyboardButton::callback(
            if tariff.enabled {
                "⛔ غیرفعال‌کردن تعرفه"
            } else {
                "✅ فعال‌کردن تعرفه"
            },
            format!("owner:tariffs:toggle:{}", tariff.id),
        )],
        vec![InlineKeyboardButton::callback(
            "🔙 بازگشت به تعرفه‌ها",
            "owner:tariffs:manage",
        )],
    ])
}

async fn update_sales_feature(
    bot: &Bot,
    owner_id: UserId,
    state: &AppState,
    feature: SalesFeature,
    enabled: bool,
) -> ResponseResult<()> {
    match feature.set_enabled(state, enabled) {
        Ok(()) => {
            log::info!(
                "Owner {owner_id} {} {}",
                if enabled { "enabled" } else { "disabled" },
                feature.label()
            );
            send_sales_management(bot, owner_id, state).await?;
        }
        Err(error) => {
            log::error!("Could not update {} availability: {error}", feature.label());
            bot.send_message(
                owner_id,
                "ذخیره‌ی وضعیت جدید انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
        }
    }
    Ok(())
}

fn purchase_status_message(purchases_enabled: bool, renewals_enabled: bool) -> String {
    let status_text = |enabled| {
        if enabled {
            "✅ فعال"
        } else {
            "⛔ غیرفعال"
        }
    };
    format!(
        "🛒 خرید اشتراک: {}\n♻️ تمدید اشتراک: {}\n\nوضعیت هرکدام را جداگانه با دکمه‌ی مربوط تغییر دهید.",
        status_text(purchases_enabled),
        status_text(renewals_enabled)
    )
}

fn purchase_management_keyboard(
    purchases_enabled: bool,
    renewals_enabled: bool,
) -> InlineKeyboardMarkup {
    let purchase_button = if purchases_enabled {
        InlineKeyboardButton::callback("⛔ غیرفعال‌کردن خرید اشتراک", "owner:sales:purchase:disable")
    } else {
        InlineKeyboardButton::callback("✅ فعال‌کردن خرید اشتراک", "owner:sales:purchase:enable")
    };
    let renewal_button = if renewals_enabled {
        InlineKeyboardButton::callback("⛔ غیرفعال‌کردن تمدید اشتراک", "owner:sales:renewal:disable")
    } else {
        InlineKeyboardButton::callback("✅ فعال‌کردن تمدید اشتراک", "owner:sales:renewal:enable")
    };
    InlineKeyboardMarkup::new(vec![
        vec![purchase_button],
        vec![renewal_button],
        vec![InlineKeyboardButton::callback("🔙 بازگشت", "owner:main")],
    ])
}

fn required_channel_management_message(enabled: bool, channels: &[RequiredChannel]) -> String {
    let status = if enabled {
        "✅ فعال"
    } else {
        "⛔ غیرفعال"
    };
    let channels = if channels.is_empty() {
        "هنوز کانالی ثبت نشده است.".to_owned()
    } else {
        channels
            .iter()
            .map(|channel| format!("• @{}", channel.username))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "📢 عضویت اجباری: {status}\n\nکانال‌ها:\n{channels}\n\nکاربر باید در همه‌ی کانال‌های فهرست‌شده عضو باشد. ربات باید در کانال‌ها ادمین باشد."
    )
}

fn required_channel_management_keyboard(
    enabled: bool,
    channels: &[RequiredChannel],
) -> InlineKeyboardMarkup {
    let mut rows = channels
        .iter()
        .map(|channel| {
            vec![InlineKeyboardButton::callback(
                format!("❌ حذف @{}", channel.username),
                format!("owner:channels:remove:{}", channel.username),
            )]
        })
        .collect::<Vec<_>>();
    rows.push(vec![InlineKeyboardButton::callback(
        "➕ افزودن کانال",
        "owner:channels:add",
    )]);
    rows.push(vec![if enabled {
        InlineKeyboardButton::callback("⛔ غیرفعال‌کردن عضویت اجباری", "owner:channels:toggle")
    } else {
        InlineKeyboardButton::callback("✅ فعال‌کردن عضویت اجباری", "owner:channels:toggle")
    }]);
    rows.push(vec![InlineKeyboardButton::callback(
        "🔙 بازگشت",
        "owner:main",
    )]);
    InlineKeyboardMarkup::new(rows)
}

fn admin_management_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "➕ افزودن ادمین با شناسه",
            "owner:admins:add",
        )],
        vec![InlineKeyboardButton::callback("🔙 بازگشت", "owner:main")],
    ])
}

fn admin_list_keyboard(admin_ids: &[u64]) -> InlineKeyboardMarkup {
    let mut rows = admin_ids
        .iter()
        .map(|id| {
            vec![InlineKeyboardButton::callback(
                format!("❌ حذف ادمین {}", persian_digits(id)),
                format!("owner:admins:remove:{id}"),
            )]
        })
        .collect::<Vec<_>>();
    rows.push(vec![InlineKeyboardButton::callback(
        "➕ افزودن ادمین",
        "owner:admins:add",
    )]);
    rows.push(vec![InlineKeyboardButton::callback(
        "🔙 بازگشت",
        "owner:main",
    )]);
    InlineKeyboardMarkup::new(rows)
}

fn user_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "🚀 خرید و تمدید اشتراک 💰",
            "user:purchase-menu",
        )],
        vec![InlineKeyboardButton::callback(
            "⚡️ مدیریت اشتراک ⚙️",
            "user:management-menu",
        )],
        vec![InlineKeyboardButton::callback(
            "🎁 زیر مجموعه گیری ‼️",
            "user:referral",
        )],
        vec![InlineKeyboardButton::callback(
            "〽️ کیف پول من 💵",
            "user:wallet",
        )],
        vec![InlineKeyboardButton::callback(
            "👨🏻‍💻 پشتیبانی",
            "user:support",
        )],
    ])
}

fn referral_keyboard(link: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::copy_text_button(
            "📋 کپی لینک دعوت",
            CopyTextButton {
                text: link.to_owned(),
            },
        )],
        vec![InlineKeyboardButton::callback("🔙 بازگشت", "user:main")],
    ])
}

fn referral_message(link: &str, referrals: u64, commissions: i64, percent: u8) -> String {
    format!(
        "🎁 پورسانت فعلی: {}٪\n👥 اگر دوستتان با لینک دعوت شما ربات را Start کند، از مبلغ هر خرید یا تمدید موفق او، {}٪ به کیف پول شما اضافه می‌شود.\n\n🔗 لینک دعوت اختصاصی شما:\n{link}\n\n👥 تعداد زیرمجموعه‌ها: {}\n💰 مجموع پورسانت ثبت‌شده: {}",
        persian_digits(percent),
        persian_digits(percent),
        persian_digits(referrals),
        format_signed_toman(commissions)
    )
}

fn referral_commission_notification(
    credit: &state::ReferralCommissionCredit,
    is_renewal: bool,
) -> String {
    let action = if is_renewal {
        "اشتراک خود را با موفقیت تمدید کرد"
    } else {
        "یک اشتراک را با موفقیت خرید"
    };
    format!(
        "🎉 تبریک! یکی از زیرمجموعه‌های شما {action}.\n💰 مبلغ {} به کیف پول شما اضافه شد.\n👛 موجودی جدید کیف پول شما: {}",
        format_signed_toman(credit.amount_toman),
        format_signed_toman(credit.balance_after_toman)
    )
}

fn purchase_menu_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "🔋 خرید اشتراک  🚀",
            "user:buy",
        )],
        vec![InlineKeyboardButton::callback(
            "🚀 تمدید اشتراک ♻️",
            "user:renew",
        )],
        vec![InlineKeyboardButton::callback(
            "🏠 بازگشت به منوی اصلی",
            "user:menu",
        )],
    ])
}

fn subscription_management_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "📊 چقدر حجم دارم؟",
            "user:usage",
        )],
        vec![InlineKeyboardButton::callback(
            "🔗 دریافت لینک کانفیگ",
            "user:link",
        )],
        vec![InlineKeyboardButton::callback(
            "🏠 بازگشت به منوی اصلی",
            "user:menu",
        )],
    ])
}

fn panel_client_keyboard(
    clients: &[panel::PanelClient],
    purpose: ClientListPurpose,
) -> InlineKeyboardMarkup {
    let mut rows = clients
        .iter()
        .map(|client| {
            let status = if client.is_active { "🟢" } else { "🔴" };
            let callback = match purpose {
                ClientListPurpose::Renewal => {
                    format!("user:renew:select:{}", client.username)
                }
                ClientListPurpose::Usage => format!("user:usage:select:{}", client.username),
                ClientListPurpose::Link => format!("user:link:select:{}", client.username),
            };
            vec![InlineKeyboardButton::callback(
                format!("{status} {}", client.username),
                callback,
            )]
        })
        .collect::<Vec<_>>();
    rows.push(vec![InlineKeyboardButton::callback(
        "🔙 بازگشت",
        "user:main",
    )]);
    InlineKeyboardMarkup::new(rows)
}

fn support_keyboard(support_username: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::url(
            "💬 ارسال پیام به پشتیبانی",
            Url::parse(&format!("https://t.me/{support_username}"))
                .expect("validated support username produces a valid Telegram URL"),
        )],
        vec![InlineKeyboardButton::callback("🔙 بازگشت", "user:main")],
    ])
}

fn format_gigabytes(bytes: u64) -> String {
    let gigabytes = bytes as f64 / 1_073_741_824_f64;
    persian_digits(format!("{gigabytes:.2}").replace('.', "٫"))
}

fn format_consumed_traffic(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    let (value, unit) = if bytes >= GIB {
        (bytes as f64 / GIB as f64, "گیگابایت")
    } else if bytes >= MIB {
        (bytes as f64 / MIB as f64, "مگابایت")
    } else if bytes >= KIB {
        (bytes as f64 / KIB as f64, "کیلوبایت")
    } else {
        return format!("{} بایت", persian_digits(bytes.to_string()));
    };

    format!(
        "{} {unit}",
        persian_digits(format!("{value:.2}").replace('.', "٫"))
    )
}

fn subscription_usage_message(usage: &panel::SubscriptionUsage) -> String {
    let (remaining, total) = match (usage.remaining_bytes, usage.data_limit_bytes) {
        (Some(remaining), Some(total)) => (
            format!("{} گیگابایت", format_gigabytes(remaining)),
            format!("{} گیگابایت", format_gigabytes(total)),
        ),
        _ => ("نامحدود".into(), "نامحدود".into()),
    };
    let time_remaining = match usage.remaining_days {
        Some(0) => "منقضی شده".into(),
        Some(days) => format!("{} روز", persian_digits(days)),
        None => "بدون محدودیت زمانی".into(),
    };
    let expired_notice = if usage.expired {
        "\n⚠️ اشتراک شما منقضی شده است."
    } else {
        ""
    };
    format!(
        "📋 وضعیت اشتراک شما\n👤 نام کاربری پنل: {}\n📦 حجم باقی‌مانده: {} از {}\n📊 مصرف‌شده: {}\n⏳ زمان باقی‌مانده: {}{}",
        usage.username,
        remaining,
        total,
        format_consumed_traffic(usage.used_traffic_bytes),
        time_remaining,
        expired_notice
    )
}

fn subscription_period_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "📅 اشتراک یک‌ماهه",
            "user:plans:month",
        )],
        vec![InlineKeyboardButton::callback("🔙 بازگشت", "user:main")],
    ])
}

fn monthly_plans_keyboard(tariffs: &[SubscriptionTariff]) -> InlineKeyboardMarkup {
    monthly_plan_keyboard(
        tariffs,
        |tariff| format!("user:plan:id:{}", tariff.id),
        "user:buy",
    )
}

fn monthly_renewal_plans_keyboard(
    tariffs: &[SubscriptionTariff],
    username: &str,
) -> InlineKeyboardMarkup {
    monthly_plan_keyboard(
        tariffs,
        |tariff| format!("user:renew:plan:id:{}:{username}", tariff.id),
        "user:renew",
    )
}

fn subscription_payment_options_keyboard(
    tariff_id: i64,
    renewal_username: Option<&str>,
) -> InlineKeyboardMarkup {
    let (card_callback, wallet_callback) = if let Some(username) = renewal_username {
        (
            format!("user:rc:{tariff_id}:{username}"),
            format!("user:rw:{tariff_id}:{username}"),
        )
    } else {
        (
            format!("user:pc:{tariff_id}"),
            format!("user:pw:{tariff_id}"),
        )
    };
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("💳 کارت‌به‌کارت", card_callback),
            InlineKeyboardButton::callback("👛 پرداخت با کیف پول", wallet_callback),
        ],
        vec![InlineKeyboardButton::callback(
            "🏠 بازگشت به منوی اصلی",
            "user:main",
        )],
    ])
}

fn monthly_plan_keyboard(
    tariffs: &[SubscriptionTariff],
    callback_for_plan: impl Fn(&SubscriptionTariff) -> String,
    back_callback: &str,
) -> InlineKeyboardMarkup {
    let mut rows = tariffs
        .chunks(2)
        .map(|plans| {
            plans
                .iter()
                .map(|tariff| {
                    InlineKeyboardButton::callback(
                        format!(
                            "📦 {} گیگ | 💰 {}",
                            persian_digits(tariff.volume_gb),
                            format_toman(tariff.price_toman)
                        ),
                        callback_for_plan(tariff),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    rows.push(vec![InlineKeyboardButton::callback(
        "🔙 بازگشت",
        back_callback,
    )]);
    InlineKeyboardMarkup::new(rows)
}

fn persian_digits(value: impl ToString) -> String {
    value
        .to_string()
        .chars()
        .map(|digit| match digit {
            '0' => '۰',
            '1' => '۱',
            '2' => '۲',
            '3' => '۳',
            '4' => '۴',
            '5' => '۵',
            '6' => '۶',
            '7' => '۷',
            '8' => '۸',
            '9' => '۹',
            other => other,
        })
        .collect()
}

fn normalize_card_number(value: &str) -> Option<String> {
    let mut digits = String::new();
    for character in value.chars() {
        let digit = match character {
            '0'..='9' => character,
            '۰' => '0',
            '۱' => '1',
            '۲' => '2',
            '۳' => '3',
            '۴' => '4',
            '۵' => '5',
            '۶' => '6',
            '۷' => '7',
            '۸' => '8',
            '۹' => '9',
            '٠' => '0',
            '١' => '1',
            '٢' => '2',
            '٣' => '3',
            '٤' => '4',
            '٥' => '5',
            '٦' => '6',
            '٧' => '7',
            '٨' => '8',
            '٩' => '9',
            ' ' | '\t' | '-' | '–' => continue,
            _ => return None,
        };
        digits.push(digit);
    }

    (digits.len() == 16).then_some(digits)
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn format_toman(amount: u32) -> String {
    let reversed_digits = amount.to_string().chars().rev().collect::<Vec<_>>();
    let mut grouped_reversed = String::new();
    for (index, digit) in reversed_digits.iter().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped_reversed.push('٬');
        }
        grouped_reversed.push(*digit);
    }

    let grouped = grouped_reversed.chars().rev().collect::<String>();
    format!("{} تومان", persian_digits(grouped))
}

fn format_signed_toman(amount: i64) -> String {
    let reversed_digits = amount
        .unsigned_abs()
        .to_string()
        .chars()
        .rev()
        .collect::<Vec<_>>();
    let mut grouped_reversed = String::new();
    for (index, digit) in reversed_digits.iter().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped_reversed.push('٬');
        }
        grouped_reversed.push(*digit);
    }

    let grouped = grouped_reversed.chars().rev().collect::<String>();
    format!(
        "{}{} تومان",
        if amount < 0 { "−" } else { "" },
        persian_digits(grouped)
    )
}

fn wallet_message(balance: i64, debt: i64) -> String {
    let mut paragraphs = vec![format!(
        "〽️ موجودی کیف پول شما: {}",
        format_signed_toman(balance)
    )];
    if debt > 0 {
        paragraphs.push(format!("⚠️ بدهی کمیسیون: {}", format_signed_toman(debt)));
    }
    paragraphs.push(
        "🚫 این کیف پول فعلاً قابل شارژ نیست و فقط مبالغ حاصل از زیرمجموعه‌گیری به آن اضافه می‌شود."
            .to_owned(),
    );
    paragraphs.push(
        "🛍️ هر وقت بخواهید، می‌توانید هنگام خرید یا تمدید اشتراک از موجودی کیف پول استفاده کنید."
            .to_owned(),
    );
    paragraphs.join("\n\n")
}

fn parse_management_number(value: &str) -> Option<u64> {
    let mut digits = String::new();
    for character in value.chars() {
        match character {
            '0'..='9' => digits.push(character),
            '۰' => digits.push('0'),
            '۱' => digits.push('1'),
            '۲' => digits.push('2'),
            '۳' => digits.push('3'),
            '۴' => digits.push('4'),
            '۵' => digits.push('5'),
            '۶' => digits.push('6'),
            '۷' => digits.push('7'),
            '۸' => digits.push('8'),
            '۹' => digits.push('9'),
            '٠' => digits.push('0'),
            '١' => digits.push('1'),
            '٢' => digits.push('2'),
            '٣' => digits.push('3'),
            '٤' => digits.push('4'),
            '٥' => digits.push('5'),
            '٦' => digits.push('6'),
            '٧' => digits.push('7'),
            '٨' => digits.push('8'),
            '٩' => digits.push('9'),
            ',' | '٬' | ' ' | '\t' => {}
            _ => return None,
        }
    }
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn normalize_support_username(value: &str) -> Option<String> {
    let value = value.trim().strip_prefix('@').unwrap_or(value.trim());
    if !(5..=32).contains(&value.len())
        || !value
            .bytes()
            .all(|character| character.is_ascii_alphanumeric() || character == b'_')
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

fn parse_management_signed_number(value: &str) -> Option<i64> {
    let value = value.trim();
    let (negative, digits) =
        if let Some(value) = value.strip_prefix('-').or_else(|| value.strip_prefix('−')) {
            (true, value)
        } else if let Some(value) = value.strip_prefix('+') {
            (false, value)
        } else {
            (false, value)
        };
    let magnitude = i64::try_from(parse_management_number(digits)?).ok()?;
    if negative {
        magnitude.checked_neg()
    } else {
        Some(magnitude)
    }
}

fn join_channel_keyboard(channels: &[RequiredChannel]) -> InlineKeyboardMarkup {
    let mut rows = channels
        .iter()
        .map(|channel| {
            vec![InlineKeyboardButton::url(
                format!("📢 عضویت در @{}", channel.username),
                Url::parse(&format!("https://t.me/{}", channel.username))
                    .expect("validated channel username forms a valid Telegram URL"),
            )]
        })
        .collect::<Vec<_>>();
    rows.push(vec![InlineKeyboardButton::callback(
        "✅ عضو شدم",
        "user:join:check",
    )]);
    InlineKeyboardMarkup::new(rows)
}

fn panel_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            "🔐 ورود / اتصال به پنل",
            "owner:panel:login",
        )],
        vec![InlineKeyboardButton::callback("🔙 بازگشت", "owner:main")],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use teloxide::types::InlineKeyboardButtonKind;

    #[test]
    fn reconnects_panel_after_transient_or_expired_session_errors() {
        assert!(should_reconnect_after_panel_error(
            &panel::PanelError::Request
        ));
        assert!(should_reconnect_after_panel_error(
            &panel::PanelError::HttpStatus(401)
        ));
        assert!(should_reconnect_after_panel_error(
            &panel::PanelError::HttpStatus(503)
        ));
        assert!(!should_reconnect_after_panel_error(
            &panel::PanelError::HttpStatus(403)
        ));
        assert!(!should_reconnect_after_panel_error(
            &panel::PanelError::UserNotFound
        ));
    }

    #[test]
    fn retries_panel_authentication_only_for_transient_errors() {
        assert!(is_transient_panel_error(&panel::PanelError::Request));
        assert!(is_transient_panel_error(&panel::PanelError::HttpStatus(
            503
        )));
        assert!(!is_transient_panel_error(&panel::PanelError::Unauthorized));
    }

    #[test]
    fn formats_prices_with_persian_digits_and_separators() {
        assert_eq!(format_toman(600_000), "۶۰۰٬۰۰۰ تومان");
    }

    #[test]
    fn parses_admin_tariff_values_in_persian_digits() {
        assert_eq!(parse_management_number("۱٬۰۲۵٬۰۰۰"), Some(1_025_000));
        assert_eq!(parse_management_number("300"), Some(300));
        assert_eq!(parse_management_number("۰"), Some(0));
        assert_eq!(parse_management_number("-10"), None);
        assert_eq!(parse_management_number("10 گیگ"), None);
    }

    #[test]
    fn normalizes_support_usernames_with_optional_at_sign() {
        assert_eq!(
            normalize_support_username("@Falcon_Help"),
            Some("falcon_help".to_owned())
        );
        assert_eq!(
            normalize_support_username("falcon_help"),
            Some("falcon_help".to_owned())
        );
        assert!(normalize_support_username("abc").is_none());
        assert!(normalize_support_username("support-name").is_none());
        assert!(normalize_support_username("support name").is_none());
    }

    #[test]
    fn normalizes_public_required_channel_usernames() {
        assert_eq!(
            state::normalize_required_channel_username("@Falcon_News"),
            Some("falcon_news".to_owned())
        );
        assert_eq!(
            state::normalize_required_channel_username("  Falcon_News  "),
            Some("falcon_news".to_owned())
        );
        assert!(state::normalize_required_channel_username("abc").is_none());
        assert!(state::normalize_required_channel_username("@12345").is_none());
        assert!(state::normalize_required_channel_username("news-channel").is_none());
    }

    #[test]
    fn required_channel_management_and_join_keyboards_show_all_actions() {
        let channels = vec![
            RequiredChannel {
                username: "falcon_news".into(),
            },
            RequiredChannel {
                username: "haku_news".into(),
            },
        ];
        let keyboard = required_channel_management_keyboard(true, &channels).inline_keyboard;
        let callbacks = keyboard
            .iter()
            .flatten()
            .filter_map(|button| match &button.kind {
                InlineKeyboardButtonKind::CallbackData(data) => Some(data.as_str()),
                InlineKeyboardButtonKind::Url(_) | InlineKeyboardButtonKind::CopyText(_) => None,
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(callbacks.contains(&"owner:channels:add"));
        assert!(callbacks.contains(&"owner:channels:toggle"));
        assert!(callbacks.contains(&"owner:channels:remove:falcon_news"));
        assert!(callbacks.contains(&"owner:channels:remove:haku_news"));

        let join_rows = join_channel_keyboard(&channels).inline_keyboard;
        assert_eq!(join_rows.len(), 3);
        assert!(matches!(
            &join_rows[0][0].kind,
            InlineKeyboardButtonKind::Url(url) if url.as_str() == "https://t.me/falcon_news"
        ));
        assert!(matches!(
            &join_rows[1][0].kind,
            InlineKeyboardButtonKind::Url(url) if url.as_str() == "https://t.me/haku_news"
        ));
        assert!(matches!(
            &join_rows[2][0].kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:join:check"
        ));
    }

    #[test]
    fn monthly_tariff_buttons_use_two_columns_and_have_emojis() {
        let tariffs = test_tariffs();
        let rows = monthly_plans_keyboard(&tariffs).inline_keyboard;

        assert_eq!(rows.len(), tariffs.chunks(2).len() + 1);
        assert!(rows[..rows.len() - 1].iter().all(|row| {
            (1..=2).contains(&row.len()) && row.iter().all(|button| button.text.starts_with("📦"))
        }));
        assert!(rows[..rows.len() - 1]
            .iter()
            .flatten()
            .zip(tariffs.iter())
            .all(|(button, tariff)| {
                button
                    .text
                    .contains(&format!("{} گیگ", persian_digits(tariff.volume_gb)))
                    && button.text.contains(&format_toman(tariff.price_toman))
            }));
        assert!(rows.last().unwrap()[0].text.starts_with("🔙"));
        assert!(matches!(
            &rows[0][0].kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:plan:id:7"
        ));
    }

    #[test]
    fn renewal_plan_buttons_match_purchase_plan_format() {
        let tariffs = test_tariffs();
        let rows = monthly_renewal_plans_keyboard(&tariffs, "haku-123456").inline_keyboard;

        assert_eq!(rows.len(), tariffs.chunks(2).len() + 1);
        assert!(rows[..rows.len() - 1].iter().all(|row| {
            (1..=2).contains(&row.len()) && row.iter().all(|button| button.text.starts_with("📦"))
        }));
        assert!(rows[..rows.len() - 1]
            .iter()
            .flatten()
            .zip(tariffs.iter())
            .all(|(button, tariff)| {
                button
                    .text
                    .contains(&format!("{} گیگ", persian_digits(tariff.volume_gb)))
                    && button.text.contains(&format_toman(tariff.price_toman))
            }));
    }

    fn test_tariffs() -> Vec<SubscriptionTariff> {
        vec![
            SubscriptionTariff {
                id: 7,
                volume_gb: 10,
                price_toman: 60_000,
                enabled: true,
            },
            SubscriptionTariff {
                id: 8,
                volume_gb: 20,
                price_toman: 115_000,
                enabled: true,
            },
            SubscriptionTariff {
                id: 9,
                volume_gb: 25,
                price_toman: 140_000,
                enabled: true,
            },
        ]
    }

    #[test]
    fn user_menu_matches_requested_rows_labels_and_styles() {
        let rows = user_keyboard().inline_keyboard;

        assert_eq!(
            rows.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![1, 1, 1, 1, 1]
        );
        assert_eq!(
            rows.iter()
                .flatten()
                .map(|button| button.text.as_str())
                .collect::<Vec<_>>(),
            vec![
                "🚀 خرید و تمدید اشتراک 💰",
                "⚡️ مدیریت اشتراک ⚙️",
                "🎁 زیر مجموعه گیری ‼️",
                "〽️ کیف پول من 💵",
                "👨🏻‍💻 پشتیبانی"
            ]
        );
        assert!(rows.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:purchase-menu"
        )));
        assert!(rows.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:management-menu"
        )));
        assert!(rows.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:referral"
        )));
        assert!(rows.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:wallet"
        )));
        assert!(rows.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:support"
        )));
        assert!(purchase_menu_keyboard()
            .inline_keyboard
            .iter()
            .flatten()
            .any(|button| matches!(
                &button.kind,
                InlineKeyboardButtonKind::CallbackData(data) if data == "user:buy"
            )));
        assert!(purchase_menu_keyboard()
            .inline_keyboard
            .iter()
            .flatten()
            .any(|button| matches!(
                &button.kind,
                InlineKeyboardButtonKind::CallbackData(data) if data == "user:renew"
            )));
        let purchase_buttons = purchase_menu_keyboard().inline_keyboard;
        assert_eq!(purchase_buttons[0][0].text, "🔋 خرید اشتراک  🚀");
        assert_eq!(purchase_buttons[1][0].text, "🚀 تمدید اشتراک ♻️");
        assert!(subscription_management_keyboard()
            .inline_keyboard
            .iter()
            .flatten()
            .any(|button| matches!(
                &button.kind,
                InlineKeyboardButtonKind::CallbackData(data) if data == "user:usage"
            )));
        assert!(subscription_management_keyboard()
            .inline_keyboard
            .iter()
            .flatten()
            .any(|button| matches!(
                &button.kind,
                InlineKeyboardButtonKind::CallbackData(data) if data == "user:link"
            )));
        assert!(matches!(
            &support_keyboard("ipublicsupport").inline_keyboard[0][0].kind,
            InlineKeyboardButtonKind::Url(url) if url.as_str() == "https://t.me/ipublicsupport"
        ));
        assert!(matches!(
            &support_keyboard("falcon_help").inline_keyboard[0][0].kind,
            InlineKeyboardButtonKind::Url(url) if url.as_str() == "https://t.me/falcon_help"
        ));

        let rich_message = user_menu_rich_message(USER_MENU_PROMPT);
        assert!(rich_message.is_rtl);
        assert_eq!(
            rich_message.blocks[0],
            UserMenuBlock::Paragraph {
                text: UserMenuRichText::Plain(USER_MENU_PROMPT.to_owned()),
            }
        );
        let button_rows = rich_message.blocks[1..]
            .iter()
            .map(|block| match block {
                UserMenuBlock::Buttons { buttons } => buttons,
                UserMenuBlock::Paragraph { .. } | UserMenuBlock::BlockQuotation { .. } => {
                    panic!("expected a button row")
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            button_rows.iter().map(|row| row.len()).collect::<Vec<_>>(),
            vec![1, 1, 1, 1, 1]
        );
        assert_eq!(
            button_rows
                .iter()
                .flat_map(|row| row.iter())
                .map(|button| button.text.as_str())
                .collect::<Vec<_>>(),
            vec![
                "🚀 خرید و تمدید اشتراک 💰",
                "⚡️ مدیریت اشتراک ⚙️",
                "🎁 زیر مجموعه گیری ‼️",
                "〽️ کیف پول من 💵",
                "👨🏻‍💻 پشتیبانی"
            ]
        );
        assert_eq!(
            button_rows
                .iter()
                .flat_map(|row| row.iter())
                .map(|button| button.callback_data.as_deref().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec![
                "user:purchase-menu",
                "user:management-menu",
                "user:referral",
                "user:wallet",
                "user:support"
            ]
        );
        assert!(button_rows[..4]
            .iter()
            .flat_map(|row| row.iter())
            .all(|button| button.style == UserMenuButtonStyle::Primary));
        assert_eq!(button_rows[4][0].style, UserMenuButtonStyle::Danger);
    }

    #[test]
    fn text_keyboard_preserves_actions_and_marks_back_buttons_red() {
        let rich_message = text_message_rich_message(
            "برای ارتباط با پشتیبانی از دکمه زیر استفاده کن 👇🏼",
            &support_keyboard("ipublicsupport"),
        )
        .unwrap();

        assert_eq!(rich_message.blocks.len(), 3);
        assert!(matches!(
            &rich_message.blocks[1],
            UserMenuBlock::Buttons { buttons }
                if buttons[0].url.as_deref() == Some("https://t.me/ipublicsupport")
        ));
        assert!(matches!(
            &rich_message.blocks[2],
            UserMenuBlock::Buttons { buttons }
                if buttons[0].callback_data.as_deref() == Some("user:main")
                    && buttons[0].style == UserMenuButtonStyle::Danger
        ));
    }

    #[test]
    fn subscription_checkout_offers_card_and_wallet_for_new_and_renewal_orders() {
        let purchase = subscription_payment_options_keyboard(7, None).inline_keyboard;
        assert!(matches!(
            &purchase[0][0].kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:pc:7"
        ));
        assert!(matches!(
            &purchase[0][1].kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:pw:7"
        ));
        let renewal = subscription_payment_options_keyboard(7, Some("haku-123456")).inline_keyboard;
        assert!(matches!(
            &renewal[0][0].kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:rc:7:haku-123456"
        ));
        assert!(matches!(
            &renewal[0][1].kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:rw:7:haku-123456"
        ));
        assert_eq!(
            parse_subscription_checkout_callback("user:rw:7:haku-123456"),
            Some((7, Some("haku-123456"), true))
        );
    }

    #[test]
    fn owner_back_buttons_are_red_in_rich_text_keyboards() {
        let rich_message = text_message_rich_message("مدیریت پنل", &panel_keyboard()).unwrap();

        assert!(matches!(
            &rich_message.blocks[2],
            UserMenuBlock::Buttons { buttons }
                if buttons[0].callback_data.as_deref() == Some("owner:main")
                    && buttons[0].style == UserMenuButtonStyle::Danger
        ));
    }

    #[test]
    fn receipt_confirmation_rich_message_has_formatting_and_red_home_button() {
        let rich_message = receipt_confirmation_rich_message();

        assert!(matches!(
            &rich_message.blocks[0],
            UserMenuBlock::Paragraph {
                text: UserMenuRichText::Entity(UserMenuRichTextEntity::Bold { .. })
            }
        ));
        assert!(matches!(
            &rich_message.blocks[2],
            UserMenuBlock::BlockQuotation { blocks }
                if matches!(
                    &blocks[0],
                    UserMenuBlock::Paragraph {
                        text: UserMenuRichText::Plain(text)
                    } if text.contains("از ارسال پیام به پشتیبانی خودداری کنید")
                )
        ));
        assert!(matches!(
            rich_message.blocks.last(),
            Some(UserMenuBlock::Buttons { buttons })
                if buttons[0].callback_data.as_deref() == Some("user:menu")
                    && buttons[0].style == UserMenuButtonStyle::Danger
        ));
    }

    #[test]
    fn rich_message_edit_payload_targets_the_existing_message() {
        let payload = serde_json::to_value(EditRichMessageRequest {
            chat_id: 123,
            message_id: 456,
            rich_message: user_menu_rich_message("منو"),
        })
        .unwrap();

        assert_eq!(payload["chat_id"], 123);
        assert_eq!(payload["message_id"], 456);
        assert!(payload["rich_message"]["blocks"].is_array());
        assert!(payload.get("text").is_none());
    }

    #[test]
    fn final_review_caption_replaces_pending_status_and_keeps_order_details() {
        let pending =
            "🧾 سفارش #۱۲ (جدید) — ⏳ در انتظار بررسی\n📦 پلن: ۲۰ گیگ\n🕓 ۱۴۰۵/۰۸/۲۹\nوضعیت: ⏳ منتظر تأیید";

        let approved = final_payment_review_caption(pending, PaymentReviewStatus::Approved);
        assert!(approved.contains("🧾 سفارش #۱۲ (جدید) — ✅ تأیید شد"));
        assert!(approved.contains("وضعیت: ✅ تأیید شد"));
        assert!(approved.contains("🕓 ۱۴۰۵/۰۸/۲۹"));

        let rejected = final_payment_review_caption(pending, PaymentReviewStatus::Rejected);
        assert!(rejected.contains("🧾 سفارش #۱۲ (جدید) — ❌ رد شد"));
        assert!(rejected.contains("وضعیت: ❌ رد شد"));
        assert!(rejected.contains("📦 پلن: ۲۰ گیگ"));
    }

    #[test]
    fn wallet_review_caption_shows_partial_breakdown_and_full_wallet_alert() {
        let mut order = PaymentOrder {
            id: 8,
            telegram_user_id: 42,
            account_name: "Test User".into(),
            telegram_username: Some("test".into()),
            panel_username: "haku-420001".into(),
            plan_gb: 10,
            price_toman: 100_000,
            wallet_used_toman: 30_000,
            amount_due_toman: 70_000,
            is_wallet_payment: true,
            is_renewal: false,
            receipt_file_id: Some("receipt".into()),
        };
        let partial = payment_review_caption(&order);
        assert!(partial.contains("💰 مبلغ اصلی اشتراک: ۱۰۰٬۰۰۰ تومان"));
        assert!(partial.contains("👛 کسر از کیف پول: ۳۰٬۰۰۰ تومان"));
        assert!(partial.contains("<b>💳 مبلغ نهایی پرداخت: ۷۰٬۰۰۰ تومان</b>"));
        assert!(!partial.contains("‼️‼️‼️‼️‼️"));

        order.wallet_used_toman = 100_000;
        order.amount_due_toman = 0;
        order.receipt_file_id = None;
        let fully_paid = payment_review_caption(&order);
        assert!(fully_paid.contains("‼️‼️‼️‼️‼️"));
        assert!(fully_paid.contains("پرداخت کامل با کیف پول"));
        assert!(fully_paid.contains("لطفاً درخواست را بررسی و تأیید یا رد کنید"));
    }

    #[test]
    fn final_payment_amount_is_bold_in_user_and_reviewer_messages() {
        let order = PaymentOrder {
            id: 8,
            telegram_user_id: 42,
            account_name: "Test User".into(),
            telegram_username: None,
            panel_username: "haku-420001".into(),
            plan_gb: 10,
            price_toman: 60_000,
            wallet_used_toman: 0,
            amount_due_toman: 60_000,
            is_wallet_payment: false,
            is_renewal: false,
            receipt_file_id: Some("receipt".into()),
        };
        assert!(payment_amount_details_html(&order)
            .contains("<b>💳 مبلغ نهایی پرداخت: ۶۰٬۰۰۰ تومان</b>"));

        let rich_message = text_message_rich_message_with_bold_lines(
            "جزئیات سفارش\n💳 مبلغ نهایی پرداخت: ۶۰٬۰۰۰ تومان",
            &subscription_back_keyboard(),
            &["💳 مبلغ نهایی پرداخت:"],
        )
        .unwrap();
        assert!(matches!(
            &rich_message.blocks[1],
            UserMenuBlock::Paragraph {
                text: UserMenuRichText::Entity(UserMenuRichTextEntity::Bold { text })
            } if matches!(
                text.as_ref(),
                UserMenuRichText::Plain(line)
                    if line == "💳 مبلغ نهایی پرداخت: ۶۰٬۰۰۰ تومان"
            )
        ));
        assert!(matches!(
            rich_message.blocks.last(),
            Some(UserMenuBlock::Buttons { buttons })
                if buttons[0].style == UserMenuButtonStyle::Danger
        ));
    }

    #[test]
    fn client_selection_lists_status_and_separates_usage_from_renewal() {
        let clients = vec![
            panel::PanelClient {
                username: "haku-123456".into(),
                is_active: true,
            },
            panel::PanelClient {
                username: "haku-123789".into(),
                is_active: false,
            },
        ];
        let renewal = panel_client_keyboard(&clients, ClientListPurpose::Renewal).inline_keyboard;
        let usage = panel_client_keyboard(&clients, ClientListPurpose::Usage).inline_keyboard;
        let links = panel_client_keyboard(&clients, ClientListPurpose::Link).inline_keyboard;

        assert_eq!(renewal[0][0].text, "🟢 haku-123456");
        assert_eq!(renewal[1][0].text, "🔴 haku-123789");
        assert!(matches!(
            &renewal[0][0].kind,
            InlineKeyboardButtonKind::CallbackData(data)
                if data == "user:renew:select:haku-123456"
        ));
        assert!(matches!(
            &usage[0][0].kind,
            InlineKeyboardButtonKind::CallbackData(data)
                if data == "user:usage:select:haku-123456"
        ));
        assert!(matches!(
            &links[0][0].kind,
            InlineKeyboardButtonKind::CallbackData(data)
                if data == "user:link:select:haku-123456"
        ));
        assert!(matches!(
            &monthly_renewal_plans_keyboard(&test_tariffs(), "haku-123456").inline_keyboard[0][0]
                .kind,
            InlineKeyboardButtonKind::CallbackData(data)
                if data == "user:renew:plan:id:7:haku-123456"
        ));
    }

    #[test]
    fn formats_gigabytes_with_persian_digits() {
        assert_eq!(format_gigabytes(1_073_741_824), "۱٫۰۰");
    }

    #[test]
    fn subscription_usage_message_shows_consumed_traffic_for_unlimited_users() {
        let usage = panel::SubscriptionUsage {
            username: "haku-123456".into(),
            data_limit_bytes: None,
            used_traffic_bytes: 125 * 1024 * 1024,
            remaining_bytes: None,
            remaining_days: None,
            expired: false,
        };

        let message = subscription_usage_message(&usage);

        assert!(message.contains("📊 مصرف‌شده: ۱۲۵٫۰۰ مگابایت"));
        assert!(message.contains("📦 حجم باقی‌مانده: نامحدود از نامحدود"));
    }

    #[test]
    fn start_user_menu_greets_the_user_and_shows_service_features() {
        assert_eq!(
            start_user_menu_prompt("نام کاربر"),
            "🌐 سلام نام کاربر عزیز!\n\n⚡️ سرعت بالا، بدون قطعی\n🚗 ترافیک نیم بها\n💎 کانفیگ بدون تبلیغ\n👨🏼‍💻 پشتیبانی تا آخرین روز سرویس\n💸 ضمانت بازگشت وجه\n\nحالا انتخاب کن🔻"
        );
        assert!(start_user_menu_prompt(" ").starts_with("🌐 سلام دوست عزیز!"));
    }

    #[test]
    fn subscription_qr_is_png_and_caption_includes_copyable_link() {
        let link = "https://panel.example/subscription/example";
        let png = subscription_qr_png(link).unwrap();
        let caption = subscription_link_caption("haku-123456", link);

        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(caption.contains(link));
        assert!(caption.contains("یک‌بار"));
    }

    #[test]
    fn owner_menu_shows_admin_management() {
        let owner_menu = owner_keyboard().inline_keyboard;

        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:admins:manage"
        )));
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:payment:manage"
        )));
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:support:set"
        )));
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:channels:manage"
        )));
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:sales:manage"
        )));
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:referral:manage"
        )));
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:database:manage"
        )));
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:users:manage"
        )));
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:wallet:manage"
        )));
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:finance:manage"
        )));
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:disk:manage"
        )));
    }

    #[test]
    fn financial_statistics_keyboard_has_separate_owner_options() {
        let rows = financial_statistics_keyboard().inline_keyboard;
        let callbacks = rows
            .iter()
            .flatten()
            .filter_map(|button| match &button.kind {
                InlineKeyboardButtonKind::CallbackData(data) => Some(data.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            callbacks,
            vec![
                "owner:finance:revenue",
                "owner:finance:commissions",
                "owner:finance:referred_users",
                "owner:finance:referral_orders",
                "owner:finance:average_referral_revenue",
                "owner:finance:wallet_balances",
                "owner:main",
            ]
        );
        assert!(rows.iter().all(|row| row.len() == 1));
    }

    #[test]
    fn managed_user_details_show_inviter_referrals_and_paid_commission() {
        let message = managed_user_details_message(&BotUserDetails {
            telegram_user_id: 20,
            username: Some("invitee".into()),
            first_name: Some("Invitee".into()),
            referrer_user_id: Some(10),
            referrer_username: Some("inviter".into()),
            referrer_first_name: Some("Inviter".into()),
            referral_count: 3,
            commissions_paid_toman: 17_000,
        });

        assert!(message.contains("شناسه‌ی ۱۰"));
        assert!(message.contains("@inviter"));
        assert!(message.contains("تعداد زیرمجموعه‌ها: ۳"));
        assert!(message.contains("پورسانت پرداخت‌شده به این کاربر: ۱۷٬۰۰۰ تومان"));
    }

    #[test]
    fn wallet_management_keyboard_offers_manual_adjustment() {
        let rows = wallet_management_keyboard().inline_keyboard;
        let callbacks = rows
            .iter()
            .flatten()
            .filter_map(|button| match &button.kind {
                InlineKeyboardButtonKind::CallbackData(data) => Some(data.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(callbacks.contains(&"owner:wallet:adjust"));
        assert!(callbacks.contains(&"owner:wallet:increase_all"));
        assert!(callbacks.contains(&"owner:wallet:decrease_all"));
        assert!(callbacks.contains(&"owner:main"));
    }

    #[test]
    fn bulk_wallet_adjustment_message_reports_results_and_reason() {
        let message = bulk_wallet_adjustment_message(
            WalletBulkAction::Decrease,
            10_000,
            "اصلاح موجودی",
            WalletBulkAdjustmentSummary {
                targeted_users: 5,
                changed_users: 3,
                unchanged_users: 2,
                total_adjusted_toman: 25_000,
            },
        );

        assert!(message.contains("کاربران بررسی‌شده: ۵"));
        assert!(message.contains("کیف پول‌های تغییرکرده: ۳"));
        assert!(message.contains("مجموع مبلغ کاهش: ۲۵٬۰۰۰ تومان"));
        assert!(message.contains("دلیل: اصلاح موجودی"));
    }

    #[test]
    fn database_management_keyboard_offers_backup_and_restore() {
        let rows = database_management_keyboard().inline_keyboard;
        let callbacks = rows
            .iter()
            .flatten()
            .filter_map(|button| match &button.kind {
                InlineKeyboardButtonKind::CallbackData(data) => Some(data.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(callbacks.contains(&"owner:database:backup"));
        assert!(callbacks.contains(&"owner:database:restore"));
        assert!(callbacks.contains(&"owner:main"));
    }

    #[test]
    fn daily_backup_delay_targets_midnight_in_iran_time() {
        let now = chrono_tz::Asia::Tehran
            .with_ymd_and_hms(2026, 11, 21, 23, 30, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(
            duration_until_next_iran_midnight(now).unwrap(),
            std::time::Duration::from_secs(30 * 60)
        );
    }

    #[test]
    fn daily_disk_space_check_delay_targets_three_pm_in_iran_time() {
        let now = chrono_tz::Asia::Tehran
            .with_ymd_and_hms(2026, 11, 21, 14, 30, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(
            duration_until_next_iran_time(now, 15, 0).unwrap(),
            std::time::Duration::from_secs(30 * 60)
        );
    }

    #[test]
    fn disk_space_warning_uses_a_strict_fifteen_percent_threshold() {
        assert!(disk_space_below_alert_threshold(DiskSpaceUsage {
            total_bytes: 1_000,
            free_bytes: 149,
        }));
        assert!(!disk_space_below_alert_threshold(DiskSpaceUsage {
            total_bytes: 1_000,
            free_bytes: 150,
        }));
    }

    #[test]
    fn disk_space_is_displayed_in_gigabytes_without_byte_totals() {
        assert_eq!(format_disk_space(1_073_741_824), "۱٫۰۰ گیگابایت");
    }

    #[test]
    fn start_referral_parameter_is_parsed_only_from_start_commands() {
        assert!(is_start_command("/start ref_123"));
        assert!(is_start_command("/start@foxngbot ref_123"));
        assert!(!is_start_command("/starting ref_123"));
        assert_eq!(start_referrer_id("/start ref_123"), Some(123));
        assert_eq!(start_referrer_id("/start@foxngbot ref_123"), Some(123));
        assert_eq!(start_referrer_id("/start ref_0"), None);
        assert_eq!(start_referrer_id("/start ref_invalid"), None);
        assert_eq!(start_referrer_id("/help ref_123"), None);
    }

    #[test]
    fn referral_keyboard_contains_the_invite_link_and_home_button() {
        let link = "https://t.me/foxngbot?start=ref_123";
        let keyboard = referral_keyboard(link);
        let rows = &keyboard.inline_keyboard;
        assert!(matches!(
            &rows[0][0].kind,
            InlineKeyboardButtonKind::CopyText(button) if button.text == link
        ));
        assert_eq!(rows[0][0].text, "📋 کپی لینک دعوت");
        assert!(matches!(
            &rows[1][0].kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "user:main"
        ));

        let rich_message = text_message_rich_message("لینک دعوت", &keyboard).unwrap();
        let payload = serde_json::to_value(rich_message).unwrap();
        assert_eq!(
            payload["blocks"][1]["buttons"][0]["copy_text"]["text"],
            link
        );
        assert_eq!(payload["blocks"][2]["buttons"][0]["style"], "danger");
    }

    #[test]
    fn referral_message_explains_the_commission_rate_and_qualifying_orders() {
        let message = referral_message("https://t.me/foxngbot?start=ref_123", 2, 15_000, 15);

        assert!(message.contains("پورسانت فعلی: ۱۵٪"));
        assert!(message.contains("ربات را Start کند"));
        assert!(message.contains("هر خرید یا تمدید موفق او، ۱۵٪"));
        assert!(message.contains("به کیف پول شما اضافه می‌شود"));
        assert!(message.contains("https://t.me/foxngbot?start=ref_123"));
        assert!(message.contains("مجموع پورسانت ثبت‌شده: ۱۵٬۰۰۰ تومان"));
    }

    #[test]
    fn referral_commission_notice_reports_purchase_or_renewal_and_new_balance() {
        let credit = state::ReferralCommissionCredit {
            referrer_user_id: 10,
            amount_toman: 17_000,
            balance_after_toman: 42_000,
        };

        let purchase = referral_commission_notification(&credit, false);
        assert!(purchase.contains("🎉"));
        assert!(purchase.contains("یکی از زیرمجموعه‌های شما یک اشتراک را با موفقیت خرید"));
        assert!(purchase.contains("مبلغ ۱۷٬۰۰۰ تومان به کیف پول شما اضافه شد"));
        assert!(purchase.contains("موجودی جدید کیف پول شما: ۴۲٬۰۰۰ تومان"));

        let renewal = referral_commission_notification(&credit, true);
        assert!(renewal.contains("یکی از زیرمجموعه‌های شما اشتراک خود را با موفقیت تمدید کرد"));
    }

    #[test]
    fn parses_and_formats_signed_wallet_amounts() {
        assert_eq!(parse_management_signed_number("-۱۲٬۵۰۰"), Some(-12_500));
        assert_eq!(parse_management_signed_number("−250"), Some(-250));
        assert_eq!(parse_management_signed_number("+900"), Some(900));
        assert_eq!(parse_management_signed_number("۹۰۰"), Some(900));
        assert_eq!(parse_management_signed_number("--1"), None);
        assert_eq!(parse_management_signed_number(""), None);
        assert_eq!(format_signed_toman(-12_500), "−۱۲٬۵۰۰ تومان");
    }

    #[test]
    fn wallet_message_shows_exact_balance_and_usage_notice() {
        let message = wallet_message(12_500, 0);

        assert!(message.contains("موجودی دقیق کیف پول شما: ۱۲٬۵۰۰ تومان"));
        assert!(message.contains("فعلاً قابل شارژ نیست"));
        assert!(message.contains("فقط مبالغ حاصل از زیرمجموعه‌گیری"));
        assert!(message.contains("هنگام خرید اشتراک از موجودی کیف پول استفاده کنید"));
        assert!(!message.contains("بدهی کمیسیون"));
    }

    #[test]
    fn purchase_status_keyboard_toggles_purchase_and_renewal_independently() {
        fn callback_data(button: &InlineKeyboardButton) -> &str {
            match &button.kind {
                InlineKeyboardButtonKind::CallbackData(data) => data,
                _ => panic!("expected callback button"),
            }
        }
        let combinations = [
            (
                true,
                true,
                "owner:sales:purchase:disable",
                "owner:sales:renewal:disable",
            ),
            (
                false,
                true,
                "owner:sales:purchase:enable",
                "owner:sales:renewal:disable",
            ),
            (
                true,
                false,
                "owner:sales:purchase:disable",
                "owner:sales:renewal:enable",
            ),
            (
                false,
                false,
                "owner:sales:purchase:enable",
                "owner:sales:renewal:enable",
            ),
        ];

        for (purchases_enabled, renewals_enabled, purchase_action, renewal_action) in combinations {
            let rows =
                purchase_management_keyboard(purchases_enabled, renewals_enabled).inline_keyboard;
            assert_eq!(callback_data(&rows[0][0]), purchase_action);
            assert_eq!(callback_data(&rows[1][0]), renewal_action);
            assert_eq!(callback_data(&rows[2][0]), "owner:main");
        }
    }

    #[test]
    fn card_numbers_accept_ascii_persian_digits_and_separators() {
        assert_eq!(
            normalize_card_number("۶۰۳۷-۹۹۷۵ ۱۲۳۴ ۵۶۷۸"),
            Some("6037997512345678".into())
        );
        assert_eq!(normalize_card_number("1234abcd56781234"), None);
        assert_eq!(normalize_card_number("123456"), None);
    }

    #[test]
    fn html_escapes_dynamic_values_for_copyable_message_entities() {
        assert_eq!(
            html_escape("https://x.test/a?x=1&y=<value>"),
            "https://x.test/a?x=1&amp;y=&lt;value&gt;"
        );
    }

    #[test]
    fn admin_list_has_removal_button_per_id() {
        let keyboard = admin_list_keyboard(&[123, 456]).inline_keyboard;

        assert!(matches!(
            &keyboard[0][0].kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:admins:remove:123"
        ));
        assert!(matches!(
            &keyboard[1][0].kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:admins:remove:456"
        ));
    }

    #[test]
    fn owner_menu_shows_broadcast_buttons() {
        let owner_menu = owner_keyboard().inline_keyboard;

        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:broadcast:all"
        )));
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:broadcast:subscribers"
        )));
    }

    #[test]
    fn owner_can_manage_tariffs_from_the_main_menu() {
        let owner_menu = owner_keyboard().inline_keyboard;
        assert!(owner_menu.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:tariffs:manage"
        )));

        let tariff = test_tariffs().remove(0);
        let detail = subscription_tariff_detail_keyboard(&tariff).inline_keyboard;
        assert_eq!(detail.len(), 4);
        assert!(detail.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data)
                if data == &format!("owner:tariffs:volume:{}", tariff.id)
        )));
        assert!(detail.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data)
                if data == &format!("owner:tariffs:price:{}", tariff.id)
        )));
        assert!(detail.iter().flatten().any(|button| matches!(
            &button.kind,
            InlineKeyboardButtonKind::CallbackData(data)
                if data == &format!("owner:tariffs:toggle:{}", tariff.id)
        )));
    }

    #[test]
    fn admins_can_review_receipts_and_return_to_menu_only() {
        assert!(is_admin_allowed_callback("payment:approve:123"));
        assert!(is_admin_allowed_callback("payment:reject:123"));
        assert!(is_admin_allowed_callback("user:menu"));
        assert!(!is_admin_allowed_callback("owner:panel:manage"));
        assert!(!is_admin_allowed_callback("user:buy"));
        assert!(!is_admin_allowed_callback("payment:manage"));
    }

    #[test]
    fn formats_receipt_timestamp_as_jalali_with_persian_digits() {
        let datetime = chrono::DateTime::parse_from_rfc3339("2026-11-20T18:25:05Z")
            .unwrap()
            .with_timezone(&chrono_tz::Asia::Tehran);
        let expected_time = persian_digits(format!(
            "{:02}:{:02}:{:02}",
            datetime.hour(),
            datetime.minute(),
            datetime.second()
        ));
        assert!(persian_datetime(datetime).ends_with(&expected_time));
        assert!(persian_datetime(datetime).starts_with("۱۴۰۵/۰۸/۲۹"));
        assert_eq!(datetime.format("%H:%M %:z").to_string(), "21:55 +03:30");
    }
}
