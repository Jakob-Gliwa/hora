//! Shared mqtt-glide connections and bounded, latest-message-only caches.

use std::collections::{HashMap, HashSet};
use std::num::{NonZeroU16, NonZeroU32, NonZeroUsize};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock, Weak};
use std::time::Duration;

use mqtt_glide::{ClientBuilder, LifecycleHook, MqttClient, QoS, SessionPhase, StaticCredentials};
use serde::Deserialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::{Config, MonitorKind, MqttSpec, Secret};
use crate::probe::{FailureKind, Outcome};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Broker {
    pub name: String,
    pub url: Secret,
    #[serde(default)]
    pub username: Option<Secret>,
    #[serde(default)]
    pub password: Option<Secret>,
    #[serde(default = "default_packet_kb")]
    pub max_packet_kb: u32,
    #[serde(default = "default_keep_alive")]
    pub keep_alive_secs: u64,
}
fn default_packet_kb() -> u32 {
    64
}
fn default_keep_alive() -> u64 {
    15
}

pub(crate) fn validate(config: &Config) -> anyhow::Result<()> {
    let mut names = HashSet::new();
    let mut endpoints = HashSet::new();
    for broker in &config.brokers {
        anyhow::ensure!(
            !broker.name.is_empty() && names.insert(&broker.name),
            "broker names must be nonempty and unique"
        );
        let url = reqwest::Url::parse(broker.url.as_ref())
            .map_err(|_| anyhow::anyhow!("invalid MQTT broker URL"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "mqtt" | "mqtts")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && matches!(url.path(), "" | "/"),
            "broker URL must be mqtt(s)://host:port; use username/password fields"
        );
        anyhow::ensure!(
            ClientBuilder::new(broker.url.as_ref()).is_ok(),
            "invalid MQTT broker URL"
        );
        let identity = (
            url.scheme().to_owned(),
            url.host_str().unwrap_or_default().to_owned(),
            url.port()
                .unwrap_or(if url.scheme() == "mqtts" { 8883 } else { 1883 }),
            broker.username.as_ref().map(AsRef::as_ref),
        );
        anyhow::ensure!(
            endpoints.insert(identity),
            "duplicate MQTT endpoint/user; share one named broker"
        );
        anyhow::ensure!(
            broker.password.is_none() || broker.username.is_some(),
            "MQTT password requires username"
        );
        anyhow::ensure!(
            (1..=1024).contains(&broker.max_packet_kb),
            "MQTT max_packet_kb must be 1..1024"
        );
        anyhow::ensure!(
            (5..=300).contains(&broker.keep_alive_secs),
            "MQTT keep_alive_secs must be 5..300"
        );
    }
    for monitor in &config.monitors {
        if let MonitorKind::Mqtt(spec) = &monitor.spec {
            anyhow::ensure!(
                names.contains(&spec.broker),
                "monitor {}: unknown MQTT broker",
                monitor.id
            );
        }
    }
    Ok(())
}

#[derive(Clone)]
struct Message {
    payload: Arc<[u8]>,
    received: Instant,
}

#[derive(Default)]
struct Topic {
    message: Option<Message>,
    allowed: bool,
}

struct BrokerState {
    topics: Mutex<HashMap<String, Topic>>,
    client: OnceLock<MqttClient>,
    cap: usize,
}

impl BrokerState {
    fn new(topics: &[String], cap: usize) -> Self {
        Self {
            topics: Mutex::new(
                topics
                    .iter()
                    .map(|s| (s.clone(), Topic::default()))
                    .collect(),
            ),
            client: OnceLock::new(),
            cap,
        }
    }

    fn receive(&self, topic: &str, payload: &[u8], now: Instant) {
        let mut topics = self.topics.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = topics.get_mut(topic) {
            // Copy exactly the payload, avoiding retaining a large codec buffer.
            entry.message = (payload.len() <= self.cap).then(|| Message {
                payload: Arc::from(payload),
                received: now,
            });
        }
    }

    fn clear(&self) {
        for entry in self
            .topics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values_mut()
        {
            entry.message = None;
        }
    }

    fn outcome(&self, spec: &MqttSpec, now: Instant) -> Outcome {
        let Some(client) = self.client.get().filter(|c| c.is_connected()) else {
            return Outcome::down(
                FailureKind::ConnectionFailed,
                "MQTT broker disconnected".into(),
            );
        };
        if client
            .refused_subscriptions()
            .borrow()
            .iter()
            .any(|s| &*s.filter == spec.topic.as_str())
        {
            return Outcome::down(
                FailureKind::ConnectionFailed,
                "MQTT subscription refused".into(),
            );
        }
        let (allowed, message) = self
            .topics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&spec.topic)
            .map_or((false, None), |t| (t.allowed, t.message.clone()));
        if !allowed {
            return Outcome::down(
                FailureKind::ConnectionFailed,
                "MQTT subscription not ready".into(),
            );
        }
        evaluate(spec, message.as_ref(), now)
    }
}

struct Lifecycle(Weak<BrokerState>);
impl LifecycleHook for Lifecycle {
    fn on_lifecycle(&self, phase: &SessionPhase) {
        if !matches!(phase, SessionPhase::Connected { .. })
            && let Some(state) = self.0.upgrade()
        {
            state.clear();
        }
    }
}

fn evaluate(spec: &MqttSpec, message: Option<&Message>, now: Instant) -> Outcome {
    let Some(message) = message else {
        return Outcome::down(
            FailureKind::MissingHeartbeat,
            "MQTT message not received".into(),
        );
    };
    if spec.max_age_secs.is_some_and(|age| {
        now.saturating_duration_since(message.received) > Duration::from_secs(age)
    }) {
        return Outcome::down(
            FailureKind::MissingHeartbeat,
            "MQTT message is stale".into(),
        );
    }
    if spec.max_body_kb.is_some_and(|kb| {
        message.payload.len()
            > usize::try_from(kb)
                .unwrap_or(usize::MAX)
                .saturating_mul(1024)
    }) {
        return Outcome::down(
            FailureKind::Content,
            "MQTT payload exceeds assertion limit".into(),
        );
    }
    if let Some(keyword) = &spec.keyword
        && let Some(failure) = crate::assertion::check_keyword(keyword, &message.payload)
    {
        return Outcome::down(FailureKind::Content, failure);
    }
    if let Some(json) = &spec.json
        && let Some(failure) =
            crate::probe::check_json(json, &String::from_utf8_lossy(&message.payload))
    {
        return Outcome::down(FailureKind::Content, failure);
    }
    if let Some(jq) = &spec.jq
        && let Some(failure) = jq.check(&message.payload)
    {
        return Outcome::down(FailureKind::Content, failure);
    }
    Outcome::up(false, None, None)
}

#[derive(Default)]
pub struct MqttHub {
    brokers: RwLock<HashMap<String, Arc<BrokerState>>>,
}
impl MqttHub {
    #[must_use]
    pub fn outcome(&self, spec: &MqttSpec) -> Outcome {
        let state = self
            .brokers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&spec.broker)
            .cloned();
        state.map_or_else(
            || {
                Outcome::down(
                    FailureKind::ConnectionFailed,
                    "MQTT broker not ready".into(),
                )
            },
            |s| s.outcome(spec, Instant::now()),
        )
    }
}

struct Running {
    spec: Broker,
    topics: Vec<String>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// Start one client per used broker. Unchanged brokers survive config reloads.
#[must_use]
pub fn spawn(
    mut config: watch::Receiver<Arc<Config>>,
    mut shutdown: watch::Receiver<bool>,
) -> (Arc<MqttHub>, JoinHandle<()>) {
    let hub = Arc::new(MqttHub::default());
    let shared = Arc::clone(&hub);
    let task = tokio::spawn(async move {
        let mut running: HashMap<String, Running> = HashMap::new();
        loop {
            let snapshot = config.borrow_and_update().clone();
            let desired: HashMap<String, (Broker, Vec<String>)> = snapshot
                .brokers
                .iter()
                .filter_map(|broker| {
                    let mut topics: Vec<_> = snapshot
                        .monitors
                        .iter()
                        .filter_map(|m| match &m.spec {
                            MonitorKind::Mqtt(s) if s.broker == broker.name => {
                                Some(s.topic.clone())
                            }
                            _ => None,
                        })
                        .collect();
                    topics.sort();
                    topics.dedup();
                    (!topics.is_empty()).then(|| (broker.name.clone(), (broker.clone(), topics)))
                })
                .collect();
            let obsolete: Vec<_> = running
                .iter()
                .filter(|(name, run)| {
                    desired
                        .get(*name)
                        .is_none_or(|(b, topics)| *b != run.spec || *topics != run.topics)
                        || run.task.is_finished()
                })
                .map(|(n, _)| n.clone())
                .collect();
            for name in obsolete {
                shared
                    .brokers
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&name);
                if let Some(run) = running.remove(&name) {
                    let _ = run.stop.send(true);
                    let _ = run.task.await;
                }
            }
            for (name, (spec, topics)) in desired {
                if running.contains_key(&name) {
                    continue;
                }
                let state = Arc::new(BrokerState::new(
                    &topics,
                    usize::try_from(spec.max_packet_kb).unwrap_or(1024) * 1024,
                ));
                shared
                    .brokers
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name.clone(), Arc::clone(&state));
                let (stop, stopped) = watch::channel(false);
                let task = tokio::spawn(run_broker(spec.clone(), topics.clone(), state, stopped));
                running.insert(
                    name,
                    Running {
                        spec,
                        topics,
                        stop,
                        task,
                    },
                );
            }
            tokio::select! {
                changed = config.changed() => { if changed.is_err() { break; } }
                _ = shutdown.changed() => break,
                // Recover a worker that stopped unexpectedly without waking per monitor.
                () = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
        shared
            .brokers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        for run in running.values() {
            let _ = run.stop.send(true);
        }
        for (_, run) in running {
            let _ = run.task.await;
        }
    });
    (hub, task)
}

async fn run_broker(
    spec: Broker,
    topics: Vec<String>,
    state: Arc<BrokerState>,
    mut stopped: watch::Receiver<bool>,
) {
    let Ok(builder) = ClientBuilder::new(spec.url.as_ref()) else {
        return;
    };
    let inbound = Arc::downgrade(&state);
    let mut builder = builder
        .on_inbound(move |msg| {
            if let Some(state) = inbound.upgrade() {
                state.receive(msg.topic.as_ref(), &msg.payload, Instant::now());
            }
        })
        .lifecycle(Lifecycle(Arc::downgrade(&state)))
        .wait_connected(false)
        .options(|o| {
            o.keep_alive(Duration::from_secs(spec.keep_alive_secs))
                .max_packet_size(
                    NonZeroU32::new(spec.max_packet_kb * 1024).expect("validated packet limit"),
                )
                .receive_maximum(NonZeroU16::new(4).unwrap())
                .command_channel_capacity(NonZeroUsize::new(8).unwrap())
        });
    if let Some(username) = &spec.username {
        builder = builder.credentials(StaticCredentials::new(
            username.as_ref(),
            spec.password
                .as_ref()
                .map_or("", AsRef::as_ref)
                .as_bytes()
                .to_vec(),
        ));
    }
    let Ok(client) = builder.connect().await else {
        tracing::warn!(broker = %spec.name, "MQTT client initialization failed");
        return;
    };
    let _ = state.client.set(client.clone());
    let mut subscribed = false;
    loop {
        if *stopped.borrow() {
            break;
        }
        if !subscribed && client.is_connected() {
            let result = tokio::select! {
                result = client.subscribe(topics.iter().map(|t| (t.as_str(), QoS::AtLeastOnce))) => result,
                _ = stopped.changed() => break,
            };
            if let Ok(ack) = result {
                let mut cache = state.topics.lock().unwrap_or_else(PoisonError::into_inner);
                for (topic, reason) in topics.iter().zip(&ack.reasons) {
                    if let Some(entry) = cache.get_mut(topic) {
                        entry.allowed = reason.is_granted();
                    }
                }
                subscribed =
                    ack.reasons.len() == topics.len() && ack.reasons.iter().all(|r| r.is_granted());
            }
        }
        if subscribed {
            let _ = stopped.changed().await;
            break;
        }
        tokio::select! {
            _ = stopped.changed() => break,
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
    state.clear();
    let _ = tokio::time::timeout(Duration::from_secs(3), client.shutdown()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> MqttSpec {
        MqttSpec {
            broker: "test".into(),
            topic: "device/state".into(),
            max_age_secs: Some(10),
            keyword: Some(crate::config::KeywordAssert {
                text: "online".into(),
                invert: false,
            }),
            json: None,
            jq: None,
            max_body_kb: None,
        }
    }
    #[test]
    fn mqtt_missing_stale_keyword_inversion_and_bounded_cache() {
        let now = Instant::now();
        let mut spec = spec();
        assert!(!evaluate(&spec, None, now).is_up());
        let msg = Message {
            payload: Arc::from(b"online".as_slice()),
            received: now,
        };
        assert!(evaluate(&spec, Some(&msg), now).is_up());
        assert!(!evaluate(&spec, Some(&msg), now + Duration::from_secs(11)).is_up());
        spec.keyword.as_mut().unwrap().invert = true;
        assert!(!evaluate(&spec, Some(&msg), now).is_up());
        let state = BrokerState::new(&[spec.topic.clone()], 16);
        state.receive("unexpected", b"data", now);
        state.receive(&spec.topic, b"online", now);
        assert_eq!(state.topics.lock().unwrap().len(), 1);
        state.receive(&spec.topic, &[0; 17], now);
        assert!(state.topics.lock().unwrap()[&spec.topic].message.is_none());
        state.receive(&spec.topic, b"online", now);
        state.clear();
        assert!(state.topics.lock().unwrap()[&spec.topic].message.is_none());
    }
    #[test]
    fn mqtt_reuses_jq_assertions_and_fails_bad_json() {
        let now = Instant::now();
        let mut spec = spec();
        spec.keyword = None;
        spec.jq = Some(crate::assertion::JqAssert {
            query: crate::config::Compiled::parse("type == \"number\" and . > 100").unwrap(),
            expected: None,
            operator: crate::assertion::Comparison::Eq,
        });
        for (payload, up) in [
            (b"101".as_slice(), true),
            (b"99".as_slice(), false),
            (b"null".as_slice(), false),
            (b"invalid".as_slice(), false),
        ] {
            let message = Message {
                payload: Arc::from(payload),
                received: now,
            };
            assert_eq!(evaluate(&spec, Some(&message), now).is_up(), up);
        }
    }
}
