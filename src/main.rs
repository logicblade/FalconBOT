mod panel;
mod state;

use anyhow::{anyhow, Context, Result};
use chrono::{Datelike, Timelike};
use image::{DynamicImage, ImageFormat, Luma};
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use state::{AppState, NewPaymentOrder, PaymentOrder, SetupState};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Cursor, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};
use teloxide::{
    dispatching::UpdateFilterExt,
    dptree,
    prelude::*,
    types::{
        FileId, InlineKeyboardButton, InlineKeyboardButtonKind, InlineKeyboardMarkup, InputFile,
        MessageId, ParseMode, Update, User,
    },
};
use url::Url;
use zeroize::Zeroizing;

const REQUIRED_CHANNEL: &str = "@HakuNGVPN";
const REQUIRED_CHANNEL_URL: &str = "https://t.me/HakuNGVPN";
const MONTHLY_PLAN_GB: [u16; 10] = [10, 20, 30, 50, 80, 100, 150, 200, 250, 300];
const USER_MENU_PROMPT: &str = "از منوی زیر انتخاب کن 👇🏻";

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
    let mut blocks = vec![UserMenuBlock::Paragraph {
        text: UserMenuRichText::Plain(text.to_owned()),
    }];
    for button in keyboard.inline_keyboard.iter().flatten() {
        let (callback_data, url) = match &button.kind {
            InlineKeyboardButtonKind::CallbackData(data) => (Some(data.clone()), None),
            InlineKeyboardButtonKind::Url(url) => (None, Some(url.to_string())),
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
    restore_panel_session(&state).await;

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback_query));

    let bot = Bot::new(token);
    bot.get_me()
        .await
        .context("Could not connect to Telegram. Check network, firewall, and proxy settings")?;

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

async fn restore_panel_session(state: &AppState) {
    let panel_config = match state.panel_config() {
        Ok(config) => config,
        Err(error) => {
            log::error!("Could not read panel configuration during startup: {error}");
            return;
        }
    };
    let Some((saved_url, username)) = panel_config else {
        return;
    };
    let password = match state.panel_password() {
        Ok(Some(password)) => password,
        Ok(None) => {
            log::info!("No panel password is stored in the operating-system credential store");
            return;
        }
        Err(error) => {
            log::error!("Could not load panel credentials from the operating-system credential store: {error}");
            return;
        }
    };
    let base_url = match panel::normalize_panel_url(&saved_url) {
        Ok(url) => url,
        Err(error) => {
            log::error!("Saved panel URL is invalid: {error}");
            return;
        }
    };

    match panel::authenticate(&base_url, &username, &password).await {
        Ok(access_token) => {
            state.set_session(access_token);
            log::info!("Panel session restored automatically");
        }
        Err(error) => {
            log::warn!("Could not restore the panel session automatically: {error}");
        }
    }
}

async fn handle_message(bot: Bot, msg: Message, state: Arc<AppState>) -> Result<()> {
    let Some(sender) = msg.from.as_ref() else {
        return Ok(());
    };
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

    if text == "/start" || text.starts_with("/start ") {
        if msg.chat.is_private() {
            if let Err(error) = state.record_bot_user(sender.id.0) {
                log::error!("Could not record bot user {}: {error}", sender.id);
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
            match is_required_channel_member(&bot, sender.id).await {
                Ok(true) => {
                    let prompt = start_user_menu_prompt(&sender.first_name);
                    send_user_menu(&bot, msg.chat.id, &prompt).await?;
                }
                Ok(false) => {
                    send_join_prompt(&bot, msg.chat.id).await?;
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
                        join_channel_keyboard(),
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
                    "لغو سفارش انجام نشد؛ لطفاً دوباره تلاش کنید یا با پشتیبانی تماس بگیرید.",
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
        SetupState::BroadcastAll => {
            broadcast_message(&bot, &msg, &state, false).await?;
        }
        SetupState::BroadcastSubscribers => {
            broadcast_message(&bot, &msg, &state, true).await?;
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
            let authentication = panel::authenticate(&base_url, &username, &password).await;

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
                    bot.send_message(msg.chat.id, error.user_message())
                        .reply_markup(owner_keyboard())
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

    let is_purchase_action =
        data == "user:buy" || data == "user:plans:month" || data.starts_with("user:plan:");
    let is_renewal_action = data == "user:renew" || data.starts_with("user:renew:");
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
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
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
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
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
        "user:referral" | "user:wallet" => {
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
                return Ok(());
            }
            let message = if data == "user:referral" {
                "🎁 قابلیت زیرمجموعه‌گیری به‌زودی فعال می‌شود."
            } else {
                "〽️ قابلیت کیف پول به‌زودی فعال می‌شود."
            };
            edit_user_message_with_keyboard(&bot, target, message, subscription_back_keyboard())
                .await?;
        }
        data if data.starts_with("payment:cancel:") => {
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
                return Ok(());
            }
            match state.cancel_awaiting_order(query.from.id.0) {
                Ok(()) => {
                    send_user_menu(&bot, query.from.id, "سفارش در انتظار رسید لغو شد.").await?;
                }
                Err(error) => {
                    log::error!(
                        "Could not cancel payment order for user {}: {error}",
                        query.from.id
                    );
                    send_user_menu(
                        &bot,
                        query.from.id,
                        "به صفحه‌ی اصلی برگشتید؛ لغو سفارش در انتظار رسید انجام نشد. برای پیگیری با پشتیبانی تماس بگیرید.",
                    )
                    .await?;
                }
            }
        }
        "user:buy" => {
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
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
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
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
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
                return Ok(());
            }
            let username = data.strip_prefix("user:renew:select:").unwrap_or_default();
            send_renewal_plans(&bot, target, &query.from, &state, username).await?;
        }
        data if data.starts_with("user:renew:plan:") => {
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
                return Ok(());
            }
            let selection = data.strip_prefix("user:renew:plan:").unwrap_or_default();
            let (plan_gb, username) = selection
                .split_once(':')
                .and_then(|(gb, username)| gb.parse::<u16>().ok().map(|gb| (gb, username)))
                .filter(|(gb, _)| monthly_plan_price(*gb).is_some())
                .unwrap_or((0, ""));
            if plan_gb == 0 || username.is_empty() {
                edit_user_message_with_keyboard(
                    &bot,
                    target,
                    "تعرفه یا کلاینت انتخاب‌شده معتبر نیست؛ دوباره از منو انتخاب کنید.",
                    subscription_back_keyboard(),
                )
                .await?;
            } else {
                process_subscription_selection(
                    &bot,
                    target,
                    &query.from,
                    plan_gb,
                    Some(username),
                    &state,
                )
                .await?;
            }
        }
        "user:plans:month" => {
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
                return Ok(());
            }
            edit_user_message_with_keyboard(
                &bot,
                target,
                "پلن خریدتو انتخاب کن 👇",
                monthly_plans_keyboard(),
            )
            .await?;
        }
        "user:main" => {
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
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
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
                return Ok(());
            }
            send_user_menu(&bot, query.from.id, USER_MENU_PROMPT).await?;
        }
        "user:usage" => {
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
                return Ok(());
            }
            send_panel_client_choices(&bot, target, &query.from, &state, ClientListPurpose::Usage)
                .await?;
        }
        "user:link" => {
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
                return Ok(());
            }
            send_panel_client_choices(&bot, target, &query.from, &state, ClientListPurpose::Link)
                .await?;
        }
        data if data.starts_with("user:usage:select:") => {
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
                return Ok(());
            }
            let username = data.strip_prefix("user:usage:select:").unwrap_or_default();
            send_subscription_usage(&bot, target, &query.from, &state, username).await?;
        }
        data if data.starts_with("user:link:select:") => {
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
                return Ok(());
            }
            let username = data.strip_prefix("user:link:select:").unwrap_or_default();
            send_client_subscription_link(&bot, target, &query.from, &state, username).await?;
        }
        "user:support" => {
            edit_user_message_with_keyboard(
                &bot,
                target,
                "برای ارتباط با پشتیبانی از دکمه زیر استفاده کن 👇🏼",
                support_keyboard(),
            )
            .await?;
        }
        data if data.starts_with("payment:approve:") && (is_owner || is_admin) => {
            if let Some(order_id) = data
                .strip_prefix("payment:approve:")
                .and_then(|value| value.parse::<i64>().ok())
            {
                let source_message = query.regular_message().and_then(|message| {
                    message
                        .caption()
                        .map(|caption| (message.id, caption.to_owned()))
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
                        .map(|caption| (message.id, caption.to_owned()))
                });
                reject_payment_order(&bot, query.from.id, &state, order_id, source_message).await?;
            } else {
                bot.send_message(query.from.id, "شناسه‌ی سفارش معتبر نیست.")
                    .await?;
            }
        }
        data if data.starts_with("user:plan:") => {
            if !is_owner && !ensure_required_channel_member(&bot, target, query.from.id).await? {
                return Ok(());
            }
            let plan_gb = data
                .strip_prefix("user:plan:")
                .and_then(|value| value.parse::<u16>().ok());
            match plan_gb.filter(|gb| monthly_plan_price(*gb).is_some()) {
                Some(gb) => {
                    process_subscription_selection(&bot, target, &query.from, gb, None, &state)
                        .await?
                }
                None => {
                    edit_user_message_with_keyboard(
                        &bot,
                        target,
                        "تعرفه‌ی انتخاب‌شده معتبر نیست؛ دوباره از منو انتخاب کنید.",
                        monthly_plans_keyboard(),
                    )
                    .await?;
                }
            }
        }
        "user:join:check" => match is_required_channel_member(&bot, query.from.id).await {
            Ok(true) => {
                log::info!("User {} joined the required channel", query.from.id);
                let prompt = start_user_menu_prompt(&query.from.first_name);
                edit_user_menu(&bot, target, &prompt).await?;
            }
            Ok(false) => {
                edit_user_message_with_keyboard(
                    &bot,
                    target,
                    "هنوز عضویت شما تأیید نشده است. ابتدا عضو کانال شوید و دوباره بررسی کنید.",
                    join_channel_keyboard(),
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
                    join_channel_keyboard(),
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

fn panel_api_context(state: &AppState) -> Result<Option<(Url, Zeroizing<String>)>> {
    let Some((saved_url, _)) = state
        .panel_config()
        .context("Could not read panel configuration")?
    else {
        return Ok(None);
    };
    let base_url = panel::normalize_panel_url(&saved_url).map_err(anyhow::Error::new)?;
    let Some(access_token) = state.panel_access_token() else {
        return Ok(None);
    };
    Ok(Some((base_url, access_token)))
}

async fn send_panel_client_choices(
    bot: &Bot,
    target: Option<CallbackMessageTarget>,
    telegram_user: &User,
    state: &AppState,
    purpose: ClientListPurpose,
) -> Result<()> {
    let (base_url, access_token) = match panel_api_context(state) {
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
    let clients = match panel::panel_clients_for_telegram_user(
        &base_url,
        access_token.as_str(),
        telegram_user.id.0,
    )
    .await
    {
        Ok(clients) => clients,
        Err(error) => {
            log::warn!(
                "Could not list panel clients for user {}: {error}",
                telegram_user.id
            );
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
    let (base_url, access_token) = match panel_api_context(state) {
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
    let clients = match panel::panel_clients_for_telegram_user(
        &base_url,
        access_token.as_str(),
        telegram_user.id.0,
    )
    .await
    {
        Ok(clients) => clients,
        Err(error) => {
            log::warn!(
                "Could not verify renewal client for user {}: {error}",
                telegram_user.id
            );
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
    edit_user_message_with_keyboard(
        bot,
        target,
        &format!("بسته‌ی تمدید برای «{}» را انتخاب کنید:", client.username),
        monthly_renewal_plans_keyboard(&client.username),
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
    let (base_url, access_token) = match panel_api_context(state) {
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

    match panel::subscription_usage(
        &base_url,
        access_token.as_str(),
        telegram_user.id.0,
        username,
    )
    .await
    {
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
                "برای حساب شما اشتراکی در پنل پیدا نشد. اگر قبلاً پرداخت کرده‌اید، لطفاً با پشتیبانی تماس بگیرید.",
            subscription_back_keyboard(),
        )
            .await?;
        }
        Err(error) => {
            log::warn!(
                "Could not look up subscription usage for user {}: {error}",
                telegram_user.id
            );
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
    let (base_url, access_token) = match panel_api_context(state) {
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

    match panel::subscription_link(
        &base_url,
        access_token.as_str(),
        telegram_user.id.0,
        username,
    )
    .await
    {
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
                    "ارسال لینک اشتراک با خطا روبه‌رو شد؛ لطفاً دوباره تلاش کنید یا با پشتیبانی تماس بگیرید.",
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
    plan_gb: u16,
    renewal_username: Option<&str>,
    state: &AppState,
) -> Result<()> {
    let target = require_callback_message_target(target)?;
    let telegram_user_id = telegram_user.id;
    let Some(price) = monthly_plan_price(plan_gb) else {
        edit_user_message_with_keyboard(
            bot,
            target,
            "تعرفه‌ی انتخاب‌شده معتبر نیست؛ دوباره از منو انتخاب کنید.",
            subscription_back_keyboard(),
        )
        .await?;
        return Ok(());
    };

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
            edit_user_message_with_keyboard(
                bot,
                target,
                "خواندن اطلاعات پرداخت ممکن نشد؛ لطفاً بعداً دوباره تلاش کنید.",
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
    };
    let Some((card_number, cardholder_name)) = payment_details else {
        log::error!("Payment card configuration is missing");
        edit_user_message_with_keyboard(
            bot,
            target,
            "پرداخت در حال حاضر تنظیم نشده است. لطفاً بعداً دوباره تلاش کنید یا با پشتیبانی تماس بگیرید.",
            subscription_back_keyboard(),
        )
        .await?;
        return Ok(());
    };

    let (base_url, access_token) = match panel_api_context(state) {
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
    let clients = match panel::panel_clients_for_telegram_user(
        &base_url,
        access_token.as_str(),
        telegram_user_id.0,
    )
    .await
    {
        Ok(clients) => clients,
        Err(error) => {
            log::warn!(
                "Could not verify panel clients for user {}: {error}",
                telegram_user_id
            );
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
        let username = match panel::suggest_available_panel_username(
            &base_url,
            access_token.as_str(),
            telegram_user_id.0,
            &reserved,
        )
        .await
        {
            Ok(Some(username)) => username,
            Ok(None) => {
                edit_user_message_with_keyboard(
                    bot,
                    target,
                    "ظرفیت ساخت نام کاربری جدید برای حساب شما تکمیل شده است؛ لطفاً با پشتیبانی تماس بگیرید.",
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
    let order_id = match state.create_payment_order(NewPaymentOrder {
        telegram_user_id: telegram_user_id.0,
        account_name: &account_name,
        telegram_username: telegram_user.username.as_deref(),
        panel_username: &panel_username,
        plan_gb,
        price_toman: price,
        is_renewal,
    }) {
        Ok(order_id) => order_id,
        Err(error) => {
            log::error!(
                "Could not create payment order for Telegram user {}: {error}",
                telegram_user_id
            );
            edit_user_message_with_keyboard(
                bot,
                target,
                "ثبت سفارش ممکن نشد. لطفاً دوباره تلاش کنید یا با پشتیبانی تماس بگیرید.",
                subscription_back_keyboard(),
            )
            .await?;
            return Ok(());
        }
    };

    edit_text_message(
        bot,
        target,
        format!(
            "🧾 جزئیات سفارش\n📦 بسته: {} گیگابایت\n⏳ مدت: ۳۰ روز\n💰 مبلغ: {}\n\n💳 شماره کارت:\n(برای کپی، کافی است یک‌بار روی شماره کارت ضربه بزنید)\n\n<blockquote><code>{}</code></blockquote>\n👤 به نام: {}\n\nپس از پرداخت، عکس رسید تراکنش را همین‌جا ارسال کنید.⚠️\n فقط عکس رسید پذیرفته می‌شود.\nاگر به عکس رسید دسترسی ندارید، با آیدی پشتیبانی\n 🆔: @ipublicsupport\n در ارتباط باشید.",
            persian_digits(plan_gb),
            format_toman(price),
            html_escape(&card_number),
            html_escape(&cardholder_name)
        ),
        Some("HTML"),
        Some(payment_order_keyboard(order_id)),
    )
    .await?;

    Ok(())
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
    let Some(photo) = msg.photo().and_then(|photos| photos.last()) else {
        bot.send_message(
            msg.chat.id,
            "فقط عکس رسید تراکنش قابل ارسال است؛ لطفاً عکس رسید را همین‌جا بفرستید.\nاگر به عکس رسید دسترسی ندارید، با پشتیبانی @ipublicsupport در ارتباط باشید.",
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
                "خواندن سفارش پس از دریافت رسید انجام نشد. لطفاً عکس رسید را دوباره ارسال کنید یا با پشتیبانی @ipublicsupport تماس بگیرید.",
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
                "خواندن سفارش پس از دریافت رسید انجام نشد. لطفاً کمی بعد عکس را دوباره ارسال کنید یا با پشتیبانی @ipublicsupport تماس بگیرید.",
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
            "رسید به مالک یا ادمین‌ها تحویل نشد؛ لطفاً کمی بعد دوباره عکس رسید را ارسال کنید یا با پشتیبانی @ipublicsupport تماس بگیرید.",
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
    source_message: Option<(MessageId, String)>,
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
    let Some(access_token) = state.panel_access_token() else {
        let released = release_order_for_retry(state, order_id);
        bot.send_message(
            reviewer_id,
            if released {
                "نشست پنل فعال نیست؛ پس از اتصال پنل دوباره سفارش را تأیید کنید."
            } else {
                "نشست پنل فعال نیست و وضعیت سفارش به صف بررسی برنگشت؛ برای جلوگیری از فعال‌سازی تکراری با پشتیبانی فنی تماس بگیرید."
            },
        )
        .await?;
        return Ok(());
    };
    let (saved_url, _) = match state.panel_config() {
        Ok(Some(config)) => config,
        Ok(None) => {
            release_order_for_retry(state, order_id);
            bot.send_message(reviewer_id, "تنظیمات پنل موجود نیست؛ سفارش تأیید نشد.")
                .await?;
            return Ok(());
        }
        Err(error) => {
            log::error!("Could not read panel configuration for order {order_id}: {error}");
            release_order_for_retry(state, order_id);
            bot.send_message(
                reviewer_id,
                "خواندن تنظیمات پنل انجام نشد؛ لاگ برنامه را بررسی کنید.",
            )
            .await?;
            return Ok(());
        }
    };
    let base_url = match panel::normalize_panel_url(&saved_url) {
        Ok(url) => url,
        Err(error) => {
            log::error!("Saved panel URL is invalid during order approval: {error}");
            release_order_for_retry(state, order_id);
            bot.send_message(
                reviewer_id,
                "آدرس پنل معتبر نیست؛ مالک باید تنظیمات پنل را بررسی کند.",
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
            if let Err(error) = state.complete_payment_order(
                order_id,
                order.telegram_user_id,
                &subscription.username,
            ) {
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
                "✅ {action}\n👤 نام کاربری: {}\n📦 حجم: {} گیگ\n💰 مبلغ: {}\n📅 انقضا: {}\n\n🔗 لینک کانفیگ (برای کپی، یک‌بار روی لینک ضربه بزنید):\n\n<code>{}</code>",
                html_escape(&subscription.username),
                persian_digits(order.plan_gb),
                format_toman(order.price_toman),
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
    source_message: Option<(MessageId, String)>,
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
            if let Err(error) = bot
                .send_message(
                    UserId(telegram_user_id),
                    "❌ رسید پرداخت شما تأیید نشد و اشتراکی فعال نشد. برای پیگیری با پشتیبانی @ipublicsupport در ارتباط باشید.",
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
    status: PaymentReviewStatus,
) {
    if let Err(error) = bot
        .edit_message_caption(UserId(reviewer_id), message_id)
        .caption(final_payment_review_caption(caption, status))
        .reply_markup(InlineKeyboardMarkup::default())
        .await
    {
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
    source_message: Option<&(MessageId, String)>,
) {
    let messages = match state.payment_review_messages(order_id) {
        Ok(messages) => messages,
        Err(error) => {
            log::error!("Could not load review messages for order {order_id}: {error}");
            Vec::new()
        }
    };
    let source_message_id = source_message.map(|(message_id, _)| message_id.0);
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
            status,
        )
        .await;
    }

    if !source_is_tracked {
        if let Some((message_id, caption)) = source_message {
            edit_payment_review_message(bot, order_id, reviewer_id.0, *message_id, caption, status)
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
        InlineKeyboardButton::callback("❌ رد رسید", format!("payment:reject:{order_id}")),
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
    let receipt_status = if order.receipt_file_id.is_some() {
        "دریافت شد ✅"
    } else {
        "دریافت نشده"
    };

    format!(
        "🧾 سفارش #{} ({order_kind}) — ⏳ در انتظار بررسی\n👤 کاربر: {} ({})\n🔗 یوزرنیم: {}\n📧 ایمیل کانفیگ: {}\n📦 پلن: {} گیگ | ⏳ ۳۰ روزه | 💰 {}\n🧾 رسید: {}\n🕓 {}\nوضعیت: ⏳ منتظر تأیید",
        persian_digits(order.id),
        order.account_name,
        persian_digits(order.telegram_user_id),
        telegram_username,
        order.panel_username,
        persian_digits(order.plan_gb),
        format_toman(order.price_toman),
        receipt_status,
        persian_datetime(chrono::Utc::now().with_timezone(&chrono_tz::Asia::Tehran))
    )
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
    target: Option<CallbackMessageTarget>,
    user_id: UserId,
) -> Result<bool> {
    match is_required_channel_member(bot, user_id).await {
        Ok(true) => Ok(true),
        Ok(false) => {
            edit_user_message_with_keyboard(
                bot,
                target,
                "برای استفاده از ربات، ابتدا در کانال ما عضو شوید و سپس «عضو شدم» را بزنید.",
                join_channel_keyboard(),
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
                join_channel_keyboard(),
            )
            .await?;
            Ok(false)
        }
    }
}

async fn is_required_channel_member(bot: &Bot, user_id: UserId) -> ResponseResult<bool> {
    let member = bot
        .get_chat_member(REQUIRED_CHANNEL.to_owned(), user_id)
        .await?;
    Ok(member.kind.is_present())
}

async fn send_join_prompt(bot: &Bot, chat_id: ChatId) -> ResponseResult<()> {
    send_user_message_with_keyboard(
        bot,
        chat_id,
        "برای استفاده از ربات، ابتدا در کانال ما عضو شوید و سپس «عضو شدم» را بزنید.",
        join_channel_keyboard(),
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
        vec![
            InlineKeyboardButton::callback("📢 پیام همگانی", "owner:broadcast:all"),
            InlineKeyboardButton::callback("💳 پیام به مشترکین", "owner:broadcast:subscribers"),
        ],
        vec![InlineKeyboardButton::callback(
            "🛒 وضعیت خرید و تمدید",
            "owner:sales:manage",
        )],
    ];
    InlineKeyboardMarkup::new(rows)
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

fn support_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::url(
            "💬 ارسال پیام به پشتیبانی",
            Url::parse("https://t.me/ipublicsupport").expect("support URL is valid"),
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

fn monthly_plans_keyboard() -> InlineKeyboardMarkup {
    monthly_plan_keyboard(|gb| format!("user:plan:{gb}"), "user:buy", true)
}

fn monthly_renewal_plans_keyboard(username: &str) -> InlineKeyboardMarkup {
    monthly_plan_keyboard(
        |gb| format!("user:renew:plan:{gb}:{username}"),
        "user:renew",
        true,
    )
}

fn monthly_plan_keyboard(
    callback_for_plan: impl Fn(u16) -> String,
    back_callback: &str,
    combine_quota_and_price: bool,
) -> InlineKeyboardMarkup {
    let mut rows = if combine_quota_and_price {
        MONTHLY_PLAN_GB
            .chunks(2)
            .map(|plans| {
                plans
                    .iter()
                    .map(|gb| {
                        let price =
                            monthly_plan_price(*gb).expect("configured monthly plan has a price");
                        InlineKeyboardButton::callback(
                            format!("📦 {} گیگ | 💰 {}", persian_digits(gb), format_toman(price)),
                            callback_for_plan(*gb),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    } else {
        MONTHLY_PLAN_GB
            .iter()
            .map(|gb| {
                let price = monthly_plan_price(*gb).expect("configured monthly plan has a price");
                let callback = callback_for_plan(*gb);
                vec![
                    InlineKeyboardButton::callback(
                        format!("💰 {}", format_toman(price)),
                        callback.clone(),
                    ),
                    InlineKeyboardButton::callback(
                        format!("📦 {} گیگ", persian_digits(gb)),
                        callback,
                    ),
                ]
            })
            .collect::<Vec<_>>()
    };
    rows.push(vec![InlineKeyboardButton::callback(
        "🔙 بازگشت",
        back_callback,
    )]);
    InlineKeyboardMarkup::new(rows)
}

fn monthly_plan_price(gb: u16) -> Option<u32> {
    match gb {
        10 => Some(60_000),
        20 => Some(115_000),
        30 => Some(165_000),
        50 => Some(255_000),
        80 => Some(370_000),
        100 => Some(450_000),
        150 => Some(675_000),
        200 => Some(850_000),
        250 => Some(1_025_000),
        300 => Some(1_200_000),
        _ => None,
    }
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

fn join_channel_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::url(
            "📢 عضویت در کانال",
            Url::parse(REQUIRED_CHANNEL_URL).expect("required channel URL is valid"),
        )],
        vec![InlineKeyboardButton::callback(
            "✅ عضو شدم",
            "user:join:check",
        )],
    ])
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
    fn monthly_plan_prices_follow_the_requested_tiers() {
        assert_eq!(
            MONTHLY_PLAN_GB,
            [10, 20, 30, 50, 80, 100, 150, 200, 250, 300]
        );
        let prices = MONTHLY_PLAN_GB
            .iter()
            .map(|gb| monthly_plan_price(*gb).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            prices,
            vec![
                60_000, 115_000, 165_000, 255_000, 370_000, 450_000, 675_000, 850_000, 1_025_000,
                1_200_000
            ]
        );
        assert_eq!(monthly_plan_price(101), None);
    }

    #[test]
    fn formats_prices_with_persian_digits_and_separators() {
        assert_eq!(format_toman(600_000), "۶۰۰٬۰۰۰ تومان");
    }

    #[test]
    fn monthly_tariff_buttons_use_two_columns_and_have_emojis() {
        let rows = monthly_plans_keyboard().inline_keyboard;

        assert_eq!(rows.len(), MONTHLY_PLAN_GB.chunks(2).len() + 1);
        assert!(rows[..rows.len() - 1].iter().all(|row| {
            row.len() == 2 && row.iter().all(|button| button.text.starts_with("📦"))
        }));
        assert!(rows[..rows.len() - 1]
            .iter()
            .flatten()
            .zip(MONTHLY_PLAN_GB.iter())
            .all(|(button, gb)| {
                button.text.contains(&format!("{} گیگ", persian_digits(gb)))
                    && button
                        .text
                        .contains(&format_toman(monthly_plan_price(*gb).unwrap()))
            }));
        assert!(rows.last().unwrap()[0].text.starts_with("🔙"));
    }

    #[test]
    fn renewal_plan_buttons_match_purchase_plan_format() {
        let rows = monthly_renewal_plans_keyboard("haku-123456").inline_keyboard;

        assert_eq!(rows.len(), MONTHLY_PLAN_GB.chunks(2).len() + 1);
        assert!(rows[..rows.len() - 1].iter().all(|row| {
            row.len() == 2 && row.iter().all(|button| button.text.starts_with("📦"))
        }));
        assert!(rows[..rows.len() - 1]
            .iter()
            .flatten()
            .zip(MONTHLY_PLAN_GB.iter())
            .all(|(button, gb)| {
                button.text.contains(&format!("{} گیگ", persian_digits(gb)))
                    && button
                        .text
                        .contains(&format_toman(monthly_plan_price(*gb).unwrap()))
            }));
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
            &support_keyboard().inline_keyboard[0][0].kind,
            InlineKeyboardButtonKind::Url(url) if url.as_str() == "https://t.me/ipublicsupport"
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
            &support_keyboard(),
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
            &monthly_renewal_plans_keyboard("haku-123456").inline_keyboard[0][0].kind,
            InlineKeyboardButtonKind::CallbackData(data)
                if data == "user:renew:plan:10:haku-123456"
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
            InlineKeyboardButtonKind::CallbackData(data) if data == "owner:sales:manage"
        )));
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
