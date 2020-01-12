use failure::{Error, format_err};

use futures::Future;
use futures::channel::oneshot;

use serde_json::{self, json};

use std::fmt;

use url::Url;

const AUDIO_URI: &str = "ssap://audio";
const SYSTEM_URI: &str = "ssap://system";
const TV_URI: &str = "ssap://tv";
const NETWORK_INPUT_URI: &str = "ssap://com.webos.service.networkinput";

fn mk_uri(base: &str, cmd: &str) -> String {
    format!("{}/{}", base, cmd)
}

#[derive(Debug)]
pub enum TvCmd {
    Register(String, oneshot::Sender<Result<(), Error>>),
    TurnOff(oneshot::Sender<Result<(), Error>>),
    OpenChannel(u8, oneshot::Sender<Result<(), Error>>),
    GetVolume(oneshot::Sender<Result<u8, Error>>),
    SetVolume(u8, oneshot::Sender<Result<(), Error>>),
    VolumeUp(oneshot::Sender<Result<(), Error>>),
    VolumeDown(oneshot::Sender<Result<(), Error>>),
    GetInputs(oneshot::Sender<Result<Vec<String>, Error>>),
    SwitchInput(String, oneshot::Sender<Result<(), Error>>),

    GetPointerInputSocket(oneshot::Sender<Result<Url, Error>>),
}

type CmdChannelResult<T> = Result<Result<T, Error>, oneshot::Canceled>;

impl TvCmd {
    pub fn register(client_key: String) -> (impl Future<Output = CmdChannelResult<()>>, TvCmd) {
        let (res_tx, res_rx) = oneshot::channel();
        (res_rx, TvCmd::Register(client_key, res_tx))
    }

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

    pub fn get_pointer_input_socket() -> (impl Future<Output = CmdChannelResult<Url>>, TvCmd) {
        let (res_tx, res_rx) = oneshot::channel();
        (res_rx, TvCmd::GetPointerInputSocket(res_tx))
    }

    pub(crate) fn prepare(&self, counter: u64) -> serde_json::Value {
        use TvCmd::*;
        use serde_json::*;

        let (req_type, uri, cmd_name, payload) = match self {
            Register(client_key, _) => {
                let mut payload: Value = from_str(include_str!("pairing.json"))
                    .expect("Invalid pairing payload");
                payload.as_object_mut()
                    .expect("Invalid pairing payload")
                    .insert("client-key".to_string(), Value::String(client_key.clone()));
                ("register", None, "register", Some(payload))
            }
            TurnOff(_) => {
                ("request", Some(SYSTEM_URI), "turnOff", None)
            }
            OpenChannel(channel, _) => {
                ("request", Some(TV_URI), "openChannel", Some(json!({"channelNumber": channel})))
            }
            GetInputs(_) => {
                ("request", Some(TV_URI), "getExternalInputList", None)
            }
            SwitchInput(input, _) => {
                ("request", Some(TV_URI), "switchInput", Some(json!({"inputId": input})))
            }
            GetVolume(_) => {
                ("request", Some(AUDIO_URI), "getVolume", None)
            }
            SetVolume(level, _) => {
                ("request", Some(AUDIO_URI), "setVolume", Some(json!({"volume": level})))
            }
            VolumeUp(_) => {
                ("request", Some(AUDIO_URI), "volumeUp", None)
            }
            VolumeDown(_) => {
                ("request", Some(AUDIO_URI), "volumeDown", None)
            }
            GetPointerInputSocket(_) => {
                ("request", Some(NETWORK_INPUT_URI), "getPointerInputSocket", None)
            }
        };

        let mut msg = json!({
            "id": Value::String(format!("{}_{}", cmd_name, counter)),
            "type": req_type,
        });
        if let Some(uri) = uri {
            msg["uri"] = Value::String(mk_uri(uri, cmd_name));
        }
        if let Some(payload) = payload {
            msg["payload"] = payload;
        }
        println!("Sending message: {:?}", &msg);
        msg
    }

    pub(crate) fn process(self, resp: Result<serde_json::Value, Error>) {
        use TvCmd::*;

        match self {
            Register(_, ch) |
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
            GetPointerInputSocket(ch) => {
                let pointer_url = resp
                    .and_then(|r| {
                        r["payload"]["socketPath"].as_str()
                            .map(|s| s.to_string())
                            .ok_or(format_err!("Invalid response"))
                    })
                    .and_then(|s| Url::parse(&s).map_err(|e| format_err!("Cannot parse url: {}", e)));
                ch.send(pointer_url).ok();
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum PointerCmd {
    Button(ButtonKey),
    MouseMove { dx: f64, dy: f64, drag: bool },
    Scroll { dx: f64, dy: f64 },
}

#[derive(Debug)]
pub enum ButtonKey {
    Back,
}

impl fmt::Display for ButtonKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ButtonKey::*;

        match self {
            Back => write!(f, "BACK"),
        }
    }
}

impl PointerCmd {
    pub fn prepare(&self) -> String {
        use PointerCmd::*;

        match self {
            Button(key) => {
                format!("type:button\nname:{}\n\n", key)
            }
            MouseMove {dx, dy, drag} => {
                format!(
                    "type:move\ndx:{}\ndy:{}\ndown:{}\n\n",
                    dx, dy, if *drag { 1 } else { 0 }
                )
            }
            Scroll {dx, dy} => {
                format!("type:scroll\ndx:{}\ndy:{}\n\n", dx, dy)
            }
        }
    }
}
