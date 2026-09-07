use super::{
    Result,
    config::Config,
    error,
    jupyter::{self, Jupyter},
    malformed,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use notebook_protocol::*;
use notebook_runtime::{OutputState, collaboration::Collaboration};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;
mod playground;

struct Cached {
    fingerprint: u64,
    result: Option<CommandResult>,
}
#[derive(Default)]
struct Authority {
    collaboration: Collaboration,
    revisions: HashMap<String, (Vec<Cell>, u64)>,
    cache: BTreeMap<String, Cached>,
    cache_bytes: usize,
    locks: HashMap<String, Arc<Mutex<()>>>,
    uncertain: HashSet<String>,
}
struct Host {
    config: Arc<Config>,
    jupyter: Jupyter,
    authority: Mutex<Authority>,
    started: Instant,
    artifact_lock: Mutex<()>,
    playground: Mutex<Option<playground::Playground>>,
}
impl Host {
    fn now(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}
type App = Arc<Host>;
fn public_error(error: ProtocolError) -> Response {
    let status = if error.code == ErrorCode::NotDriver {
        StatusCode::FORBIDDEN
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, Json(error)).into_response()
}
fn headers_value<'a>(headers: &'a HeaderMap, key: &str) -> &'a str {
    headers.get(key).and_then(|v| v.to_str().ok()).unwrap_or("")
}
fn path(host: &Host, headers: &HeaderMap) -> Result<String> {
    let raw = headers_value(headers, "x-notebook-path");
    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| malformed())?;
    host.config.path(
        if raw.is_empty() {
            &host.config.notebook
        } else {
            &decoded
        },
        false,
    )
}
fn writes(kind: &NotebookCommandKind) -> bool {
    !matches!(
        kind,
        NotebookCommandKind::Query { .. }
            | NotebookCommandKind::Reconnect
            | NotebookCommandKind::Complete { .. }
            | NotebookCommandKind::Inspect { .. }
            | NotebookCommandKind::ReadMicroscope { .. }
            | NotebookCommandKind::Setup { create: false, .. }
    )
}

fn blocked_by_uncertain_execution(kind: &NotebookCommandKind) -> bool {
    matches!(kind, NotebookCommandKind::ExecuteCell { .. })
}
fn empty_result(command: &NotebookCommand) -> CommandResult {
    CommandResult {
        microscope: None,
        protocol_version: 1,
        command_id: command.command_id,
        idempotency_key: command.idempotency_key.clone(),
        base_revision: None,
        committed_revision: None,
        snapshot: None,
        completion: None,
        inspection: None,
        error: None,
    }
}
fn failed(command: &NotebookCommand, error: ProtocolError) -> CommandResult {
    let mut result = empty_result(command);
    result.error = Some(error);
    result
}

pub async fn serve() -> Result<()> {
    let config = Arc::new(Config::load()?);
    let host = Arc::new(Host {
        jupyter: Jupyter::new(config.clone())?,
        config: config.clone(),
        authority: Mutex::new(Authority::default()),
        started: Instant::now(),
        artifact_lock: Mutex::new(()),
        playground: Mutex::new(None),
    });
    let mut router = Router::new()
        .route(
            "/api/v1/playground",
            get(playground::read).post(playground::open),
        )
        .route("/api/v1/playground/close", post(playground::close))
        .route(
            "/api/v1/playground/{id}/commands",
            post(playground::command),
        )
        .route(
            "/api/v1/playground/{id}/commands/stream",
            post(playground::stream),
        )
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/readyz", get(ready))
        .route("/api/v1/config", get(configuration))
        .route("/api/v1/notebooks", get(list))
        .route(
            "/api/v1/artifacts",
            post(create_artifact).layer(DefaultBodyLimit::max(1_400_000)),
        )
        .route("/api/v1/download", get(download))
        .route("/api/v1/workspace-export", get(export_workspace))
        .route("/api/v1/commands", post(command))
        .route("/api/v1/commands/stream", post(stream))
        .route("/api/v1/collaboration/join", post(join))
        .route("/api/v1/collaboration/leave", post(leave))
        .route("/api/v1/collaboration/driver/{target}", post(handoff))
        .route("/api/v1/collaboration/release", post(release_driver))
        .route("/api/v1/collaboration/claim", post(claim_driver))
        .route("/api/v1/collaboration/events", get(events))
        .route("/api/v1/collaboration/view", get(view).post(publish_view));
    if let Some(directory) = &config.static_dir {
        router = router.fallback_service(tower_http::services::ServeDir::new(directory));
    }
    let router = router
        .layer(DefaultBodyLimit::max(config.request_limit))
        .layer(middleware::from_fn_with_state(config.clone(), origin_guard))
        .with_state(host.clone());
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .map_err(|_| error(ErrorCode::Internal, "Could not bind gateway listener"))?;
    let cleanup = tokio::spawn(playground::reap(host.clone()));
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|_| error(ErrorCode::Internal, "Gateway listener failed"))?;
    // Socket cancellation cannot cancel accepted notebook effects. Drain them.
    while host
        .authority
        .lock()
        .await
        .collaboration
        .rooms
        .values()
        .any(|r| r.active > 0)
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cleanup.abort();
    playground::dispose(&host).await?;
    Ok(())
}
async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
async fn origin_guard(State(config): State<Arc<Config>>, request: Request, next: Next) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if let Some(origin) = request.headers().get(header::ORIGIN)
        && !origin.to_str().is_ok_and(|value| {
            config.origin_allowed(value, headers_value(request.headers(), "host"))
        })
    {
        return (StatusCode::FORBIDDEN, Json(json!({"code":"invalid_input"}))).into_response();
    }
    let preflight = request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut response = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    if let Some(origin) = origin.and_then(|value| HeaderValue::from_str(&value).ok()) {
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static(
                "content-type, x-notebook-path, x-notebook-client, x-notebook-target-client",
            ),
        );
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
async fn ready(State(host): State<App>) -> Response {
    match host.jupyter.discover().await {
        Ok(profile) => Json(json!({"status":"ready","jupyter_profile":profile})).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status":"not_ready","jupyter_profile":null})),
        )
            .into_response(),
    }
}
async fn configuration(State(host): State<App>) -> Response {
    Json(json!({"path":host.config.path(&host.config.notebook,false).unwrap_or_default(),"kernel":host.config.kernel,"default_kernel_profile":host.config.default_profile,"kernel_profiles":host.config.kernel_profiles.iter().map(|(id,p)| json!({"id":id,"kernelspec":p.kernelspec})).collect::<Vec<_>>()})).into_response()
}
async fn list(State(host): State<App>, Query(query): Query<HashMap<String, String>>) -> Response {
    match host
        .jupyter
        .list(query.get("directory").map(String::as_str).unwrap_or(""))
        .await
    {
        Ok(value) => Json(value).into_response(),
        Err(e) => public_error(e),
    }
}
async fn create_artifact(
    State(host): State<App>,
    headers: HeaderMap,
    Json(input): Json<super::artifacts::CreateArtifact>,
) -> Response {
    // An accepted upload must finish even if its browser disconnects.
    let task = tokio::spawn(async move {
        let _serial = host.artifact_lock.try_lock().map_err(|_| {
            error(
                ErrorCode::ExecutionRejected,
                "Another workspace upload is running; wait for it to finish",
            )
        })?;
        let scope = path(&host, &headers)?;
        let target = host.config.path(&input.path, true)?;
        input.body()?;
        let lock = {
            let mut authority = host.authority.lock().await;
            authority.collaboration.require_driver(
                &scope,
                headers_value(&headers, "x-notebook-client"),
                host.now(),
            )?;
            if authority.locks.len() >= 4096 {
                return Err(error(
                    ErrorCode::BoundsExceeded,
                    "Workspace operation limit reached; restart gateway",
                ));
            }
            authority.collaboration.room(&scope)?.active += 1;
            authority.locks.entry(target).or_default().clone()
        };
        let _target = lock.lock().await;
        let result = host.jupyter.create_artifact(input).await;
        if let Ok(room) = host.authority.lock().await.collaboration.room(&scope) {
            room.active = room.active.saturating_sub(1);
        }
        result
    });
    match task.await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(e)) => public_error(e),
        Err(_) => public_error(error(
            ErrorCode::Internal,
            "Upload outcome unknown; refresh before retrying",
        )),
    }
}
async fn download(State(host): State<App>, headers: HeaderMap) -> Response {
    let result = async { host.jupyter.read(&path(&host, &headers)?).await }.await;
    match result {
        Ok(raw) => (
            [
                (header::CONTENT_TYPE, "application/x-ipynb+json"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"notebook.ipynb\"",
                ),
            ],
            raw.to_string(),
        )
            .into_response(),
        Err(e) => public_error(e),
    }
}
async fn export_workspace(State(host): State<App>) -> Response {
    let Ok(_serial) = host.artifact_lock.try_lock() else {
        return public_error(error(
            ErrorCode::InvalidInput,
            "Workspace is busy; wait for operations to finish before exporting",
        ));
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        host.jupyter.export_workspace(),
    )
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(e)) => public_error(e),
        Err(_) => public_error(error(ErrorCode::Timeout, "Workspace export timed out")),
    }
}
async fn join(State(host): State<App>, headers: HeaderMap) -> Response {
    let result = async {
        let path = path(&host, &headers)?;
        let given = headers_value(&headers, "x-notebook-client");
        let token = if given.is_empty() {
            format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
        } else {
            given.into()
        };
        host.authority.lock().await.collaboration.join(
            &path,
            &token,
            given
                .is_empty()
                .then(|| Uuid::new_v4().simple().to_string()),
            host.now(),
        )
    }
    .await;
    match result {
        Ok(state) => Json(state).into_response(),
        Err(e) => public_error(e),
    }
}
async fn leave(State(host): State<App>, headers: HeaderMap) -> Response {
    let result = async {
        host.authority.lock().await.collaboration.leave(
            &path(&host, &headers)?,
            headers_value(&headers, "x-notebook-client"),
            host.now(),
        )
    }
    .await;
    unit_response(result)
}
fn unit_response(result: Result<()>) -> Response {
    match result {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(e) => public_error(e),
    }
}
async fn handoff(
    State(host): State<App>,
    headers: HeaderMap,
    Path(target): Path<String>,
) -> Response {
    let result = async {
        let mut authority = host.authority.lock().await;
        authority.collaboration.require_driver(
            &path(&host, &headers)?,
            headers_value(&headers, "x-notebook-client"),
            host.now(),
        )?;
        authority.collaboration.change_driver(&target)
    }
    .await;
    unit_response(result)
}
async fn release_driver(State(host): State<App>, headers: HeaderMap) -> Response {
    let result = async {
        host.authority.lock().await.collaboration.release_driver(
            &path(&host, &headers)?,
            headers_value(&headers, "x-notebook-client"),
            host.now(),
        )
    }
    .await;
    unit_response(result)
}
async fn claim_driver(State(host): State<App>, headers: HeaderMap) -> Response {
    let result = async {
        host.authority.lock().await.collaboration.claim_driver(
            &path(&host, &headers)?,
            headers_value(&headers, "x-notebook-client"),
            host.now(),
        )
    }
    .await;
    unit_response(result)
}
async fn publish_view(
    State(host): State<App>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Response {
    let result = async {
        host.config.path(
            input["notebook_path"].as_str().ok_or_else(malformed)?,
            false,
        )?;
        host.authority.lock().await.collaboration.publish_view(
            &path(&host, &headers)?,
            headers_value(&headers, "x-notebook-client"),
            headers_value(&headers, "x-notebook-target-client"),
            input,
            host.now(),
        )
    }
    .await;
    unit_response(result)
}
async fn events(
    State(host): State<App>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    poll(host, headers, query, false).await
}
async fn view(
    State(host): State<App>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    poll(host, headers, query, true).await
}
async fn poll(
    host: App,
    headers: HeaderMap,
    query: HashMap<String, String>,
    view: bool,
) -> Response {
    let result = async {
        let path = path(&host,&headers)?;
        let token = headers_value(&headers,"x-notebook-client");
        let after: i64 = query.get("after").and_then(|v| v.parse().ok()).unwrap_or(-1);
        let deadline = Instant::now()+Duration::from_secs(10);
        loop {
            let value = {
                let mut authority = host.authority.lock().await;
                let path = authority.collaboration.event_path(&path,token);
                let state = authority.collaboration.state(&path,token,host.now())?;
                if view { json!({"sequence":authority.collaboration.view_sequence,"view":authority.collaboration.view}) } else { state }
            };
            if value["sequence"].as_i64().unwrap_or(0) > after || Instant::now() >= deadline { return Ok::<_,ProtocolError>(value); }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }.await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => public_error(e),
    }
}
async fn command(
    State(host): State<App>,
    headers: HeaderMap,
    Json(input): Json<NotebookCommand>,
) -> Response {
    let task = tokio::spawn(async move { dispatch(host, headers, input, None).await });
    match task.await {
        Ok(result) => Json(result).into_response(),
        Err(_) => public_error(error(
            ErrorCode::Internal,
            "Command task failed; refresh before retrying",
        )),
    }
}
async fn stream(
    State(host): State<App>,
    headers: HeaderMap,
    Json(input): Json<NotebookCommand>,
) -> Response {
    let (sender, receiver) = mpsc::channel::<String>(16);
    tokio::spawn(async move {
        let result = if matches!(input.kind, NotebookCommandKind::ExecuteCell { .. }) {
            dispatch(host, headers, input, Some(sender.clone())).await
        } else {
            failed(
                &input,
                error(
                    ErrorCode::UnsupportedOperation,
                    "Only cell execution supports streaming",
                ),
            )
        };
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            sender.send(format!("{}\n", serde_json::to_string(&result).unwrap())),
        )
        .await;
    });
    (
        [
            (header::CONTENT_TYPE, "application/x-ndjson"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(ReceiverStream::new(receiver).map(Ok::<_, std::convert::Infallible>)),
    )
        .into_response()
}

async fn dispatch(
    host: App,
    headers: HeaderMap,
    command: NotebookCommand,
    progress: Option<mpsc::Sender<String>>,
) -> CommandResult {
    if let Err(e) = validate_command(&command) {
        return failed(&command, e);
    }
    // Namespace changes share the create-only upload guard, including rename
    // whose destination differs from its normal notebook lock key.
    let _namespace = if matches!(
        command.kind,
        NotebookCommandKind::Setup { create: true, .. }
            | NotebookCommandKind::RenameNotebook { .. }
            | NotebookCommandKind::CreateMicroscope { .. }
            | NotebookCommandKind::SetMicroscopeWalkthrough { .. }
            | NotebookCommandKind::DeleteMicroscope { .. }
    ) {
        match host.artifact_lock.try_lock() {
            Ok(guard) => Some(guard),
            Err(_) => {
                return failed(
                    &command,
                    error(
                        ErrorCode::ExecutionRejected,
                        "Another workspace file operation is running; wait for it to finish",
                    ),
                );
            }
        }
    } else {
        None
    };
    let mut path = match path(&host, &headers) {
        Ok(path) => path,
        Err(e) => return failed(&command, e),
    };
    if let NotebookCommandKind::Setup {
        path: requested, ..
    } = &command.kind
    {
        let requested = match host.config.path(requested, false) {
            Ok(p) => p,
            Err(e) => return failed(&command, e),
        };
        if !headers_value(&headers, "x-notebook-path").is_empty() && path != requested {
            return failed(
                &command,
                error(
                    ErrorCode::InvalidInput,
                    "Notebook identity does not match request scope",
                ),
            );
        }
        path = requested;
    }
    let token = headers_value(&headers, "x-notebook-client");
    let key = format!("{path}\n{token}\n{}", command.idempotency_key);
    let fingerprint = {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(&command).unwrap().hash(&mut hasher);
        hasher.finish()
    };
    let write = writes(&command.kind);
    let begin = async {
        let mut authority = host.authority.lock().await;
        if write {
            authority
                .collaboration
                .require_driver(&path, token, host.now())?;
        }
        if let Some(cached) = authority.cache.get(&key) {
            if cached.fingerprint != fingerprint {
                return Err(error(
                    ErrorCode::DuplicateCommand,
                    "Idempotency key was used for a different command",
                ));
            }
            return cached.result.clone().map(Some).ok_or_else(|| {
                error(
                    ErrorCode::ExecutionRejected,
                    "Command already accepted; refresh before retrying",
                )
            });
        }
        if authority.uncertain.contains(&path) && blocked_by_uncertain_execution(&command.kind) {
            return Err(error(
                ErrorCode::ExecutionRejected,
                "Execution outcome is uncertain; restart the kernel before executing again",
            ));
        }
        if authority.collaboration.room(&path)?.active > 0
            && !matches!(command.kind, NotebookCommandKind::InterruptKernel)
        {
            if matches!(
                command.kind,
                NotebookCommandKind::Query { .. } | NotebookCommandKind::Reconnect
            ) && let Some(snapshot) = &authority.collaboration.room(&path)?.snapshot
            {
                let mut result = empty_result(&command);
                result.base_revision = command.expected_revision;
                result.committed_revision = Some(snapshot.revision);
                result.snapshot = Some(snapshot.clone());
                return Ok(Some(result));
            }
            return Err(error(
                ErrorCode::ExecutionRejected,
                "Another command is running; retry when idle",
            ));
        }
        if let NotebookCommandKind::RenameNotebook { path: new } = &command.kind {
            let new = host.config.path(new, false)?;
            authority.collaboration.can_rename(&path, &new)?;
        }
        if authority.cache.len() >= 4096 {
            return Err(error(
                ErrorCode::BoundsExceeded,
                "Gateway command ledger is full; restart gateway",
            ));
        }
        authority.cache.insert(
            key.clone(),
            Cached {
                fingerprint,
                result: None,
            },
        );
        if write {
            authority.collaboration.room(&path)?.active += 1;
        }
        Ok(None)
    }
    .await;
    match begin {
        Ok(Some(result)) => return result,
        Err(e) => return failed(&command, e),
        Ok(None) => {}
    }
    let lock = {
        let mut authority = host.authority.lock().await;
        authority.locks.entry(path.clone()).or_default().clone()
    };
    let _guard = if matches!(command.kind, NotebookCommandKind::InterruptKernel) {
        None
    } else {
        Some(lock.lock().await)
    };
    let outcome = run(&host, &path, token, &command, progress).await;
    let result = outcome.unwrap_or_else(|e| failed(&command, e));
    let mut authority = host.authority.lock().await;
    if result.error.is_some() && matches!(command.kind, NotebookCommandKind::ExecuteCell { .. }) {
        authority.uncertain.insert(path.clone());
        if let Ok(room) = authority.collaboration.room(&path)
            && let Some(snapshot) = &mut room.snapshot
        {
            snapshot.kernel.state = KernelState::Error;
            room.sequence += 1;
        }
    }
    if result.error.is_none() && matches!(command.kind, NotebookCommandKind::RestartKernel) {
        authority.uncertain.remove(&path);
    }
    if write && let Ok(room) = authority.collaboration.room(&path) {
        room.active = room.active.saturating_sub(1);
    }
    if result.error.is_none()
        && let NotebookCommandKind::RenameNotebook { path: new } = &command.kind
        && let Ok(new) = host.config.path(new, false)
    {
        authority.collaboration.rename(&path, &new);
        if let Some(snapshot) = &result.snapshot {
            let _ = authority
                .collaboration
                .publish(&new, token, snapshot.clone());
        }
    }
    // Results are capped independently from the durable-in-process ledger.
    // Eviction removes replay data, never the fact that an effect was accepted.
    let size = serde_json::to_vec(&result).unwrap().len();
    if authority.cache_bytes + size > 16_000_000 {
        for entry in authority.cache.values_mut() {
            entry.result = None;
        }
        authority.cache_bytes = 0;
    }
    if let Some(entry) = authority.cache.get_mut(&key) {
        entry.result = Some(result.clone());
        authority.cache_bytes += size;
    }
    result
}
async fn committed(
    host: &Host,
    path: &str,
    token: &str,
    raw: &Value,
    state: KernelState,
) -> Result<NotebookSnapshot> {
    let mut snapshot = host.jupyter.snapshot(path, raw, 0, state)?;
    let mut authority = host.authority.lock().await;
    let revision = authority
        .revisions
        .entry(path.into())
        .or_insert_with(|| (Vec::new(), 0));
    if revision.0 != snapshot.cells || revision.1 == 0 {
        revision.0 = snapshot.cells.clone();
        revision.1 += 1;
    }
    snapshot.revision = revision.1;
    authority
        .collaboration
        .publish(path, token, snapshot.clone())?;
    Ok(snapshot)
}
async fn run(
    host: &Host,
    path: &str,
    token: &str,
    command: &NotebookCommand,
    progress: Option<mpsc::Sender<String>>,
) -> Result<CommandResult> {
    let mut result = empty_result(command);
    let base = host
        .authority
        .lock()
        .await
        .revisions
        .get(path)
        .map(|(_, revision)| *revision);
    result.base_revision = base;
    if command.expected_revision.is_some() && base.is_some() && command.expected_revision != base {
        return Err(error(
            ErrorCode::StaleRevision,
            "Notebook revision changed; refresh and retry",
        ));
    }
    use NotebookCommandKind::*;
    let mut output_path = path.to_owned();
    let raw = match &command.kind {
        CreateMicroscope {
            cell_id,
            microscope_id,
            ..
        }
        | SetMicroscopeWalkthrough {
            cell_id,
            microscope_id,
            ..
        }
        | DeleteMicroscope {
            cell_id,
            microscope_id,
        }
        | ReadMicroscope {
            cell_id,
            microscope_id,
        } => {
            let mut raw = host.jupyter.read(path).await?;
            let current = committed(host, path, token, &raw, KernelState::Idle).await?;
            let proposed = notebook_runtime::prepare(current.clone(), command.clone()).map_err(
                |e| match e {
                    notebook_core::DomainError::Protocol(e) => e,
                    _ => error(
                        ErrorCode::StaleRevision,
                        "Notebook changed; refresh before changing microscopes",
                    ),
                },
            )?;
            let create = matches!(command.kind, CreateMicroscope { .. });
            let mut doc = notebook_protocol::microscope::document(
                if create { &proposed } else { &current },
                cell_id,
                microscope_id,
            )?;
            if let CreateMicroscope { walkthrough, .. } = &command.kind {
                doc.walkthrough = Some(walkthrough.clone());
            }
            let stored = host.jupyter.read_microscope(&doc).await?;
            let replacement = if let SetMicroscopeWalkthrough { walkthrough, .. } = &command.kind {
                let mut updated =
                    notebook_protocol::microscope::document(&proposed, cell_id, microscope_id)?;
                updated.walkthrough = Some(walkthrough.clone());
                Some(updated)
            } else {
                None
            };
            let original = raw.clone();
            if matches!(
                command.kind,
                SetMicroscopeWalkthrough { .. } | DeleteMicroscope { .. }
            ) {
                let affected = host
                    .playground
                    .lock()
                    .await
                    .as_ref()
                    .is_some_and(|p| p.belongs_to(path, cell_id, microscope_id));
                if affected {
                    playground::dispose(host).await?;
                }
            }
            if matches!(command.kind, ReadMicroscope { .. }) {
                result.microscope = Some(stored.ok_or_else(|| {
                    error(
                        ErrorCode::InvalidInput,
                        "Microscope content file is missing",
                    )
                })?);
            } else {
                if let Some(updated) = &replacement {
                    let previous = stored.as_ref().ok_or_else(|| {
                        error(
                            ErrorCode::InvalidInput,
                            "Microscope content file is missing",
                        )
                    })?;
                    host.jupyter.replace_microscope(previous, updated).await?;
                    result.microscope = Some(updated.clone());
                } else if create {
                    if stored.is_some() {
                        return Err(error(
                            ErrorCode::InvalidInput,
                            "Microscope sidecar already exists",
                        ));
                    }
                    host.jupyter.create_microscope(&doc).await?;
                } else if stored.is_some() {
                    host.jupyter.delete_microscope(&doc).await?;
                }
                jupyter::merge_cells(&mut raw, &proposed)?;
                if let Err(failure) = host.jupyter.save(path, &raw).await {
                    // Contents has no multi-file transaction. Restore the sidecar on
                    // a confirmed failed notebook write, but never claim atomicity.
                    let observed = host.jupyter.read(path).await;
                    if observed.as_ref().is_ok_and(|value| value == &raw) {
                        // The write succeeded but its acknowledgement was lost.
                    } else {
                        if create && observed.as_ref().is_ok_and(|value| value == &original) {
                            let _ = host.jupyter.delete_microscope(&doc).await;
                        } else if let Some(previous) = &stored {
                            if let Some(updated) = &replacement {
                                if observed.as_ref().is_ok_and(|value| value == &original) {
                                    let _ =
                                        host.jupyter.replace_microscope(updated, previous).await;
                                }
                            } else {
                                let _ = host.jupyter.create_microscope(previous).await;
                            }
                        }
                        return Err(failure);
                    }
                }
            }
            raw
        }
        Setup { create, kernel, .. } => {
            host.jupyter.setup(path, *create, kernel.as_deref()).await?
        }
        Query { .. } | Reconnect => {
            host.jupyter.ensure_kernel(path).await?;
            host.jupyter.read(path).await?
        }
        ModifyCells { .. } => {
            let mut raw = host.jupyter.read(path).await?;
            let current = committed(host, path, token, &raw, KernelState::Idle).await?;
            let proposed = notebook_runtime::prepare(current, command.clone()).map_err(
                |failure| match failure {
                    notebook_core::DomainError::Protocol(e) => e,
                    _ => error(
                        ErrorCode::StaleRevision,
                        "Notebook revision changed; refresh and retry",
                    ),
                },
            )?;
            jupyter::merge_cells(&mut raw, &proposed)?;
            host.jupyter.save(path, &raw).await?;
            raw
        }
        ExecuteCell { cell_id } => {
            execute(host, path, token, command, cell_id, &result, progress).await?
        }
        Complete { code, cursor_pos } | Inspect { code, cursor_pos } => {
            let inspect = matches!(command.kind, Inspect { .. });
            let reply = host
                .jupyter
                .language(
                    path,
                    if inspect { "inspect" } else { "complete" },
                    code,
                    *cursor_pos,
                    command.timeout_ms,
                )
                .await?;
            if inspect {
                result.inspection = Some(InspectionReply {
                    found: reply["found"].as_bool().unwrap_or(false),
                    text: reply["data"]["text/plain"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(32768)
                        .collect(),
                });
            } else {
                let matches = reply["matches"]
                    .as_array()
                    .ok_or_else(malformed)?
                    .iter()
                    .take(100)
                    .map(|v| {
                        v.as_str()
                            .ok_or_else(malformed)
                            .map(|s| s.chars().take(512).collect())
                    })
                    .collect::<Result<_>>()?;
                let start = reply["cursor_start"].as_u64().ok_or_else(malformed)? as usize;
                let end = reply["cursor_end"].as_u64().ok_or_else(malformed)? as usize;
                if start > end || end > code.chars().count() {
                    return Err(malformed());
                }
                result.completion = Some(CompletionReply {
                    matches,
                    cursor_start: start,
                    cursor_end: end,
                });
            }
            result.committed_revision = base;
            return Ok(result);
        }
        InterruptKernel => {
            host.jupyter.kernel_action(path, "interrupt").await?;
            host.jupyter.read(path).await?
        }
        RestartKernel => {
            host.jupyter.kernel_action(path, "restart").await?;
            host.jupyter.read(path).await?
        }
        CreateCheckpoint => {
            if host
                .jupyter
                .request_for_path(
                    path,
                    reqwest::Method::POST,
                    &format!("api/contents/{path}/checkpoints"),
                    None,
                )
                .await?
                .0
                != 201
            {
                return Err(error(
                    ErrorCode::TransportError,
                    "Checkpoint could not be created",
                ));
            }
            host.jupyter.read(path).await?
        }
        RenameNotebook { path: new } => {
            let raw = host.jupyter.read(path).await?;
            let snapshot =
                host.jupyter
                    .snapshot(path, &raw, base.unwrap_or(0), KernelState::Idle)?;
            if snapshot.cells.iter().any(|cell| {
                !notebook_protocol::microscope::list(cell)
                    .unwrap_or_default()
                    .is_empty()
            }) {
                return Err(error(
                    ErrorCode::InvalidInput,
                    "Delete microscopes before renaming this notebook",
                ));
            }
            notebook_runtime::prepare(snapshot, command.clone())
                .map_err(|_| error(ErrorCode::InvalidInput, "Notebook rename is invalid"))?;
            output_path = host.config.path(new, false)?;
            host.jupyter.rename(path, &output_path).await?;
            let mut authority = host.authority.lock().await;
            if let Some(revision) = authority.revisions.remove(path) {
                authority.revisions.insert(output_path.clone(), revision);
            }
            drop(authority);
            host.jupyter.read(&output_path).await?
        }
        Close => host.jupyter.read(path).await?,
        _ => {
            return Err(error(
                ErrorCode::UnsupportedOperation,
                "Unsupported notebook command",
            ));
        }
    };
    let snapshot = committed(host, &output_path, token, &raw, KernelState::Idle).await?;
    result.committed_revision = Some(snapshot.revision);
    result.snapshot = Some(snapshot);
    if serde_json::to_vec(&result).unwrap().len() > host.config.response_limit {
        return Err(error(
            ErrorCode::BoundsExceeded,
            "Response exceeds configured limit",
        ));
    }
    Ok(result)
}
async fn execute(
    host: &Host,
    path: &str,
    token: &str,
    command: &NotebookCommand,
    cell_id: &str,
    base: &CommandResult,
    progress: Option<mpsc::Sender<String>>,
) -> Result<Value> {
    let mut raw = host.jupyter.read(path).await?;
    let index = raw["cells"]
        .as_array()
        .ok_or_else(malformed)?
        .iter()
        .position(|c| c["id"] == cell_id)
        .ok_or_else(|| error(ErrorCode::InvalidInput, "Cell identity is stale"))?;
    let snapshot = host.jupyter.snapshot(path, &raw, 0, KernelState::Idle)?;
    if snapshot.cells[index].cell_type != CellType::Code {
        return Err(error(ErrorCode::InvalidInput, "Only code cells execute"));
    }
    let source = snapshot.cells[index].source.clone();
    let mut socket = host.jupyter.socket(path).await?;
    raw["cells"][index]["outputs"] = json!([]);
    raw["cells"][index]["execution_count"] = Value::Null;
    emit(host, path, token, &raw, base, &progress).await?;
    let id=jupyter::send(&mut socket,"execute_request",json!({"code":source,"silent":false,"store_history":true,"user_expressions":{},"allow_stdin":false,"stop_on_error":true})).await?;
    let mut outputs = OutputState::default();
    let execution = tokio::time::timeout(Duration::from_millis(command.timeout_ms.into()), async {
        let mut replied = false;
        let mut idle = false;
        while !replied || !idle {
            let message = jupyter::receive(&mut socket).await?;
            if message["parent_header"]["msg_id"] != id {
                continue;
            }
            let kind = message["header"]["msg_type"]
                .as_str()
                .ok_or_else(malformed)?;
            match kind {
                "execute_reply" => {
                    replied = true;
                    raw["cells"][index]["execution_count"] =
                        message["content"]["execution_count"].clone();
                }
                "status" => {
                    if message["content"]["execution_state"] == "idle" {
                        idle = true;
                    }
                }
                "execute_input" => {
                    raw["cells"][index]["execution_count"] =
                        message["content"]["execution_count"].clone();
                    emit(host, path, token, &raw, base, &progress).await?;
                }
                "stream"
                | "display_data"
                | "execute_result"
                | "update_display_data"
                | "clear_output"
                | "error" => {
                    outputs.apply_jupyter_message(&message)?;
                    raw["cells"][index]["outputs"] = Value::Array(
                        outputs
                            .outputs()
                            .iter()
                            .map(jupyter::output_nb)
                            .collect::<Result<_>>()?,
                    );
                    emit(host, path, token, &raw, base, &progress).await?;
                }
                _ => {}
            }
        }
        emit(host, path, token, &raw, base, &progress).await?;
        Ok::<_, ProtocolError>(())
    })
    .await;
    match execution {
        Ok(Ok(())) => Ok(raw),
        outcome => {
            let _ = host.jupyter.kernel_action(path, "interrupt").await;
            match outcome {
                Err(_) => Err(error(
                    ErrorCode::Timeout,
                    "Cell timed out; interrupt requested; refresh before retrying",
                )),
                Ok(Err(e)) => Err(e),
                _ => unreachable!(),
            }
        }
    }
}
async fn emit(
    host: &Host,
    path: &str,
    token: &str,
    raw: &Value,
    base: &CommandResult,
    progress: &Option<mpsc::Sender<String>>,
) -> Result<()> {
    host.jupyter.save(path, raw).await?;
    let snapshot = committed(host, path, token, raw, KernelState::Busy).await?;
    if let Some(sender) = progress {
        let mut result = base.clone();
        result.committed_revision = Some(snapshot.revision);
        result.snapshot = Some(snapshot);
        let encoded = serde_json::to_string(&result).map_err(|_| malformed())?;
        if encoded.len() > host.config.response_limit {
            return Err(error(
                ErrorCode::BoundsExceeded,
                "Progress response exceeds limit",
            ));
        }
        let _ = sender.try_send(format!("{encoded}\n"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::blocked_by_uncertain_execution;
    use notebook_protocol::{CellMutation, NotebookCommandKind};

    #[test]
    fn uncertain_execution_blocks_execution_but_not_notebook_edits() {
        assert!(blocked_by_uncertain_execution(
            &NotebookCommandKind::ExecuteCell {
                cell_id: "cell-1".into(),
            }
        ));
        assert!(!blocked_by_uncertain_execution(
            &NotebookCommandKind::ModifyCells {
                changes: vec![CellMutation::Delete {
                    cell_id: "cell-1".into(),
                }],
            }
        ));
        assert!(!blocked_by_uncertain_execution(
            &NotebookCommandKind::RestartKernel
        ));
    }
}
