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

use crate::cmd::{PointerCmd, TvCmd};

struct InnerConn {
    url: Url,
    client_key: String,
    connect_timeout: Option<Duration>,
    cmd_rx: mpsc::Receiver<TvCmd>,
    pointer_cmd_rx: mpsc::Receiver<PointerCmd>,
    is_connected: Arc<AtomicBool>,
}

struct OuterConn {
    cmd_tx: mpsc::Sender<TvCmd>,
    pointer_cmd_tx: mpsc::Sender<PointerCmd>,
    is_connected: Arc<AtomicBool>,
}

fn create_conn_parts(
    url: Url,
    client_key: String,
    connect_timeout: Option<Duration>,
) -> (InnerConn, OuterConn) {
    let (cmd_tx, cmd_rx) = mpsc::channel(1);
    let (pointer_cmd_tx, pointer_cmd_rx) = mpsc::channel(1);
    let is_connected = Arc::new(AtomicBool::new(false));
    (
        InnerConn {
            url,
            client_key,
            connect_timeout,
            cmd_rx,
            pointer_cmd_rx,
            is_connected: is_connected.clone(),
        },
        OuterConn {
            cmd_tx,
            pointer_cmd_tx,
            is_connected,
        },
    )
}

impl InnerConn {
    async fn connect_and_wait(mut self) -> Result<ConnExitStatus<Self>, Self> {
        let ws_stream = match self.connect().await {
            Ok(ws_stream) => {
                info!("Connected to {}", &self.url);
                ws_stream
            },
            Err(e) => {
                info!("Connection error: {}", e);
                return Err(self);
            }
        };

        let main_conn_processor = MainConnProcessor::new();
        let (ws_tx, _ws_rx) = ws_stream.split();
        let ws_rx = _ws_rx.fuse();

        // We should start a connection before pairing with TV
        let main_conn = start_conn(
            main_conn_processor, ws_tx, ws_rx, self.cmd_rx
        );

        match self.pair().await {
            Ok(_) => {}
            Err(e) => {
                warn!("Error when pairing: {}", e);
                task::sleep(Duration::from_secs(10)).await;
                main_conn.close().await.ok();
            }
        }



        self.cmd_rx = match main_conn.task_fut.await {
            ConnExitStatus::Reconnect(cmd_rx) => {
                info!("Disconnected. Trying to reconnect ...");
                self.is_connected.store(false, Ordering::Relaxed);
                cmd_rx
            },
            ConnExitStatus::Finish => {
                return Ok(ConnExitStatus::Finish);
            }
        };

        Ok(ConnExitStatus::Reconnect(self))
    }

    async fn connect(&self) -> Result<WebSocketStream<impl AsyncRead + AsyncWrite + Unpin>, Error> {
        let (ws_stream, _) = if let Some(conn_timeout) = self.connect_timeout {
            timeout(conn_timeout, connect_async(self.url.clone())).await??
        } else {
            connect_async(self.url.clone()).await?
        };
        Ok(ws_stream)
    }

    async fn pair(&self) -> Result<(), Error> {
        self.send_cmd_inner(TvCmd::register(self.client_key.clone())).await
    }

    async fn send_cmd_inner<T>(
        &self,
        rx_fut_and_cmd: (impl Future<Output = Result<Result<T, Error>, oneshot::Canceled>>, TvCmd)
    ) -> Result<T, Error> {
        let mut cmd_tx = self.outer.cmd_tx.clone();
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
}

// #[derive(Clone)]
pub(crate) struct PersistentConn {
//    client_key: String,
//    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    wait_conn_tx: mpsc::Sender<oneshot::Sender<()>>,
    outer: OuterConn,
}

impl PersistentConn {
    pub async fn start(
        url: Url,
        client_key: String,
        connect_timeout: Option<Duration>,
        read_timeout: Option<Duration>,
    ) -> PersistentConn {
        let (wait_conn_tx, mut wait_conn_rx) = mpsc::channel(1);
        let (mut inner_conn, outer_conn) = create_conn_parts(url, client_key, connect_timeout);

        let conn = PersistentConn {
//            client_key,
//            connect_timeout,
            read_timeout,
            wait_conn_tx,
            outer: outer_conn,
        };

        let conn_waiters_tx = Arc::new(Mutex::new(
            Vec::<oneshot::Sender<()>>::new()
        ));
        let conn_waiters_rx = conn_waiters_tx.clone();
        let is_connected = inner_conn.is_connected.clone();

        task::spawn(async move {
            trace!("Starting persistent connection loop ...");
            loop {
//                trace!("Connecting to {} ...", &inner_conn.url);
                inner_conn = match inner_conn.connect_and_wait().await {
                    Ok(ConnExitStatus::Reconnect(c)) => c,
                    Ok(ConnExitStatus::Finish) => {
                        break;
                    }
                    Err(c) => {
                        c
                    },
                };

//                let conn_res = PersistentConn::connect(url.clone(), connect_timeout).await;
//                let ws_stream = match conn_res {
//                    Ok(ws_stream) => {
//                        info!("Connected to {}", &url);
//                        ws_stream
//                    },
//                    Err(e) => {
//                        info!("Connection error: {}", e);
//                        task::sleep(Duration::from_secs(10)).await;
//                        continue;
//                    },
//                };
//
//                let main_conn_processor = MainConnProcessor::new();
//                let (ws_tx, _ws_rx) = ws_stream.split();
//                let ws_rx = _ws_rx.fuse();
//
//                // We should start a connection before pairing with TV
//                let main_conn = start_conn(
//                    main_conn_processor, ws_tx, ws_rx, cmd_rx
//                );
//
//                match conn.pair().await {
//                    Ok(_) => {}
//                    Err(e) => {
//                        warn!("Error when pairing: {}", e);
//                        task::sleep(Duration::from_secs(10)).await;
//                        main_conn.close().await.ok();
//                    }
//                }
//
//                let pointer_url = match conn.get_pointer_input_url().await {
//                    Ok(url) => url,
//                    Err(e) => {
//                        warn!("Error when receiving pointer websocket url: {}", e);
//                        task::sleep(Duration::from_secs(10)).await;
//                        main_conn.close().await.ok();
//                        continue;
//                    }
//                };
//                let pointer_ws_stream = match PersistentConn::connect(pointer_url.clone(), connect_timeout).await {
//                    Ok(ws_stream) => ws_stream,
//                    Err(e) => {
//                        warn!("Cannot connect to pointer: {}", e);
//                        task::sleep(Duration::from_secs(10)).await;
//                        main_conn.close().await.ok();
//                        continue;
//                    }
//                };
//                let (pointer_ws_tx, _pointer_ws_rx) = pointer_ws_stream.split();
//                let pointer_ws_rx = _pointer_ws_rx.fuse();
//                let pointer_conn_processor = PointerConnProcessor::new();
//                let pointer_conn = start_conn(
//                    pointer_conn_processor, pointer_ws_tx, pointer_ws_rx, pointer_cmd_rx
//                );
//
//                // Notify all who waits for connection
//                let mut conn_waiters = conn_waiters_rx.lock().await;
//                is_connected_tx.store(true, Ordering::Relaxed);
//                for conn_waiter in (*conn_waiters).drain(..) {
//                    conn_waiter.send(()).ok();
//                }
//
//                cmd_rx = match main_conn.task_fut.await {
//                    ConnExitStatus::Reconnect(cmd_rx) => {
//                        info!("Disconnected. Trying to reconnect ...");
//                        is_connected_tx.store(false, Ordering::Relaxed);
//                        cmd_rx
//                    },
//                    ConnExitStatus::Finish => break,
//                };
//
//                pointer_cmd_rx = match pointer_conn.task_fut.await {
//                    ConnExitStatus::Reconnect(cmd_rx) => cmd_rx,
//                    ConnExitStatus::Finish => break,
//                };
            }
            trace!("Finished persistent connection loop ...");
        });

        task::spawn(async move {
            loop {
                match wait_conn_rx.next().await {
                    Some(ch) => {
                        let mut conn_waiters = conn_waiters_tx.lock().await;
                        if is_connected.load(Ordering::Relaxed) {
                            ch.send(()).ok();
                        } else {
                            (*conn_waiters).push(ch);
                        }
                    }
                    None => break
                }
            }
        });

        conn
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

    async fn connect_and_wait() {

    }

    pub fn is_connected(&self) -> bool {
        self.outer.is_connected.load(Ordering::Relaxed)
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
        let mut cmd_tx = self.outer.cmd_tx.clone();
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

    pub async fn send_pointer_cmd(
        &self, cmd: PointerCmd
    ) -> Result<(), Error> {
        if !self.is_connected() {
            return Err(format_err!("Not connected"));
        }
        let mut cmd_tx = self.outer.pointer_cmd_tx.clone();
        cmd_tx.send(cmd).await.ok();
        Ok(())
    }

//    async fn pair(&self) -> Result<(), Error> {
//        self.send_cmd_inner(TvCmd::register(self.client_key.clone())).await
//    }
//
//    async fn get_pointer_input_url(&self) -> Result<Url, Error> {
//        self.send_cmd_inner(TvCmd::get_pointer_input_socket()).await
//    }
}

enum ConnExitStatus<T> {
    Reconnect(T),
    Finish,
}

struct Conn<H, CmdRx, Cmd, CloseTx>
    where
        H: Future<Output = ConnExitStatus<CmdRx>>,
        CmdRx: Stream<Item = Cmd>,
        Cmd: Debug,
        CloseTx: Sink<()> + Clone,
{
    pub close_tx: CloseTx,
    pub task_fut: H,
}

impl<H, CmdRx, Cmd, CloseTx> Conn<H, CmdRx, Cmd, CloseTx>
    where
        H: Future<Output = ConnExitStatus<CmdRx>>,
        CmdRx: Stream<Item = Cmd>,
        Cmd: Debug,
        CloseTx: Sink<()> + Clone + Send + Unpin + 'static,
{
    async fn close(&self) -> Result<(), CloseTx::Error> {
        self.close_tx.clone().send(()).await
    }
}

fn start_conn<P, WsTx, WsRx, Cmd, CmdRx>(
    mut processor: P, mut ws_tx: WsTx, mut ws_rx: WsRx, mut cmd_rx: CmdRx
) -> Conn<impl Future<Output=ConnExitStatus<CmdRx>>, CmdRx, Cmd, impl Sink<()> + Clone>
    where
        P: CmdProcessor<Cmd> + PingProcessor + MsgProcessor + Send + Unpin + 'static,
        WsTx: Sink<Message> + Send + Unpin + 'static,
        WsRx: Stream<Item = WsResult<Message>> + FusedStream + Send + Unpin + 'static,
        CmdRx: Stream<Item =Cmd> + FusedStream + Send + Unpin + 'static,
        Cmd: Debug,
{
    let (close_tx, _close_rx) = mpsc::channel(1);
    let mut ping_close_tx = close_tx.clone();
    let mut close_rx = _close_rx.fuse();

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
                    ping_close_tx.send(()).await.ok();
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
                    ws_tx.send(resp_msg).await.ok();
                    continue;
                },
                ProcessorResp::Reconnect => break ConnExitStatus::Reconnect(cmd_rx),
                ProcessorResp::Finish => break ConnExitStatus::Finish,
            }
        };
        exit_status
    });

    Conn {
        close_tx,
        task_fut: ping_task_handle.then(|_| conn_task_handle),
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

#[derive(Debug)]
struct PointerConnProcessor;

impl PointerConnProcessor {
    fn new() -> PointerConnProcessor {
        PointerConnProcessor {}
    }
}

impl CmdProcessor<PointerCmd> for PointerConnProcessor {
    fn on_cmd(&mut self, cmd: PointerCmd) -> ProcessorResp {
        let prepared_cmd = cmd.prepare();
        ProcessorResp::ContinueWith(Message::text(prepared_cmd))
    }
}

impl MsgProcessor for PointerConnProcessor {}

impl PingProcessor for PointerConnProcessor {}