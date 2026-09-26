use anyhow::{Context, anyhow, bail};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, trace, warn};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::select;
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio::time::{Duration, Instant, interval, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PING_INTERVAL: Duration = Duration::from_secs(30);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Deserialize, Debug)]
pub struct State {
    pub state: Value,
}

pub struct Api {
    url: String,
    ws_url: String,
    bearer_token: String,
    client: Client,
}

impl Api {
    pub fn new(url: String, ws_url: String, bearer_token: String) -> Self {
        Self {
            url,
            ws_url,
            bearer_token,
            client: Client::new(),
        }
    }

    /// Returns the current state of an entity, or None if it does not exist.
    pub async fn get_state(&self, entity_id: &str) -> anyhow::Result<Option<State>> {
        let res = self
            .client
            .get(format!("{}/api/states/{}", self.url, entity_id))
            .bearer_auth(&self.bearer_token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .context("requesting entity state from HA")?;
        if res.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = res.error_for_status()?.bytes().await?;
        Ok(Some(serde_json::from_slice(&body)?))
    }

    /// Streams state changes of a single entity. The channel closes when the websocket
    /// connection to HA is lost or stops answering pings.
    pub async fn state_updates(&self, entity_id: String) -> anyhow::Result<Receiver<State>> {
        let url = format!("{}/websocket", self.ws_url.replace("http", "ws"));
        let (ws, _) = timeout(REQUEST_TIMEOUT, connect_async(url))
            .await
            .context("connecting to HA websocket timed out")??;
        let (mut write, mut read) = ws.split();

        let msg = next_json(&mut read).await?;
        if msg["type"] != "auth_required" {
            bail!("Invalid ws handshake: {}", msg);
        }

        write
            .send(Message::text(
                json!({"type": "auth", "access_token": self.bearer_token}).to_string(),
            ))
            .await?;
        let msg = next_json(&mut read).await?;
        if msg["type"] != "auth_ok" {
            bail!("Invalid ws auth credentials: {}", msg);
        }

        write
            .send(Message::text(
                json!({"id": 1, "type": "subscribe_events", "event_type": "state_changed"})
                    .to_string(),
            ))
            .await?;
        let msg = next_json(&mut read).await?;
        if msg["success"] != true {
            bail!("Failed to subscribe: {}", msg);
        }

        let (tx, rx) = channel(10);
        tokio::spawn(async move {
            if let Err(e) = pump(write, read, entity_id, tx).await {
                error!("HA websocket connection lost: {:#}", e);
            }
        });

        Ok(rx)
    }
}

async fn next_json(read: &mut SplitStream<Ws>) -> anyhow::Result<Value> {
    loop {
        let msg = timeout(REQUEST_TIMEOUT, read.next())
            .await
            .context("timed out waiting for HA websocket")?
            .ok_or_else(|| anyhow!("HA websocket closed"))??;
        if let Message::Text(text) = msg {
            trace!("{}", text);
            return Ok(serde_json::from_str(text.as_str())?);
        }
    }
}

async fn pump(
    mut write: SplitSink<Ws, Message>,
    mut read: SplitStream<Ws>,
    entity_id: String,
    tx: Sender<State>,
) -> anyhow::Result<()> {
    let mut ping = interval(PING_INTERVAL);
    let mut ping_id: u64 = 1;
    let mut last_seen = Instant::now();

    loop {
        select! {
            msg = read.next() => {
                let msg = msg.ok_or_else(|| anyhow!("connection closed"))??;
                last_seen = Instant::now();
                let text = match msg {
                    Message::Text(text) => text,
                    Message::Close(frame) => bail!("closed by HA: {:?}", frame),
                    _ => continue,
                };
                // we get every state change in HA, skip the ones that can't be ours before parsing
                if !text.as_str().contains(entity_id.as_str()) {
                    continue;
                }
                let value: Value = match serde_json::from_str(text.as_str()) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Failed to parse HA websocket message: {}\n{}", e, text);
                        continue;
                    }
                };
                if value["type"] != "event" {
                    trace!("HA websocket message: {}", value);
                    continue;
                }
                let new_state = &value["event"]["data"]["new_state"];
                if new_state["entity_id"] != entity_id.as_str() {
                    continue;
                }
                match State::deserialize(new_state) {
                    Ok(state) => {
                        if tx.send(state).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(e) => warn!("Failed to parse state of {}: {}", entity_id, e),
                }
            }
            _ = ping.tick() => {
                if last_seen.elapsed() > PING_INTERVAL * 3 {
                    bail!("no messages from HA for {:?}", last_seen.elapsed());
                }
                ping_id += 1;
                debug!("Sending HA websocket ping");
                write.send(Message::text(json!({"id": ping_id, "type": "ping"}).to_string())).await?;
            }
            _ = tx.closed() => return Ok(()),
        }
    }
}
