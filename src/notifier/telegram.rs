//! The Telegram [`NotificationChannel`]: one bot, one chat, `sendMessage`
//! for delivery and `getMe` to prove the credentials work before the bot
//! starts trusting them.
//!
//! The bot token sits in the request path (`/bot{token}/...`), not in a
//! header or the body, so it is the one secret this module has to keep out
//! of everything it hands back to a caller. [`reqwest::Error::without_url`]
//! is the gate every transport error passes through before it becomes
//! [`NotifyError`] text, and a refusal's error text is built only from the
//! `description` field Telegram's own JSON answers with — never the raw
//! response body, which echoes the request URL (token included) on some of
//! Telegram's own error pages.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Secret;
use crate::notifier::{Notification, NotificationChannel, NotifyError, Severity};

/// Telegram's own Bot API host.
pub const TELEGRAM_API: &str = "https://api.telegram.org";

/// How long [`TelegramChannel::send`](NotificationChannel::send) and
/// [`TelegramChannel::verify`] wait for Telegram to answer.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest text a `sendMessage` call accepts, in Unicode scalar values.
/// [`format_text`] truncates to this on a char boundary, never a byte one.
pub const MAX_TEXT: usize = 4096;

/// Telegram's envelope around every Bot API method: `ok` says whether the
/// call succeeded, and `description` is the only piece of a failure this
/// channel ever repeats back — never the body it came in, which is not
/// guaranteed free of the request URL.
#[derive(Debug, Deserialize)]
struct TelegramReply {
    ok: bool,
    description: Option<String>,
}

/// `getMe`'s own reply shape: [`TelegramReply`]'s fields plus the bot's
/// identity.
#[derive(Debug, Deserialize)]
struct GetMeReply {
    ok: bool,
    description: Option<String>,
    result: Option<GetMeResult>,
}

/// The part of `getMe`'s `result` this channel reads.
#[derive(Debug, Deserialize)]
struct GetMeResult {
    username: Option<String>,
}

/// Delivers notifications to one Telegram chat through a bot token.
///
/// Holds **wallet-adjacent nothing**: no state but the HTTP client and the
/// three values every call needs. [`Debug`] prints only `base` and
/// `chat_id` — the token is never rendered, through `Debug` or otherwise;
/// see the invariant on [`Secret`].
pub struct TelegramChannel {
    client: reqwest::Client,
    base: String,
    token: Secret,
    chat_id: String,
}

impl std::fmt::Debug for TelegramChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramChannel")
            .field("base", &self.base)
            .field("chat_id", &self.chat_id)
            .finish_non_exhaustive()
    }
}

impl TelegramChannel {
    /// A channel for `chat_id`, authenticating as the bot `token` names.
    /// Points at [`TELEGRAM_API`] until [`TelegramChannel::with_base_url`]
    /// overrides it.
    ///
    /// # Errors
    ///
    /// Only if the underlying [`reqwest::Client`] cannot be built — no
    /// network call is made here.
    pub fn new(token: Secret, chat_id: String) -> Result<Self, NotifyError> {
        let client = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .build()
            .map_err(|error| NotifyError::Channel(error.without_url().to_string()))?;
        Ok(Self {
            client,
            base: TELEGRAM_API.to_string(),
            token,
            chat_id,
        })
    }

    /// Points every call at `base` instead of [`TELEGRAM_API`]. What a test
    /// uses to aim this channel at a local mock server.
    #[must_use]
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// One Bot API call: `POST` when `body` is given, `GET` otherwise, to
    /// `{base}/bot{token}/{method}`. Returns the response's status and raw
    /// text — parsing and the success/failure judgement are each caller's,
    /// since `sendMessage` and `getMe` read different fields out of a
    /// success. Every transport error is passed through
    /// [`reqwest::Error::without_url`] before it becomes text, since the
    /// token is part of the URL this call just built.
    async fn call(
        &self,
        method: &str,
        body: Option<&Value>,
    ) -> Result<(StatusCode, String), NotifyError> {
        let url = format!("{}/bot{}/{method}", self.base, self.token.expose());
        let request = body.map_or_else(
            || self.client.get(&url),
            |body| self.client.post(&url).json(body),
        );
        let response = request
            .send()
            .await
            .map_err(|error| NotifyError::Channel(error.without_url().to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| NotifyError::Channel(error.without_url().to_string()))?;
        Ok((status, text))
    }

    /// Sends `text` to [`TelegramChannel::chat_id`] via `sendMessage`.
    async fn post_send_message(&self, text: &str) -> Result<(), NotifyError> {
        let body = json!({
            "chat_id": self.chat_id,
            "text": text,
            "disable_web_page_preview": true,
        });
        let (status, raw) = self.call("sendMessage", Some(&body)).await?;
        let reply: TelegramReply = parse_reply(status, &raw)?;
        if status.is_success() && reply.ok {
            Ok(())
        } else {
            Err(refusal(status, reply.description.as_deref()))
        }
    }

    /// `getMe`: proves the token authenticates, and answers the bot's own
    /// username — what an operator checks at startup before trusting the
    /// configured credentials.
    ///
    /// # Errors
    ///
    /// A transport failure, a refusal (non-2xx or `ok: false`), an
    /// unreadable body, or a 2xx `ok: true` reply with no username.
    pub async fn verify(&self) -> Result<String, NotifyError> {
        let (status, raw) = self.call("getMe", None).await?;
        let reply: GetMeReply = parse_reply(status, &raw)?;
        if status.is_success() && reply.ok {
            reply
                .result
                .and_then(|result| result.username)
                .ok_or_else(|| {
                    NotifyError::Channel(format!("telegram answered {status} with no username"))
                })
        } else {
            Err(refusal(status, reply.description.as_deref()))
        }
    }
}

impl NotificationChannel for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    fn send<'a>(
        &'a self,
        notification: &'a Notification,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>> {
        Box::pin(async move { self.post_send_message(&format_text(notification)).await })
    }
}

/// Parses `text` as `T`, or reports it as unreadable — deliberately without
/// the text itself, which is not guaranteed free of the request URL.
fn parse_reply<T: for<'de> Deserialize<'de>>(
    status: StatusCode,
    text: &str,
) -> Result<T, NotifyError> {
    serde_json::from_str(text).map_err(|_error| {
        NotifyError::Channel(format!(
            "telegram answered {status} with an unreadable body"
        ))
    })
}

/// The error for a non-2xx status or an `ok: false` reply: the status and
/// Telegram's own `description`, or a stand-in when it gave none. Never the
/// raw body — see the module doc.
fn refusal(status: StatusCode, description: Option<&str>) -> NotifyError {
    NotifyError::Channel(format!(
        "telegram answered {status}: {}",
        description.unwrap_or("<no description>")
    ))
}

/// Renders `n` as the text a Telegram message carries: `[SEVERITY] kind`,
/// `pool: …`, `account: …` when there is one, a blank line, then the
/// message — truncated to [`MAX_TEXT`] Unicode scalar values (never bytes,
/// which could split a multi-byte character).
///
/// No `parse_mode` is set on the `sendMessage` call this feeds, so nothing
/// in `n.message` needs escaping for Telegram's markup.
#[must_use]
pub fn format_text(n: &Notification) -> String {
    let severity = match n.severity {
        Severity::Low => "LOW",
        Severity::Medium => "MEDIUM",
        Severity::High => "HIGH",
    };
    let mut text = format!("[{severity}] {}\npool: {}\n", n.kind.as_str(), n.pool);
    if let Some(account) = &n.account {
        text.push_str("account: ");
        text.push_str(account);
        text.push('\n');
    }
    text.push('\n');
    text.push_str(&n.message);
    text.chars().take(MAX_TEXT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifier::{NotificationKind, Severity};

    fn sample(severity: Severity) -> Notification {
        Notification {
            kind: NotificationKind::UnwindLeftovers,
            severity,
            pool: "CPOOLQZ7Y2Y3ZFRUZWNJSVHZ6VQIJDNZL2WW3XLCF74BRJZ2K5V3B7X4".to_string(),
            account: Some("GACCTQZ7Y2Y3ZFRUZWNJSVHZ6VQIJDNZL2WW3XLCF74BRJZ2K5V3B7X".to_string()),
            message: "left 12.3 XLM of collateral unsold".to_string(),
        }
    }

    #[tokio::test]
    async fn send_posts_the_chat_id_and_the_formatted_text_to_the_token_path() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/bot123:abc/sendMessage"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "chat_id": "42",
                "disable_web_page_preview": true
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "result": {}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let channel = TelegramChannel::new(Secret::new("123:abc"), "42".into())
            .expect("client")
            .with_base_url(server.uri());
        channel
            .send(&sample(Severity::High))
            .await
            .expect("accepted");
        // wiremock verifies `.expect(1)` on drop
    }

    #[tokio::test]
    async fn a_refusal_and_a_transport_failure_are_errors_that_never_carry_the_token() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"ok": false, "description": "Unauthorized"})),
            )
            // What was scripted was consumed: wiremock asserts it on drop.
            .expect(1)
            .mount(&server)
            .await;
        let channel = TelegramChannel::new(Secret::new("123:abc"), "42".into())
            .expect("client")
            .with_base_url(server.uri());
        let error = channel
            .send(&sample(Severity::Low))
            .await
            .expect_err("refused");
        let text = error.to_string();
        assert!(
            text.contains("401") && text.contains("Unauthorized"),
            "{text}"
        );
        assert!(
            !text.contains("123:abc"),
            "the token is in the URL and must not be in the error: {text}"
        );

        let dead = TelegramChannel::new(Secret::new("123:abc"), "42".into())
            .expect("client")
            .with_base_url("http://127.0.0.1:1");
        let text = dead
            .send(&sample(Severity::Low))
            .await
            .expect_err("unreachable")
            .to_string();
        assert!(!text.contains("123:abc"), "{text}");
        assert!(!format!("{dead:?}").contains("123:abc"));
    }

    #[tokio::test]
    async fn verify_answers_the_bot_username() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/bot123:abc/getMe"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"ok": true, "result": {"username": "blend_bot"}}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let channel = TelegramChannel::new(Secret::new("123:abc"), "42".into())
            .expect("client")
            .with_base_url(server.uri());
        assert_eq!(channel.verify().await.expect("verified"), "blend_bot");

        let broken = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .expect(1)
            .mount(&broken)
            .await;
        let channel = TelegramChannel::new(Secret::new("123:abc"), "42".into())
            .expect("client")
            .with_base_url(broken.uri());
        let text = channel.verify().await.expect_err("not found").to_string();
        assert!(text.contains("404"));
        assert!(!text.contains("123:abc"), "{text}");
    }

    #[test]
    fn the_text_names_severity_kind_pool_and_account_and_is_capped() {
        let text = format_text(&sample(Severity::High));
        assert!(text.starts_with("[HIGH] unwind_leftovers\n"));
        assert!(text.contains("pool: CPOOL") && text.contains("account: GACCT"));

        let long = Notification {
            message: "x".repeat(10_000),
            ..sample(Severity::Low)
        };
        assert!(format_text(&long).chars().count() <= MAX_TEXT);
    }
}
