mod ebusd;
mod homeassistant;

use crate::ebusd::Ebusd;
use crate::homeassistant::Api;
use anyhow::{anyhow, bail, Result};
use log::{debug, error, info, LevelFilter};
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, MqttOptions, QoS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::str::FromStr;
use std::{env, fs};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::time::{sleep, Duration, Instant};
use tokio::{io, pin, select};

#[tokio::main]
async fn main() {
    env_logger::builder()
        .filter(None, LevelFilter::Debug)
        .init();

    let options_file = Path::new("/data/options.json");
    let options: Options =
        serde_json::from_slice(fs::read(options_file).unwrap().as_slice()).unwrap();
    debug!("Read options: {:?}", options);

    let mut thermostat = match Thermostat::new(
        options
            .ha_api_address
            .unwrap_or_else(|| "http://supervisor/core".to_string()),
        options
            .ha_api_token
            .unwrap_or_else(|| match env::var("SUPERVISOR_TOKEN") {
                Ok(v) => v,
                Err(_) => {
                    error!("SUPERVISOR_TOKEN env var is not set\n Available env vars:");
                    for (key, value) in env::vars() {
                        error!("{key}: {value}");
                    }
                    panic!("Exiting");
                }
            }),
        options.ebusd_address,
        options.thermometer_entity,
        options.mqtt_host,
        options.mqtt_username,
        options.mqtt_password,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            error!("Failed to initialise thermostat: {:?}", e);
            return;
        }
    };

    let mut tp = TemperaturePreferences::default();
    tp.tap_water_set_point = options.tap_water_temp as f32;
    thermostat.set_temp_preference(tp);

    match thermostat.run().await {
        Ok(_) => {}
        Err(e) => error!("Unhandled exception: {:?}", e),
    }
}

#[derive(Clone, Debug)]
pub struct TemperaturePreferences {
    low_watermark: f32,
    high_watermark: f32,
    set_point: f32,
    maintain_state_for: Duration,
    tap_water_set_point: f32,
}

impl Default for TemperaturePreferences {
    fn default() -> Self {
        TemperaturePreferences {
            low_watermark: 19.0,
            high_watermark: 23.0,
            set_point: 22.0,
            maintain_state_for: Duration::from_secs(60),
            tap_water_set_point: 45.0,
        }
    }
}

#[derive(Clone, Debug)]
pub enum HeaterMode {
    AUTO,
    HEAT,
    OFF,
}

impl ToString for HeaterMode {
    fn to_string(&self) -> String {
        match self {
            HeaterMode::AUTO => String::from("0"),
            HeaterMode::HEAT => String::from("0"),
            HeaterMode::OFF => String::from("off"),
        }
    }
}

impl FromStr for HeaterMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "auto" => Ok(HeaterMode::AUTO),
            "heat" => Ok(HeaterMode::HEAT),
            "off" => Ok(HeaterMode::OFF),
            _ => Err(()),
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
    pub fn into_cmd_arg(self) -> String {
        format!(
            "{};{};{};{};-;{};0;{};-;0;0;0",
            self.hc_mode.to_string(),
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

pub struct Thermostat {
    ebusd: Ebusd,
    ha_api: Api,
    mqtt_host: String,
    mqtt_username: String,
    mqtt_password: String,
    thermometer_entity: String,
    prefs: TemperaturePreferences,
    settings: HeaterSettings,
    current_temperature: f32,
    last_mode_set_time: Option<Instant>,
    mqtt_tx: Sender<(String, String)>,
    mqtt_rx: Option<Receiver<(String, String)>>,
    set_fails: u8,
}

impl Thermostat {
    pub async fn new(
        ha_address: String,
        ha_api_token: String,
        ebusd_address: String,
        thermometer_entity: String,
        mqtt_host: String,
        mqtt_username: String,
        mqtt_password: String,
    ) -> anyhow::Result<Self> {
        let api = Api::new(ha_address, ha_api_token);
        // check if thermometer entity exists
        let devices = api.get_devices().await?;
        let entities: Vec<String> = devices
            .into_iter()
            .flat_map(|d| d.entities)
            .flatten()
            .collect();
        let mut found = false;
        for entity in entities {
            if entity == thermometer_entity {
                found = true;
                break;
            }
        }

        if !found {
            bail!("Thermometer entity {} not found", thermometer_entity);
        }

        let mut ebusd = Ebusd::new(ebusd_address).await?;
        ebusd.define_message( "wi,BAI,SetModeOverride,OperatingMode,,08,B510,00,hcmode,,UCH,,,,flowtempdesired,,D1C,,,,hwctempdesired,,D1C,,,,hwcflowtempdesired,,UCH,,,,setmode1,,UCH,,,,disablehc,,BI0,,,,disablehwctapping,,BI1,,,,disablehwcload,,BI2,,,,setmode2,,UCH,,,,remoteControlHcPump,,BI0,,,,releaseBackup,,BI1,,,,releaseCooling,,BI2".to_string()).await?;

        let (tx, rx) = channel(50);

        Ok(Self {
            ebusd,
            ha_api: api,
            thermometer_entity,
            mqtt_host,
            mqtt_username,
            mqtt_password,
            prefs: TemperaturePreferences::default(),
            settings: HeaterSettings::default(),
            last_mode_set_time: None,
            current_temperature: 0.0,
            mqtt_tx: tx,
            mqtt_rx: Some(rx),
            set_fails: 0,
        })
    }

    async fn mqtt_reconnect(&self) -> Result<(AsyncClient, EventLoop)> {
        let mut mqttoptions = MqttOptions::new("ebus-thermostat", self.mqtt_host.clone(), 1883);
        mqttoptions.set_keep_alive(Duration::from_secs(5));
        mqttoptions.set_credentials(self.mqtt_username.clone(), self.mqtt_password.clone());

        let (client, eventloop) = AsyncClient::new(mqttoptions, 10);

        client
            .subscribe("ebus-thermostat/#", QoS::AtLeastOnce)
            .await?;

        Ok((client, eventloop))
    }

    pub async fn run(&mut self) -> Result<()> {
        let (mut client, mut mqtt_eventloop) = self.mqtt_reconnect().await?;

        let mut temp_rx = self.temperature_changes().await?;
        let mut mqtt_rx = self.mqtt_rx.take().unwrap();

        let hold_timer = sleep(self.prefs.maintain_state_for);
        pin!(hold_timer);
        let mut hold = false;
        let mut update_pending = false;
        let mut retries = 0;

        self.settings.hwc_temp_desired = self.prefs.tap_water_set_point as u8;
        self.apply_settings().await?;

        loop {
            let mut repeat_timer = Duration::from_secs(5 * 60);
            if let Some(last_set) = self.last_mode_set_time {
                repeat_timer = repeat_timer.saturating_sub(Instant::now().duration_since(last_set));
            }

            select! {
                event = mqtt_eventloop.poll() => {
                    match event {
                        Ok(ev) => {
                            self.handle_mqtt_message(ev).await?;
                        }
                        Err(e) => {
                            error!("MQTT error: {:?}", e);
                            retries += 1;
                            if retries > 5 {
                                tokio::time::sleep(Duration::from_secs(60)).await;
                                retries = 0;
                            }
                            (client, mqtt_eventloop) = self.mqtt_reconnect().await?;
                        }
                    }
                }
                temp = temp_rx.recv() => {
                    if temp.is_none() {
                        continue;
                    }
                    let temp = temp.unwrap();
                    client.publish("ebus-thermostat/temp", QoS::AtLeastOnce, true, format!("{}", temp)).await?;
                    debug!("Published MQTT update: {}", temp);

                    if temp == self.current_temperature {
                        continue;
                    } else {
                        self.current_temperature = temp;
                    }

                    if let Some(mode_update) = self.update_mode(temp) {
                        debug!("Active mode update: {:?}", mode_update);
                        if hold {
                            update_pending = true;
                            continue;
                        } else {
                            hold_timer.as_mut().reset(Instant::now() + self.prefs.maintain_state_for);
                            hold = true;
                            debug!("Setting active mode");
                            self.apply_settings().await?;
                        }
                    }
                }
                m = mqtt_rx.recv() => {
                    if let Some((topic, msg)) = m {
                        client.publish(format!("ebus-thermostat/{}", topic), QoS::AtLeastOnce, true, msg).await?;
                    }
                }
                // SetMode needs to be called at least once every 10 mins as a keepalive, we use 5 mins
                _ = sleep(repeat_timer) => {
                    debug!("Repeating mode");
                    self.apply_settings().await?;
                }
                _ = &mut hold_timer => {
                    hold = false;

                    if update_pending {
                        debug!("Setting mode after hold timer");
                        self.apply_settings().await?;
                        update_pending = false;
                    }

                    hold_timer.as_mut().reset(Instant::now() + Duration::from_secs(999999999));
                }
            }
        }
    }

    fn update_mode(&mut self, current_temp: f32) -> Option<HeaterSettings> {
        if self.settings.flow_temp_desired == 0 {
            // heater is currently inactive
            if current_temp <= self.prefs.low_watermark {
                self.settings.flow_temp_desired = 60;
                return Some(self.settings.clone());
            }
        } else if self.settings.flow_temp_desired != 0 {
            // heater is active
            if current_temp >= self.prefs.high_watermark {
                self.settings.flow_temp_desired = 0;
                return Some(self.settings.clone());
            }
        }

        None
    }

    async fn publish_settings(&self) -> Result<()> {
        self.mqtt_tx
            .send(("mode".to_string(), self.settings.hc_mode.to_string()))
            .await?;
        self.mqtt_tx
            .send((
                "temp/low".to_string(),
                format!("{}", self.prefs.low_watermark),
            ))
            .await?;
        self.mqtt_tx
            .send((
                "temp/high".to_string(),
                format!("{}", self.prefs.high_watermark),
            ))
            .await?;
        self.mqtt_tx
            .send(("temp".to_string(), format!("{}", self.prefs.set_point)))
            .await?;
        Ok(())
    }

    async fn apply_settings(&mut self) -> Result<()> {
        match self.ebusd.apply_settings(self.settings.clone()).await {
            Ok(_) => {
                self.set_fails = 0;
            }
            Err(e) => {
                error!("Failed applying settings: {:?}", e);
                self.set_fails += 1;

                match e.downcast_ref::<io::Error>() {
                    None => {}
                    Some(e) => {
                        debug!("Original error: {}", e.to_string());
                        let msg = e.to_string().to_lowercase();
                        if msg.contains("broken pipe") || msg.contains("connection") {
                            debug!("Reconnecting...");
                            self.ebusd.reconnect().await?;
                            self.ebusd.define_message( "wi,BAI,SetModeOverride,OperatingMode,,08,B510,00,hcmode,,UCH,,,,flowtempdesired,,D1C,,,,hwctempdesired,,D1C,,,,hwcflowtempdesired,,UCH,,,,setmode1,,UCH,,,,disablehc,,BI0,,,,disablehwctapping,,BI1,,,,disablehwcload,,BI2,,,,setmode2,,UCH,,,,remoteControlHcPump,,BI0,,,,releaseBackup,,BI1,,,,releaseCooling,,BI2".to_string()).await?;
                        }
                    }
                }

                if self.set_fails > 5 {
                    tokio::time::sleep(Duration::from_secs(10 * 60)).await;
                    self.set_fails = 0;
                }
            }
        }
        self.last_mode_set_time = Some(Instant::now());
        self.publish_settings().await?;
        Ok(())
    }

    fn set_temp_preference(&mut self, prefs: TemperaturePreferences) {
        self.prefs = prefs;
    }

    pub async fn temperature_changes(&self) -> Result<Receiver<f32>> {
        let mut event_rx = self.ha_api.state_updates().await?;
        let (tx, rx) = channel(10);
        let thermometer_entity = self.thermometer_entity.clone();

        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if event.event_type != "state_changed" {
                    continue;
                }
                if event.data.new_state.entity_id != thermometer_entity {
                    continue;
                }
                let val = event.data.new_state.state;
                let temp = match val {
                    Value::Null => {
                        continue;
                    }
                    Value::Bool(_) => {
                        continue;
                    }
                    Value::Number(n) => n.as_f64().unwrap() as f32,
                    Value::String(s) => f32::from_str(&s).unwrap(),
                    Value::Array(_) => {
                        continue;
                    }
                    Value::Object(_) => {
                        continue;
                    }
                };
                debug!("Temp update: {}", temp);
                tx.send(temp).await.unwrap();
            }
        });

        Ok(rx)
    }

    pub async fn handle_mqtt_message(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Incoming(v) => match v {
                Incoming::Publish(publish) => {
                    let topic_parts: Vec<&str> = publish.topic.split('/').collect();
                    match topic_parts[1] {
                        "temp" => {
                            if topic_parts.len() <= 2 {
                                return Ok(());
                            }

                            match topic_parts[2] {
                                "set" => {
                                    self.prefs.set_point = f32::from_str(&String::from_utf8(
                                        publish.payload.to_vec(),
                                    )?)?;
                                    info!("New temp set point: {}", self.prefs.set_point);
                                    self.publish_settings().await?;
                                }
                                _ => {}
                            }
                        }
                        "mode" => {
                            if topic_parts.len() <= 2 {
                                return Ok(());
                            }

                            match topic_parts[2] {
                                "set" => {
                                    self.settings.hc_mode = HeaterMode::from_str(
                                        String::from_utf8(publish.payload.to_vec())?.as_str(),
                                    )
                                    .map_err(|_| anyhow!("Invalid heater mode"))?;
                                    info!("New heater mode: {}", self.settings.hc_mode.to_string());
                                    self.apply_settings().await?;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                Incoming::Disconnect => {
                    bail!("MQTT disconnect")
                }
                _ => {}
            },
            Event::Outgoing(v) => {
                debug!("OUT {:?}", v);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Options {
    ha_api_address: Option<String>,
    ha_api_token: Option<String>,
    ebusd_address: String,
    thermometer_entity: String,
    tap_water_temp: u8,
    mqtt_host: String,
    mqtt_username: String,
    mqtt_password: String,
}