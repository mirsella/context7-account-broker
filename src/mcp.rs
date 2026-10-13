use std::{error::Error, time::Duration};

use reqwest::Client;
use rmcp::{
    ErrorData, Peer, RoleClient, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResponse, CancelledNotificationParam,
        ClientRequest, ListToolsResult, PaginatedRequestParams, ServerInfo, ServerResult,
    },
    service::{PeerRequestOptions, RequestContext, RunningService, ServiceError},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};

use crate::ServerState;

struct Proxy {
    client: Peer<RoleClient>,
}

impl ServerHandler for Proxy {
    fn get_info(&self) -> ServerInfo {
        crate::mcp_info()
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.client
            .list_tools(request)
            .await
            .map_err(upstream_error)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if context.ct.is_cancelled() {
            return Err(ErrorData::internal_error("stdio request cancelled", None));
        }
        let handle = self
            .client
            .send_cancellable_request(
                ClientRequest::CallToolRequest(CallToolRequest::new(request)),
                PeerRequestOptions::with_timeout(Duration::from_secs(30)),
            )
            .await
            .map_err(upstream_error)?;
        let request_id = handle.id.clone();
        let response = tokio::select! {
            result = handle.await_response() => result,
            _ = context.ct.cancelled() => {
                self.client.notify_cancelled(CancelledNotificationParam::new(
                    Some(request_id), Some("stdio request cancelled".into()),
                )).await.map_err(upstream_error)?;
                Err(ServiceError::Cancelled { reason: Some("stdio request cancelled".into()) })
            }
        }
        .map_err(upstream_error)?;
        match response {
            ServerResult::CallToolResult(result) => Ok(result.into()),
            _ => Err(ErrorData::internal_error(
                "unexpected broker tool response",
                None,
            )),
        }
    }
}

fn upstream_error(error: ServiceError) -> ErrorData {
    match error {
        ServiceError::McpError(error) => error,
        error => ErrorData::internal_error(format!("Context7 broker: {error}"), None),
    }
}

async fn connect(state: ServerState) -> Result<RunningService<RoleClient, ()>, Box<dyn Error>> {
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let transport = StreamableHttpClientTransport::with_client(
        client,
        StreamableHttpClientTransportConfig::with_uri(format!(
            "http://127.0.0.1:{}/mcp",
            state.port
        ))
        .auth_header(state.token),
    );
    Ok(().serve(transport).await?)
}

pub(super) async fn run(state: ServerState) -> Result<(), Box<dyn Error>> {
    let upstream = connect(state).await?;
    let proxy = Proxy {
        client: upstream.peer().clone(),
    };
    proxy
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    upstream.cancel().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application,
        broker::Broker,
        config::AccountRecord,
        context7::{Context7, Request, Upstream},
    };
    use rmcp::model::{CallToolResult, ErrorCode};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn stdio_preserves_tools_parallel_calls_errors_and_http_authentication() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let directory = tempfile::tempdir().unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let upstream_calls = Arc::clone(&calls);
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let context7 = Context7::with_sender(move |_, request| {
                let calls = Arc::clone(&upstream_calls);
                let barrier = Arc::clone(&barrier);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    // Neither reply can return until both distinct requests arrive.
                    barrier.wait().await;
                    let Request::Docs { query, .. } = request else {
                        panic!("unexpected request")
                    };
                    Ok(Upstream {
                        status: reqwest::StatusCode::OK,
                        result: CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                            query,
                        )]),
                        quota: None,
                        retry_after: None,
                        libraries: Vec::new(),
                    })
                }
            });
            let broker = Broker::new(
                vec![AccountRecord {
                    name: "test".into(),
                    api_key: "ctx7sk-test".into(),
                }],
                context7,
                directory.path().to_owned(),
            )
            .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let state = ServerState {
                port: listener.local_addr().unwrap().port(),
                token: "a".repeat(64),
            };
            let application = application(broker, &state.token, CancellationToken::new());
            let http_server = tokio::spawn(async move {
                axum::serve(listener, application).await.unwrap();
            });
            assert!(
                connect(ServerState {
                    token: "b".repeat(64),
                    ..state.clone()
                })
                .await
                .is_err()
            );
            let upstream = connect(state).await.unwrap();
            let proxy = Proxy {
                client: upstream.peer().clone(),
            };
            let (client_io, server_io) = tokio::io::duplex(65_536);
            let (server, client) = tokio::join!(proxy.serve(server_io), ().serve(client_io));
            let server = server.unwrap();
            let client = client.unwrap();
            assert_eq!(
                client
                    .peer_info()
                    .unwrap()
                    .server_info
                    .as_ref()
                    .unwrap()
                    .name,
                "context7-account-broker"
            );
            let tools = client.list_all_tools().await.unwrap();
            assert_eq!(tools.len(), 2);
            assert!(tools.iter().any(|tool| tool.name == "resolve-library-id"));
            assert!(tools.iter().any(|tool| tool.name == "query-docs"));
            let request = |query: &str| {
                CallToolRequestParams::new("query-docs").with_arguments(
                    json!({"libraryId": "/x/y", "query": query})
                        .as_object()
                        .unwrap()
                        .clone(),
                )
            };
            let (first, second) = tokio::join!(
                client.call_tool(request("first")),
                client.call_tool(request("second"))
            );
            assert_eq!(first.unwrap().content[0].as_text().unwrap().text, "first");
            assert_eq!(second.unwrap().content[0].as_text().unwrap().text, "second");
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            let error_result = client
                .call_tool(CallToolRequestParams::new("query-docs"))
                .await
                .unwrap();
            assert_eq!(error_result.is_error, Some(true));
            assert!(
                error_result.content[0]
                    .as_text()
                    .unwrap()
                    .text
                    .contains("libraryId")
            );
            let error = client
                .call_tool(CallToolRequestParams::new("unknown-tool"))
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                ServiceError::McpError(ErrorData {
                    code: ErrorCode::INVALID_PARAMS,
                    ..
                })
            ));
            client.cancel().await.unwrap();
            server.waiting().await.unwrap();
            upstream.cancel().await.unwrap();
            http_server.abort();
            let _ = http_server.await;
        })
        .await
        .expect("stdio calls must overlap and shutdown must finish");
    }
}
