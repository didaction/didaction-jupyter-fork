use super::{Result, config::Config, disconnected, error, malformed};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use notebook_protocol::*;
use notebook_runtime::{KernelEvent, OutputState};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub struct Jupyter {
    pub config: Arc<Config>,
    http: Client,
    bindings: RwLock<HashMap<String, String>>,
    kernel_profiles: RwLock<HashMap<String, String>>,
}
impl Jupyter {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        let http = Client::builder()
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| disconnected())?;
        Ok(Self {
            config,
            http,
            bindings: RwLock::new(HashMap::new()),
            kernel_profiles: RwLock::new(HashMap::new()),
        })
    }
    fn profile(&self, id: &str) -> Result<&super::config::KernelProfile> {
        self.config.kernel_profiles.get(id).ok_or_else(|| {
            error(
                ErrorCode::UnsupportedOperation,
                "Notebook kernelspec has no enabled server profile",
            )
        })
    }
    fn profile_for_path(&self, path: &str) -> String {
        self.bindings
            .read()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_else(|| self.config.default_profile.clone())
    }
    fn resolve_profile(&self, selector: &str) -> Result<String> {
        if self.config.kernel_profiles.contains_key(selector) {
            return Ok(selector.into());
        }
        let matches = self
            .config
            .kernel_profiles
            .iter()
            .filter(|(_, profile)| profile.kernelspec == selector)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [id] => Ok(id.clone()),
            _ => Err(error(
                ErrorCode::UnsupportedOperation,
                "Kernel selector must identify one enabled server profile",
            )),
        }
    }
    pub fn inherit_profile(&self, target: &str, source: &str) {
        self.bindings
            .write()
            .unwrap()
            .insert(target.into(), self.profile_for_path(source));
    }
    pub async fn request_for_path(
        &self,
        path: &str,
        method: Method,
        route: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        self.request_on(&self.profile_for_path(path), method, route, body)
            .await
    }
    pub async fn request_for_kernel(
        &self,
        id: &str,
        method: Method,
        route: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let profile = self
            .kernel_profiles
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_else(|| self.config.default_profile.clone());
        self.request_on(&profile, method, route, body).await
    }
    pub async fn request_on(
        &self,
        profile: &str,
        method: Method,
        route: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let url = self
            .profile(profile)?
            .url
            .join(route)
            .map_err(|_| malformed())?;
        self.request_url(url, method, body).await
    }
    pub async fn request(
        &self,
        method: Method,
        route: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let url = self.config.url.join(route).map_err(|_| malformed())?;
        self.request_url(url, method, body).await
    }
    async fn request_url(
        &self,
        url: url::Url,
        method: Method,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut request = self
            .http
            .request(method, url)
            .header("Authorization", format!("token {}", self.config.token));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| disconnected())?;
        let status = response.status().as_u16();
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| disconnected())?;
            if bytes.len() + chunk.len() > self.config.response_limit {
                return Err(error(
                    ErrorCode::BoundsExceeded,
                    "Jupyter response exceeds limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        // Error pages can contain sensitive connection information. Never echo.
        if !(200..300).contains(&status) {
            return Ok((status, Value::Null));
        }
        Ok((
            status,
            if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).map_err(|_| malformed())?
            },
        ))
    }
    pub async fn discover(&self) -> Result<Value> {
        for (id, profile) in &self.config.kernel_profiles {
            if self
                .request_on(id, Method::GET, "api/status", None)
                .await?
                .0
                != 200
            {
                return Err(disconnected());
            }
            let (status, specs) = self
                .request_on(id, Method::GET, "api/kernelspecs", None)
                .await?;
            if status != 200 || specs["kernelspecs"].get(&profile.kernelspec).is_none() {
                return Err(error(
                    ErrorCode::UnsupportedOperation,
                    "Configured kernelspec is not installed",
                ));
            }
            if self
                .request_on(id, Method::GET, "api/contents?content=0", None)
                .await?
                .0
                != 200
            {
                return Err(disconnected());
            }
        }
        Ok(
            json!({"adapter":"jupyter","profiles":self.config.kernel_profiles.keys().collect::<Vec<_>>(),"services":["contents","sessions","kernels","kernel_channels"]}),
        )
    }
    pub async fn list(&self, directory: &str) -> Result<Value> {
        let path = self.config.path(directory, true)?;
        let (status, raw) = self
            .request(
                Method::GET,
                &format!("api/contents/{path}?content=1&type=directory"),
                None,
            )
            .await?;
        if status != 200 {
            return Err(error(
                ErrorCode::PathRejected,
                "Folder is unavailable inside the workspace",
            ));
        }
        let mut entries = Vec::new();
        for entry in raw["content"].as_array().ok_or_else(malformed)? {
            if !matches!(
                entry["type"].as_str(),
                Some("directory" | "notebook" | "file")
            ) {
                continue;
            }
            let Some(candidate) = entry["path"].as_str() else {
                continue;
            };
            if self.config.path(candidate, true).is_err()
                || candidate
                    .rsplit_once('/')
                    .map(|(parent, _)| parent)
                    .unwrap_or("")
                    != path
            {
                continue;
            }
            entries.push(
                json!({"path":candidate,"name":candidate.rsplit('/').next(),"type":entry["type"]}),
            );
            if entries.len() > 1000 {
                return Err(error(
                    ErrorCode::BoundsExceeded,
                    "Folder contains too many notebooks",
                ));
            }
        }
        entries.sort_by_key(|e| {
            (
                e["type"] != "directory",
                e["name"].as_str().unwrap_or_default().to_lowercase(),
            )
        });
        Ok(json!({"directory":path,"entries":entries}))
    }
    pub async fn setup(&self, path: &str, create: bool, requested: Option<&str>) -> Result<Value> {
        let requested = self.resolve_profile(requested.unwrap_or(&self.config.default_profile))?;
        self.profile(&requested)?;
        let (status, _) = self
            .request(Method::GET, &format!("api/contents/{path}?content=0"), None)
            .await?;
        if status == 404 && create {
            let profile = self.profile(&requested)?;
            let notebook = json!({"nbformat":4,"nbformat_minor":5,"metadata":{"kernelspec":{"name":profile.kernelspec,"display_name":profile.kernelspec}},"cells":[{"id":Uuid::new_v4().to_string(),"cell_type":"code","source":"","metadata":{},"outputs":[],"execution_count":null}]});
            self.bindings
                .write()
                .unwrap()
                .insert(path.into(), requested.clone());
            self.save(path, &notebook).await?;
        } else if status != 200 {
            return Err(error(ErrorCode::InvalidInput, "Notebook does not exist"));
        } else {
            let (_, raw) = self
                .request(
                    Method::GET,
                    &format!("api/contents/{path}?content=1&type=notebook"),
                    None,
                )
                .await?;
            let name = raw["content"]["metadata"]["kernelspec"]["name"].as_str();
            let selected = if let Some(name) = name {
                self.config
                    .kernel_profiles
                    .iter()
                    .find(|(_, profile)| profile.kernelspec == name)
                    .map(|(id, _)| id.as_str())
                    .ok_or_else(|| {
                        error(
                            ErrorCode::UnsupportedOperation,
                            "Notebook kernelspec has no enabled server profile",
                        )
                    })?
            } else {
                requested.as_str()
            };
            if requested != self.config.default_profile && selected != requested {
                return Err(error(
                    ErrorCode::UnsupportedOperation,
                    "Requested profile differs from notebook kernelspec metadata",
                ));
            }
            self.bindings
                .write()
                .unwrap()
                .insert(path.into(), selected.into());
        }
        self.ensure_kernel(path).await?;
        self.read(path).await
    }
    pub async fn read(&self, path: &str) -> Result<Value> {
        let profile = self.profile_for_path(path);
        let (status, raw) = self
            .request_on(
                &profile,
                Method::GET,
                &format!("api/contents/{path}?content=1&type=notebook"),
                None,
            )
            .await?;
        if status != 200 {
            return Err(error(
                ErrorCode::TransportError,
                "Notebook could not be read",
            ));
        }
        let mut notebook = raw["content"].clone();
        if notebook["nbformat"] != 4 {
            return Err(malformed());
        }
        let cells = notebook["cells"].as_array_mut().ok_or_else(malformed)?;
        let mut changed = false;
        for cell in cells {
            if cell.get("id").is_none() {
                cell["id"] = Uuid::new_v4().to_string().into();
                changed = true;
            }
        }
        self.snapshot(path, &notebook, 0, KernelState::Idle)?;
        if changed {
            self.save(path, &notebook).await?;
        }
        Ok(notebook)
    }
    pub async fn save(&self, path: &str, notebook: &Value) -> Result<()> {
        self.snapshot(path, notebook, 0, KernelState::Idle)?;
        if serde_json::to_vec(notebook).map_err(|_| malformed())?.len() > self.config.response_limit
        {
            return Err(error(ErrorCode::BoundsExceeded, "Notebook exceeds limit"));
        }
        let (status, _) = self
            .request_on(
                &self.profile_for_path(path),
                Method::PUT,
                &format!("api/contents/{path}"),
                Some(json!({"type":"notebook","format":"json","content":notebook})),
            )
            .await?;
        if !matches!(status, 200 | 201) {
            return Err(error(
                ErrorCode::TransportError,
                "Notebook could not be saved; refresh before retrying",
            ));
        }
        Ok(())
    }
    pub fn snapshot(
        &self,
        path: &str,
        raw: &Value,
        revision: u64,
        state: KernelState,
    ) -> Result<NotebookSnapshot> {
        let items = raw["cells"].as_array().ok_or_else(malformed)?;
        if items.len() > MAX_CELLS {
            return Err(error(ErrorCode::BoundsExceeded, "Too many notebook cells"));
        }
        let mut ids = HashSet::new();
        let mut cells = Vec::new();
        for item in items {
            let mut outputs = OutputState::default();
            if let Some(raw_outputs) = item.get("outputs") {
                for raw_output in raw_outputs.as_array().ok_or_else(malformed)? {
                    let kind = raw_output["output_type"].as_str().ok_or_else(malformed)?;
                    let mut bundle = raw_output.clone();
                    if kind == "stream" {
                        bundle["text"] = source(&bundle["text"])?.into();
                    }
                    if let Some(data) = bundle.get_mut("data").and_then(Value::as_object_mut) {
                        for value in data.values_mut() {
                            if value.is_array() {
                                *value = source(value)?.into();
                            }
                        }
                    }
                    outputs.apply(KernelEvent {
                        kind: kind.into(),
                        bundle,
                    })?;
                }
            }
            let id = item["id"].as_str().ok_or_else(malformed)?.to_string();
            if !ids.insert(id.clone()) {
                return Err(malformed());
            }
            cells.push(Cell {
                id,
                cell_type: serde_json::from_value(item["cell_type"].clone())
                    .map_err(|_| malformed())?,
                source: source(&item["source"])?,
                metadata: item["metadata"].clone(),
                execution_count: item["execution_count"].as_u64(),
                outputs: outputs.outputs().to_vec(),
            });
        }
        let kernel = self
            .profile(&self.profile_for_path(path))?
            .kernelspec
            .clone();
        let snapshot = NotebookSnapshot {
            protocol_version: 1,
            schema_version: 1,
            notebook: NotebookIdentity {
                path: path.into(),
                workspace: "local".into(),
            },
            kernel: KernelIdentity {
                name: kernel.clone(),
                display_name: kernel,
                session_id: None,
                state,
            },
            revision,
            selected_cell_id: cells.first().map(|c| c.id.clone()),
            cells,
        };
        validate_snapshot(&snapshot)?;
        Ok(snapshot)
    }
    pub async fn ensure_kernel(&self, path: &str) -> Result<Value> {
        let profile_id = self.profile_for_path(path);
        let kernelspec = self.profile(&profile_id)?.kernelspec.clone();
        let (status, sessions) = self
            .request_on(&profile_id, Method::GET, "api/sessions", None)
            .await?;
        if status != 200 {
            return Err(disconnected());
        }
        if let Some(session) = sessions
            .as_array()
            .ok_or_else(malformed)?
            .iter()
            .find(|s| s["path"] == path)
        {
            if session["kernel"]["name"] != kernelspec {
                return Err(error(
                    ErrorCode::UnsupportedOperation,
                    "Existing notebook kernel differs from startup configuration",
                ));
            }
            if let Some(id) = session["kernel"]["id"].as_str() {
                self.kernel_profiles
                    .write()
                    .unwrap()
                    .insert(id.into(), profile_id);
            }
            return Ok(session.clone());
        }
        let (status, session) = self.request_on(&profile_id, Method::POST, "api/sessions", Some(json!({"path":path,"name":path.rsplit('/').next(),"type":"notebook","kernel":{"name":kernelspec}}))).await?;
        if status != 201 {
            return Err(error(ErrorCode::TransportError, "Kernel could not start"));
        }
        if let Some(id) = session["kernel"]["id"].as_str() {
            self.kernel_profiles
                .write()
                .unwrap()
                .insert(id.into(), profile_id);
        }
        Ok(session)
    }
    pub async fn kernel_action(&self, path: &str, action: &str) -> Result<()> {
        let session = self.ensure_kernel(path).await?;
        let id = session["kernel"]["id"].as_str().ok_or_else(malformed)?;
        let status = self
            .request_on(
                &self.profile_for_path(path),
                Method::POST,
                &format!("api/kernels/{id}/{action}"),
                Some(json!({})),
            )
            .await?
            .0;
        if !matches!(status, 200 | 204) {
            return Err(disconnected());
        }
        Ok(())
    }
    pub async fn rename(&self, old: &str, new: &str) -> Result<()> {
        let profile = self.profile_for_path(old);
        let status = self
            .request_on(
                &profile,
                Method::PATCH,
                &format!("api/contents/{old}"),
                Some(json!({"path":new})),
            )
            .await?
            .0;
        if status != 200 {
            return Err(error(
                ErrorCode::TransportError,
                "Notebook could not be renamed",
            ));
        }
        let (_, sessions) = self
            .request_on(&profile, Method::GET, "api/sessions", None)
            .await?;
        if let Some(session) = sessions
            .as_array()
            .and_then(|s| s.iter().find(|s| s["path"] == old))
        {
            let id = session["id"].as_str().ok_or_else(malformed)?;
            if self
                .request_on(
                    &profile,
                    Method::PATCH,
                    &format!("api/sessions/{id}"),
                    Some(json!({"path":new})),
                )
                .await?
                .0
                != 200
            {
                return Err(error(
                    ErrorCode::TransportError,
                    "Notebook renamed but session update failed; reconnect",
                ));
            }
        }
        let mut bindings = self.bindings.write().unwrap();
        if let Some(binding) = bindings.remove(old) {
            bindings.insert(new.into(), binding);
        }
        Ok(())
    }
    pub async fn socket(&self, path: &str) -> Result<Socket> {
        let session = self.ensure_kernel(path).await?;
        let id = session["kernel"]["id"].as_str().ok_or_else(malformed)?;
        self.socket_kernel(id).await
    }
    pub async fn socket_kernel(&self, id: &str) -> Result<Socket> {
        let profile_id = self
            .kernel_profiles
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_else(|| self.config.default_profile.clone());
        let profile = self.profile(&profile_id)?;
        let mut url = profile
            .url
            .join(&format!("api/kernels/{id}/channels"))
            .map_err(|_| malformed())?;
        url.set_scheme(if profile.url.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| malformed())?;
        url.query_pairs_mut()
            .append_pair("session_id", &Uuid::new_v4().to_string());
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| malformed())?;
        request.headers_mut().insert(
            "Authorization",
            format!("token {}", self.config.token)
                .parse()
                .map_err(|_| malformed())?,
        );
        let limits = WebSocketConfig::default()
            .max_message_size(Some(self.config.response_limit))
            .max_frame_size(Some(self.config.response_limit));
        let (mut socket, _) = tokio::time::timeout(
            self.config.timeout,
            connect_async_with_config(request, Some(limits), false),
        )
        .await
        .map_err(|_| disconnected())?
        .map_err(|_| disconnected())?;
        // Establish the channel before execution, including a real kernel reply.
        let id = send(&mut socket, "kernel_info_request", json!({})).await?;
        tokio::time::timeout(self.config.timeout, async {
            loop {
                let message = receive(&mut socket).await?;
                if message["parent_header"]["msg_id"] == id
                    && message["header"]["msg_type"] == "kernel_info_reply"
                {
                    return Ok::<_, ProtocolError>(());
                }
            }
        })
        .await
        .map_err(|_| disconnected())??;
        Ok(socket)
    }
    pub async fn language(
        &self,
        path: &str,
        kind: &str,
        code: &str,
        cursor: usize,
        timeout: u32,
    ) -> Result<Value> {
        let mut socket = self.socket(path).await?;
        let id = send(
            &mut socket,
            &format!("{kind}_request"),
            json!({"code":code,"cursor_pos":cursor,"detail_level":0}),
        )
        .await?;
        tokio::time::timeout(Duration::from_millis(timeout.into()), async {
            loop {
                let message = receive(&mut socket).await?;
                if message["parent_header"]["msg_id"] == id
                    && message["header"]["msg_type"] == format!("{kind}_reply")
                {
                    return Ok(message["content"].clone());
                }
            }
        })
        .await
        .map_err(|_| error(ErrorCode::Timeout, "Kernel language request timed out"))?
    }
}
pub async fn send(socket: &mut Socket, kind: &str, content: Value) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    socket.send(Message::Text(json!({"header":{"msg_id":id,"username":"didaction","session":Uuid::new_v4().to_string(),"msg_type":kind,"version":"5.3"},"parent_header":{},"metadata":{},"content":content,"channel":"shell","buffers":[]}).to_string().into())).await.map_err(|_| disconnected())?;
    Ok(id)
}
pub async fn receive(socket: &mut Socket) -> Result<Value> {
    loop {
        match socket
            .next()
            .await
            .ok_or_else(disconnected)?
            .map_err(|_| disconnected())?
        {
            Message::Text(text) => return serde_json::from_str(&text).map_err(|_| malformed()),
            Message::Binary(bytes) => {
                // Default Jupyter framing: big-endian count followed by offsets.
                let word = |i: usize| -> Result<usize> {
                    Ok(u32::from_be_bytes(
                        bytes
                            .get(i..i + 4)
                            .ok_or_else(malformed)?
                            .try_into()
                            .map_err(|_| malformed())?,
                    ) as usize)
                };
                let count = word(0)?;
                if count == 0 || count > 128 {
                    return Err(malformed());
                }
                let start = word(4)?;
                let end = if count > 1 { word(8)? } else { bytes.len() };
                if start < (count + 1) * 4 || end < start {
                    return Err(malformed());
                }
                return serde_json::from_slice(bytes.get(start..end).ok_or_else(malformed)?)
                    .map_err(|_| malformed());
            }
            Message::Close(_) => return Err(disconnected()),
            _ => {}
        }
    }
}
fn source(value: &Value) -> Result<String> {
    if let Some(text) = value.as_str() {
        return Ok(text.into());
    }
    value
        .as_array()
        .ok_or_else(malformed)?
        .iter()
        .map(|v| v.as_str().ok_or_else(malformed))
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.concat())
}
pub fn output_nb(output: &CellOutput) -> Result<Value> {
    Ok(match output {
        CellOutput::Stream { name, text } => {
            json!({"output_type":"stream","name":name,"text":text})
        }
        CellOutput::Error {
            name,
            message,
            traceback,
        } => json!({"output_type":"error","ename":name,"evalue":message,"traceback":traceback}),
        CellOutput::Text { text } => {
            json!({"output_type":"display_data","metadata":{},"data":{"text/plain":text}})
        }
        CellOutput::Rich { mime, data } => {
            let text = if mime == "image/svg+xml" {
                String::from_utf8(STANDARD.decode(data).map_err(|_| malformed())?)
                    .map_err(|_| malformed())?
            } else {
                data.clone()
            };
            json!({"output_type":"display_data","metadata":{},"data":{mime:text}})
        }
    })
}
pub fn merge_cells(raw: &mut Value, proposed: &NotebookSnapshot) -> Result<()> {
    let previous = raw["cells"].as_array().ok_or_else(malformed)?;
    let mut cells = Vec::new();
    for cell in &proposed.cells {
        let mut next = previous
            .iter()
            .find(|old| old["id"] == cell.id)
            .cloned()
            .unwrap_or_else(|| json!({}));
        next["id"] = cell.id.clone().into();
        next["cell_type"] = serde_json::to_value(&cell.cell_type).map_err(|_| malformed())?;
        next["source"] = cell.source.clone().into();
        next["metadata"] = cell.metadata.clone();
        if cell.cell_type == CellType::Code {
            next["execution_count"] = json!(cell.execution_count);
            // Preserve original MIME bundles when outputs were not changed.
            if cell.outputs.is_empty() {
                next["outputs"] = json!([]);
            } else if next.get("outputs").is_none() {
                next["outputs"] =
                    Value::Array(cell.outputs.iter().map(output_nb).collect::<Result<_>>()?);
            }
            next.as_object_mut()
                .ok_or_else(malformed)?
                .remove("attachments");
        } else {
            let object = next.as_object_mut().ok_or_else(malformed)?;
            object.remove("outputs");
            object.remove("execution_count");
        }
        cells.push(next);
    }
    raw["cells"] = cells.into();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn adapter() -> Jupyter {
        Jupyter::new(Arc::new(Config {
            url: url::Url::parse("http://127.0.0.1:1/").unwrap(),
            token: String::new(),
            kernel: "python3".into(),
            default_profile: "default".into(),
            kernel_profiles: std::collections::BTreeMap::from([(
                "default".into(),
                super::super::config::KernelProfile {
                    url: url::Url::parse("http://127.0.0.1:1/").unwrap(),
                    kernelspec: "python3".into(),
                },
            )]),
            notebook: "test.ipynb".into(),
            workspace: "/tmp".into(),
            static_dir: None,
            listen: "127.0.0.1:0".into(),
            request_limit: 300000,
            response_limit: 4000000,
            timeout: Duration::from_secs(1),
            allowed_origins: std::collections::HashSet::new(),
        }))
        .unwrap()
    }
    fn notebook() -> Value {
        json!({"nbformat":4,"nbformat_minor":5,"metadata":{"custom":"keep"},"cells":[{
            "id":"code","cell_type":"code","metadata":{},"source":["a = 1\n","a"],
            "execution_count":1,"outputs":[{"output_type":"execute_result","execution_count":1,"metadata":{},"data":{"text/plain":"1","application/custom+json":{"keep":true}}}]
        },{"id":"md","cell_type":"markdown","source":"![x](attachment:x)","metadata":{},"attachments":{"x":{"image/png":"data"}}}]})
    }
    #[tokio::test]
    async fn upstream_failures_are_bounded_and_redacted() {
        use axum::{Router, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let routes = Router::new()
            .route("/malformed", get(|| async { "not-json SECRET-CODE" }))
            .route("/large", get(|| async { "x".repeat(2048) }))
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    "{}"
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
        let mut original = adapter();
        let config = Arc::get_mut(&mut original.config).unwrap();
        config.url = url::Url::parse(&format!("http://{address}/")).unwrap();
        config.response_limit = 1024;
        config.timeout = Duration::from_millis(100);
        config.token = "SECRET-TOKEN".into();
        let adapter = Jupyter::new(original.config).unwrap();
        for (route, code) in [
            ("malformed", ErrorCode::MalformedResponse),
            ("large", ErrorCode::BoundsExceeded),
            ("slow", ErrorCode::Disconnected),
        ] {
            let failure = adapter.request(Method::GET, route, None).await.unwrap_err();
            assert_eq!(failure.code, code);
            assert!(!failure.to_string().contains("SECRET"));
        }
        server.abort();
        let _ = server.await;
        assert_eq!(
            adapter
                .request(Method::GET, "api/status", None)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Disconnected
        );
    }
    #[test]
    fn normalizes_outputs_preserves_nbformat_extras_and_rejects_malformed() {
        let adapter = adapter();
        let mut raw = notebook();
        let mut snapshot = adapter
            .snapshot("test.ipynb", &raw, 1, KernelState::Idle)
            .unwrap();
        assert_eq!(snapshot.cells[0].source, "a = 1\na");
        snapshot.cells[0].source = "a = 2".into();
        merge_cells(&mut raw, &snapshot).unwrap();
        assert_eq!(
            raw["cells"][0]["outputs"][0]["data"]["application/custom+json"]["keep"],
            true
        );
        assert_eq!(raw["cells"][1]["attachments"]["x"]["image/png"], "data");
        raw["cells"][1]["id"] = "code".into();
        assert!(
            adapter
                .snapshot("test.ipynb", &raw, 1, KernelState::Idle)
                .is_err()
        );
        let mut raw = notebook();
        raw["cells"][0]["outputs"][0]["data"]["text/plain"] =
            json!("x".repeat(MAX_OUTPUT_BYTES + 1));
        assert_eq!(
            adapter
                .snapshot("test.ipynb", &raw, 1, KernelState::Idle)
                .unwrap_err()
                .code,
            ErrorCode::BoundsExceeded
        );
    }
    #[test]
    fn notebook_bindings_select_profiles_and_kernelspecs() {
        let mut adapter = adapter();
        let config = Arc::get_mut(&mut adapter.config).unwrap();
        config.kernel_profiles.insert(
            "tlaplus".into(),
            super::super::config::KernelProfile {
                url: url::Url::parse("http://127.0.0.1:2/").unwrap(),
                kernelspec: "tlaplus_jupyter".into(),
            },
        );
        let adapter = Jupyter::new(adapter.config).unwrap();
        assert_eq!(
            adapter.resolve_profile("tlaplus_jupyter").unwrap(),
            "tlaplus"
        );
        adapter
            .bindings
            .write()
            .unwrap()
            .insert("spec.ipynb".into(), "tlaplus".into());
        let snapshot = adapter
            .snapshot("spec.ipynb", &notebook(), 1, KernelState::Idle)
            .unwrap();
        assert_eq!(snapshot.kernel.name, "tlaplus_jupyter");
        assert_eq!(
            adapter.resolve_profile("missing").unwrap_err().code,
            ErrorCode::UnsupportedOperation
        );
    }
}
