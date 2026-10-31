//! Pushover notifier: POSTs a message to the Pushover API.

use async_trait::async_trait;
use reqwest::Client;

use crate::message::{Message, Urgency};
use crate::util::{Unit, send_retrying};
use crate::{Event, Notifier};

const PUSHOVER_API: &str = "https://api.pushover.net/1/messages.json";

/// Pushover rejects a message over 1024 characters (and a title over 250;
/// ours is a short constant).
const HEAD_MAX: usize = 256;
const BODY_MAX: usize = 1024 - HEAD_MAX - 1;

/// Channel overrides; absent priority keeps Hora's event-based priority.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct PushoverOptions {
    pub priority: Option<i8>,
    pub sound: Option<String>,
    pub retry: Option<u32>,
    pub expire: Option<u32>,
}

impl PushoverOptions {
    /// Reject combinations the API cannot deliver, before starting the daemon.
    ///
    /// # Errors
    /// Returns an error for invalid priorities, emergency timing or an empty sound.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.priority.is_none_or(|p| (-2..=2).contains(&p)),
            "Pushover priority must be -2..2"
        );
        if self.priority == Some(2) {
            anyhow::ensure!(
                self.retry.is_some_and(|s| s >= 30),
                "Pushover priority 2 requires retry >= 30"
            );
            anyhow::ensure!(
                self.expire.is_some_and(|s| (1..=10800).contains(&s)),
                "Pushover priority 2 requires expire in 1..10800"
            );
        } else {
            anyhow::ensure!(
                self.retry.is_none() && self.expire.is_none(),
                "Pushover retry/expire require priority 2"
            );
        }
        anyhow::ensure!(
            self.sound.as_ref().is_none_or(|s| !s.is_empty()),
            "Pushover sound must not be empty"
        );
        Ok(())
    }
}

pub struct PushoverNotifier {
    client: Client,
    token: String,
    user: String,
    options: PushoverOptions,
    endpoint: String,
}

impl PushoverNotifier {
    #[must_use]
    pub fn new(client: Client, token: String, user: String) -> Self {
        Self {
            client,
            token,
            user,
            options: PushoverOptions::default(),
            endpoint: PUSHOVER_API.to_owned(),
        }
    }

    #[must_use]
    pub fn with_options(mut self, options: PushoverOptions) -> Self {
        self.options = options;
        self
    }

    fn message(event: Event<'_>) -> (String, i8) {
        let message = Message::render(event).fit(HEAD_MAX, BODY_MAX, Unit::Chars);
        (message.plain(), priority(message.urgency()))
    }
}

/// Pushover priority: -1 quiet, 0 normal, 1 high. We stop at 1 - priority 2
/// (emergency) would require retry/expire parameters.
fn priority(urgency: Urgency) -> i8 {
    match urgency {
        Urgency::Quiet => -1,
        Urgency::Notice | Urgency::Warning => 0,
        Urgency::High | Urgency::Critical => 1,
    }
}

#[async_trait]
impl Notifier for PushoverNotifier {
    fn name(&self) -> &'static str {
        "pushover"
    }

    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
        self.options.validate()?;
        let (message, priority) = Self::message(event);
        let mut payload = serde_json::json!({
                "token": self.token,
                "user": self.user,
                "message": message,
                "title": "Hora Alert",
                "priority": self.options.priority.unwrap_or(priority),
        });
        if let Some(sound) = &self.options.sound {
            payload["sound"] = sound.clone().into();
        }
        if let Some(retry) = self.options.retry {
            payload["retry"] = retry.into();
        }
        if let Some(expire) = self.options.expire {
            payload["expire"] = expire.into();
        }
        let build = || self.client.post(&self.endpoint).json(&payload);
        // The user key is a quasi-secret too: it lets anyone message the user.
        send_retrying(
            build,
            self.name(),
            &[self.token.as_str(), self.user.as_str()],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AlertSeverity;

    #[test]
    fn maps_events_to_text_and_priority() {
        let (text, priority) = PushoverNotifier::message(Event::CertChanged {
            monitor: "API",
            old_fingerprint: "aa",
            new_fingerprint: "bb",
        });
        assert!(text.starts_with("API TLS certificate changed unexpectedly\nold: aa\nnew: bb"));
        assert_eq!(priority, 1);

        let (_, priority) = PushoverNotifier::message(Event::Digest {
            period: "week",
            summary: "ok",
        });
        assert_eq!(priority, -1);
    }

    #[test]
    fn long_alerts_fit_the_message_limit() {
        let title = "t".repeat(200);
        let message = "m".repeat(3000);
        let (text, _) = PushoverNotifier::message(Event::Alert {
            monitor: "API",
            severity: AlertSeverity::Error,
            title: &title,
            message: &message,
        });
        assert!(text.chars().count() <= 1024, "{}", text.chars().count());
        assert!(text.starts_with("[ERROR] API: ttt"));
    }

    #[tokio::test]
    async fn channel_overrides_reach_mock_api() {
        use axum::{Json, Router, routing::post};
        use tokio::sync::mpsc;
        let (tx, mut rx) = mpsc::channel(8);
        let app = Router::new().route(
            "/",
            post(move |Json(body): Json<serde_json::Value>| {
                let tx = tx.clone();
                async move {
                    tx.send(body).await.unwrap();
                    Json(serde_json::json!({"status": 1}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for priority in [-2, -1, 0, 1, 2] {
            let options = PushoverOptions {
                priority: Some(priority),
                sound: Some("none".into()),
                retry: (priority == 2).then_some(30),
                expire: (priority == 2).then_some(600),
            };
            let mut notifier =
                PushoverNotifier::new(Client::new(), "test-token".into(), "test-user".into())
                    .with_options(options);
            notifier.endpoint.clone_from(&endpoint);
            notifier
                .notify(Event::Digest {
                    period: "week",
                    summary: "ok",
                })
                .await
                .unwrap();
            let body = rx.recv().await.unwrap();
            assert_eq!(body["priority"], priority);
            assert_eq!(body["sound"], "none");
            assert_eq!(body.get("retry").is_some(), priority == 2);
            assert_eq!(body.get("expire").is_some(), priority == 2);
        }
        server.abort();
    }

    #[test]
    fn validates_emergency_parameters() {
        for options in [
            PushoverOptions {
                priority: Some(3),
                ..Default::default()
            },
            PushoverOptions {
                priority: Some(2),
                ..Default::default()
            },
            PushoverOptions {
                priority: Some(2),
                retry: Some(29),
                expire: Some(600),
                ..Default::default()
            },
            PushoverOptions {
                priority: Some(2),
                retry: Some(30),
                expire: Some(10801),
                ..Default::default()
            },
            PushoverOptions {
                retry: Some(60),
                ..Default::default()
            },
        ] {
            assert!(options.validate().is_err());
        }
        assert!(PushoverOptions::default().validate().is_ok());
    }
}
