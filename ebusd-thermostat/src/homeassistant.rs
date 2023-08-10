use anyhow::bail;
use futures_util::{SinkExt, StreamExt};
use log::trace;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use time::OffsetDateTime;
use tokio::sync::mpsc::Receiver;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message::Text;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(transparent)]
pub struct Namespaces {
    namespaces: Vec<Devices>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Devices {
    #[serde(flatten)]
    devices: HashMap<String, Device>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Device {
    name: String,
    entities: Vec<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HaMessage {
    id: u8,
    #[serde(alias = "type")]
    msg_type: String,
    event: Event,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Event {
    event_type: String,
    data: EventData,
    origin: String,
    #[serde(with = "time::serde::rfc3339")]
    time_fired: OffsetDateTime,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EventData {
    old_state: State,
    new_state: State,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct State {
    entity_id: String,
    state: String,
    attributes: HashMap<String, String>,
    #[serde(with = "time::serde::rfc3339")]
    last_changed: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    last_updated: OffsetDateTime,
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
            .post(Url::from_str(&*format!(
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
        let url = Url::parse(&*format!("ws://{}/api/websocket", self.url.clone()))?;
        let (ws, _) = connect_async(url).await?;
        let (mut write, mut read) = ws.split();
        // await auth required message
        let msg = read.next().await.unwrap()?;
        if let Text(str) = msg {
            trace!("{}", str);
            if !str.contains("auth_required") {
                bail!("invalid ws handshake");
            }
        } else {
            bail!("invalid ws handshake");
        }

        write
            .send(Text(
                r#"
{
  "type": "auth",
  "access_token": "blurg"
}
        "#
                .replace("blurg", &*self.bearer_token),
            ))
            .await?;

        let msg = read.next().await.unwrap()?;
        if let Text(str) = msg {
            trace!("{}", str);
            if !str.contains("auth_ok") {
                bail!("invalid ws auth credentials: {:?}", str);
            }
        } else {
            bail!("invalid ws handshake");
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
                bail!("failed to subscribe: {:?}", str);
            }
        } else {
            bail!("unexpected ws message type: {:?}", msg);
        }

        let (tx, rx) = tokio::sync::mpsc::channel(10);

        read.for_each(|msg| async {
            let msg = msg.unwrap().into_text().unwrap();
            let ha_msg: HaMessage = serde_json::from_str(&*msg).unwrap();
            trace!("{:?}", ha_msg);
            tx.send(ha_msg.event).await.unwrap();
        })
        .await;

        Ok(rx)
    }
}
