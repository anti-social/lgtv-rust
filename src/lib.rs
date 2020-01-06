#![recursion_limit="1024"]

use failure::{Error, format_err};

use futures::{Future, select, SinkExt, StreamExt};
use futures::sink::Sink;
use futures::channel::oneshot;

use async_std::future::timeout;
use async_std::net::TcpStream;
use async_std::sync::{channel, Sender};
use async_std::task;

use async_tls::client::TlsStream;

use async_tungstenite::{connect_async, WebSocketStream};
use async_tungstenite::stream::Stream;

use tungstenite::error::{Error as WSError};
use tungstenite::protocol::Message;

use serde_json::{self, json};

use std::time::Duration;

use url::Url;

//use wakey::WolPacket;

mod scan;
pub use scan::scan;
use std::collections::HashMap;

const PAIRING: &'static str = include_str!("pairing.json");
// const MAC_ADDR: &'static str = "cc:2d:8c:cf:c4:3e";

const AUDIO_URI: &str = "ssap://audio";
const SYSTEM_URI: &str = "ssap://system";
const TV_URI: &str = "ssap://tv";

fn mk_uri(base: &str, cmd: &str) -> String {
    format!("{}/{}", base, cmd)
}

#[derive(Debug)]
pub enum TvCmd {
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

pub struct Lgtv {
    _url: Url,
    read_timeout: Option<Duration>,
    cmd_tx: Sender<TvCmd>,
//    ws_tx: SplitSink<WebSocketStream<Stream<TcpStream, TlsStream<TcpStream>>>, Message>,
//    ws_resp_tx: Sender<(String, oneshot::Sender<Option<Result<serde_json::Value, Error>>>)>
}

#[derive(Clone, Copy)]
pub struct LgtvBuilder {
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
}

struct CmdContext<S: Sink<Message>> {
    waiting_cmds: HashMap<String, TvCmd>,
    cmd_count: u64,
    ws_tx: S,
}

impl<S: Sink<Message> + Unpin> CmdContext<S> {
    fn new(ws_tx: S) -> CmdContext<S> {
         CmdContext {
             waiting_cmds: HashMap::new(),
             cmd_count: 0,
             ws_tx,
         }
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

    async fn process_msg(&mut self, msg: Result<Message, WSError>) {
        match msg {
            Ok(Message::Text(resp)) => {
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
            Ok(Message::Ping(data)) => {
                self.ws_tx.send(Message::Pong(data)).await.ok();
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
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

    async fn pair(
        &self,
        ws_stream: &mut WebSocketStream<Stream<TcpStream, TlsStream<TcpStream>>>,
        client_key: &str,
    ) -> Result<(), Error> {
        let mut pairing_json: serde_json::Value = serde_json::from_str(PAIRING)?;
        pairing_json["payload"].as_object_mut()
            .ok_or(format_err!("Corrupted pairing.json file"))?
            .insert("client-key".to_string(), serde_json::Value::String(client_key.to_string()));
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

    pub async fn connect(self, url: &str, client_key: &str) -> Result<Lgtv, Error> {
        let url = Url::parse(url)?;
        let (mut ws_stream, _) = if let Some(connect_timeout) = self.connect_timeout {
            timeout(connect_timeout, connect_async(url.clone())).await??
        } else {
            connect_async(url.clone()).await?
        };
        self.pair(&mut ws_stream, client_key).await?;

        let (ws_tx, _ws_rx) = ws_stream.split();
        let mut ws_rx = _ws_rx.fuse();

        let (cmd_tx, _cmd_rx) = channel::<TvCmd>(1);
        let mut cmd_rx = _cmd_rx.fuse();

        task::spawn(async move {
            let mut ctx = CmdContext::new(ws_tx);
            loop {
                select! {
                    cmd = cmd_rx.next() => {
                        dbg!(&cmd);
                        match cmd {
                            Some(cmd) => {
                                ctx.process_cmd(cmd).await;
                            }
                            None => {
                                dbg!("Stop processing");
                                break;
                            }
                        }
                    }
                    msg = ws_rx.next() => {
                        dbg!(&msg);
                        match msg {
                            Some(msg) => {
                                ctx.process_msg(msg).await;
                            }
                            None => {
                                dbg!("Websocket closed");
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(Lgtv {
            _url: url,
            read_timeout: self.read_timeout,
            cmd_tx,
        })
    }
}

impl Lgtv {
    pub fn builder() -> LgtvBuilder {
        LgtvBuilder {
            connect_timeout: None,
            read_timeout: None,
        }
    }

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
        let (cmd_res_rx, cmd) = rx_fut_and_cmd;
        self.cmd_tx.send(cmd).await;
        let res = cmd_res_rx.await;
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
        Lgtv::builder()
            .connect_timeout(Duration::from_secs(1))
            .read_timeout(Duration::from_secs(1))
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
        let mut tv = connect(URL).await;
        let inputs = tv.get_inputs().await.expect("Error when sending a command");
        println!("{:?}", inputs);
    }

    #[async_std::test]
    async fn switch_input() {
        let mut tv = connect(URL).await;
        tv.switch_input("HDMI_3").await.expect("Cannot send command");
    }

//    #[test]
//    fn volume_up() {
//        let mut tv = connect(URL);
//        tv.volume_up().expect("Cannot send command");
//    }
}
