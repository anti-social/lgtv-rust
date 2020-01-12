use failure::{Error, format_err};

use futures::{select, Sink, SinkExt, Stream, StreamExt};
use futures::channel::{mpsc, oneshot};
use futures::io::{AsyncRead, AsyncWrite};
use futures::future::{Future, FutureExt};
use futures::lock::Mutex;
use futures::stream::FusedStream;

use async_std::future::{timeout, TimeoutError};
use async_std::task;

use async_tungstenite::{connect_async, WebSocketStream};

use log::{info, trace, warn};

use tungstenite::error::{Error as WsError, Result as WsResult};
use tungstenite::protocol::{Message, CloseFrame};

use serde_json;

use std::time::Duration;

use url::Url;

use std::collections::HashMap;
use std::convert::TryInto;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::cmd::TvCmd;

#[derive(Clone)]
pub(crate) struct PersistentConn {
    client_key: String,
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    wait_conn_tx: mpsc::Sender<oneshot::Sender<()>>,
    cmd_tx: mpsc::Sender<TvCmd>,
    is_connected: Arc<AtomicBool>,
}

impl PersistentConn {
    pub async fn start(
        url: Url,
        client_key: String,
        connect_timeout: Option<Duration>,
        read_timeout: Option<Duration>,
    ) -> PersistentConn {
        let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
        // let mut cmd_rx = _cmd_rx.fuse();
        let (wait_conn_tx, mut wait_conn_rx) = mpsc::channel(1);
        let is_connected = Arc::new(AtomicBool::new(false));
        let (is_connected_tx, is_connected_rx) = (is_connected.clone(), is_connected.clone());

        let conn = PersistentConn {
            client_key,
            connect_timeout,
            read_timeout,
            wait_conn_tx,
            cmd_tx,
            is_connected,
        };
        let conn_ret = conn.clone();

        let conn_waiters_tx = Arc::new(Mutex::new(
            Vec::<oneshot::Sender<()>>::new()
        ));
        let conn_waiters_rx = conn_waiters_tx.clone();

        task::spawn(async move {
            trace!("Starting persistent connection loop ...");
            loop {
                trace!("Connecting to {} ...", &url);
                let conn_res = PersistentConn::connect(url.clone(), connect_timeout).await;
                let ws_stream = match conn_res {
                    Ok(ws_stream) => {
                        info!("Connected to {}", &url);
                        ws_stream
                    },
                    Err(e) => {
                        info!("Connection error: {}", e);
                        task::sleep(Duration::from_secs(10)).await;
                        continue;
                    },
                };

                let main_conn_processor = MainConnProcessor::new();
                let (ws_tx, _ws_rx) = ws_stream.split();
                let ws_rx = _ws_rx.fuse();
                let (mut close_tx, _close_rx) = mpsc::channel(1);
                let close_rx = _close_rx.fuse();

                // We should start a connection before pairing with TV
                let main_conn_fut = Conn::start(
                    main_conn_processor, ws_tx, ws_rx, cmd_rx, close_tx.clone(), close_rx
                );

                match conn.pair().await {
                    Ok(_) => {
                        // Notify all who waits for connection
                        let mut conn_waiters = conn_waiters_rx.lock().await;
                        is_connected_tx.store(true, Ordering::Relaxed);
                        for conn_waiter in (*conn_waiters).drain(..) {
                            conn_waiter.send(()).ok();
                        }
                    }
                    Err(e) => {
                        warn!("Error when pairing: {}", e);
                        task::sleep(Duration::from_secs(10)).await;
                        close_tx.send(()).await.ok();
                    }
                }

                cmd_rx = match main_conn_fut.await {
                    ConnExitStatus::Reconnect(cmd_rx) => {
                        info!("Disconnected. Trying to reconnect ...");
                        is_connected_tx.store(false, Ordering::Relaxed);
                        cmd_rx
                    },
                    ConnExitStatus::Finish => break,
                };
            }
            trace!("Finished persistent connection loop ...");
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

        conn_ret
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected.load(Ordering::Relaxed)
    }

    pub async fn wait(&self) {
        let (conn_tx, conn_rx) = oneshot::channel();
        self.wait_conn_tx.clone().send(conn_tx).await.ok();
        conn_rx.await.ok();
    }

    pub async fn send_cmd<T>(
        &self,
        rx_fut_and_cmd: (impl Future<Output = Result<Result<T, Error>, oneshot::Canceled>>, TvCmd)
    ) -> Result<T, Error> {
        if !self.is_connected() {
            return Err(format_err!("Not connected"));
        }
        self.send_cmd_inner(rx_fut_and_cmd).await
    }

    async fn send_cmd_inner<T>(
        &self,
        rx_fut_and_cmd: (impl Future<Output = Result<Result<T, Error>, oneshot::Canceled>>, TvCmd)
    ) -> Result<T, Error> {
        let mut cmd_tx = self.cmd_tx.clone();
        let (cmd_res_rx, cmd) = rx_fut_and_cmd;
        let res_fut = async {
            cmd_tx.send(cmd).await.ok();
            cmd_res_rx.await
        };
        let res = if let Some(read_timeout) = self.read_timeout {
            timeout(read_timeout, res_fut).await?
        } else {
            res_fut.await
        };
        res.map_err(|e| format_err!("Canceled channel: {}", e))?
    }

    async fn connect(
        url: Url, connect_timeout: Option<Duration>
    ) -> Result<WebSocketStream<impl AsyncRead + AsyncWrite + Unpin>, Error> {
        let (ws_stream, _) = if let Some(conn_timeout) = connect_timeout {
            timeout(conn_timeout, connect_async(url.clone())).await??
        } else {
            connect_async(url.clone()).await?
        };
        Ok(ws_stream)
    }

    async fn pair(&self) -> Result<(), Error> {
        self.send_cmd_inner(TvCmd::register(self.client_key.clone())).await
    }
}

enum ConnExitStatus<T> {
    Reconnect(T),
    Finish,
}

struct Conn<WSTX>
    where WSTX: Sink<Message>
{
    ws_tx: WSTX,
}

impl<WSTX> Conn<WSTX>
    where WSTX: Sink<Message> + Send + Unpin + 'static,
{
    fn start<P, WSRX, CMDRX, CMD, CLOSETX, CLOSERX>(
        mut processor: P, ws_tx: WSTX, mut ws_rx: WSRX, mut cmd_rx: CMDRX, mut close_tx: CLOSETX, mut close_rx: CLOSERX,
    ) -> impl Future<Output = ConnExitStatus<CMDRX>>
        where
            P: CmdProcessor<CMD> + PingProcessor + MsgProcessor + Send + Unpin + 'static,
            WSRX: Stream<Item = WsResult<Message>> + FusedStream + Send + Unpin + 'static,
            CMDRX: Stream<Item = CMD> + FusedStream + Send + Unpin + 'static,
            CMD: Debug,
            CLOSETX: Sink<()> + Send + Unpin + 'static,
            CLOSERX: Stream<Item = ()> + FusedStream + Send + Unpin + 'static,
    {
        let mut conn = Conn {
            ws_tx,
        };

        let (mut ping_tx, _ping_rx) = mpsc::channel(1);
        let mut ping_rx = _ping_rx.fuse();
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
                        close_tx.send(()).await.ok();
                        break;
                    }
                }
            }
        });

        let conn_task_handle = task::spawn(async move {
            trace!("Starting message processing ...");
            let exit_status = loop {
                let resp = select! {
                    _ = close_rx.next() => {
                        ProcessorResp::Reconnect
                    }
                    ping_req = ping_rx.next() => {
                        trace!("Pinging");
                        processor.process_ping(ping_req)
                    }
                    cmd = cmd_rx.next() => {
                        trace!("Received command: {:?}", &cmd);
                        processor.process_cmd(cmd)
                    }
                    msg = ws_rx.next() => {
                        trace!("Received message: {:?}", msg);
                        processor.process_msg(msg)
                    }
                };
                match resp {
                    ProcessorResp::Continue => continue,
                    ProcessorResp::ContinueWith(resp_msg) => {
                        conn.ws_tx.send(resp_msg).await.ok();
                        continue;
                    },
                    ProcessorResp::Reconnect => break ConnExitStatus::Reconnect(cmd_rx),
                    ProcessorResp::Finish => break ConnExitStatus::Finish,
                }
            };
            exit_status
        });

        ping_task_handle.then(|_| conn_task_handle)
    }
}

enum ProcessorResp {
    Continue,
    ContinueWith(Message),
    Reconnect,
    Finish,
}

trait PingProcessor {
    fn process_ping(&mut self, _ping_req: Option<(u64, oneshot::Sender<()>)>) -> ProcessorResp {
        ProcessorResp::Continue
    }
}

trait MsgProcessor {
    fn process_msg(&mut self, msg: Option<WsResult<Message>>) -> ProcessorResp {
        match msg {
            Some(Ok(Message::Text(data))) => {
                trace!("Received a text message: {}", &data);
                self.on_text(data)
            }
            Some(Ok(Message::Binary(data))) => {
                trace!("Received a binary message");
                self.on_binary(data)
            }
            Some(Ok(Message::Ping(data))) => {
                trace!("Received a ping message");
                self.on_ping(data)
            }
            Some(Ok(Message::Pong(data))) => {
                trace!("Received a pong message");
                self.on_pong(data)
            }
            Some(Ok(Message::Close(data))) => {
                trace!("Received a close message");
                self.on_close(data)
            }
            Some(Err(WsError::ConnectionClosed)) => {
                info!("Connection closed");
                self.on_conn_closed()
            }
            Some(Err(e)) => {
                trace!("Received error: {}", e);
                self.on_err(e)
            }
            None => {
                trace!("Websocket dropped?");
                self.on_channel_dropped()
            }
        }
    }
    fn on_text(&mut self, _data: String) -> ProcessorResp {
        ProcessorResp::Continue
    }
    fn on_binary(&mut self, _data: Vec<u8>) -> ProcessorResp {
        ProcessorResp::Continue
    }
    fn on_ping(&mut self, data: Vec<u8>) -> ProcessorResp {
        ProcessorResp::ContinueWith(Message::Pong(data))
    }
    fn on_pong(&mut self, _data: Vec<u8>) -> ProcessorResp {
        return ProcessorResp::Continue;
    }
    fn on_close(&mut self, _data: Option<CloseFrame>) -> ProcessorResp {
        return ProcessorResp::Continue;
    }
    fn on_conn_closed(&mut self) -> ProcessorResp {
        ProcessorResp::Reconnect
    }
    fn on_err(&mut self, _err: WsError) -> ProcessorResp {
        ProcessorResp::Reconnect
    }
    fn on_channel_dropped(&mut self) -> ProcessorResp {
        ProcessorResp::Finish
    }
}

trait CmdProcessor<T> {
    fn process_cmd(&mut self, cmd: Option<T>) -> ProcessorResp {
        match cmd {
            Some(cmd) => self.on_cmd(cmd),
            None => self.on_channel_dropped(),
        }
    }
    fn on_cmd(&mut self, _cmd: T) -> ProcessorResp {
        ProcessorResp::Continue
    }
    fn on_channel_dropped(&mut self) -> ProcessorResp {
        ProcessorResp::Finish
    }
}

struct MainConnProcessor {
    cmd_counter: u64,
    waiting_cmds: HashMap<String, TvCmd>,
    waiting_pong: Option<(u64, oneshot::Sender<()>)>,
}

impl MainConnProcessor {
    fn new() -> MainConnProcessor {
        MainConnProcessor {
            cmd_counter: 0,
            waiting_cmds: HashMap::new(),
            waiting_pong: None,
        }
    }
}

impl Drop for MainConnProcessor {
    fn drop(&mut self) {
        for (_, cmd) in self.waiting_cmds.drain() {
            cmd.process(Err(format_err!("Connection closed")));
        }
    }
}

impl PingProcessor for MainConnProcessor {
    fn process_ping(&mut self, ping_req: Option<(u64, oneshot::Sender<()>)>) -> ProcessorResp {
        match ping_req {
            Some((ping_counter, pong_tx)) => {
                let data = ping_counter.to_be_bytes().to_vec();
                self.waiting_pong = Some((ping_counter, pong_tx));
                ProcessorResp::ContinueWith(Message::Ping(data))
            }
            None => {
                ProcessorResp::Continue
            }
        }
    }
}

impl CmdProcessor<TvCmd> for MainConnProcessor {
    fn on_cmd(&mut self, cmd: TvCmd) -> ProcessorResp {
        let json_cmd = cmd.prepare(self.cmd_counter);
        self.cmd_counter += 1;
        let msg_id = json_cmd["id"].as_str().unwrap();
        self.waiting_cmds.insert(msg_id.to_string(), cmd);
        ProcessorResp::ContinueWith(Message::text(json_cmd.to_string()))
    }
}

impl MsgProcessor for MainConnProcessor {
    fn on_text(&mut self, data: String) -> ProcessorResp {
        serde_json::from_str::<serde_json::Value>(&data)
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
        ProcessorResp::Continue
    }

    fn on_pong(&mut self, data: Vec<u8>) -> ProcessorResp {
        if let Some((ping_counter, pong_tx)) = self.waiting_pong.take() {
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
        ProcessorResp::Continue
    }
}
