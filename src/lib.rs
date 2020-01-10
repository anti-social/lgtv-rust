#![recursion_limit="1024"]

use failure::{Error, format_err};

use futures::Future;
use futures::channel::oneshot;

use async_std::future::timeout;
use async_std::sync::{channel, Sender};

use std::time::Duration;

use url::Url;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// use wakey::WolPacket;

mod cmd;
pub use cmd::TvCmd;
mod conn;
use conn::PersistentConn;
mod scan;
pub use scan::scan;


#[derive(Clone, Copy, Default)]
pub struct LgtvBuilder {
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
}

impl LgtvBuilder {
    pub fn connect_timeout(&mut self, timeout: Duration) -> &mut LgtvBuilder {
        self.connect_timeout = Some(timeout);
        self
    }

    pub fn read_timeout(&mut self, timeout: Duration) -> &mut LgtvBuilder {
        self.read_timeout = Some(timeout);
        self
    }

    pub async fn connect(self, url: &str, client_key: &str) -> Result<Lgtv, Error> {
        let url = Url::parse(url)?;
        let (cmd_tx, cmd_rx) = channel::<TvCmd>(1);

        let (wait_conn_tx, wait_conn_rx) = channel(1);

        let is_connected = PersistentConn::start(
            url.clone(),
            client_key.to_string(),
            self.connect_timeout,
            self.read_timeout,
            wait_conn_rx,
            cmd_rx
        ).await;

        Ok(Lgtv {
            _url: url,
            read_timeout: self.read_timeout,
            wait_conn_tx,
            cmd_tx,
            is_connected,
        })
    }
}

pub struct Lgtv {
    _url: Url,
    read_timeout: Option<Duration>,
    wait_conn_tx: Sender<oneshot::Sender<()>>,
    cmd_tx: Sender<TvCmd>,
    is_connected: Arc<AtomicBool>,
}

impl Lgtv {
    pub fn builder() -> LgtvBuilder {
        LgtvBuilder::default()
    }

//    pub fn close(self) {}

    pub async fn wait_conn(&self) {
        let (conn_tx, conn_rx) = oneshot::channel();
        self.wait_conn_tx.send(conn_tx).await;
        conn_rx.await.ok();
    }

    pub async fn turn_off(&self) -> Result<(), Error> {
        self.send_cmd(TvCmd::turn_off()).await
    }

    pub async fn open_channel(&self, num: u8) -> Result<(), Error> {
        self.send_cmd(TvCmd::open_channel(num)).await
    }

    pub async fn get_inputs(&self) -> Result<Vec<String>, Error> {
        self.send_cmd(TvCmd::get_inputs()).await
    }

    pub async fn switch_input(&self, input: &str) -> Result<(), Error> {
        self.send_cmd(TvCmd::switch_input(input)).await
    }

    pub async fn get_volume(&self) -> Result<u8, Error> {
        self.send_cmd(TvCmd::get_volume()).await
    }

    pub async fn set_volume(&self, level: u8) -> Result<(), Error> {
        self.send_cmd(TvCmd::set_volume(level)).await
    }

    pub async fn volume_up(&self) -> Result<(), Error> {
        self.send_cmd(TvCmd::volume_up()).await
    }

    pub async fn volume_down(&self) -> Result<(), Error> {
        self.send_cmd(TvCmd::volume_down()).await
    }

    pub async fn get_pointer_input_socket(&self) -> Result<serde_json::Value, Error> {
        self.send_cmd(TvCmd::get_pointer_input_socket()).await
    }

    async fn send_cmd<T>(
        &self,
        rx_fut_and_cmd: (impl Future<Output = Result<Result<T, Error>, oneshot::Canceled>>, TvCmd)
    ) -> Result<T, Error> {
        if !self.is_connected.load(Ordering::Relaxed) {
            return Err(format_err!("Not connected"));
        }
        let (cmd_res_rx, cmd) = rx_fut_and_cmd;
        let res_fut = async {
            self.cmd_tx.send(cmd).await;
            cmd_res_rx.await
        };
        let res = if let Some(read_timeout) = self.read_timeout {
            timeout(read_timeout, res_fut).await?
        } else {
            res_fut.await
        };
        res.map_err(|e| format_err!("Canceled channel: {}", e))?
    }
}

#[cfg(test)]
mod tests {
    use async_std::future::timeout;
    use std::time::Duration;
    use crate::Lgtv;

//    const URL: &str = "ws://192.168.2.3:3000";
    const URL: &str = "ws://localhost:3000";
    const CLIENT_KEY: &str = "0b85eb0d4f4a9a5b29e2f32c2f469eb5";

    async fn connect(url: &str) -> Lgtv {
        env_logger::init();
        let tv = Lgtv::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(5))
            .connect(url, CLIENT_KEY).await
            .expect(&format!("Cannot connect to {}", url));
        timeout(Duration::from_secs(5), tv.wait_conn()).await.expect("Cannot connect");
        tv
    }

//    #[async_std::test]
//    async fn open_channel() {
//        let mut tv = connect(URL).await;
//        tv.open_channel(10).await.expect("Cannot send command");
//    }

//    #[async_std::test]
//    async fn volume_status() {
//        let mut tv = connect(URL).await;
//        let res = tv.volume_status().await.expect("Cannot send command");
//    }

//    #[async_std::test]
//    async fn volume_down() {
//        let mut tv = connect(URL).await;
//        tv.volume_down().await.expect("Cannot send command");
//    }

//    #[async_std::test]
//    async fn set_and_get_volume() {
//        let mut tv = connect(URL).await;
//        tv.set_volume(10).await.expect("Cannot send command");
//        let volume = tv.get_volume().await.expect("Cannot send command");
//        assert_eq!(volume, 10);
//        tv.close().await.expect("Error when closing the websocket");
//    }

    #[async_std::test]
    async fn get_inputs() {
        println!("Connecting ...");
        let tv = connect(URL).await;
        println!("Sending command ...");
        let inputs = tv.get_inputs().await.expect("Error when sending a command");
        println!("{:?}", inputs);
        let mouse_socket = tv.get_pointer_input_socket().await.unwrap();
        println!("{:?}", mouse_socket);
    }

//    #[async_std::test]
//    async fn switch_input() {
//        let mut tv = connect(URL).await;
//        tv.switch_input("HDMI_3").await.expect("Cannot send command");
//    }

//    #[test]
//    fn volume_up() {
//        let mut tv = connect(URL);
//        tv.volume_up().expect("Cannot send command");
//    }
}
