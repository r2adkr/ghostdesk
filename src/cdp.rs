//! A minimal Chrome DevTools Protocol (CDP) client.
//!
//! Uses a single browser-level WebSocket with `flatten` sessions attached, and tags each command
//! with the `sessionId` of its target (tab).

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Debug)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>>>;

pub struct Cdp {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    events: broadcast::Sender<CdpEvent>,
}

impl Cdp {
    pub async fn connect(url: &str) -> Result<Arc<Cdp>> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .context("CDP 웹소켓 연결 실패")?;
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Default::default();
        let (ev_tx, _) = broadcast::channel::<CdpEvent>(512);

        tokio::spawn(async move {
            while let Some(s) = out_rx.recv().await {
                if sink.send(Message::Text(s.into())).await.is_err() {
                    break;
                }
            }
        });

        let p2 = pending.clone();
        let ev2 = ev_tx.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = v.get("id").and_then(|x| x.as_u64()) {
                    if let Some(tx) = p2.lock().remove(&id) {
                        let r = match v.get("error") {
                            Some(e) => Err(e.to_string()),
                            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(r);
                    }
                } else if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                    let _ = ev2.send(CdpEvent {
                        method: m.to_string(),
                        params: v.get("params").cloned().unwrap_or(Value::Null),
                        session_id: v
                            .get("sessionId")
                            .and_then(|s| s.as_str())
                            .map(String::from),
                    });
                }
            }
            for (_, tx) in p2.lock().drain() {
                let _ = tx.send(Err("CDP 연결이 종료되었습니다".into()));
            }
            let _ = ev2.send(CdpEvent {
                method: "__closed".into(),
                params: Value::Null,
                session_id: None,
            });
        });

        Ok(Arc::new(Cdp {
            out: out_tx,
            pending,
            next_id: AtomicU64::new(1),
            events: ev_tx,
        }))
    }

    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.call_timeout(method, params, session, Duration::from_secs(30))
            .await
    }

    pub async fn call_timeout(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        if self.out.send(msg.to_string()).is_err() {
            self.pending.lock().remove(&id);
            return Err(anyhow!("CDP 연결이 닫혔습니다"));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!("CDP {method} 오류: {e}")),
            Ok(Err(_)) => Err(anyhow!("CDP 연결이 종료되었습니다")),
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(anyhow!("CDP {method} 시간 초과"))
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }
}
