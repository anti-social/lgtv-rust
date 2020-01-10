use failure::{Error, format_err};

use futures::Future;
use futures::channel::oneshot;

use serde_json::{self, json};

const AUDIO_URI: &str = "ssap://audio";
const SYSTEM_URI: &str = "ssap://system";
const TV_URI: &str = "ssap://tv";
const NETWORK_INPUT_URI: &str = "ssap://com.webos.service.networkinput";

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

    GetPointerInputSocket(oneshot::Sender<Result<serde_json::Value, Error>>),
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

    pub fn get_pointer_input_socket() -> (impl Future<Output = CmdChannelResult<serde_json::Value>>, TvCmd) {
        let (res_tx, res_rx) = oneshot::channel();
        (res_rx, TvCmd::GetPointerInputSocket(res_tx))
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
            GetPointerInputSocket(_) => {
                (NETWORK_INPUT_URI, "getPointerInputSocket", None)
            }
        };

        msg["id"] = Value::String(format!("{}-{}", cmd_name, counter));
        msg["uri"] = Value::String(mk_uri(uri, cmd_name));
        if let Some(payload) = payload {
            msg["payload"] = payload;
        }
        msg
    }

    pub(crate) fn process(self, resp: Result<serde_json::Value, Error>) {
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
            GetPointerInputSocket(ch) => {
                ch.send(resp).ok();
            }
        }
    }
}
