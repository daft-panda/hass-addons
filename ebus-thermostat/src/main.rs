mod ebusd;
mod homeassistant;

use crate::ebusd::Ebusd;
use crate::homeassistant::Api;
use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use log::{LevelFilter, debug, error, info, warn};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, MqttOptions, Publish, QoS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tokio::select;
use tokio::time::{Duration, Instant, sleep, sleep_until, timeout_at};

const TOPIC_PREFIX: &str = "ebus-thermostat/";
/// SetMode needs to be sent at least once every 10 mins as a keepalive, we use 5 mins
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// How soon to retry when the heater could not be reached
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
/// How long to wait for the MQTT subscription to be acknowledged on startup
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to collect retained MQTT messages after subscribing, before taking control
const RETAINED_WINDOW: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

const STATE_TOPICS: [&str; 4] = ["temp", "temp/low", "temp/high", "mode"];
// temp/set derives low/high, so it has to be applied before those
const COMMAND_TOPICS: [&str; 4] = ["temp/set", "temp/low/set", "temp/high/set", "mode/set"];

#[tokio::main]
async fn main() {
    env_logger::builder()
        .filter(None, LevelFilter::Info)
        .filter(Some("ebus_thermostat"), LevelFilter::Debug)
        .init();

    let options = Options::parse();

    let token = match options
        .ha_api_token
        .clone()
        .or_else(|| env::var("SUPERVISOR_TOKEN").ok())
    {
        Some(v) => v,
        None => {
            error!("No HA API token configured and the SUPERVISOR_TOKEN env var is not set");
            std::process::exit(1);
        }
    };

    // Never exit: HA restarts, broker restarts and ebusd hiccups are all transient, and the
    // supervisor watchdog does not restart an add-on that exits cleanly.
    // The thermostat outlives its connections, so a reconnect doesn't reset the heater state.
    let mut thermostat = Thermostat::new(&options, token);
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        if let Err(e) = thermostat.run().await {
            error!("Thermostat stopped: {:#}", e);
        }

        if started.elapsed() > KEEPALIVE_INTERVAL {
            backoff = Duration::from_secs(1);
        }
        info!("Restarting thermostat in {:?}", backoff);
        sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

#[derive(Clone, Debug, Copy)]
pub struct TemperaturePreferences {
    temperature_band: f32,
    lower_bound: f32,
    higher_bound: f32,
    set_point: f32,
    maintain_state_for: Duration,
}

impl TemperaturePreferences {
    fn set_set_point(&mut self, set_point: f32) {
        self.set_point = set_point;
        self.lower_bound = set_point - self.temperature_band;
        self.higher_bound = set_point + self.temperature_band;
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaterMode {
    AUTO,
    HEAT,
    OFF,
}

impl HeaterMode {
    fn to_command_value(&self) -> String {
        match self {
            HeaterMode::AUTO => String::from("0"),
            HeaterMode::HEAT => String::from("0"),
            HeaterMode::OFF => String::from("0"),
        }
    }
}

impl fmt::Display for HeaterMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HeaterMode::AUTO => "auto",
            HeaterMode::HEAT => "heat",
            HeaterMode::OFF => "off",
        })
    }
}

impl FromStr for HeaterMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "auto" => Ok(HeaterMode::AUTO),
            "heat" => Ok(HeaterMode::HEAT),
            "off" => Ok(HeaterMode::OFF),
            _ => bail!("invalid heater mode"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HeaterSettings {
    hc_mode: HeaterMode,
    flow_temp_desired: u8,
    hwc_temp_desired: u8,
    hwc_flow_temp_desired: Option<u8>,
    disable_hc: bool,
    disable_hwc_load: bool,
}

impl HeaterSettings {
    pub fn to_cmd_arg(&self) -> String {
        format!(
            "{};{};{};{};-;{};0;{};-;0;0;0",
            self.hc_mode.to_command_value(),
            self.flow_temp_desired,
            self.hwc_temp_desired,
            if let Some(v) = self.hwc_flow_temp_desired {
                format!("{}", v)
            } else {
                "-".to_string()
            },
            if self.disable_hc { "1" } else { "0" },
            if self.disable_hwc_load { "1" } else { "0" }
        )
    }
}

impl Default for HeaterSettings {
    fn default() -> Self {
        Self {
            hc_mode: HeaterMode::AUTO,
            flow_temp_desired: 0,
            hwc_temp_desired: 0,
            hwc_flow_temp_desired: None,
            disable_hc: false,
            disable_hwc_load: false,
        }
    }
}

/// Climate settings that survive add-on restarts.
#[derive(Serialize, Deserialize, Debug)]
struct PersistedState {
    mode: HeaterMode,
    set_point: f32,
    lower_bound: f32,
    higher_bound: f32,
    /// Last payload handled per command topic, so retained commands that were already applied
    /// are not replayed on startup.
    #[serde(default)]
    last_commands: HashMap<String, String>,
}

pub struct Thermostat {
    ebusd: Ebusd,
    ha_api: Api,
    mqtt: Option<AsyncClient>,
    mqtt_host: String,
    mqtt_username: String,
    mqtt_password: String,
    thermometer_entity: String,
    state_file: PathBuf,
    loaded_from_file: bool,
    last_commands: HashMap<String, String>,
    prefs: TemperaturePreferences,
    settings: HeaterSettings,
    current_temperature: Option<f32>,
    /// Flow temp last successfully sent to the heater, and when it changed
    applied_flow_temp: Option<u8>,
    last_flow_change: Option<Instant>,
    next_apply: Instant,
}

impl Thermostat {
    pub fn new(options: &Options, ha_api_token: String) -> Self {
        let mut prefs = TemperaturePreferences {
            temperature_band: options.temperature_band,
            lower_bound: 0.0,
            higher_bound: 0.0,
            set_point: 0.0,
            maintain_state_for: Duration::from_secs(60),
        };
        prefs.set_set_point(22.0);

        let mut settings = HeaterSettings {
            hwc_temp_desired: options.tap_water_temp,
            ..Default::default()
        };

        let state_file = options.state_file.clone();
        let mut last_commands = HashMap::new();
        let persisted = load_state(&state_file);
        let loaded_from_file = persisted.is_some();
        if let Some(s) = persisted {
            info!("Restored settings from {}: {:?}", state_file.display(), s);
            settings.hc_mode = s.mode;
            prefs.set_point = s.set_point;
            prefs.lower_bound = s.lower_bound;
            prefs.higher_bound = s.higher_bound;
            last_commands = s.last_commands;
        }

        Self {
            ebusd: Ebusd::new(options.ebusd_address.clone()),
            ha_api: Api::new(
                options.ha_api_address.clone(),
                options
                    .ha_ws_address
                    .clone()
                    .unwrap_or_else(|| options.ha_api_address.clone()),
                ha_api_token,
            ),
            mqtt: None,
            mqtt_host: options.mqtt_host.clone(),
            mqtt_username: options.mqtt_username.clone(),
            mqtt_password: options.mqtt_password.clone(),
            thermometer_entity: options.thermometer_entity.clone(),
            state_file,
            loaded_from_file,
            last_commands,
            prefs,
            settings,
            current_temperature: None,
            applied_flow_temp: None,
            last_flow_change: None,
            next_apply: Instant::now(),
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        info!("Starting new run");

        let initial = self
            .ha_api
            .get_state(&self.thermometer_entity)
            .await
            .context("fetching thermometer state from HA")?
            .ok_or_else(|| anyhow!("Thermometer entity {} not found", self.thermometer_entity))?;
        let mut temp_rx = self
            .ha_api
            .state_updates(self.thermometer_entity.clone())
            .await
            .context("subscribing to HA state updates")?;

        let mut mqtt_options = MqttOptions::new("ebus-thermostat", self.mqtt_host.clone(), 1883);
        mqtt_options.set_keep_alive(Duration::from_secs(60));
        mqtt_options.set_credentials(self.mqtt_username.clone(), self.mqtt_password.clone());
        let (client, mut eventloop) = AsyncClient::new(mqtt_options, 100);
        client
            .try_subscribe(format!("{}#", TOPIC_PREFIX), QoS::AtLeastOnce)
            .context("subscribing to MQTT topics")?;
        self.mqtt = Some(client);

        // Pick up the current climate settings before touching the heater
        self.initial_sync(&mut eventloop).await?;
        self.publish_settings();
        self.handle_temperature(&initial.state);
        self.next_apply = Instant::now();

        loop {
            select! {
                event = eventloop.poll() => {
                    self.handle_mqtt_event(event.context("MQTT connection failed")?)?;
                }
                state = temp_rx.recv() => {
                    let state = state.ok_or_else(|| anyhow!("Lost HA state updates"))?;
                    self.handle_temperature(&state.state);
                }
                _ = sleep_until(self.next_apply) => {
                    self.apply_settings().await;
                }
            }
        }
    }

    /// Waits for the MQTT subscription and collects retained messages, then restores settings:
    /// retained commands that arrived while we were not running are applied, and on first start
    /// (no state file yet) our own retained state topics are used.
    async fn initial_sync(&mut self, eventloop: &mut EventLoop) -> Result<()> {
        let mut retained: HashMap<String, String> = HashMap::new();
        let mut deadline = Instant::now() + SUBSCRIBE_TIMEOUT;
        let mut subscribed = false;

        loop {
            let event = match timeout_at(deadline, eventloop.poll()).await {
                Ok(event) => event.context("MQTT connection failed")?,
                Err(_) if subscribed => break,
                Err(_) => bail!(
                    "MQTT subscription not acknowledged within {:?}",
                    SUBSCRIBE_TIMEOUT
                ),
            };
            match event {
                Event::Incoming(Incoming::Publish(p)) if p.retain => {
                    if let Some((topic, payload)) = decode(&p) {
                        debug!("Retained {}: {}", topic, payload);
                        retained.insert(topic.to_string(), payload.to_string());
                    }
                }
                Event::Incoming(Incoming::SubAck(_)) => {
                    subscribed = true;
                    deadline = Instant::now() + RETAINED_WINDOW;
                }
                event => self.handle_mqtt_event(event)?,
            }
        }

        let has_state = STATE_TOPICS.iter().any(|t| retained.contains_key(*t));
        if !self.loaded_from_file && has_state {
            info!("No saved settings, restoring from retained MQTT state");
            for topic in STATE_TOPICS {
                if let Some(payload) = retained.get(topic)
                    && let Err(e) = self.restore_state(topic, payload)
                {
                    warn!("Ignoring retained {}={}: {:#}", topic, payload, e);
                }
            }
            // the retained state already reflects these
            for topic in COMMAND_TOPICS {
                if let Some(payload) = retained.get(topic) {
                    self.last_commands
                        .insert(topic.to_string(), payload.clone());
                }
            }
        } else {
            for topic in COMMAND_TOPICS {
                if let Some(payload) = retained.get(topic)
                    && self.last_commands.get(topic) != Some(payload)
                {
                    self.handle_command(topic, payload);
                }
            }
        }

        info!("Settings: {}", self.describe());
        self.save_state();
        self.loaded_from_file = true;
        Ok(())
    }

    fn handle_mqtt_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Incoming(Incoming::Publish(p)) => {
                if let Some((topic, payload)) = decode(&p)
                    && COMMAND_TOPICS.contains(&topic)
                {
                    self.handle_command(topic, payload);
                }
            }
            Event::Incoming(Incoming::ConnAck(_)) => info!("Connected to MQTT broker"),
            Event::Incoming(Incoming::Disconnect) => bail!("MQTT broker disconnected"),
            _ => {}
        }
        Ok(())
    }

    fn handle_command(&mut self, topic: &str, payload: &str) {
        let result = match topic {
            // a single set point also moves the band around it
            "temp/set" => parse_temp(payload).map(|v| self.prefs.set_set_point(v)),
            _ => self.restore_state(topic.trim_end_matches("/set"), payload),
        };
        if let Err(e) = result {
            warn!("Ignoring invalid {} command {:?}: {:#}", topic, payload, e);
            return;
        }
        if topic == "mode/set" {
            // mode changes take effect immediately, regardless of the hold time
            self.next_apply = Instant::now();
        }

        info!("{} {}: {}", topic, payload, self.describe());
        self.last_commands
            .insert(topic.to_string(), payload.to_string());
        self.save_state();
        self.publish_settings();
        self.evaluate();
    }

    fn restore_state(&mut self, topic: &str, payload: &str) -> Result<()> {
        match topic {
            "temp" => self.prefs.set_point = parse_temp(payload)?,
            "temp/low" => self.prefs.lower_bound = parse_temp(payload)?,
            "temp/high" => self.prefs.higher_bound = parse_temp(payload)?,
            "mode" => self.settings.hc_mode = payload.parse()?,
            _ => {}
        }
        Ok(())
    }

    fn handle_temperature(&mut self, state: &Value) {
        let temp = match state {
            Value::Number(n) => n.as_f64().map(|v| v as f32),
            Value::String(s) => f32::from_str(s).ok(),
            _ => None,
        };
        let Some(temp) = temp.filter(|t| t.is_finite()) else {
            warn!("Thermometer state is not a temperature: {}", state);
            return;
        };

        if self.current_temperature == Some(temp) {
            return;
        }
        debug!("Temp update: {}", temp);
        self.current_temperature = Some(temp);
        self.publish("temp/current", temp.to_string());
        self.evaluate();
    }

    /// Decides whether the heater should be running and schedules an update if that changed.
    fn evaluate(&mut self) {
        let flow_temp = self.settings.flow_temp_desired;
        let desired = if self.settings.hc_mode == HeaterMode::OFF {
            0
        } else {
            match self.current_temperature {
                Some(t) if flow_temp == 0 && t <= self.prefs.lower_bound => 60,
                Some(t) if flow_temp != 0 && t >= self.prefs.higher_bound => 0,
                _ => flow_temp,
            }
        };
        if desired == flow_temp {
            return;
        }

        debug!("Flow temp {} -> {}", flow_temp, desired);
        self.settings.flow_temp_desired = desired;
        // don't toggle the heater more often than maintain_state_for
        let now = Instant::now();
        let at = self
            .last_flow_change
            .map_or(now, |t| (t + self.prefs.maintain_state_for).max(now));
        self.next_apply = self.next_apply.min(at);
    }

    async fn apply_settings(&mut self) {
        match self.ebusd.apply_settings(&self.settings).await {
            Ok(()) => {
                let flow_temp = self.settings.flow_temp_desired;
                if self.applied_flow_temp != Some(flow_temp) {
                    self.applied_flow_temp = Some(flow_temp);
                    self.last_flow_change = Some(Instant::now());
                }
                self.next_apply = Instant::now() + KEEPALIVE_INTERVAL;
            }
            Err(e) => {
                error!(
                    "Failed to apply settings, retrying in {:?}: {:#}",
                    RETRY_INTERVAL, e
                );
                self.next_apply = Instant::now() + RETRY_INTERVAL;
            }
        }
    }

    fn describe(&self) -> String {
        format!(
            "mode {}, set point {}, low {}, high {}",
            self.settings.hc_mode,
            self.prefs.set_point,
            self.prefs.lower_bound,
            self.prefs.higher_bound
        )
    }

    fn publish(&self, topic: &str, payload: String) {
        let Some(client) = &self.mqtt else { return };
        if let Err(e) = client.try_publish(
            format!("{}{}", TOPIC_PREFIX, topic),
            QoS::AtLeastOnce,
            true,
            payload,
        ) {
            error!("Failed to publish to {}: {:?}", topic, e);
        }
    }

    fn publish_settings(&self) {
        self.publish("mode", self.settings.hc_mode.to_string());
        self.publish("temp/low", self.prefs.lower_bound.to_string());
        self.publish("temp/high", self.prefs.higher_bound.to_string());
        self.publish("temp", self.prefs.set_point.to_string());
    }

    fn save_state(&self) {
        let state = PersistedState {
            mode: self.settings.hc_mode.clone(),
            set_point: self.prefs.set_point,
            lower_bound: self.prefs.lower_bound,
            higher_bound: self.prefs.higher_bound,
            last_commands: self.last_commands.clone(),
        };
        if let Err(e) = write_atomic(
            &self.state_file,
            &serde_json::to_vec_pretty(&state).unwrap(),
        ) {
            warn!(
                "Failed to save settings to {}: {:#}",
                self.state_file.display(),
                e
            );
        }
    }
}

fn load_state(path: &Path) -> Option<PersistedState> {
    let data = match std::fs::read(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!("Failed to read {}: {}", path.display(), e);
            return None;
        }
    };
    match serde_json::from_slice(&data) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("Ignoring invalid state file {}: {}", path.display(), e);
            None
        }
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// Splits one of our publishes into its topic (without prefix) and trimmed payload.
fn decode(p: &Publish) -> Option<(&str, &str)> {
    let topic = p.topic.strip_prefix(TOPIC_PREFIX)?;
    match std::str::from_utf8(&p.payload) {
        Ok(payload) => Some((topic, payload.trim())),
        Err(e) => {
            warn!("Ignoring non UTF-8 payload on {}: {}", p.topic, e);
            None
        }
    }
}

fn parse_temp(payload: &str) -> Result<f32> {
    let v = f32::from_str(payload)?;
    if !v.is_finite() {
        bail!("not a finite temperature");
    }
    Ok(v)
}

#[derive(Parser, Debug)]
pub struct Options {
    #[arg(long, default_value = "http://supervisor/core")]
    ha_api_address: String,
    #[arg(long)]
    ha_ws_address: Option<String>,
    #[arg(long)]
    ha_api_token: Option<String>,
    #[arg(long, default_value_t = String::new())]
    ebusd_address: String,
    #[arg(long, default_value_t = String::new())]
    thermometer_entity: String,
    #[arg(long, default_value_t = 0.5)]
    temperature_band: f32,
    #[arg(long, default_value_t = 55)]
    tap_water_temp: u8,
    #[arg(long)]
    mqtt_host: String,
    #[arg(long)]
    mqtt_username: String,
    #[arg(long)]
    mqtt_password: String,
    /// Where climate settings are persisted across restarts
    #[arg(long, default_value = "/data/state.json")]
    state_file: PathBuf,
}
