#![recursion_limit="1024"]

use failure::{Error, format_err};

use futures::{Future, select, Sink, SinkExt, Stream, StreamExt};
use futures::channel::oneshot;
use futures::io::{AsyncRead, AsyncWrite};
use futures::stream::FusedStream;

use async_std::future::timeout;
use async_std::sync::{channel, Sender, Receiver};
use async_std::task;

use async_tungstenite::{connect_async, WebSocketStream};

use log::{info, trace};

use tungstenite::error::{Error as WsError, Result as WsResult};
use tungstenite::protocol::Message;

use serde_json::{self, json};

use std::time::Duration;

use url::Url;

// use wakey::WolPacket;

mod scan;
pub use scan::scan;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const PAIRING: &'static str = include_str!("pairing.json");

const AUDIO_URI: &str = "ssap://audio";
const SYSTEM_URI: &str = "ssap://system";
const TV_URI: &str = "ssap://tv";

fn mk_uri(base: &str, cmd: &str) -> String {
    format!("{}/{}", base, cmd)
}

#[derive(Debug)]
pub enum TvCmd {
    // WaitConn(oneshot::Sender<Result<(), Error>>),
    TurnOff(oneshot::Sender<Result<(), Error>>),
    OpenChannel(u8, oneshot::Sender<Result<(), Error>>),
    GetVolume(oneshot::Sender<Result<u8, Error>>),
    SetVolume(u8, oneshot::Sender<Result<(), Error>>),
    VolumeUp(oneshot::Sender<Result<(), Error>>),
    VolumeDown(oneshot::Sender<Result<(), Error>>),
    GetInputs(oneshot::Sender<Result<Vec<String>, Error>>),
    SwitchInput(String, oneshot::Sender<Result<(), Error>>),
}

type CmdChannelResult<T> = Result<Result<T, Error>, oneshot::Canceled>;

impl TvCmd {
    pub fn turn_off() -> (impl Future<Output = CmdChannelResult<()>>, TvCmd) {
        let (res_tx, res_rx) = oneshot::channel();
        (res_rx, TvCmd::TurnOff(res_tx))
    }

    pub fn open_channel(num: u8) -> (impl Future<Output = CmdChannelResult<()>>, TvCmd) {
        let (res_tx, res_rx) = oneshot::channel();
        (res_rx, TvCmd::OpenChannel(num, res_tx))
    }

    pub fn get_volume() -> (impl Future<Output = CmdChannelResult<u8>>, TvCmd) {
        let (res_tx, res_rx) = oneshot::channel();
        (res_rx, TvCmd::GetVolume(res_tx))
    }

    pub fn set_volume(level: u8) -> (impl Future<Output = CmdChannelResult<()>>, TvCmd) {
        let (res_tx, res_rx) = oneshot::channel();
        (res_rx, TvCmd::SetVolume(level, res_tx))
    }

    pub fn volume_up() -> (impl Future<Output = CmdChannelResult<()>>, TvCmd) {
        let (res_tx, res_rx) = oneshot::channel();
        (res_rx, TvCmd::VolumeUp(res_tx))
    }

    pub fn volume_down() -> (impl Future<Output = CmdChannelResult<()>>, TvCmd) {
        let (res_tx, res_rx) = oneshot::channel();
        (res_rx, TvCmd::VolumeDown(res_tx))
    }

    pub fn get_inputs() -> (impl Future<Output = CmdChannelResult<Vec<String>>>, TvCmd) {
        let (res_tx, res_rx) = oneshot::channel();
        (res_rx, TvCmd::GetInputs(res_tx))
    }

    pub fn switch_input(input: &str) -> (impl Future<Output = CmdChannelResult<()>>, TvCmd) {
        let (res_tx, res_rx) = oneshot::channel();
        (res_rx, TvCmd::SwitchInput(input.to_string(), res_tx))
    }

    pub(crate) fn prepare(&self, counter: u64) -> serde_json::Value {
        use TvCmd::*;
        use serde_json::*;

        let mut msg = json!({
            "type": "request",
        });

        let (uri, cmd_name, payload) = match self {
            TurnOff(_) => {
                (SYSTEM_URI, "turnOff", None)
            }
            OpenChannel(channel, _) => {
                (TV_URI, "openChannel", Some(json!({"channelNumber": channel})))
            }
            GetInputs(_) => {
                (TV_URI, "getExternalInputList", None)
            }
            SwitchInput(input, _) => {
                (TV_URI, "switchInput", Some(json!({"inputId": input})))
            }
            GetVolume(_) => {
                (AUDIO_URI, "getVolume", None)
            }
            SetVolume(level, _) => {
                (AUDIO_URI, "setVolume", Some(json!({"volume": level})))
            }
            VolumeUp(_) => {
                (AUDIO_URI, "volumeUp", None)
            }
            VolumeDown(_) => {
                (AUDIO_URI, "volumeDown", None)
            }
        };

        msg["id"] = Value::String(format!("{}-{}", cmd_name, counter));
        msg["uri"] = Value::String(mk_uri(uri, cmd_name));
        if let Some(payload) = payload {
            msg["payload"] = payload;
        }
        msg
    }

    pub(crate) async fn process(self, resp: Result<serde_json::Value, Error>) {
        use TvCmd::*;

        match self {
            TurnOff(ch) |
            OpenChannel(_, ch) |
            SwitchInput(_, ch) |
            SetVolume(_, ch) |
            VolumeUp(ch) |
            VolumeDown(ch) => {
                ch.send(Ok(())).ok();
            }
            GetInputs(ch) => {
                let inputs = resp
                    .and_then(|r| {
                        r["payload"]["devices"].as_array()
                            .ok_or(format_err!("Invalid response"))
                            .map(|values| {
                                values.iter()
                                    .filter_map(|v| v["id"].as_str().map(|id| id.to_string()))
                                    .collect()
                            })
                    });
                ch.send(inputs).ok();
            }
            GetVolume(ch) => {
                let level = resp
                    .and_then(|r| {
                        r["payload"]["volume"].as_u64()
                            .ok_or(format_err!("Invalid response"))
                    })
                    .map(|l| l as u8);
                ch.send(level).ok();
            }
        }
    }
}

struct PersistentConn {
    url: Url,
    client_key: String,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
}

impl PersistentConn {
    async fn start(
        url: Url,
        client_key: String,
        connect_timeout: Option<Duration>,
        read_timeout: Option<Duration>,
        wait_connection: bool,
        cmd_rx: Receiver<TvCmd>,
    ) -> Arc<AtomicBool> {
        let mut cmd_rx = cmd_rx.fuse();

        let conn = PersistentConn {
            url,
            client_key,
            connect_timeout,
            read_timeout,
        };

        let (mut wait_conn_tx, wait_conn_rx) = if wait_connection {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        let is_connected = Arc::new(AtomicBool::new(false));
        let ret = is_connected.clone();

        trace!("Starting persistent connection ...");
        task::spawn(async move {
            loop {
                trace!("Connecting to {} ...", &conn.url);
                let ws_stream = match conn.connect().await {
                    Ok(ws_stream) => {
                        is_connected.store(true, Ordering::Relaxed);
                        wait_conn_tx.take().map(|tx| tx.send(()).ok());
                        ws_stream
                    },
                    Err(e) => {
                        info!("Connection error: {}", e);
                        task::sleep(Duration::from_secs(1)).await;
                        continue;
                    },
                };
                let (ws_tx, _ws_rx) = ws_stream.split();
                let ws_rx = _ws_rx.fuse();
                cmd_rx = match Conn::start(ws_tx, ws_rx, cmd_rx).await {
                    ConnExitStatus::Reconnect(cmd_rx) => {
                        trace!("Reconnecting ...");
                        is_connected.store(false, Ordering::Relaxed);
                        cmd_rx
                    },
                    ConnExitStatus::Finish => break,
                }
            }
        });

        if let Some(wait_conn_rx) = wait_conn_rx {
            wait_conn_rx.await.ok();
        }

        ret
    }

    async fn connect(
        &self,
    ) -> Result<WebSocketStream<impl AsyncRead + AsyncWrite + Unpin>, Error> {
        let (mut ws_stream, _) = if let Some(conn_timeout) = self.connect_timeout {
            timeout(conn_timeout, connect_async(self.url.clone())).await??
        } else {
            connect_async(self.url.clone()).await?
        };
        self.pair(&mut ws_stream).await?;
        Ok(ws_stream)
    }

    async fn pair(
        &self,
        ws_stream: &mut WebSocketStream<impl AsyncRead + AsyncWrite + Unpin>,
    ) -> Result<(), Error> {
        let mut pairing_json: serde_json::Value = serde_json::from_str(PAIRING)?;
        pairing_json["payload"].as_object_mut()
            .ok_or(format_err!("Corrupted pairing.json file"))?
            .insert("client-key".to_string(), serde_json::Value::String(self.client_key.clone()));
        let pairing_msg = Message::text(pairing_json.to_string());

        ws_stream.send(pairing_msg).await?;
        let pairing_resp = if let Some(read_timeout) = self.read_timeout {
            timeout(read_timeout, ws_stream.next()).await?
        } else {
            ws_stream.next().await
        };
        if let Some(pairing_resp) = pairing_resp {
            // TODO: parse pairing response
            pairing_resp.map(|_| ())
                .map_err(|e| format_err!("Error when pairing: {}", e))
        } else {
            Err(format_err!("Connection was closed"))
        }
    }
}

enum ConnExitStatus<T> {
    Reconnect(T),
    Finish,
}

struct Conn<S, R>
    where S: Sink<Message>,
          R: Stream<Item = WsResult<Message>>,
{
    ws_tx: S,
    ws_rx: R,
    cmd_count: u64,
    waiting_cmds: HashMap<String, TvCmd>,
}

impl<S, R> Conn<S, R>
    where S: Sink<Message> + Send + Unpin + 'static,
          R: Stream<Item = WsResult<Message>> + FusedStream + Send + Unpin + 'static,
{
    fn start<CMD>(
        ws_tx: S, ws_rx: R, mut cmd_rx: CMD
    ) -> task::JoinHandle<ConnExitStatus<CMD>>
        where CMD: Stream<Item = TvCmd> + FusedStream + Send + Unpin + 'static
    {
        let mut conn = Conn {
            ws_tx,
            ws_rx,
            cmd_count: 0,
            waiting_cmds: HashMap::new(),
        };

        task::spawn(async move {
            loop {
                select! {
                    cmd = cmd_rx.next() => {
                        trace!("Received command: {:?}", &cmd);
                        match cmd {
                            Some(cmd) => {
                                conn.process_cmd(cmd).await;
                            }
                            None => {
                                trace!("Finish processing");
                                break ConnExitStatus::Finish;
                            }
                        }
                    }
                    msg = conn.ws_rx.next() => {
                        trace!("Received message: {:?}", msg);
                        match msg {
                            Some(Ok(msg)) => {
                                conn.process_msg(msg).await;
                            }
                            Some(Err(WsError::ConnectionClosed)) => {
                                trace!("Connection closed");
                                break ConnExitStatus::Reconnect(cmd_rx);
                            }
                            Some(Err(_)) => {
                                // TODO error processing
                                break ConnExitStatus::Reconnect(cmd_rx);
                            }
                            None => {
                                trace!("Websocket dropped");
                                break ConnExitStatus::Finish;
                            }
                        }
                    }
                };
            }
        })
    }

    async fn process_cmd(&mut self, cmd: TvCmd) {
        let json_cmd = cmd.prepare(self.cmd_count);
        self.cmd_count += 1;
        let msg_id = json_cmd["id"].as_str().unwrap();
        self.waiting_cmds.insert(msg_id.to_string(), cmd);
        let send_res = self.ws_tx.send(
            dbg!(Message::text(json_cmd.to_string()))
        ).await;
        if let Err(_) = send_res {
            self.waiting_cmds.remove(msg_id);
        }
    }

    async fn process_msg(&mut self, msg: Message) {
        match msg {
            Message::Text(resp) => {
                let fut = serde_json::from_str::<serde_json::Value>(&resp)
                    .ok()
                    .and_then(|resp| {
                        resp["id"].as_str()
                            .and_then(|id| {
                                self.waiting_cmds.remove(id)
                            })
                            .map(|cmd| async {
                                cmd.process(Ok(resp)).await;
                            })
                    })
                    .map(|fut| fut);
                if let Some(fut) = fut {
                    fut.await;
                }
            }
            Message::Ping(data) => {
                self.ws_tx.send(Message::Pong(data)).await.ok();
            }
            msg => {
                trace!("Unsupported websocket message type: {}", msg);
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct LgtvBuilder {
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    wait_connection: bool,
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

    pub fn wait_connection(&mut self, wait_conn: bool) -> &mut LgtvBuilder {
        self.wait_connection = wait_conn;
        self
    }

    pub async fn connect(self, url: &str, client_key: &str) -> Result<Lgtv, Error> {
        let url = Url::parse(url)?;
        let (cmd_tx, cmd_rx) = channel::<TvCmd>(1);

        let is_connected = PersistentConn::start(
            url.clone(),
            client_key.to_string(),
            self.connect_timeout,
            self.read_timeout,
            self.wait_connection,
            cmd_rx
        ).await;

        Ok(Lgtv {
            _url: url,
            read_timeout: self.read_timeout,
            cmd_tx,
            is_connected,
        })
    }
}

pub struct Lgtv {
    _url: Url,
    read_timeout: Option<Duration>,
    cmd_tx: Sender<TvCmd>,
    is_connected: Arc<AtomicBool>,
}

impl Lgtv {
    pub fn builder() -> LgtvBuilder {
        LgtvBuilder::default()
    }

//    pub fn close(self) {}

    pub async fn turn_off(&mut self) -> Result<(), Error> {
        self.send_cmd(TvCmd::turn_off()).await
    }

    pub async fn open_channel(&mut self, num: u8) -> Result<(), Error> {
        self.send_cmd(TvCmd::open_channel(num)).await
    }

    pub async fn get_inputs(&mut self) -> Result<Vec<String>, Error> {
        self.send_cmd(TvCmd::get_inputs()).await
    }

    pub async fn switch_input(&mut self, input: &str) -> Result<(), Error> {
        self.send_cmd(TvCmd::switch_input(input)).await
    }

    pub async fn get_volume(&mut self) -> Result<u8, Error> {
        self.send_cmd(TvCmd::get_volume()).await
    }

    pub async fn set_volume(&mut self, level: u8) -> Result<(), Error> {
        self.send_cmd(TvCmd::set_volume(level)).await
    }

    pub async fn volume_up(&mut self) -> Result<(), Error> {
        self.send_cmd(TvCmd::volume_up()).await
    }

    pub async fn volume_down(&mut self) -> Result<(), Error> {
        self.send_cmd(TvCmd::volume_down()).await
    }

    async fn send_cmd<T>(
        &mut self,
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
    use std::time::Duration;
    use crate::Lgtv;

    const URL: &str = "ws://192.168.2.3:3000";
    const CLIENT_KEY: &str = "0b85eb0d4f4a9a5b29e2f32c2f469eb5";

    async fn connect(url: &str) -> Lgtv {
        env_logger::init();
        Lgtv::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(1))
            .wait_connection(true)
            .connect(url, CLIENT_KEY).await
            .expect(&format!("Cannot connect to {}", url))
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
        let mut tv = connect(URL).await;
        println!("Sending command ...");
        let inputs = tv.get_inputs().await.expect("Error when sending a command");
        println!("{:?}", inputs);
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
