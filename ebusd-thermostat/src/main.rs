mod homeassistant;

use crate::homeassistant::Api;
use rumqttc::{AsyncClient, MqttOptions, QoS};
use std::time::Duration;
use tokio::{task, time};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let api = Api::new("homeassistant:8123".to_string(), "".to_string());
    let devices = api.get_devices().await?;
    api.state_updates().await?;

    let mut mqttoptions = MqttOptions::new("ebusd-thermostat", "homeassistant", 1883);
    mqttoptions.set_keep_alive(Duration::from_secs(5));
    mqttoptions.set_credentials("mqtt", "mqtt");

    let (mut client, mut eventloop) = AsyncClient::new(mqttoptions, 10);
    client
        .subscribe("ebusd-thermostat/", QoS::AtMostOnce)
        .await
        .unwrap();

    task::spawn(async move {
        for i in 0..10 {
            client
                .publish("hello/rumqtt", QoS::AtLeastOnce, false, vec![i; i as usize])
                .await
                .unwrap();
            time::sleep(Duration::from_millis(100)).await;
        }
    });

    loop {
        let notification = eventloop.poll().await;
        match notification {
            Ok(notification) => println!("Received = {:?}", notification),
            Err(err) => println!("Err {:?}", err),
        }
    }
}
