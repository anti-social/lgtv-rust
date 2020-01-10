use failure::{Error, format_err};

use futures::{select, Sink, SinkExt, Stream, StreamExt};
use futures::channel::{mpsc, oneshot};
use futures::io::{AsyncRead, AsyncWrite};
use futures::future::{Future, FutureExt};
use futures::lock::Mutex;
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

use crate::cmd::{InputCmd, TvCmd};

const PAIRING: &'static str = include_str!("pairing.json");

pub(crate) struct PersistentConn {
    pub url: Url,
    pub connect_timeout: Option<Duration>,
    pub read_timeout: Option<Duration>,
    pub wait_conn_tx: mpsc::Sender<oneshot::Sender<()>>,
    pub cmd_tx: mpsc::Sender<TvCmd>,
    pub pointer_cmd_tx: mpsc::Sender<InputCmd>,
    pub is_connected: Arc<AtomicBool>,
}

impl PersistentConn {
    pub async fn start(
        url: Url,
        client_key: String,
        connect_timeout: Option<Duration>,
        read_timeout: Option<Duration>,
    ) -> PersistentConn {
        let (mut cmd_tx, mut cmd_rx) = mpsc::channel::<TvCmd>(1);
        let mut cmd_tx2 = cmd_tx.clone();
        let (pointer_cmd_tx, mut pointer_cmd_rx) = mpsc::channel::<InputCmd>(1);

        let (wait_conn_tx, mut wait_conn_rx) = mpsc::channel::<oneshot::Sender<()>>(1);

        let is_connected = Arc::new(AtomicBool::new(false));
        let is_connected_tx = is_connected.clone();
        let is_connected_rx = is_connected.clone();
        let ret = is_connected.clone();

        let conn_waiters_tx = Arc::new(Mutex::new(
            Vec::<oneshot::Sender<()>>::new()
        ));
        let conn_waiters_rx = conn_waiters_tx.clone();

        let ws_url = url.clone();
        task::spawn(async move {
            trace!("Starting persistent connection loop ...");
            loop {
                trace!("Connecting to {} ...", &ws_url);
                let conn_fut = async {
                    let conn_res = PersistentConn::connect(ws_url.clone(), connect_timeout).await;
                    match conn_res {
                        Ok(mut ws_stream) => {
                            PersistentConn::pair(&mut ws_stream, client_key.clone(), read_timeout).await
                                .map(|_| ws_stream)
                        }
                        Err(e) => Err(e)
                    }
                };
                let ws_stream = match conn_fut.await {
                    Ok(ws_stream) => {
                        info!("Connected to {}", &ws_url);
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
                let cmd_fut = Conn::start(ws_tx, ws_rx, cmd_rx);

                let (input_cmd_rx, input_cmd) = TvCmd::get_pointer_input_socket();
                cmd_tx2.send(input_cmd).await;
                let input_url = input_cmd_rx.await.unwrap().unwrap();
                println!("Input ws url: {:?}", input_url);

                let pointer_ws_stream = PersistentConn::connect(input_url, connect_timeout).await.unwrap();
                let (pointer_ws_tx, _pointer_ws_rx) = pointer_ws_stream.split();
                let pointer_ws_rx = _pointer_ws_rx.fuse();
                let input_cmd_fut = InputConn::start(pointer_ws_tx, pointer_ws_rx, pointer_cmd_rx);

                // Notify all who waits for connection
                let mut conn_waiters = conn_waiters_rx.lock().await;
                is_connected_tx.store(true, Ordering::Relaxed);
                for conn_waiter in (*conn_waiters).drain(..) {
                    conn_waiter.send(()).ok();
                }

                cmd_rx = match cmd_fut.await {
                    ConnExitStatus::Reconnect(cmd_rx) => {
                        info!("Disconnected. Trying to reconnect ...");
                        is_connected_tx.store(false, Ordering::Relaxed);
                        cmd_rx
                    },
                    ConnExitStatus::Finish => break,
                };

                pointer_cmd_rx = input_cmd_fut.await;
            }
        });

        task::spawn(async move {
            loop {
                match wait_conn_rx.next().await {
                    Some(ch) => {
                        let mut conn_waiters = conn_waiters_tx.lock().await;
                        if is_connected_rx.load(Ordering::Relaxed) {
                            ch.send(()).ok();
                        } else {
                            (*conn_waiters).push(ch);
                        }
                    }
                    None => break
                }
            }
        });

        PersistentConn {
            url,
            connect_timeout,
            read_timeout,
            wait_conn_tx,
            cmd_tx,
            pointer_cmd_tx,
            is_connected,
        }
    }

    async fn connect(
        url: Url, connect_timeout: Option<Duration>,
    ) -> Result<WebSocketStream<impl AsyncRead + AsyncWrite + Unpin>, Error> {
        let (mut ws_stream, _) = if let Some(conn_timeout) = connect_timeout {
            timeout(conn_timeout, connect_async(url)).await??
        } else {
            connect_async(url).await?
        };
        Ok(ws_stream)
    }

    async fn pair(
        ws_stream: &mut WebSocketStream<impl AsyncRead + AsyncWrite + Unpin>,
        client_key: String,
        read_timeout: Option<Duration>,
    ) -> Result<(), Error> {
        let mut pairing_json: serde_json::Value = serde_json::from_str(PAIRING)?;
        pairing_json["payload"].as_object_mut()
            .ok_or(format_err!("Corrupted pairing.json file"))?
            .insert("client-key".to_string(), serde_json::Value::String(client_key.clone()));
        let pairing_msg = Message::text(pairing_json.to_string());

        ws_stream.send(pairing_msg).await?;
        let pairing_resp = if let Some(read_timeout) = read_timeout {
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
                                conn.ws_tx.send(Message::Close(None)).await.ok();
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
                                trace!("Received close message");
                            }
                            Some(Err(WsError::ConnectionClosed)) => {
                                info!("Connection closed");
                                break ConnExitStatus::Reconnect(cmd_rx);
                            }
                            Some(Err(e)) => {
                                trace!("Received error: {}", e);
                                // TODO Error processing
                                break ConnExitStatus::Reconnect(cmd_rx);
                            }
                            None => {
                                trace!("Websocket dropped?");
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

struct InputConn {

}

impl InputConn {
    fn start<S, R, CMD>(mut ws_tx: S, mut ws_rx: R, mut cmd_rx: CMD) -> impl Future<Output = CMD>
        where S: Sink<Message> + Send + Unpin + 'static,
              R: Stream<Item = WsResult<Message>> + FusedStream + Send + Unpin + 'static,
              CMD: Stream<Item = InputCmd> + FusedStream + Send + Unpin + 'static,
    {
        let task_handle = task::spawn(async move {
            loop {
                select!(
                    cmd = cmd_rx.next() => {
                        match(cmd) {
                            Some(cmd) => {
                                ws_tx.send(Message::Text(cmd.prepare())).await.ok();
                            }
                            None => {
                                break;
                            }
                        }
                    }
                    msg = ws_rx.next() => {
                        trace!("Input connection received a message: {:?}", msg);
                        match msg {
                            Some(Ok(Message::Text(data))) => {
                            }
                            Some(Ok(Message::Ping(data))) => {
                                ws_tx.send(Message::Pong(data)).await.ok();
                            }
                            Some(Ok(_)) => {}
                            Some(Err(_)) => {}
                            None => {
                                break;
                            }
                        }
                    }
                );
            }
            cmd_rx
        });

        task_handle
    }
}
