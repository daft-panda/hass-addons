use crate::HeaterSettings;
use anyhow::{Context, bail};
use log::{debug, info};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(15);

const SET_MODE_DEFINITION: &str = "wi,BAI,SetModeOverride,OperatingMode,,08,B510,00,hcmode,,UCH,,,,flowtempdesired,,D1C,,,,hwctempdesired,,D1C,,,,hwcflowtempdesired,,UCH,,,,setmode1,,UCH,,,,disablehc,,BI0,,,,disablehwctapping,,BI1,,,,disablehwcload,,BI2,,,,setmode2,,UCH,,,,remoteControlHcPump,,BI0,,,,releaseBackup,,BI1,,,,releaseCooling,,BI2";

/// Client for the ebusd TCP command port. The connection is opened lazily and dropped on any
/// error, so the next command transparently reconnects (and re-defines our custom message).
pub struct Ebusd {
    endpoint: String,
    connection: Option<BufReader<TcpStream>>,
}

impl Ebusd {
    pub fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            connection: None,
        }
    }

    pub async fn apply_settings(&mut self, settings: &HeaterSettings) -> anyhow::Result<()> {
        let arg = settings.to_cmd_arg();
        debug!("Setting mode {}", arg);
        let result = self
            .request(&format!("w -c bai SetModeOverride {}", arg))
            .await?;
        debug!("Set mode result: {}", result);
        let lower = result.to_lowercase();
        if lower.starts_with("err") || lower.contains("error") {
            bail!("Set mode {} failed: {}", arg, result);
        }
        Ok(())
    }

    async fn request(&mut self, cmd: &str) -> anyhow::Result<String> {
        let conn = match &mut self.connection {
            Some(conn) => conn,
            None => self.connection.insert(self.connect().await?),
        };
        let result = Self::command(conn, cmd).await;
        if result.is_err() {
            // the stream may be half-read or dead, start over on the next request
            self.connection = None;
        }
        result
    }

    async fn connect(&self) -> anyhow::Result<BufReader<TcpStream>> {
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(&self.endpoint))
            .await
            .with_context(|| format!("connecting to ebusd at {} timed out", self.endpoint))?
            .with_context(|| format!("connecting to ebusd at {}", self.endpoint))?;
        let mut conn = BufReader::new(stream);

        let result =
            Self::command(&mut conn, &format!("define -r {}", SET_MODE_DEFINITION)).await?;
        debug!("Define message: {}", result);
        if !result.contains("done") {
            bail!("ebusd rejected message definition: {}", result);
        }
        info!("Connected to ebusd at {}", self.endpoint);
        Ok(conn)
    }

    /// Sends a command and reads the response, which ebusd terminates with an empty line.
    async fn command(conn: &mut BufReader<TcpStream>, cmd: &str) -> anyhow::Result<String> {
        conn.get_mut()
            .write_all(format!("{}\n", cmd).as_bytes())
            .await
            .context("writing to ebusd")?;

        let mut response = String::new();
        loop {
            let mut line = String::new();
            let n = timeout(READ_TIMEOUT, conn.read_line(&mut line))
                .await
                .with_context(|| format!("ebusd read timed out after {:?}", READ_TIMEOUT))?
                .context("reading from ebusd")?;
            if n == 0 {
                bail!("ebusd closed the connection");
            }
            if line.trim().is_empty() {
                break;
            }
            response.push_str(&line);
        }
        Ok(response.trim().to_string())
    }
}
