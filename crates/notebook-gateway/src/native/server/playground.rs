//! Temporary single-cell execution. Never writes a notebook or borrows its kernel.
use super::*;
use serde::Deserialize;

pub(super) struct Playground {
    id: String,
    owner: String,
    notebook_path: String,
    cell_id: String,
    microscope_id: String,
    step_index: usize,
    step_id: String,
    step_title: String,
    kernel_id: String,
    session_id: String,
    snapshot: NotebookSnapshot,
    busy: bool,
    closing: bool,
    touched: Instant,
    seen: HashMap<String, (NotebookCommand, Option<CommandResult>)>,
}
impl Playground {
    pub(super) fn belongs_to(&self, path: &str, cell: &str, microscope: &str) -> bool {
        self.notebook_path == path && self.cell_id == cell && self.microscope_id == microscope
    }
    fn public(&self) -> Value {
        json!({"id":self.id,"notebook_path":self.notebook_path,"cell_id":self.cell_id,
            "microscope_id":self.microscope_id,"step_index":self.step_index,
            "step_id":self.step_id,"step_title":self.step_title,
            "snapshot":self.snapshot,"closing":self.closing})
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Open {
    cell_id: String,
    microscope_id: String,
    step_index: usize,
}
async fn driver(host: &Host, headers: &HeaderMap) -> Result<String> {
    let path = path(host, headers)?;
    host.authority.lock().await.collaboration.require_driver(
        &path,
        headers_value(headers, "x-notebook-client"),
        host.now(),
    )?;
    Ok(path)
}
pub(super) async fn open(
    State(host): State<App>,
    headers: HeaderMap,
    Json(input): Json<Open>,
) -> Response {
    let result = tokio::spawn(async move {
        let path = driver(&host, &headers).await?;
        let _namespace = host.artifact_lock.lock().await;
        let mut slot = host.playground.lock().await;
        if slot.is_some() {
            return Err(error(
                ErrorCode::ExecutionRejected,
                "Close the current playground first",
            ));
        }
        let raw = host.jupyter.read(&path).await?;
        let snapshot = host.jupyter.snapshot(&path, &raw, 0, KernelState::Idle)?;
        let identity = microscope::document(&snapshot, &input.cell_id, &input.microscope_id)?;
        let doc = host
            .jupyter
            .read_microscope(&identity)
            .await?
            .ok_or_else(malformed)?;
        let step = doc
            .walkthrough
            .as_ref()
            .and_then(|walkthrough| walkthrough.steps.get(input.step_index))
            .ok_or_else(malformed)?;
        let step_id = step.id.clone();
        let step_title = step.title.clone();
        let snapshot =
            microscope::playground_snapshot(&doc, input.step_index, &snapshot.kernel.name)?;
        let id = Uuid::new_v4().to_string();
        let directory = path
            .rsplit_once('/')
            .map(|(dir, _)| format!("{dir}/"))
            .unwrap_or_default();
        let playground_path = format!("{directory}playground-{id}.ipynb");
        host.jupyter.inherit_profile(&playground_path, &path);
        let session = host.jupyter.ensure_kernel(&playground_path).await?;
        let kernel_id = session["kernel"]["id"]
            .as_str()
            .ok_or_else(malformed)?
            .to_owned();
        let session_id = session["id"].as_str().ok_or_else(malformed)?.to_owned();
        let p = Playground {
            id,
            owner: headers_value(&headers, "x-notebook-client").into(),
            notebook_path: path,
            cell_id: input.cell_id,
            microscope_id: input.microscope_id,
            step_index: input.step_index,
            step_id,
            step_title,
            kernel_id,
            session_id,
            snapshot,
            busy: false,
            closing: false,
            touched: Instant::now(),
            seen: HashMap::new(),
        };
        let value = p.public();
        *slot = Some(p);
        Ok(value)
    })
    .await
    .unwrap_or_else(|_| Err(malformed()));
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => public_error(e),
    }
}
pub(super) async fn read(State(host): State<App>, headers: HeaderMap) -> Response {
    let result = async {
        let path = path(&host, &headers)?;
        let state = host.authority.lock().await.collaboration.state(
            &path,
            headers_value(&headers, "x-notebook-client"),
            host.now(),
        )?;
        let mut slot = host.playground.lock().await;
        Ok::<_, ProtocolError>(match slot.as_mut() {
            Some(p) if p.notebook_path == path => {
                if state["is_driver"] == true
                    && p.owner == headers_value(&headers, "x-notebook-client")
                {
                    p.touched = Instant::now();
                }
                p.public()
            }
            _ => Value::Null,
        })
    }
    .await;
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => public_error(e),
    }
}
pub(super) async fn dispose(host: &Host) -> Result<()> {
    dispose_expected(host, None).await
}
async fn dispose_expected(host: &Host, expected: Option<&str>) -> Result<()> {
    let mut slot = host.playground.lock().await;
    if let Some(p) = slot.as_mut() {
        if expected.is_some_and(|id| id != p.id) {
            return Err(error(
                ErrorCode::InvalidInput,
                "Playground identity changed",
            ));
        }
        p.closing = true;
        let status = host
            .jupyter
            .request_for_kernel(
                &p.kernel_id,
                reqwest::Method::DELETE,
                &format!("api/sessions/{}", p.session_id),
                None,
            )
            .await?
            .0;
        if !matches!(status, 204 | 404) {
            return Err(error(
                ErrorCode::TransportError,
                "Playground cleanup failed; retry closing",
            ));
        }
    }
    *slot = None;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Close {
    id: String,
}
pub(super) async fn close(
    State(host): State<App>,
    headers: HeaderMap,
    Json(input): Json<Close>,
) -> Response {
    let result = async {
        driver(&host, &headers).await?;
        dispose_expected(&host, Some(&input.id)).await
    }
    .await;
    unit_response(result)
}
pub(super) async fn reap(host: App) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let identity = host.playground.lock().await.as_ref().map(|p| {
            (
                p.id.clone(),
                p.notebook_path.clone(),
                p.owner.clone(),
                p.closing || p.touched.elapsed() > Duration::from_secs(60),
            )
        });
        if let Some((id, path, owner, expired)) = identity {
            let lost = host
                .authority
                .lock()
                .await
                .collaboration
                .require_driver(&path, &owner, host.now())
                .is_err();
            if expired || lost {
                let _ = dispose_expected(&host, Some(&id)).await;
            }
        }
    }
}
pub(super) async fn command(
    State(host): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(c): Json<NotebookCommand>,
) -> Response {
    let task = tokio::spawn(run(host, headers, id, c.clone(), None));
    Json(task.await.unwrap_or_else(|_| failed(&c, malformed()))).into_response()
}
pub(super) async fn stream(
    State(host): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(c): Json<NotebookCommand>,
) -> Response {
    let (tx, rx) = mpsc::channel::<String>(16);
    tokio::spawn(async move {
        let result = run(host, headers, id, c, Some(tx.clone())).await;
        if let Ok(encoded) = serde_json::to_string(&result) {
            let _ = tx.send(format!("{encoded}\n")).await;
        }
    });
    (
        [(header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(ReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>)),
    )
        .into_response()
}
async fn publish(
    host: &Host,
    id: &str,
    c: &NotebookCommand,
    snapshot: &NotebookSnapshot,
    progress: &Option<mpsc::Sender<String>>,
) -> Result<CommandResult> {
    validate_snapshot(snapshot)?;
    let mut result = empty_result(c);
    result.base_revision = c.expected_revision;
    result.committed_revision = Some(snapshot.revision);
    result.snapshot = Some(snapshot.clone());
    let mut slot = host.playground.lock().await;
    let p = slot
        .as_mut()
        .filter(|p| p.id == id && !p.closing)
        .ok_or_else(|| error(ErrorCode::ExecutionRejected, "Playground was closed"))?;
    p.snapshot = snapshot.clone();
    if let Some(tx) = progress {
        let _ = tx.try_send(format!(
            "{}\n",
            serde_json::to_string(&result).map_err(|_| malformed())?
        ));
    }
    Ok(result)
}
async fn run(
    host: App,
    headers: HeaderMap,
    id: String,
    c: NotebookCommand,
    progress: Option<mpsc::Sender<String>>,
) -> CommandResult {
    let mut accepted = false;
    let outcome = async {
        validate_command(&c)?;
        let path = driver(&host,&headers).await?;
        let (mut snapshot,kernel_id) = {
            let mut slot = host.playground.lock().await;
            let p = slot.as_mut().filter(|p|p.id==id && p.notebook_path==path && p.owner==headers_value(&headers,"x-notebook-client") && !p.closing).ok_or_else(||error(ErrorCode::InvalidInput,"Playground is no longer open"))?;
            if let Some((old,result))=p.seen.get(&c.idempotency_key) {
                if old!=&c {return Err(error(ErrorCode::DuplicateCommand,"Idempotency key changed"));}
                return result.clone().ok_or_else(||error(ErrorCode::ExecutionRejected,"Command already accepted"));
            }
            if p.seen.len()>=256 {return Err(error(ErrorCode::BoundsExceeded,"Reopen playground to continue"));}
            if p.busy && !matches!(c.kind,NotebookCommandKind::InterruptKernel) {return Err(error(ErrorCode::ExecutionRejected,"Playground is running"));}
            if c.expected_revision.is_some_and(|rev|rev!=p.snapshot.revision) {return Err(error(ErrorCode::StaleRevision,"Refresh playground"));}
            if !matches!(c.kind,NotebookCommandKind::InterruptKernel) {p.busy=true;}
            p.touched=Instant::now();
            p.seen.insert(c.idempotency_key.clone(),(c.clone(),None));
            accepted=true;
            (p.snapshot.clone(),p.kernel_id.clone())
        };
        use NotebookCommandKind::*;
        match &c.kind {
            ModifyCells {changes} if changes.iter().all(|change|matches!(change,CellMutation::Update {cell_id,cell_type:None,..}|CellMutation::ClearOutputs {cell_id} if cell_id=="playground")) => {
                snapshot=notebook_runtime::prepare(snapshot,c.clone()).map_err(|_|error(ErrorCode::InvalidInput,"Invalid playground edit"))?;
            },
            Query {..}|Reconnect => {},
            ExecuteCell {cell_id} if cell_id=="playground" => {
                snapshot.kernel.state=KernelState::Busy;
                snapshot.cells[0].outputs.clear();
                snapshot.revision+=1;
                publish(&host,&id,&c,&snapshot,&progress).await?;
                let mut socket=host.jupyter.socket_kernel(&kernel_id).await?;
                let msg=jupyter::send(&mut socket,"execute_request",json!({"code":snapshot.cells[0].source,"silent":false,"store_history":true,"user_expressions":{},"allow_stdin":false,"stop_on_error":true})).await?;
                let mut output=OutputState::default();
                tokio::time::timeout(Duration::from_millis(c.timeout_ms.into()),async {
                    let (mut replied,mut idle)=(false,false);
                    while !replied || !idle {
                        let m=jupyter::receive(&mut socket).await?;
                        if m["parent_header"]["msg_id"]!=msg {continue;}
                        match m["header"]["msg_type"].as_str().unwrap_or("") {
                            "execute_reply"=>{replied=true;snapshot.cells[0].execution_count=m["content"]["execution_count"].as_u64();},
                            "status"=>idle=m["content"]["execution_state"]=="idle",
                            "stream"|"display_data"|"execute_result"|"update_display_data"|"clear_output"|"error"=>{
                                output.apply_jupyter_message(&m)?;
                                snapshot.cells[0].outputs=output.outputs().to_vec();
                                snapshot.revision+=1;
                                publish(&host,&id,&c,&snapshot,&progress).await?;
                            },_=>{}
                        }
                    }
                    Ok::<_,ProtocolError>(())
                }).await.map_err(|_|error(ErrorCode::Timeout,"Playground timed out; exit and reopen"))??;
                snapshot.kernel.state=KernelState::Idle;
                snapshot.revision+=1;
            },
            Complete {code,cursor_pos}|Inspect {code,cursor_pos}=>{
                let kind=if matches!(c.kind,Complete {..}) {"complete"}else{"inspect"};
                let mut socket=host.jupyter.socket_kernel(&kernel_id).await?;
                let msg=jupyter::send(&mut socket,&format!("{kind}_request"),json!({"code":code,"cursor_pos":cursor_pos,"detail_level":0})).await?;
                let reply=tokio::time::timeout(Duration::from_millis(c.timeout_ms.into()),async {
                    loop {let m=jupyter::receive(&mut socket).await?;if m["parent_header"]["msg_id"]==msg && m["header"]["msg_type"]==format!("{kind}_reply"){return Ok::<_,ProtocolError>(m["content"].clone());}}
                }).await.map_err(|_|error(ErrorCode::Timeout,"Completion timed out"))??;
                let mut result=empty_result(&c);
                if kind=="complete" {result.completion=Some(CompletionReply {matches:reply["matches"].as_array().ok_or_else(malformed)?.iter().take(100).map(|s|s.as_str().unwrap_or("").chars().take(512).collect()).collect(),cursor_start:reply["cursor_start"].as_u64().ok_or_else(malformed)? as usize,cursor_end:reply["cursor_end"].as_u64().ok_or_else(malformed)? as usize});}
                else {result.inspection=Some(InspectionReply {found:reply["found"]==true,text:reply["data"]["text/plain"].as_str().unwrap_or("").chars().take(32768).collect()});}
                if result.completion.as_ref().is_some_and(|value| value.cursor_start > value.cursor_end || value.cursor_end > code.chars().count()) {return Err(malformed());}
                return Ok(result);
            },
            InterruptKernel=>{let (status, _)=host.jupyter.request_for_kernel(&kernel_id,reqwest::Method::POST,&format!("api/kernels/{kernel_id}/interrupt"),Some(json!({}))).await?;if status!=204 {return Err(error(ErrorCode::TransportError,"Kernel interrupt failed"));}return Ok(empty_result(&c));},
            _=>return Err(error(ErrorCode::UnsupportedOperation,"Playgrounds support one code cell, execution and completion only"))
        }
        publish(&host,&id,&c,&snapshot,&None).await
    }.await;
    let result = outcome.unwrap_or_else(|e| failed(&c, e));
    if accepted {
        let mut slot = host.playground.lock().await;
        if let Some(p) = slot.as_mut().filter(|p| p.id == id) {
            if !matches!(c.kind, NotebookCommandKind::InterruptKernel) {
                p.busy = false;
            }
            if result.error.is_some() && matches!(c.kind, NotebookCommandKind::ExecuteCell { .. }) {
                p.closing = true;
            }
            p.seen
                .insert(c.idempotency_key.clone(), (c, result.clone().into()));
        }
    }
    result
}
