//! Notification channels: where alerts go, and how each is reached.

use serde::Deserialize;

use super::secret::Secret;

/// A named notification channel. Several channels may share a `type` (e.g. two
/// Discord webhooks), and a monitor routes to specific ones by `name`.
///
/// `Debug` is derived: every credential field is a [`Secret`], which redacts
/// itself, so a `{config:?}` in a log line or panic message never leaks a
/// token or webhook URL (pinned by `channel_debug_redacts_secrets`).
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Channel {
    Telegram {
        name: String,
        token: Secret,
        chat_id: String,
    },
    Discord {
        name: String,
        webhook_url: Secret,
    },
    Slack {
        name: String,
        webhook_url: Secret,
    },
    Webhook {
        name: String,
        url: Secret,
    },
    Matrix {
        name: String,
        /// Homeserver base URL, e.g. `https://matrix.org`.
        homeserver: String,
        /// Access token of the sending (bot) user.
        token: Secret,
        /// Target room id, e.g. `!abcdef:matrix.org`.
        room_id: String,
    },
    FreeMobile {
        name: String,
        /// Free Mobile account identifier (the login from the SMS-notifications option).
        user: String,
        /// API key generated in the Free Mobile "Notifications par SMS" option.
        pass: Secret,
    },
    Email {
        name: String,
        host: String,
        #[serde(default = "default_smtp_port")]
        port: u16,
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: Secret,
        from: String,
        to: String,
        /// Implicit TLS (port 465) instead of STARTTLS (the default, port 587).
        #[serde(default)]
        implicit_tls: bool,
    },
    Ntfy {
        name: String,
        /// ntfy topic URL, e.g. `https://ntfy.sh/my-topic`.
        url: Secret,
        /// Optional access token for private ntfy servers.
        #[serde(default)]
        token: Option<Secret>,
    },
    Gotify {
        name: String,
        /// Gotify server URL, e.g. `https://gotify.example.com`.
        url: Secret,
        /// Application token.
        token: Secret,
    },
    Pushover {
        name: String,
        /// Pushover API token (application key).
        token: Secret,
        /// Pushover user key (recipient).
        user: Secret,
        #[serde(flatten)]
        options: hora_notify::pushover::PushoverOptions,
    },
}

impl Channel {
    /// The routing name a monitor refers to.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Telegram { name, .. }
            | Self::Discord { name, .. }
            | Self::Slack { name, .. }
            | Self::Webhook { name, .. }
            | Self::Matrix { name, .. }
            | Self::FreeMobile { name, .. }
            | Self::Email { name, .. }
            | Self::Ntfy { name, .. }
            | Self::Gotify { name, .. }
            | Self::Pushover { name, .. } => name,
        }
    }

    /// Whether the channel's required secret is present (an empty one - e.g. an
    /// unset `${VAR}` - disables the channel rather than erroring at send time).
    #[must_use]
    pub fn is_configured(&self) -> bool {
        match self {
            Self::Telegram { token, chat_id, .. } => !token.is_empty() && !chat_id.is_empty(),
            Self::Discord { webhook_url, .. } | Self::Slack { webhook_url, .. } => {
                !webhook_url.is_empty()
            }
            Self::Webhook { url, .. } | Self::Ntfy { url, .. } => !url.is_empty(),
            Self::Matrix {
                homeserver,
                token,
                room_id,
                ..
            } => !homeserver.is_empty() && !token.is_empty() && !room_id.is_empty(),
            Self::FreeMobile { user, pass, .. } => !user.is_empty() && !pass.is_empty(),
            Self::Email { host, from, to, .. } => {
                !host.is_empty() && !from.is_empty() && !to.is_empty()
            }
            Self::Gotify { url, token, .. } => !url.is_empty() && !token.is_empty(),
            Self::Pushover { token, user, .. } => !token.is_empty() && !user.is_empty(),
        }
    }
}

fn default_smtp_port() -> u16 {
    587
}
