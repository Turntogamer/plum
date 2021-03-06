use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_tungstenite::tokio::connect_async;
use async_tungstenite::tungstenite::handshake::client::Request as HandShakeRequest;
use async_tungstenite::tungstenite::http::header;
use async_tungstenite::tungstenite::protocol::Message;
use futures::channel::{mpsc, oneshot};
use futures::future;
use futures::stream::StreamExt;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use tokio::task;

use crate::errors::Result;
use crate::transports::{BatchTransport, NotificationStream, PubsubTransport, Transport};
use crate::types::{
    Call, MethodCall, Notification, Params, Request, RequestId, Response, SubscriptionId, Value,
    Version,
};

type Pending = oneshot::Sender<Result<Response>>;
type Pendings = Arc<Mutex<BTreeMap<RequestId, Pending>>>;
type Subscription = mpsc::UnboundedSender<Value>;
type Subscriptions = Arc<Mutex<BTreeMap<SubscriptionId, Subscription>>>;

type WebSocketSender = mpsc::UnboundedSender<Message>;
type WebSocketReceiver = mpsc::UnboundedReceiver<Message>;

pub struct WebSocketTransport {
    id: Arc<AtomicUsize>,
    _url: String,
    _bearer_auth_token: Option<String>,
    pendings: Pendings,
    subscriptions: Subscriptions,
    sender: WebSocketSender,
    _handle: task::JoinHandle<()>,
}

impl WebSocketTransport {
    pub fn new<U: Into<String>>(url: U) -> Self {
        let url = url.into();
        let handshake_request = HandShakeRequest::get(&url)
            .body(())
            .expect("Handshake HTTP request should be valid");

        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let subscriptions = Arc::new(Mutex::new(BTreeMap::new()));
        let (writer_tx, writer_rx) = mpsc::unbounded();

        let handle = task::spawn(ws_task(
            handshake_request,
            pending.clone(),
            subscriptions.clone(),
            writer_tx.clone(),
            writer_rx,
        ));

        Self {
            id: Arc::new(AtomicUsize::new(1)),
            _url: url,
            _bearer_auth_token: None,
            pendings: pending,
            subscriptions,
            sender: writer_tx,
            _handle: handle,
        }
    }

    pub fn new_with_bearer_auth<U: Into<String>, T: Into<String>>(url: U, token: T) -> Self {
        let url = url.into();
        let token = token.into();

        let bearer_auth_header_value = format!("Bearer {}", token);
        let handshake_request = HandShakeRequest::get(&url)
            .header(header::AUTHORIZATION, bearer_auth_header_value)
            .body(())
            .expect("Handshake HTTP request should be valid");

        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let subscriptions = Arc::new(Mutex::new(BTreeMap::new()));
        let (writer_tx, writer_rx) = mpsc::unbounded();

        let handle = task::spawn(ws_task(
            handshake_request,
            pending.clone(),
            subscriptions.clone(),
            writer_tx.clone(),
            writer_rx,
        ));

        Self {
            id: Arc::new(AtomicUsize::new(1)),
            _url: url,
            _bearer_auth_token: Some(token),
            pendings: pending,
            subscriptions,
            sender: writer_tx,
            _handle: handle,
        }
    }

    async fn send_request(&self, id: RequestId, request: &Request) -> Result<Response> {
        let request = serde_json::to_string(request)?;
        debug!("Calling: {}", request);

        let (tx, rx) = oneshot::channel();
        self.pendings.lock().insert(id, tx);
        self.sender
            .unbounded_send(Message::Text(request))
            .expect("Sending `Text` Message should be successful");

        rx.await.unwrap()
    }
}

async fn ws_task(
    handshake_request: HandShakeRequest,
    pendings: Pendings,
    sub: Subscriptions,
    tx: WebSocketSender,
    rx: WebSocketReceiver,
) {
    let (ws_stream, _) = connect_async(handshake_request)
        .await
        .expect("Handshake request is valid, but failed to connect");
    info!("WebSocket handshake has been successfully completed");
    let (sink, stream) = ws_stream.split();

    // receive request from WebSocketSender,
    // and forward the request to sink that will send message to websocket stream.
    let write_to_ws = rx.map(Ok).forward(sink);
    // read websocket message from websocket stream, and handle the incoming message.
    let read_from_ws = stream.for_each(|msg| async {
        match msg {
            Ok(msg) => handle_incoming_msg(msg, pendings.clone(), sub.clone(), tx.clone()),
            Err(err) => error!("WebSocket stream read error: {}", err),
        }
    });

    futures::pin_mut!(write_to_ws, read_from_ws);
    future::select(write_to_ws, read_from_ws).await;
}

fn handle_incoming_msg(
    msg: Message,
    pendings: Pendings,
    subscriptions: Subscriptions,
    tx: WebSocketSender,
) {
    match msg {
        Message::Text(msg) => {
            handle_subscription(subscriptions, &msg);
            handle_pending_response(pendings, &msg);
        }
        Message::Binary(msg) => warn!("Receive `Binary` Message: {:?}", msg),
        Message::Close(msg) => {
            warn!("Receive `Close` Message: {:?}", msg);
            tx.unbounded_send(Message::Close(msg))
                .expect("Sending `Close` Message should be successful")
        }
        Message::Ping(msg) => {
            warn!("Receive `Ping` Message: {:?}", msg);
            tx.unbounded_send(Message::Pong(msg))
                .expect("Sending `Pong` Message should be successful")
        }
        Message::Pong(msg) => warn!("Receive `Pong` Message: {:?}", msg),
    }
}

fn handle_subscription(subscriptions: Subscriptions, msg: &str) {
    if let Ok(notification) = serde_json::from_str::<Notification>(msg) {
        if let Params::Array(params) = notification.params {
            let id = params.get(0);
            let result = params.get(1);
            if let (Some(Value::Number(id)), Some(result)) = (id, result) {
                let id = id.as_u64().unwrap() as usize;
                if let Some(stream) = subscriptions.lock().get(&id) {
                    stream
                        .unbounded_send(result.clone())
                        .expect("Sending subscription result to the user should be successful");
                } else {
                    warn!("Got notification for unknown subscription (id: {})", id);
                }
            } else {
                error!("Got unsupported notification (id: {:?})", id);
            }
        } else {
            error!(
                "The Notification Params is not JSON array type: {}",
                serde_json::to_string(&notification.params)
                    .expect("Serialize `Params` never fails")
            );
        }
    }
}

fn handle_pending_response(pendings: Pendings, msg: &str) {
    let response = serde_json::from_str::<Response>(msg).map_err(Into::into);
    let id = match &response {
        Ok(Response::Single(output)) => output.id(),
        Ok(Response::Batch(outputs)) => outputs.get(0).map_or(0, |output| output.id()),
        Err(_) => 0,
    };
    if let Some(request) = pendings.lock().remove(&id) {
        if let Err(err) = request.send(response) {
            error!("Sending a response to deallocated channel: {:?}", err);
        }
    }
}

#[async_trait::async_trait]
impl Transport for WebSocketTransport {
    fn prepare<M: Into<String>>(&self, method: M, params: Params) -> (RequestId, Call) {
        let id = self.id.fetch_add(1, Ordering::AcqRel);
        let call = Call::MethodCall(MethodCall {
            jsonrpc: Some(Version::V2),
            id,
            method: method.into(),
            params,
        });
        (id, call)
    }

    async fn execute(&self, id: RequestId, request: &Request) -> Result<Response> {
        self.send_request(id, request).await
    }
}

#[async_trait::async_trait]
impl BatchTransport for WebSocketTransport {}

impl PubsubTransport for WebSocketTransport {
    fn subscribe<T>(&self, id: SubscriptionId) -> NotificationStream<T>
    where
        T: DeserializeOwned,
    {
        let (tx, rx) = mpsc::unbounded();
        if self.subscriptions.lock().insert(id, tx).is_some() {
            warn!("Replacing already-registered subscription with id {:?}", id);
        }
        Box::pin(
            rx.map(|value| serde_json::from_value(value).expect("Deserialize `Value` never fails")),
        )
    }

    fn unsubscribe(&self, id: SubscriptionId) {
        self.subscriptions.lock().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_version() {
        let ws = WebSocketTransport::new("ws://127.0.0.1:1234/rpc/v0");
        // Filecoin.Version need read permission
        let version: Value = ws
            .send("Filecoin.Version", Params::Array(vec![]))
            .await
            .unwrap();
        println!("Version: {:?}", version);
    }

    #[tokio::test]
    async fn test_log_list() {
        // lotus auth create-token --perm admin
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJBbGxvdyI6WyJyZWFkIiwid3JpdGUiLCJzaWduIiwiYWRtaW4iXX0.V82x4rrMmyzgLhW0jeBCL6FVN8I6iSnB0Dc05xeZjVE";
        let http = WebSocketTransport::new_with_bearer_auth("ws://127.0.0.1:1234/rpc/v0", token);
        // Filecoin.LogList need write permission
        let log_list: Value = http
            .send("Filecoin.LogList", Params::Array(vec![]))
            .await
            .unwrap();
        println!("LogList: {:?}", log_list);
    }

    #[tokio::test]
    async fn test_sync_incoming_blocks() {
        env_logger::init();
        let ws = WebSocketTransport::new("ws://127.0.0.1:1234/rpc/v0");
        let id: usize = ws
            .send("Filecoin.SyncIncomingBlocks", Params::Array(vec![]))
            .await
            .unwrap();
        println!("Subscription Id: {}", id);
        let mut stream = ws.subscribe::<Value>(id);
        while let Some(value) = stream.next().await {
            println!("Block: {:?}", value);
        }
    }
}
