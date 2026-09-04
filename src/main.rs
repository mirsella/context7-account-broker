mod broker;
mod config;
mod context7;

use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, Request as HttpRequest, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use broker::Broker;
use context7::{Context7, Request, quota_summary};
use reqwest::Client;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const STARTUP_DEADLINE: Duration = Duration::from_secs(5);
const HEALTH_TIMEOUT: Duration = Duration::from_millis(250);
#[derive(Serialize)]
struct StartOutput {
    url: String,
    token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ServerState {
    port: u16,
    token: String,
}

#[derive(Deserialize)]
struct HealthQuery {
    challenge: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResolveInput {
    /// What to look up in the library's documentation. This is used to rank library results by relevance to what the user is trying to accomplish. The query is sent to the Context7 API for processing. Do not include any sensitive or confidential information such as API keys, passwords, credentials, personal data, or proprietary code in your query.
    query: String,
    /// Library name to search for and retrieve a Context7-compatible library ID. Use the official library name with proper punctuation — e.g., 'Next.js' instead of 'nextjs', 'Customer.io' instead of 'customerio', 'Three.js' instead of 'threejs'.
    library_name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueryInput {
    /// Exact Context7-compatible library ID (e.g., '/mongodb/docs', '/vercel/next.js', '/supabase/supabase', '/vercel/next.js/v14.3.0-canary.87') retrieved from 'resolve-library-id' or directly from user query in the format '/org/project' or '/org/project/version'.
    library_id: String,
    /// What to look up in the library's documentation, scoped to a single concept. Be specific and include relevant details, but keep each query to one topic — if the user's question spans multiple distinct concepts, make a separate call per concept instead of combining them, unless the question is about how the concepts interact. Good: 'How to set up authentication with JWT in Express.js' or 'React useEffect cleanup function examples'. Bad (too vague): 'auth' or 'hooks'. Bad (too broad): 'routing and auth and caching in Next.js'. The query is sent to the Context7 API for processing. Do not include any sensitive or confidential information such as API keys, passwords, credentials, personal data, or proprietary code in your query.
    query: String,
}

#[derive(Clone)]
struct McpHandler {
    broker: Arc<Broker>,
}

#[tool_router]
impl McpHandler {
    /// Resolves a package/product name to a Context7-compatible library ID and returns matching libraries.
    ///
    /// You MUST call this function before 'query-docs' to obtain a valid Context7-compatible library ID UNLESS the user explicitly provides a library ID in the format '/org/project' or '/org/project/version' in their query.
    #[tool(
        name = "resolve-library-id",
        title = "Resolve Context7 Library ID",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn resolve_library_id(
        &self,
        Parameters(input): Parameters<ResolveInput>,
    ) -> CallToolResult {
        self.broker
            .call(Request::Search {
                query: input.query,
                library_name: input.library_name,
            })
            .await
    }

    /// Retrieves and queries up-to-date documentation and code examples from Context7 for any programming library or framework.
    ///
    /// You must call 'resolve-library-id' first to obtain the exact Context7-compatible library ID required to use this tool, UNLESS the user explicitly provides a library ID in the format '/org/project' or '/org/project/version' in their query.
    ///
    /// Do not call this tool more than 3 times per question.
    #[tool(
        name = "query-docs",
        title = "Query Context7 Documentation",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn query_docs(&self, Parameters(input): Parameters<QueryInput>) -> CallToolResult {
        self.broker
            .call(Request::Docs {
                query: input.query,
                library_id: input.library_id,
            })
            .await
    }
}

#[tool_handler(
    name = "context7-account-broker",
    instructions = "Use resolve-library-id to find a Context7-compatible library ID, then use query-docs with that ID."
)]
impl ServerHandler for McpHandler {}

async fn authenticate(
    State(expected): State<HeaderValue>,
    request: HttpRequest<Body>,
    next: Next,
) -> Response {
    if request.headers().get(header::AUTHORIZATION) != Some(&expected) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "Unauthorized",
        )
            .into_response();
    }
    next.run(request).await
}

async fn health(
    State(key): State<Arc<hmac::Key>>,
    Query(query): Query<HealthQuery>,
) -> Result<(StatusCode, [(&'static str, String); 1]), StatusCode> {
    if !is_token(&query.challenge) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(
            "x-context7-broker-proof",
            encode_hex(hmac::sign(&key, query.challenge.as_bytes()).as_ref()),
        )],
    ))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("start") => {
            reject_extra_args(&mut args, "start")?;
            start().await
        }
        Some("__serve") => {
            reject_extra_args(&mut args, "__serve")?;
            serve().await
        }
        Some("accounts") => accounts_command(args),
        Some("status") => {
            reject_extra_args(&mut args, "status")?;
            status_command().await
        }
        Some("config") => {
            reject_extra_args(&mut args, "config")?;
            config_command()
        }
        Some("help" | "--help" | "-h") => {
            reject_extra_args(&mut args, "help")?;
            print_help();
            Ok(())
        }
        Some(command) => Err(format!("unknown command {command}; use help").into()),
    }
}

async fn serve() -> Result<(), Box<dyn Error>> {
    let runtime_path = config::runtime_path()?;
    config::ensure_private_directory(&runtime_path)?;
    let startup_lock = open_directory_lock(&runtime_path)?;
    startup_lock.lock()?;
    let lock_path = runtime_path.join("broker.lock");
    let broker_lock = open_lock(&lock_path)?;
    broker_lock
        .try_lock()
        .map_err(|_| "broker already running for this package version")?;
    let cache_path = config::cache_path()?;
    let address = SocketAddr::from(([127, 0, 0, 1], 0));
    let listener = tokio::net::TcpListener::bind(address).await?;
    let address = listener.local_addr()?;
    let accounts = config::load_accounts(&config::accounts_path()?)?;
    if accounts.is_empty() {
        return Err("No accounts configured. Run accounts add".into());
    }
    let broker = Broker::new(accounts, Context7::new()?, cache_path)?;
    let state_path = runtime_path.join("broker.json");
    let _ = fs::remove_file(&state_path);
    let token = random_token().map_err(|_| "failed to generate broker token")?;
    let cancellation = CancellationToken::new();
    let app = application(broker, &token, cancellation.clone());
    config::write_private(
        &state_path,
        &serde_json::to_vec(&ServerState {
            port: address.port(),
            token,
        })?,
    )?;
    startup_lock.unlock()?;
    eprintln!("context7-account-broker listening on http://{address}");
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let shutdown = cancellation.clone();
    tokio::spawn(async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    eprintln!("warning: failed to listen for Ctrl-C: {error}");
                }
            }
            _ = terminate.recv() => {}
        }
        shutdown.cancel();
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancellation.cancelled().await })
        .await?;
    drop(broker_lock);
    Ok(())
}

fn application(broker: Arc<Broker>, token: &str, cancellation: CancellationToken) -> Router {
    let authorization = HeaderValue::from_str(&format!("Bearer {token}"))
        .expect("validated token is a valid header value");
    let health_key = Arc::new(hmac::Key::new(hmac::HMAC_SHA256, token.as_bytes()));
    let handler_broker = Arc::clone(&broker);
    let service = StreamableHttpService::new(
        move || {
            Ok(McpHandler {
                broker: Arc::clone(&handler_broker),
            })
        },
        NeverSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_cancellation_token(cancellation.clone()),
    );
    let mcp = Router::new()
        .fallback_service(service)
        .layer(middleware::from_fn_with_state(authorization, authenticate));
    Router::new()
        .route("/health", get(health))
        .nest("/mcp", mcp)
        .with_state(health_key)
}

async fn start() -> Result<(), Box<dyn Error>> {
    let runtime_path = config::runtime_path()?;
    config::ensure_private_directory(&runtime_path)?;
    let runtime_path = fs::canonicalize(runtime_path)?;
    let client = Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let state_path = runtime_path.join("broker.json");
    let broker_lock_path = runtime_path.join("broker.lock");
    let deadline = tokio::time::Instant::now() + STARTUP_DEADLINE;
    let lock = open_directory_lock(&runtime_path)?;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(fs::TryLockError::WouldBlock) => return Err("broker startup lock timed out".into()),
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    let print_launch = |state: ServerState| -> Result<(), Box<dyn Error>> {
        println!(
            "{}",
            serde_json::to_string(&StartOutput {
                url: format!("http://127.0.0.1:{}/mcp", state.port),
                token: state.token,
            })?
        );
        Ok(())
    };
    loop {
        if !broker_running(&broker_lock_path)? {
            break;
        }
        if let Some(state) = active_launch(&client, &state_path).await {
            print_launch(state)?;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("broker startup timed out".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = fs::remove_file(&state_path);

    let cache_path = config::cache_path()?;
    config::ensure_private_directory(&cache_path)?;
    let log_path = cache_path.join("broker.stderr.log");
    config::write_private(&log_path, b"")?;
    let log = fs::OpenOptions::new().append(true).open(&log_path)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .process_group(0);
    let mut child = command.spawn()?;
    lock.unlock()?;
    loop {
        let child_status = child.try_wait()?;
        if let Some(status) = child_status
            && !broker_running(&broker_lock_path)?
        {
            return Err(startup_error(&log_path, format!("child exited with {status}")).into());
        }
        if let Some(state) = active_launch(&client, &state_path).await {
            print_launch(state)?;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let detail = child_status.map_or_else(
                || "child did not become ready".to_owned(),
                |status| format!("child exited with {status}"),
            );
            return Err(startup_error(&log_path, detail).into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn startup_error(log_path: &std::path::Path, status: impl std::fmt::Display) -> String {
    let detail = fs::read_to_string(log_path).unwrap_or_default();
    format!(
        "Context7 broker startup failed: {status}; log: {}{}",
        log_path.display(),
        if detail.trim().is_empty() {
            String::new()
        } else {
            format!("\n{}", detail.trim())
        }
    )
}

async fn broker_healthy(client: &Client, port: u16, token: &str) -> bool {
    let Ok(challenge) = random_token() else {
        return false;
    };
    let expected = encode_hex(
        hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, token.as_bytes()),
            challenge.as_bytes(),
        )
        .as_ref(),
    );
    client
        .get(format!("http://127.0.0.1:{port}/health"))
        .query(&[("challenge", challenge)])
        .send()
        .await
        .is_ok_and(|response| {
            response.status() == StatusCode::NO_CONTENT
                && response
                    .headers()
                    .get("x-context7-broker-proof")
                    .is_some_and(|proof| proof.as_bytes() == expected.as_bytes())
        })
}

async fn active_launch(client: &Client, state_path: &std::path::Path) -> Option<ServerState> {
    let state = read_state(state_path)?;
    broker_healthy(client, state.port, &state.token)
        .await
        .then_some(state)
}

fn read_state(path: &std::path::Path) -> Option<ServerState> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > 256 || metadata.permissions().mode() & 0o077 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref().take(257).read_to_end(&mut bytes).ok()?;
    let state: ServerState = serde_json::from_slice(&bytes).ok()?;
    (state.port != 0 && is_token(&state.token)).then_some(state)
}

fn broker_running(path: &std::path::Path) -> Result<bool, std::io::Error> {
    let lock = open_lock(path)?;
    match lock.try_lock() {
        Ok(()) => Ok(false),
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

fn open_lock(path: &std::path::Path) -> Result<fs::File, std::io::Error> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::other(
            "broker lock must be an owner-only regular file",
        ));
    }
    Ok(file)
}

fn open_directory_lock(path: &std::path::Path) -> Result<fs::File, std::io::Error> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random_token() -> Result<String, ring::error::Unspecified> {
    let mut bytes = [0; 32];
    SystemRandom::new().fill(&mut bytes)?;
    Ok(encode_hex(&bytes))
}

fn is_token(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn accounts_command(mut args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    match args.next().as_deref() {
        Some("list") | None => {
            reject_extra_args(&mut args, "accounts list")?;
            let path = config::accounts_path()?;
            for account in config::load_accounts(&path)? {
                println!("{} {}", account.name, broker::fingerprint(&account.api_key));
            }
            Ok(())
        }
        Some("add") => {
            let name = args.next().ok_or("usage: accounts add NAME")?;
            reject_extra_args(&mut args, "accounts add NAME")?;
            let key = rpassword::prompt_password("Context7 API key: ")?;
            let key = key.trim();
            config::add_account(&config::accounts_path()?, &name, key)?;
            println!("added {name}");
            Ok(())
        }
        Some("remove") => {
            let name = args.next().ok_or("usage: accounts remove NAME")?;
            reject_extra_args(&mut args, "accounts remove NAME")?;
            config::remove_account(&config::accounts_path()?, &name)?;
            println!("removed {name}");
            Ok(())
        }
        Some(command) => Err(format!("unknown accounts command {command}").into()),
    }
}

async fn status_command() -> Result<(), Box<dyn Error>> {
    let path = config::accounts_path()?;
    let accounts = config::load_accounts(&path)?;
    if accounts.is_empty() {
        return Err("No accounts configured. Run accounts add".into());
    }
    let context7 = Context7::new()?;
    for account in accounts {
        match context7.probe_quota(&account.api_key).await {
            Ok(quota) => println!("{}: {}", account.name, quota_summary(&quota)),
            Err(error) => println!("{}: error: {}", account.name, error.message()),
        }
    }
    Ok(())
}

fn config_command() -> Result<(), Box<dyn Error>> {
    println!("accounts: {}", config::accounts_path()?.display());
    println!("cache: {}", config::cache_path()?.display());
    println!("runtime: {}", config::runtime_path()?.display());
    println!("listen: dynamic loopback port");
    Ok(())
}

fn reject_extra_args(
    args: &mut impl Iterator<Item = String>,
    usage: &str,
) -> Result<(), Box<dyn Error>> {
    if args.next().is_some() {
        return Err(format!("usage: {usage}").into());
    }
    Ok(())
}

fn print_help() {
    println!(
        "context7-account-broker\n\nCommands:\n  start                 Start or reuse the shared broker and print plugin connection JSON (default)\n  accounts list         List configured accounts\n  accounts add NAME     Add an account and prompt for its API key\n  accounts remove NAME  Remove an account\n  status                Probe each account quota\n  config                Print effective configuration\n  help                  Show this help"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exposes_only_canonical_typed_tools() {
        let resolve = McpHandler::resolve_library_id_tool_attr();
        let query = McpHandler::query_docs_tool_attr();
        assert_eq!(resolve.name, "resolve-library-id");
        assert_eq!(query.name, "query-docs");
        assert_eq!(
            resolve.title.as_deref(),
            Some("Resolve Context7 Library ID")
        );
        assert_eq!(query.title.as_deref(), Some("Query Context7 Documentation"));
        assert!(
            resolve
                .description
                .as_ref()
                .unwrap()
                .starts_with("Resolves a package/product name to a Context7-compatible library ID")
        );
        assert!(
            query
                .description
                .as_ref()
                .unwrap()
                .starts_with("Retrieves and queries up-to-date documentation and code examples")
        );
        for tool in [&resolve, &query] {
            assert_eq!(tool.input_schema["additionalProperties"], false);
            let annotations = tool.annotations.as_ref().unwrap();
            assert_eq!(annotations.read_only_hint, Some(true));
            assert_eq!(annotations.destructive_hint, Some(false));
            assert_eq!(annotations.idempotent_hint, Some(true));
            assert_eq!(annotations.open_world_hint, Some(true));
        }
        assert_eq!(
            resolve.input_schema["required"],
            json!(["query", "libraryName"])
        );
        assert_eq!(
            query.input_schema["required"],
            json!(["libraryId", "query"])
        );
        assert!(
            serde_json::from_value::<ResolveInput>(json!({
                "query": "q",
                "libraryName": "name",
                "question": "alias"
            }))
            .is_err()
        );
    }

    #[test]
    fn rejects_extra_account_arguments_before_side_effects() {
        assert!(
            accounts_command(
                ["remove", "personal", "extra"]
                    .map(str::to_owned)
                    .into_iter()
            )
            .is_err()
        );
        assert!(
            accounts_command(
                ["add", "personal", "ctx7sk-secret"]
                    .map(str::to_owned)
                    .into_iter()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn health_requires_the_token_and_discovers_the_port() {
        let directory = tempfile::tempdir().unwrap();
        let context7 = Context7::with_sender(|_, _| async {
            Err(context7::Error::Network("unused".to_owned()))
        });
        let broker = Broker::new(
            vec![config::AccountRecord {
                name: "test".to_owned(),
                api_key: "ctx7sk-test".to_owned(),
            }],
            context7,
            directory.path().join("cache"),
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let token = "0".repeat(64);
        let app = application(broker, &token, cancellation);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/health");
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::builder().timeout(HEALTH_TIMEOUT).build().unwrap();
        assert_eq!(
            client
                .get(url.replace("/health", "/mcp"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(!broker_healthy(&client, port, "wrong").await);
        assert!(broker_healthy(&client, port, &token).await);
        let runtime_path = directory.path().join("runtime");
        let state_path = runtime_path.join("broker.json");
        let state = ServerState { port, token };
        config::write_private(&state_path, &serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(active_launch(&client, &state_path).await, Some(state));
        let lock_path = runtime_path.join("broker.lock");
        assert!(!broker_running(&lock_path).unwrap());
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        lock.lock().unwrap();
        assert!(broker_running(&lock_path).unwrap());
        server.abort();
        let _ = server.await;
    }
}
