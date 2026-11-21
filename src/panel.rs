use chrono::{DateTime, Duration, SecondsFormat, Utc};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;
use url::{Host, Url};
use zeroize::Zeroizing;

#[derive(Debug, Error)]
pub enum PanelError {
    #[error("آدرس پنل معتبر نیست.")]
    InvalidUrl,
    #[error("برای امنیت ورود، آدرس پنل باید HTTPS باشد (HTTP فقط برای localhost مجاز است).")]
    InsecureUrl,
    #[error("ارتباط با پنل برقرار نشد.")]
    Request,
    #[error("نام کاربری یا گذرواژه‌ی پنل نادرست است.")]
    Unauthorized,
    #[error("پنل درخواست را نپذیرفت (HTTP {0}).")]
    HttpStatus(u16),
    #[error("پاسخ ورود پنل معتبر نیست.")]
    InvalidResponse,
    #[error("گروه seller در پنل پیدا نشد.")]
    SellerGroupNotFound,
    #[error("بیش از یک گروه seller در پنل پیدا شد.")]
    SellerGroupAmbiguous,
    #[error("یوزر پنل با شناسه تلگرام ذخیره‌شده مطابقت ندارد.")]
    UserIdentityMismatch,
    #[error("یوزر انتخاب‌شده در پنل پیدا نشد.")]
    UserNotFound,
    #[error("پنل لینک سابسکریپشن این یوزر را برنگرداند.")]
    SubscriptionUrlUnavailable,
    #[error("یوزر پنل اطلاعات انقضا یا حجم قابل تمدید ندارد.")]
    InvalidExistingSubscription,
    #[error("یوزر پنل اطلاعات حجم یا تاریخ انقضای قابل‌خواندن ندارد.")]
    InvalidSubscriptionUsage,
    #[error("نام کاربری پیشنهادی در پنل موجود است؛ سفارش جدیدی با نام کاربری تازه ثبت کنید.")]
    UsernameCollision,
}

impl PanelError {
    pub fn user_message(&self) -> String {
        match self {
            Self::Request => "ارتباط با پنل برقرار نشد. آدرس و دسترسی شبکه را بررسی کنید.".into(),
            Self::Unauthorized | Self::HttpStatus(401) => {
                "نشست پنل منقضی شده است. مالک باید دوباره به پنل وارد شود.".into()
            }
            Self::HttpStatus(403) => "دسترسی حساب پنل کافی نیست؛ مجوز خواندن گروه‌ها و کاربران، ساخت/ویرایش کاربر و بازنشانی مصرف را بررسی کنید.".into(),
            Self::SellerGroupNotFound => {
                "گروه seller در پنل پیدا نشد؛ نام گروه باید دقیقاً seller باشد.".into()
            }
            Self::SellerGroupAmbiguous => {
                "چند گروه با نام seller در پنل وجود دارد؛ فقط یک گروه seller نگه دارید.".into()
            }
            Self::UserIdentityMismatch => {
                "نگاشت کاربر تلگرام و کاربر پنل با هم مطابقت ندارد؛ برای جلوگیری از تغییر حساب اشتباه، عملیات متوقف شد."
                    .into()
            }
            Self::UserNotFound => {
                "یوزر انتخاب‌شده در پنل پیدا نشد؛ فهرست اشتراک‌ها را دوباره بررسی کنید.".into()
            }
            Self::SubscriptionUrlUnavailable => {
                "پنل لینک سابسکریپشن این یوزر را برنگرداند؛ تنظیمات لینک سابسکریپشن پنل را بررسی کنید."
                    .into()
            }
            Self::InvalidExistingSubscription => {
                "اطلاعات حجم یا تاریخ انقضای یوزر پنل قابل تمدید نیست؛ لطفاً با پشتیبانی تماس بگیرید."
                    .into()
            }
            Self::InvalidSubscriptionUsage => {
                "اطلاعات حجم یا زمان اشتراک قابل خواندن نیست؛ لطفاً با پشتیبانی تماس بگیرید."
                    .into()
            }
            Self::UsernameCollision => {
                "ساخت نام کاربری یکتا در پنل انجام نشد. لطفاً دوباره تلاش کنید.".into()
            }
            _ => self.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct GroupsResponse {
    groups: Vec<GroupResponse>,
}

#[derive(Deserialize)]
struct GroupResponse {
    id: i64,
    name: String,
}

#[derive(Deserialize)]
struct UsersResponse {
    users: Vec<PanelUser>,
}

#[derive(Deserialize)]
struct PanelUser {
    username: String,
    note: Option<String>,
    #[serde(default)]
    status: Option<String>,
    expire: Option<PanelExpiration>,
    data_limit: Option<u64>,
    #[serde(default)]
    used_traffic: u64,
    subscription_url: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PanelExpiration {
    DateTime(String),
    Timestamp(i64),
}

#[derive(Serialize)]
struct CreateUserRequest<'a> {
    username: &'a str,
    status: &'static str,
    data_limit: u64,
    expire: String,
    data_limit_reset_strategy: &'static str,
    note: String,
    group_ids: [i64; 1],
}

#[derive(Serialize)]
struct ModifyUserRequest {
    status: &'static str,
    data_limit: u64,
    expire: String,
    data_limit_reset_strategy: &'static str,
    note: String,
    group_ids: [i64; 1],
}

pub struct ProvisionedSubscription {
    pub username: String,
    pub subscription_url: String,
    pub expires_at: DateTime<Utc>,
    pub renewed: bool,
}

pub struct SubscriptionUsage {
    pub username: String,
    pub data_limit_bytes: Option<u64>,
    pub used_traffic_bytes: u64,
    pub remaining_bytes: Option<u64>,
    pub remaining_days: Option<u64>,
    pub expired: bool,
}

pub struct PanelClient {
    pub username: String,
    pub is_active: bool,
}

struct RenewalTerms {
    data_limit: u64,
    expires_at: DateTime<Utc>,
    expired: bool,
}

pub fn normalize_panel_url(input: &str) -> Result<Url, PanelError> {
    let mut url = Url::parse(input.trim()).map_err(|_| PanelError::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(PanelError::InvalidUrl);
    }

    let host = url.host().ok_or(PanelError::InvalidUrl)?;
    if url.scheme() != "https" && !is_loopback_host(host) {
        return Err(PanelError::InsecureUrl);
    }

    let panel_path = url.path().trim_end_matches('/').to_owned();
    if let Some(prefix) = panel_path.strip_suffix("/dashboard") {
        url.set_path(if prefix.is_empty() { "/" } else { prefix });
    }

    let current_path = url.path().to_owned();
    if !current_path.ends_with('/') {
        let path = format!("{current_path}/");
        url.set_path(&path);
    }
    Ok(url)
}

fn is_loopback_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => domain == "localhost",
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    }
}

pub async fn authenticate(
    base_url: &Url,
    username: &str,
    password: &str,
) -> Result<Zeroizing<String>, PanelError> {
    let endpoint = base_url
        .join("/api/admin/token")
        .map_err(|_| PanelError::InvalidUrl)?;
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| PanelError::Request)?;

    let response = client
        .post(endpoint)
        .form(&[
            ("username", username),
            ("password", password),
            ("grant_type", "password"),
        ])
        .send()
        .await
        .map_err(|_| PanelError::Request)?;

    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(PanelError::Unauthorized);
    }
    if !response.status().is_success() {
        return Err(PanelError::HttpStatus(response.status().as_u16()));
    }

    let token = response
        .json::<TokenResponse>()
        .await
        .map_err(|_| PanelError::InvalidResponse)?
        .access_token;
    if token.trim().is_empty() {
        return Err(PanelError::InvalidResponse);
    }
    Ok(Zeroizing::new(token))
}

pub async fn provision_subscription(
    base_url: &Url,
    access_token: &str,
    telegram_user_id: u64,
    mapped_username: Option<&str>,
    preferred_username: &str,
    plan_gb: u16,
) -> Result<ProvisionedSubscription, PanelError> {
    let data_limit_bytes = u64::from(plan_gb)
        .checked_mul(1024 * 1024 * 1024)
        .ok_or(PanelError::InvalidResponse)?;
    let client = api_client()?;
    let note = telegram_user_id.to_string();
    let seller_group_id = get_seller_group_id(&client, base_url, access_token).await?;

    let mapped_user = match mapped_username {
        Some(username) => {
            let user =
                get_user_for_telegram_user(&client, base_url, access_token, username, &note, false)
                    .await?
                    .ok_or(PanelError::UserNotFound)?;
            Some(user)
        }
        None => None,
    };

    if let Some(user) = mapped_user {
        renew_user(
            &client,
            base_url,
            access_token,
            user,
            &note,
            seller_group_id,
            data_limit_bytes,
        )
        .await
    } else {
        create_user(
            &client,
            base_url,
            access_token,
            &note,
            seller_group_id,
            preferred_username,
            data_limit_bytes,
        )
        .await
    }
}

pub async fn panel_clients_for_telegram_user(
    base_url: &Url,
    access_token: &str,
    telegram_user_id: u64,
) -> Result<Vec<PanelClient>, PanelError> {
    let client = api_client()?;
    let note = telegram_user_id.to_string();
    let users = find_users_by_note(&client, base_url, access_token, &note, false).await?;
    let now = Utc::now();
    let mut clients = users
        .into_iter()
        .map(|user| panel_client(user, now))
        .collect::<Result<Vec<_>, PanelError>>()?;
    clients.sort_by(|left, right| left.username.cmp(&right.username));
    Ok(clients)
}

pub async fn suggest_available_panel_username(
    base_url: &Url,
    access_token: &str,
    telegram_user_id: u64,
    reserved_usernames: &[String],
) -> Result<Option<String>, PanelError> {
    let client = api_client()?;
    let mut unavailable = reserved_usernames.to_vec();
    for _ in 0..1000 {
        let Some(username) = suggest_unused_username(telegram_user_id, &unavailable) else {
            return Ok(None);
        };
        if get_user(&client, base_url, access_token, &username)
            .await?
            .is_none()
        {
            return Ok(Some(username));
        }
        unavailable.push(username);
    }
    Ok(None)
}

pub async fn subscription_usage(
    base_url: &Url,
    access_token: &str,
    telegram_user_id: u64,
    username: &str,
) -> Result<Option<SubscriptionUsage>, PanelError> {
    let client = api_client()?;
    let note = telegram_user_id.to_string();
    let Some(user) =
        get_user_for_telegram_user(&client, base_url, access_token, username, &note, false).await?
    else {
        return Ok(None);
    };

    let (remaining_bytes, remaining_days, expired) = calculate_usage(&user, Utc::now())?;

    Ok(Some(SubscriptionUsage {
        username: user.username,
        data_limit_bytes: user.data_limit,
        used_traffic_bytes: user.used_traffic,
        remaining_bytes,
        remaining_days,
        expired,
    }))
}

pub async fn subscription_link(
    base_url: &Url,
    access_token: &str,
    telegram_user_id: u64,
    username: &str,
) -> Result<Option<String>, PanelError> {
    let client = api_client()?;
    let note = telegram_user_id.to_string();
    let Some(user) =
        get_user_for_telegram_user(&client, base_url, access_token, username, &note, true).await?
    else {
        return Ok(None);
    };
    let subscription_url = user.subscription_url.trim();
    if subscription_url.is_empty() {
        return Err(PanelError::SubscriptionUrlUnavailable);
    }
    Ok(Some(subscription_url.to_owned()))
}

fn calculate_usage(
    user: &PanelUser,
    now: DateTime<Utc>,
) -> Result<(Option<u64>, Option<u64>, bool), PanelError> {
    let (remaining_days, expired) = match user.expire.as_ref() {
        Some(expiration) => {
            let expiration =
                parse_expiration(expiration).ok_or(PanelError::InvalidSubscriptionUsage)?;
            let seconds = (expiration - now).num_seconds();
            if seconds <= 0 {
                (Some(0), true)
            } else {
                (Some(((seconds - 1) / 86_400 + 1) as u64), false)
            }
        }
        None => (None, false),
    };
    let remaining_bytes = user
        .data_limit
        .map(|limit| limit.saturating_sub(user.used_traffic));
    Ok((remaining_bytes, remaining_days, expired))
}

fn api_client() -> Result<Client, PanelError> {
    Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| PanelError::Request)
}

fn api_url(base_url: &Url, path: &str) -> Result<Url, PanelError> {
    base_url.join(path).map_err(|_| PanelError::InvalidUrl)
}

fn user_url(base_url: &Url, username: &str) -> Result<Url, PanelError> {
    let mut url = api_url(base_url, "/api/user")?;
    url.path_segments_mut()
        .map_err(|_| PanelError::InvalidUrl)?
        .push(username);
    Ok(url)
}

fn user_action_url(base_url: &Url, username: &str, action: &str) -> Result<Url, PanelError> {
    let mut url = api_url(base_url, "/api/user")?;
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| PanelError::InvalidUrl)?;
    segments.push(username).push(action);
    drop(segments);
    Ok(url)
}

fn ensure_success(response: Response) -> Result<Response, PanelError> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(PanelError::HttpStatus(response.status().as_u16()))
    }
}

async fn get_seller_group_id(
    client: &Client,
    base_url: &Url,
    access_token: &str,
) -> Result<i64, PanelError> {
    let response = client
        .get(api_url(base_url, "/api/groups/simple")?)
        .query(&[("all", "true")])
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| PanelError::Request)?;
    let groups = ensure_success(response)?
        .json::<GroupsResponse>()
        .await
        .map_err(|_| PanelError::InvalidResponse)?
        .groups;

    let mut seller_groups = groups
        .into_iter()
        .filter(|group| group.name.eq_ignore_ascii_case("seller"));
    let group_id = seller_groups
        .next()
        .map(|group| group.id)
        .ok_or(PanelError::SellerGroupNotFound)?;
    if seller_groups.next().is_some() {
        return Err(PanelError::SellerGroupAmbiguous);
    }
    Ok(group_id)
}

async fn get_user(
    client: &Client,
    base_url: &Url,
    access_token: &str,
    username: &str,
) -> Result<Option<PanelUser>, PanelError> {
    let response = client
        .get(user_url(base_url, username)?)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| PanelError::Request)?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let user = ensure_success(response)?
        .json::<PanelUser>()
        .await
        .map_err(|_| PanelError::InvalidResponse)?;
    Ok(Some(user))
}

async fn get_user_for_telegram_user(
    client: &Client,
    base_url: &Url,
    access_token: &str,
    username: &str,
    note: &str,
    include_subscription_url: bool,
) -> Result<Option<PanelUser>, PanelError> {
    if let Some(user) = get_user(client, base_url, access_token, username).await? {
        if user.note.as_deref() != Some(note) {
            return Err(PanelError::UserIdentityMismatch);
        }
        if !include_subscription_url || !user.subscription_url.trim().is_empty() {
            return Ok(Some(user));
        }
    }

    let users = find_users_by_note(
        client,
        base_url,
        access_token,
        note,
        include_subscription_url,
    )
    .await?;
    Ok(find_user_by_note_and_username(users, note, username))
}

fn find_user_by_note_and_username(
    users: Vec<PanelUser>,
    note: &str,
    username: &str,
) -> Option<PanelUser> {
    users
        .into_iter()
        .find(|user| user.username == username && user.note.as_deref() == Some(note))
}

async fn find_users_by_note(
    client: &Client,
    base_url: &Url,
    access_token: &str,
    note: &str,
    include_subscription_url: bool,
) -> Result<Vec<PanelUser>, PanelError> {
    const PAGE_SIZE: usize = 100;
    let endpoint = api_url(base_url, "/api/users")?;
    let mut offset = 0usize;
    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    for _ in 0..100 {
        let offset_query = offset.to_string();
        let load_sub_query = if include_subscription_url {
            "true"
        } else {
            "false"
        };
        let response = client
            .get(endpoint.clone())
            .query(&[
                ("search", note),
                ("limit", "100"),
                ("offset", offset_query.as_str()),
                ("load_sub", load_sub_query),
            ])
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|_| PanelError::Request)?;
        let users = ensure_success(response)?
            .json::<UsersResponse>()
            .await
            .map_err(|_| PanelError::InvalidResponse)?
            .users;
        let page_len = users.len();
        let mut added = 0;
        for user in users {
            if user.note.as_deref() == Some(note) && seen.insert(user.username.clone()) {
                matches.push(user);
                added += 1;
            }
        }
        if page_len < PAGE_SIZE || added == 0 {
            break;
        }
        offset += page_len;
    }
    Ok(matches)
}

fn panel_client(user: PanelUser, now: DateTime<Utc>) -> Result<PanelClient, PanelError> {
    let expiry_active = match user.expire.as_ref() {
        Some(expiration) => {
            parse_expiration(expiration).ok_or(PanelError::InvalidSubscriptionUsage)? > now
        }
        None => true,
    };
    let status_active = user
        .status
        .as_deref()
        .map(|status| status.eq_ignore_ascii_case("active"))
        .unwrap_or(true);
    Ok(PanelClient {
        username: user.username,
        is_active: expiry_active && status_active,
    })
}

async fn create_user(
    client: &Client,
    base_url: &Url,
    access_token: &str,
    note: &str,
    seller_group_id: i64,
    username: &str,
    data_limit_bytes: u64,
) -> Result<ProvisionedSubscription, PanelError> {
    let expires_at = Utc::now() + Duration::days(30);
    let expires = format_panel_expiration(expires_at);

    let body = CreateUserRequest {
        username,
        status: "active",
        data_limit: data_limit_bytes,
        expire: expires.clone(),
        data_limit_reset_strategy: "no_reset",
        note: note.to_owned(),
        group_ids: [seller_group_id],
    };
    let response = client
        .post(api_url(base_url, "/api/user")?)
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .await
        .map_err(|_| PanelError::Request)?;
    if response.status() == StatusCode::CONFLICT {
        return Err(PanelError::UsernameCollision);
    }
    let user = ensure_success(response)?
        .json::<PanelUser>()
        .await
        .map_err(|_| PanelError::InvalidResponse)?;
    Ok(ProvisionedSubscription {
        username: user.username,
        subscription_url: user.subscription_url,
        expires_at,
        renewed: false,
    })
}

async fn renew_user(
    client: &Client,
    base_url: &Url,
    access_token: &str,
    user: PanelUser,
    note: &str,
    seller_group_id: i64,
    package_bytes: u64,
) -> Result<ProvisionedSubscription, PanelError> {
    let terms = renewal_terms(&user, package_bytes, Utc::now())?;
    if terms.expired {
        let reset_response = client
            .post(user_action_url(base_url, &user.username, "reset")?)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|_| PanelError::Request)?;
        ensure_success(reset_response)?;
    };
    let expires = format_panel_expiration(terms.expires_at);
    let body = ModifyUserRequest {
        status: "active",
        data_limit: terms.data_limit,
        expire: expires.clone(),
        data_limit_reset_strategy: "no_reset",
        note: note.to_owned(),
        group_ids: [seller_group_id],
    };
    let response = client
        .put(user_url(base_url, &user.username)?)
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .await
        .map_err(|_| PanelError::Request)?;
    let user = ensure_success(response)?
        .json::<PanelUser>()
        .await
        .map_err(|_| PanelError::InvalidResponse)?;

    Ok(ProvisionedSubscription {
        username: user.username,
        subscription_url: user.subscription_url,
        expires_at: terms.expires_at,
        renewed: true,
    })
}

fn format_panel_expiration(value: DateTime<Utc>) -> String {
    value
        .with_timezone(&chrono_tz::Asia::Tehran)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn renewal_terms(
    user: &PanelUser,
    package_bytes: u64,
    now: DateTime<Utc>,
) -> Result<RenewalTerms, PanelError> {
    let current_expiration = user
        .expire
        .as_ref()
        .and_then(parse_expiration)
        .ok_or(PanelError::InvalidExistingSubscription)?;
    let expired = current_expiration <= now;
    let (data_limit, expires_at) = if expired {
        (package_bytes, now + Duration::days(30))
    } else {
        let current_limit = user
            .data_limit
            .filter(|limit| *limit > 0)
            .ok_or(PanelError::InvalidExistingSubscription)?;
        let data_limit = current_limit
            .max(user.used_traffic)
            .checked_add(package_bytes)
            .ok_or(PanelError::InvalidExistingSubscription)?;
        (data_limit, current_expiration + Duration::days(30))
    };

    Ok(RenewalTerms {
        data_limit,
        expires_at,
        expired,
    })
}

fn suggest_unused_username(telegram_user_id: u64, used_usernames: &[String]) -> Option<String> {
    let prefix = telegram_user_id
        .to_string()
        .chars()
        .take(3)
        .collect::<String>();
    let start = rand::random_range(0..1000);
    (0..1000).find_map(|offset| {
        let suffix = (start + offset) % 1000;
        let username = format!("haku-{prefix}{suffix:03}");
        (!used_usernames.contains(&username)).then_some(username)
    })
}

fn parse_expiration(value: &PanelExpiration) -> Option<DateTime<Utc>> {
    match value {
        PanelExpiration::DateTime(value) => DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|datetime| datetime.with_timezone(&Utc)),
        PanelExpiration::Timestamp(value) => DateTime::from_timestamp(*value, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_panel_root() {
        let url = normalize_panel_url("https://panel.example.com").unwrap();
        assert_eq!(url.as_str(), "https://panel.example.com/");
    }

    #[test]
    fn maps_dashboard_url_to_api_root() {
        let url = normalize_panel_url("https://panel.example.com/dashboard/").unwrap();
        assert_eq!(
            url.join("/api/admin/token").unwrap().as_str(),
            "https://panel.example.com/api/admin/token"
        );
    }

    #[test]
    fn maps_custom_dashboard_path_to_origin_api_root() {
        let url = normalize_panel_url("https://panel.example.com:8443/p4r34mAB/").unwrap();
        assert_eq!(
            url.join("/api/admin/token").unwrap().as_str(),
            "https://panel.example.com:8443/api/admin/token"
        );
    }

    #[test]
    fn renewal_api_urls_support_username_routes() {
        let base_url = normalize_panel_url("https://panel.example.com").unwrap();

        assert_eq!(
            user_url(&base_url, "haku-123456").unwrap().path(),
            "/api/user/haku-123456"
        );
        assert_eq!(
            user_action_url(&base_url, "haku-123456", "reset")
                .unwrap()
                .path(),
            "/api/user/haku-123456/reset"
        );
    }

    #[test]
    fn permits_http_only_for_loopback() {
        assert!(normalize_panel_url("http://localhost:8000").is_ok());
        assert!(normalize_panel_url("http://127.0.0.1:8000").is_ok());
        assert!(matches!(
            normalize_panel_url("http://panel.example.com"),
            Err(PanelError::InsecureUrl)
        ));
    }

    #[test]
    fn rejects_credentials_and_query_in_panel_url() {
        assert!(normalize_panel_url("https://user:pass@panel.example.com").is_err());
        assert!(normalize_panel_url("https://panel.example.com/?token=secret").is_err());
    }

    #[test]
    fn renewal_adds_quota_and_time_when_subscription_is_still_valid() {
        let now = Utc::now();
        let current_expiration = now + Duration::days(5);
        let user = test_panel_user(current_expiration, 1_000, 400);

        let terms = renewal_terms(&user, 200, now).unwrap();

        assert!(!terms.expired);
        assert_eq!(terms.data_limit, 1_200);
        assert_eq!(terms.expires_at, current_expiration + Duration::days(30));
    }

    #[test]
    fn renewal_starts_fresh_when_subscription_has_expired() {
        let now = Utc::now();
        let user = test_panel_user(now - Duration::days(1), 1_000, 800);

        let terms = renewal_terms(&user, 200, now).unwrap();

        assert!(terms.expired);
        assert_eq!(terms.data_limit, 200);
        assert_eq!(terms.expires_at, now + Duration::days(30));
    }

    #[test]
    fn subscription_usage_calculates_remaining_quota_and_ceil_days() {
        let now = Utc::now();
        let user = PanelUser {
            username: "haku-123456".into(),
            note: Some("123456789".into()),
            status: Some("active".into()),
            expire: Some(PanelExpiration::DateTime(
                (now + Duration::hours(25)).to_rfc3339(),
            )),
            data_limit: Some(1_000),
            used_traffic: 400,
            subscription_url: "https://example.com/sub".into(),
        };
        let (remaining_bytes, remaining_days, expired) = calculate_usage(&user, now).unwrap();
        assert_eq!(remaining_bytes, Some(600));
        assert_eq!(remaining_days, Some(2));
        assert!(!expired);
    }

    #[test]
    fn subscription_usage_reports_expiry_and_never_exceeds_zero_quota() {
        let now = Utc::now();
        let user = test_panel_user(now - Duration::days(1), 100, 150);

        let (remaining_bytes, remaining_days, expired) = calculate_usage(&user, now).unwrap();

        assert_eq!(remaining_bytes, Some(0));
        assert_eq!(remaining_days, Some(0));
        assert!(expired);
    }

    #[test]
    fn subscription_usage_supports_unlimited_quota_and_expiry() {
        let user = PanelUser {
            username: "haku-123456".into(),
            note: Some("123456789".into()),
            status: Some("active".into()),
            expire: None,
            data_limit: None,
            used_traffic: 400,
            subscription_url: "https://example.com/sub".into(),
        };

        let (remaining_bytes, remaining_days, expired) =
            calculate_usage(&user, Utc::now()).unwrap();

        assert_eq!(remaining_bytes, None);
        assert_eq!(remaining_days, None);
        assert!(!expired);
    }

    #[test]
    fn panel_expiration_uses_iran_offset_without_changing_the_instant() {
        let utc = DateTime::parse_from_rfc3339("2026-11-20T18:25:05Z")
            .unwrap()
            .with_timezone(&Utc);
        let formatted = format_panel_expiration(utc);

        assert!(formatted.ends_with("+03:30"));
        assert_eq!(
            DateTime::parse_from_rfc3339(&formatted)
                .unwrap()
                .with_timezone(&Utc),
            utc
        );
    }

    #[test]
    fn generated_username_contains_id_prefix_and_six_digits() {
        let username = suggest_unused_username(123_456_789, &[]).unwrap();
        let suffix = username.strip_prefix("haku-123").unwrap();

        assert_eq!(suffix.len(), 3);
        assert!(suffix.chars().all(|digit| digit.is_ascii_digit()));
    }

    #[test]
    fn new_username_avoids_existing_client_names() {
        let used = vec!["haku-123000".to_owned(), "haku-123001".to_owned()];

        let username = suggest_unused_username(123_456_789, &used).unwrap();

        assert!(username.starts_with("haku-123"));
        assert!(!used.contains(&username));
    }

    #[test]
    fn client_status_reflects_panel_status_and_expiration() {
        let now = Utc::now();
        let active = panel_client(test_panel_user(now + Duration::days(1), 100, 0), now).unwrap();
        let expired =
            panel_client(test_panel_user(now - Duration::seconds(1), 100, 0), now).unwrap();
        let mut disabled_user = test_panel_user(now + Duration::days(1), 100, 0);
        disabled_user.status = Some("disabled".into());
        let disabled = panel_client(disabled_user, now).unwrap();

        assert!(active.is_active);
        assert!(!expired.is_active);
        assert!(!disabled.is_active);
    }

    #[test]
    fn selected_user_fallback_matches_username_and_telegram_note() {
        let now = Utc::now();
        let mut selected = test_panel_user(now + Duration::days(1), 100, 0);
        selected.username = "haku-123789".into();
        let mut another_user = test_panel_user(now + Duration::days(1), 100, 0);
        another_user.note = Some("987654321".into());
        let users = vec![another_user, selected];

        let user = find_user_by_note_and_username(users, "123456789", "haku-123789").unwrap();

        assert_eq!(user.username, "haku-123789");
        assert_eq!(user.note.as_deref(), Some("123456789"));
    }

    fn test_panel_user(expire: DateTime<Utc>, data_limit: u64, used_traffic: u64) -> PanelUser {
        PanelUser {
            username: "haku-123456".into(),
            note: Some("123456789".into()),
            status: Some("active".into()),
            expire: Some(PanelExpiration::DateTime(expire.to_rfc3339())),
            data_limit: Some(data_limit),
            used_traffic,
            subscription_url: "https://example.com/sub".into(),
        }
    }
}
