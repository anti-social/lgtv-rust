use failure::{Error, format_err};

use futures::{select, Sink, SinkExt, Stream, StreamExt};
use futures::channel::{mpsc, oneshot};
use futures::io::{AsyncRead, AsyncWrite};
use futures::future::{Future, FutureExt};
use futures::stream::FusedStream;

use async_std::future::{timeout, TimeoutError};
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
use std::convert::TryInto;
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
        mut wait_conn_rx: Receiver<oneshot::Sender<()>>,
        cmd_rx: Receiver<TvCmd>,
    ) -> Arc<AtomicBool> {
        let mut cmd_rx = cmd_rx.fuse();

        let conn = PersistentConn {
            url,
            client_key,
            connect_timeout,
            read_timeout,
        };

        let is_connected = Arc::new(AtomicBool::new(false));
        let is_connected_check = is_connected.clone();
        let ret = is_connected.clone();

        let (mut conn_waiters_tx, mut conn_waiters_rx) = mpsc::unbounded::<oneshot::Sender<()>>();

        task::spawn(async move {
            trace!("Starting persistent connection loop ...");
            loop {
                trace!("Connecting to {} ...", &conn.url);
                let ws_stream = match conn.connect().await {
                    Ok(ws_stream) => {
                        info!("Connected to {}", &conn.url);
                        is_connected.store(true, Ordering::Relaxed);
                        loop {
                            match conn_waiters_rx.try_next() {
                                Ok(Some(ch)) => {
                                    ch.send(()).ok();
                                },
                                Err(_) => break,
                                Ok(None) => {}
                            }
                        }
                        ws_stream
                    },
                    Err(e) => {
                        info!("Connection error: {}", e);
                        task::sleep(Duration::from_secs(10)).await;
                        continue;
                    },
                };
                let (ws_tx, _ws_rx) = ws_stream.split();
                let ws_rx = _ws_rx.fuse();
                cmd_rx = match Conn::start(ws_tx, ws_rx, cmd_rx).await {
                    ConnExitStatus::Reconnect(cmd_rx) => {
                        info!("Disconnected. Trying to reconnect ...");
                        is_connected.store(false, Ordering::Relaxed);
                        cmd_rx
                    },
                    ConnExitStatus::Finish => break,
                }
            }
        });

        task::spawn(async move {
            loop {
                match wait_conn_rx.next().await {
                    Some(ch) => {
                        if is_connected_check.load(Ordering::Relaxed) {
                            ch.send(()).ok();
                        } else {
                            conn_waiters_tx.send(ch).await.ok();
                        }
                    }
                    None => break
                }
            }
        });

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

struct Conn<S>
    where S: Sink<Message>
{
    ws_tx: S,
    cmd_count: u64,
    waiting_cmds: HashMap<String, TvCmd>,
}

impl<S> Conn<S>
    where S: Sink<Message> + Send + Unpin + 'static,
{
    fn start<R, CMD>(
        ws_tx: S, mut ws_rx: R, mut cmd_rx: CMD
    ) -> impl Future<Output = ConnExitStatus<CMD>>
        where R: Stream<Item = WsResult<Message>> + FusedStream + Send + Unpin + 'static,
              CMD: Stream<Item = TvCmd> + FusedStream + Send + Unpin + 'static,
    {
        let mut conn = Conn {
            ws_tx,
            cmd_count: 0,
            waiting_cmds: HashMap::new(),
        };

        let (mut ping_tx, _ping_rx) = mpsc::channel(1);
        let mut ping_rx = _ping_rx.fuse();
        let (close_tx, _close_rx) = oneshot::channel();
        let mut close_rx = _close_rx.fuse();
        let ping_task_handle = task::spawn(async move {
            for ping_counter in 0u64.. {
                task::sleep(Duration::from_secs(10)).await;

                let (pong_tx, pong_rx) = oneshot::channel();
                if ping_tx.send((ping_counter, pong_tx)).await.is_err() {
                    break;
                }
                match timeout(Duration::from_secs(10), pong_rx).await {
                    Ok(Ok(())) => {}
                    Ok(Err(oneshot::Canceled {})) => {
                        break;
                    }
                    Err(TimeoutError {..}) => {
                        trace!("Did not receive pong message. Connection will be closed");
                        close_tx.send(()).ok();
                        break;
                    }
                }
            }
        });

        let conn_task_handle = task::spawn(async move {
            let mut waiting_pong = None;
            let exit_status = loop {
                select! {
                    _ = close_rx => {
                        break ConnExitStatus::Reconnect(cmd_rx);
                    }
                    ping_rx_data = ping_rx.next() => {
                        trace!("Pinging");
                        match ping_rx_data {
                            Some((ping_counter, pong_tx)) => {
                                let data = ping_counter.to_be_bytes().to_vec();
                                conn.ws_tx.send(Message::Ping(data)).await.ok();
                                waiting_pong = Some((ping_counter, pong_tx));
                            }
                            None => {}
                        }
                    }
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
                    msg = ws_rx.next() => {
                        trace!("Received message: {:?}", msg);
                        match msg {
                            Some(Ok(Message::Text(msg))) => {
                                conn.process_msg(msg).await;
                            }
                            Some(Ok(Message::Binary(_))) => {
                                trace!("Received a binary message. Ignoring it")
                            }
                            Some(Ok(Message::Ping(data))) => {
                                conn.ws_tx.send(Message::Pong(data)).await.ok();
                            }
                            Some(Ok(Message::Pong(data))) => {
                                trace!("Received a pong message");
                                conn.process_pong(data, waiting_pong.take());
                            }
                            Some(Ok(Message::Close(_))) => {
                                info!("Received close message. Closing connection");
                                break ConnExitStatus::Reconnect(cmd_rx);
                            }
                            Some(Err(WsError::ConnectionClosed)) => {
                                trace!("Connection closed");
                                break ConnExitStatus::Reconnect(cmd_rx);
                            }
                            Some(Err(e)) => {
                                trace!("Received error: {}", e);
                                // TODO Error processing
                                break ConnExitStatus::Reconnect(cmd_rx);
                            }
                            None => {
                                trace!("Websocket dropped");
                                // TODO Should we send a close message?
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
        });

        ping_task_handle.then(|_| conn_task_handle)
    }

    fn process_pong(&self, data: Vec<u8>, waiting_pong: Option<(u64, oneshot::Sender<()>)>) {
        if let Some((ping_counter, pong_tx)) = waiting_pong {
            let (counter_bytes, _) = data.split_at(std::mem::size_of::<u64>());
            if let Ok(counter_arr) = counter_bytes.try_into() {
                let pong_counter = u64::from_be_bytes(counter_arr);
                if ping_counter == pong_counter {
                    pong_tx.send(()).ok();
                } else {
                    info!("Pong message with outdated counter: {}", pong_counter);
                }
            } else {
                info!("Cannot convert pong data to counter");
            }
        } else {
            info!("Unknown pong message");
        }
    }

    async fn process_cmd(&mut self, cmd: TvCmd) {
        let json_cmd = cmd.prepare(self.cmd_count);
        self.cmd_count += 1;
        let msg_id = json_cmd["id"].as_str().unwrap();
        self.waiting_cmds.insert(msg_id.to_string(), cmd);
        let send_res = self.ws_tx.send(
            Message::text(json_cmd.to_string())
        ).await;
        if let Err(_) = send_res {
            self.waiting_cmds.remove(msg_id);
        }
    }

    async fn process_msg(&mut self, msg: String) {
        serde_json::from_str::<serde_json::Value>(&msg)
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
}
