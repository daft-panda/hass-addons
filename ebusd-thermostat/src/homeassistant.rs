use anyhow::bail;
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, trace};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
use time::OffsetDateTime;
use tokio::sync::mpsc::Receiver;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message::Text;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(transparent)]
pub struct Namespaces {
    pub(crate) namespaces: Vec<Devices>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Devices {
    #[serde(flatten)]
    pub(crate) devices: HashMap<String, Device>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Device {
    pub(crate) name: String,
    pub(crate) entities: Vec<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HaMessage {
    pub(crate) id: u8,
    #[serde(alias = "type")]
    pub(crate) msg_type: String,
    pub(crate) event: Event,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Event {
    pub(crate) event_type: String,
    pub(crate) data: EventData,
    pub(crate) origin: String,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) time_fired: OffsetDateTime,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EventData {
    pub(crate) old_state: State,
    pub(crate) new_state: State,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct State {
    pub(crate) entity_id: String,
    pub(crate) state: Value,
    pub(crate) attributes: HashMap<String, Value>,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) last_changed: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) last_updated: OffsetDateTime,
}

pub struct Api {
    url: String,
    bearer_token: String,
    client: Client,
}

impl Api {
    pub fn new(url: String, bearer_token: String) -> Self {
        Self {
            url,
            bearer_token,
            client: Client::new(),
        }
    }

    pub async fn get_devices(&self) -> anyhow::Result<Vec<Device>> {
        let template = "{\"template\": \"{% set devices = states | map(attribute='entity_id') | map('device_id') | unique | reject('eq',None) | list %}{%- set ns = namespace(devices = []) %}{%- for device in devices %} {%- set entities = device_entities(device) | list %}{%- if entities %}{%- set ns.devices = ns.devices +  [ {device: {\\\"name\\\": device_attr(device, \\\"name\\\"), \\\"entities\\\":[entities ]}} ] %}{%- endif %}{%- endfor %}{{ ns.devices }}\"}";
        let req = self
            .client
            .post(Url::from_str(&format!(
                "http://{}/api/template",
                self.url.clone()
            ))?)
            .bearer_auth(self.bearer_token.clone())
            .body(template.to_string())
            .build()?;
        let res = self.client.execute(req).await?;
        let mut bytes = String::from_utf8(Vec::from(res.bytes().await?))?;
        // the API does not produce valid JSON (uses single quotes). this 'fixes' it except for when
        // there is a value using single quotes in which case it breaks it in a new way
        bytes = bytes.replace('\'', "\"");
        trace!("{}", bytes);
        let ns: Namespaces = serde_json::from_str(&bytes)?;
        let devices: Vec<Device> = ns
            .namespaces
            .into_iter()
            .flat_map(|d| d.devices.into_values().collect::<Vec<Device>>())
            .collect();

        Ok(devices)
    }

    pub async fn state_updates(&self) -> anyhow::Result<Receiver<Event>> {
        let url = Url::parse(&format!("ws://{}/api/websocket", self.url.clone()))?;
        let (ws, _) = connect_async(url).await?;
        let (mut write, mut read) = ws.split();
        // await auth required message
        let msg = read.next().await.unwrap()?;
        if let Text(str) = msg {
            trace!("{}", str);
            if !str.contains("auth_required") {
                bail!("Invalid ws handshake");
            }
        } else {
            bail!("Invalid ws handshake");
        }

        write
            .send(Text(
                r#"
{
  "type": "auth",
  "access_token": "blurg"
}
        "#
                .replace("blurg", &self.bearer_token),
            ))
            .await?;

        let msg = read.next().await.unwrap()?;
        if let Text(str) = msg {
            trace!("{}", str);
            if !str.contains("auth_ok") {
                bail!("Invalid ws auth credentials: {:?}", str);
            }
        } else {
            bail!("Invalid ws handshake");
        }

        write
            .send(Text(
                r#"
{
  "id": 18,
  "type": "subscribe_events",
  "event_type": "state_changed"
}
        "#
                .to_string(),
            ))
            .await?;

        let msg = read.next().await.unwrap()?;
        if let Text(str) = msg {
            trace!("{}", str);
            if !str.contains("success\":true") {
                bail!("Failed to subscribe: {:?}", str);
            }
        } else {
            bail!("Unexpected ws message type: {:?}", msg);
        }

        let (tx, rx) = tokio::sync::mpsc::channel(10);

        tokio::spawn(async move {
            read.for_each(|msg| async {
                let msg = match msg {
                    Ok(v) => match v.into_text() {
                        Ok(v) => v,
                        Err(e) => {
                            debug!("Failed to convert WS message into text: {}", e);
                            return;
                        }
                    },
                    Err(e) => {
                        debug!("Failed to read WS message: {}", e);
                        return;
                    }
                };
                let ha_msg: HaMessage = match serde_json::from_str(&msg) {
                    Ok(v) => v,
                    Err(e) => {
                        error!("Failed to unmarshal HA state update: {}\n{}", e, msg);
                        return;
                    }
                };
                trace!("State update event: {:?}", ha_msg);
                tx.send(ha_msg.event).await.unwrap();
            })
            .await;
        });

        Ok(rx)
    }
}
