//! `tools/call` that tells the server when the host stops waiting.

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotificationParam,
    ClientRequest, RequestId, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions};
use rmcp::{RoleClient, ServiceError};

use super::McpService;

/// rmcp 2.2.0's HTTP worker queues the cancel behind a hung POST, so the wait is bounded.
const CANCEL_DELIVERY_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// A timeout cancels inline, before the caller can reset the transport; a drop cancels from a task.
pub(super) async fn call_tool_cancel_aware(
    service: &McpService,
    params: CallToolRequestParams,
    timeout: std::time::Duration,
) -> Result<CallToolResult, ServiceError> {
    let handle = service
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            PeerRequestOptions::no_options(),
        )
        .await?;
    let mut guard = CancelOnDrop {
        peer: service.peer().clone(),
        request_id: Some(handle.id.clone()),
    };
    let Ok(response) = tokio::time::timeout(timeout, handle.await_response()).await else {
        guard.cancel_within_budget().await;
        return Err(ServiceError::Timeout { timeout });
    };
    guard.request_id = None;
    match response? {
        ServerResult::CallToolResult(result) => Ok(result),
        _ => Err(ServiceError::UnexpectedResponse),
    }
}

/// Armed while the request is in flight; disarmed once it settles.
struct CancelOnDrop {
    peer: Peer<RoleClient>,
    request_id: Option<RequestId>,
}

impl CancelOnDrop {
    async fn cancel_within_budget(&mut self) {
        let Some(request_id) = self.request_id.clone() else {
            return;
        };
        let delivery = notify_cancelled(self.peer.clone(), request_id.clone());
        if tokio::time::timeout(CANCEL_DELIVERY_BUDGET, delivery)
            .await
            .is_err()
        {
            tracing::warn!(
                ?request_id,
                "notifications/cancelled still queued after budget"
            );
        }
        self.request_id = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(request_id) = self.request_id.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                ?request_id,
                "MCP call dropped outside a runtime; cancel not sent"
            );
            return;
        };
        runtime.spawn(notify_cancelled(self.peer.clone(), request_id));
    }
}

async fn notify_cancelled(peer: Peer<RoleClient>, request_id: RequestId) {
    let params = CancelledNotificationParam::new(
        Some(request_id.clone()),
        Some("client cancelled".to_owned()),
    );
    if let Err(e) = peer.notify_cancelled(params).await {
        tracing::warn!(?request_id, error = %e, "notifications/cancelled not delivered");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use rmcp::ServiceExt;
    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::super::{KigiClientHandler, McpClient};
    use super::*;

    type Received = Arc<parking_lot::Mutex<Vec<Value>>>;

    /// Duplex MCP server: answers `initialize`, records all, answers `tools/call` only when `reply`.
    async fn recording_service(reply: bool) -> (McpService, Received) {
        let received = Received::default();
        let (client_read, mut server_write) = tokio::io::duplex(64 * 1024);
        let (server_read, client_write) = tokio::io::duplex(64 * 1024);
        let sink = received.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(server_read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let msg: Value = serde_json::from_str(&line).expect("client sends JSON");
                sink.lock().push(msg.clone());
                let result = match msg["method"].as_str() {
                    Some("initialize") => serde_json::json!({
                        "protocolVersion": msg["params"]["protocolVersion"],
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "recording", "version": "0.0.0" },
                    }),
                    Some("tools/call") if reply => serde_json::json!({
                        "content": [{ "type": "text", "text": "done" }],
                        "isError": false,
                    }),
                    _ => continue,
                };
                let response =
                    serde_json::json!({ "jsonrpc": "2.0", "id": msg["id"], "result": result });
                let encoded = format!("{response}\n");
                server_write
                    .write_all(encoded.as_bytes())
                    .await
                    .expect("client reads replies");
            }
        });
        let handler = KigiClientHandler {
            info: McpClient::make_client_info("recording"),
            server_name: "recording".to_string(),
            notify_tx: Arc::default(),
        };
        let transport = rmcp::transport::async_rw::AsyncRwTransport::<RoleClient, _, _>::new(
            client_read,
            client_write,
        );
        let service = Arc::new(handler.serve(transport).await.expect("handshake"));
        (service, received)
    }

    fn values_at(received: &Received, method: &str, pointer: &str) -> Vec<Value> {
        received
            .lock()
            .iter()
            .filter(|m| m["method"] == method)
            .map(|m| m.pointer(pointer).cloned().unwrap_or(Value::Null))
            .collect()
    }

    fn call_ids(received: &Received) -> Vec<Value> {
        values_at(received, "tools/call", "/id")
    }

    fn cancelled_ids(received: &Received) -> Vec<Value> {
        values_at(received, "notifications/cancelled", "/params/requestId")
    }

    #[tokio::test]
    async fn dropped_call_cancels_its_request() {
        let (service, received) = recording_service(false).await;
        let mut call = Box::pin(call_tool_cancel_aware(
            &service,
            CallToolRequestParams::new("slow"),
            Duration::from_secs(30),
        ));
        let pending = tokio::time::timeout(Duration::from_millis(100), &mut call).await;
        assert!(pending.is_err(), "server never replies");
        drop(call);
        tokio::time::sleep(Duration::from_millis(100)).await;

        let ids = call_ids(&received);
        assert_eq!(ids.len(), 1, "{ids:?}");
        assert_eq!(cancelled_ids(&received), ids);
    }

    #[tokio::test]
    async fn timed_out_call_cancels_its_request_once() {
        let (service, received) = recording_service(false).await;
        let round = call_tool_cancel_aware(
            &service,
            CallToolRequestParams::new("slow"),
            Duration::from_millis(50),
        )
        .await;
        assert!(
            matches!(round, Err(ServiceError::Timeout { .. })),
            "{round:?}"
        );
        // The HTTP timeout arm resets the transport, which drops the last service handle.
        drop(service);
        tokio::time::sleep(Duration::from_millis(100)).await;

        let ids = call_ids(&received);
        assert_eq!(ids.len(), 1, "{ids:?}");
        assert_eq!(cancelled_ids(&received), ids);
    }

    #[tokio::test]
    async fn completed_call_sends_no_cancellation() {
        let (service, received) = recording_service(true).await;
        let result = call_tool_cancel_aware(
            &service,
            CallToolRequestParams::new("fast"),
            Duration::from_secs(5),
        )
        .await
        .expect("server replies");
        assert_eq!(result.is_error, Some(false));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(cancelled_ids(&received), Vec::<Value>::new());
    }
}
