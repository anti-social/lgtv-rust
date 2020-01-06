use failure::{Error, format_err};

use futures::{select, Sink, SinkExt, Stream, StreamExt};
use futures::channel::oneshot;
use futures::io::{AsyncRead, AsyncWrite};
use futures::stream::FusedStream;

use async_std::future::timeout;
use async_std::sync::Receiver;
use async_std::task;

use async_tungstenite::{connect_async, WebSocketStream};

use log::{info, trace};

use tungstenite::error::{Error as WsError, Result as WsResult};
use tungstenite::protocol::Message;

use serde_json;

use std::time::Duration;

use url::Url;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::cmd::TvCmd;

const PAIRING: &'static str = include_str!("pairing.json");

pub(crate) struct PersistentConn {
    url: Url,
    client_key: String,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
}

impl PersistentConn {
    pub async fn start(
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
            let exit_status = loop {
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
            };
            for (_, cmd) in conn.waiting_cmds.drain() {
                cmd.process(Err(format_err!("Connection closed")));
            }
            exit_status
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
                serde_json::from_str::<serde_json::Value>(&resp)
                    .ok()
                    .and_then(|resp| {
                        resp["id"].as_str()
                            .and_then(|id| {
                                self.waiting_cmds.remove(id)
                            })
                            .map(|cmd| {
                                cmd.process(Ok(resp));
                            })
                    });
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
