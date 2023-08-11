use crate::HeaterMode;
use anyhow::bail;
use log::{debug, trace};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Ebusd {
    endpoint: String,
    connection: TcpStream,
}

impl Ebusd {
    pub async fn new(endpoint: String) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(endpoint.clone()).await?;

        Ok(Self {
            endpoint,
            connection: stream,
        })
    }
    pub async fn define_message(&mut self, message_definition: String) -> anyhow::Result<()> {
        self.connection
            .write_all(format!("define -r {}\n\n", message_definition).as_bytes())
            .await?;

        let mut buffer = [0; 1024];
        let bytes_read = self.connection.read(&mut buffer).await?;
        let result = String::from_utf8(Vec::from(&buffer[..bytes_read]))?;
        let result = result.trim();
        debug!("Define message: {}", result);
        if result.contains("done") {
            Ok(())
        } else {
            bail!("{}", result);
        }
    }

    pub async fn set_mode(&mut self, mode: HeaterMode) -> anyhow::Result<()> {
        let arg = mode.into_cmd_arg();
        trace!("Setting mode {}", arg);
        self.connection
            .write_all(format!("w -c bai SetModeOverride {}\n\n", arg).as_bytes())
            .await?;

        let mut buffer = [0; 1024];
        let bytes_read = self.connection.read(&mut buffer).await?;
        let result = String::from_utf8(Vec::from(&buffer[..bytes_read]))?;
        let result = result.trim();
        debug!("Set mode result: {}", result);
        if result.contains("done") {
            Ok(())
        } else {
            bail!("{}", result);
        }
    }
}
