use base64::Engine as _;
use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::{Color32, CornerRadius, FontId, Key, Margin, RichText, Stroke, TextEdit};
use egui_code_editor::{CodeEditor, ColorTheme, Syntax};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use notebook_core::{NotebookState, SyncState};
use notebook_protocol::{
    Cell, CellMutation, CellOutput, CellType, CompletionReply, InspectionReply, KernelState,
    NotebookCommand, NotebookCommandKind, PROTOCOL_VERSION,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use uuid::Uuid;
mod graphics;
mod walkthrough;

const MAX_EMBEDDED_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const COMPLETION_KEY_HELP: &str = "Up/Down select · Enter or Tab apply · Esc close";

fn kernel_syntax(kernel: &str) -> Syntax {
    if !kernel.to_ascii_lowercase().starts_with("julia") {
        return Syntax::python();
    }
    let mut syntax = Syntax::new("Julia");
    syntax.comment = "#";
    syntax.comment_multiline = ["#=", "=#"];
    syntax.keywords = [
        "using", "import", "export", "function", "end", "if", "else", "elseif", "for", "while",
        "begin", "let", "return", "struct", "mutable", "module", "macro", "quote", "try", "catch",
        "finally", "const", "local", "global", "where", "do", "in", "break", "continue",
    ]
    .into_iter()
    .collect();
    syntax.special = ["true", "false", "nothing", "missing"]
        .into_iter()
        .collect();
    syntax
}

#[derive(Default)]
struct DataImageBytesLoader;

impl DataImageBytesLoader {
    const ID: &'static str = egui::generate_loader_id!(DataImageBytesLoader);
}

impl egui::load::BytesLoader for DataImageBytesLoader {
    fn id(&self) -> &str {
        Self::ID
    }

    fn load(&self, _ctx: &egui::Context, uri: &str) -> egui::load::BytesLoadResult {
        let Some((header, encoded)) = uri.split_once(',') else {
            return Err(egui::load::LoadError::NotSupported);
        };
        let mime = header
            .strip_prefix("data:")
            .and_then(|header| header.strip_suffix(";base64"))
            .filter(|mime| {
                matches!(
                    *mime,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/svg+xml"
                )
            })
            .ok_or(egui::load::LoadError::NotSupported)?;
        if encoded.len() > MAX_EMBEDDED_IMAGE_BYTES.saturating_mul(4).div_ceil(3) {
            return Err(egui::load::LoadError::Loading(
                "embedded Markdown image exceeds the 8 MiB limit".into(),
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| egui::load::LoadError::Loading("invalid base64 image data".into()))?;
        if bytes.len() > MAX_EMBEDDED_IMAGE_BYTES {
            return Err(egui::load::LoadError::Loading(
                "embedded Markdown image exceeds the 8 MiB limit".into(),
            ));
        }
        Ok(egui::load::BytesPoll::Ready {
            size: None,
            bytes: egui::load::Bytes::Shared(bytes.into()),
            mime: Some(mime.to_owned()),
        })
    }

    fn forget(&self, _uri: &str) {}

    fn forget_all(&self) {}

    fn byte_size(&self) -> usize {
        0
    }
}

fn install_data_image_loader(ctx: &egui::Context) {
    if !ctx.is_loader_installed(DataImageBytesLoader::ID) {
        ctx.add_bytes_loader(Arc::new(DataImageBytesLoader));
    }
}

fn decode_rich_image(_mime: &str, data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::STANDARD.decode(data)
}

fn rich_image_uri(mime: &str, data: &str) -> String {
    let mut hasher = DefaultHasher::new();
    mime.hash(&mut hasher);
    data.hash(&mut hasher);
    let extension = if mime == "image/svg+xml" {
        "svg"
    } else {
        "png"
    };
    format!(
        "bytes://notebook-output/{:016x}.{extension}",
        hasher.finish()
    )
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..16] != b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0 && width <= 16_384 && height <= 16_384).then_some((width, height))
}

fn svg_dimensions(bytes: &[u8]) -> Option<(f32, f32)> {
    let source = std::str::from_utf8(bytes).ok()?;
    let svg_start = source.find("<svg")?;
    let svg_end = source[svg_start..].find('>')? + svg_start;
    let tag = &source[svg_start..=svg_end];

    let numeric_attribute = |name: &str| {
        svg_attribute(tag, name).and_then(|value| {
            let number = value.trim().trim_end_matches("px").parse::<f32>().ok()?;
            number.is_finite().then_some(number)
        })
    };
    let dimensions = numeric_attribute("width").zip(numeric_attribute("height"));
    let dimensions = dimensions.or_else(|| {
        let view_box = svg_attribute(tag, "viewBox")?;
        let values = view_box
            .split(|character: char| character.is_ascii_whitespace() || character == ',')
            .filter(|value| !value.is_empty())
            .map(str::parse::<f32>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        (values.len() == 4).then_some((values[2], values[3]))
    })?;
    (dimensions.0 > 0.0
        && dimensions.1 > 0.0
        && dimensions.0 <= 16_384.0
        && dimensions.1 <= 16_384.0)
        .then_some(dimensions)
}

fn svg_attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(index) = rest.find(name) {
        let before = rest[..index].chars().next_back();
        let after_name = &rest[index + name.len()..];
        if before.is_none_or(char::is_whitespace) {
            let after_equals = after_name.trim_start().strip_prefix('=')?.trim_start();
            let quote = after_equals.chars().next()?;
            if matches!(quote, '\'' | '"') {
                return after_equals[1..].split_once(quote).map(|(value, _)| value);
            }
        }
        rest = &after_name[after_name.len().min(1)..];
    }
    None
}

fn cell_has_completed_execution(cell: &Cell) -> bool {
    cell.cell_type == CellType::Code && cell.execution_count.is_some()
}

const MARKDOWN_GROUP_METADATA_KEY: &str = "didaction_markdown_group";

fn combines_with_markdown(cell: &Cell, markdown_cell_id: &str) -> bool {
    cell.cell_type == CellType::Code
        && cell
            .metadata
            .get(MARKDOWN_GROUP_METADATA_KEY)
            .and_then(serde_json::Value::as_object)
            .is_some_and(|value| {
                value
                    .get("schema_version")
                    .and_then(serde_json::Value::as_u64)
                    == Some(1)
                    && value
                        .get("markdown_cell_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(markdown_cell_id)
            })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum OutputViewMode {
    #[default]
    Expanded,
    Windowed,
    Collapsed,
}

impl OutputViewMode {
    fn next(self) -> Self {
        match self {
            Self::Expanded => Self::Windowed,
            Self::Windowed => Self::Collapsed,
            Self::Collapsed => Self::Expanded,
        }
    }
}

pub struct NotebookEguiApp {
    pub state: NotebookState,
    outbound: VecDeque<NotebookCommand>,
    editors: Vec<(String, String)>,
    dirty_editors: HashSet<String>,
    pub external_command_active: bool,
    pub read_only: bool,
    pub scroll_fraction: f32,
    pub follow_scroll: Option<f32>,
    scroll_extent: f32,
    completion_suggestions: HashMap<String, CompletionReply>,
    completion_selection: HashMap<String, usize>,
    pending_caret_positions: HashMap<String, usize>,
    pending_completions: BTreeMap<Uuid, String>,
    pending_inspections: BTreeMap<Uuid, String>,
    pending_execution_cells: BTreeMap<Uuid, String>,
    pending_execution_sources: BTreeMap<Uuid, String>,
    scroll_to_output: Option<String>,
    inspections: HashMap<String, String>,
    caret_byte_positions: HashMap<String, usize>,
    completion_due: HashMap<String, f64>,
    dragging_cell: Option<String>,
    rendered_markdown: HashSet<String>,
    edit_mode: bool,
    pending_editor_focus: Option<String>,
    cell_clipboard: Vec<Cell>,
    undo_stack: Vec<Vec<CellMutation>>,
    redo_stack: Vec<Vec<CellMutation>>,
    suppress_history: bool,
    collapsed_cells: HashSet<String>,
    agent_highlights: HashMap<String, Color32>,
    pub reduced_motion: bool,
    capture_target: Option<(String, u32)>,
    capture_region: Option<(egui::Rect, f32, bool)>,
    pub captured_cell: Option<String>,
    microscope_capture_frames: Option<u32>,
    pub workspace_visible: bool,
    pub workspace_toggle_requested: bool,
    pub follow_toggle_requested: bool,
    pub diagnostics_toggle_requested: bool,
    pub following_driver: bool,
    pub host_status: String,
    pub checkpoints_supported: bool,
    pub microscope_target: Option<notebook_protocol::microscope::MicroscopeTarget>,
    microscope_document: Option<notebook_protocol::microscope::MicroscopeDocument>,
    pub playground_requested: Option<usize>,
    microscope_delete: Option<(String, notebook_protocol::microscope::MicroscopeRef)>,
    walkthrough_scroll_to_focus: bool,
    graphics: graphics::GraphicsSurface,
    output_views: HashMap<String, OutputViewMode>,
    hidden_line_numbers: HashSet<String>,
    find_open: bool,
    find_query: String,
    replace_query: String,
    autosave_due: Option<f64>,
    selected_cells: HashSet<String>,
    rename_open: bool,
    rename_path: String,
    restart_confirmation: bool,
    markdown_cache: CommonMarkCache,
    math_cache: Arc<Mutex<MathRenderCache>>,
    markdown_link: Option<(String, egui::Pos2)>,
}

impl NotebookEguiApp {
    pub fn open_microscope(
        &mut self,
        target: Option<notebook_protocol::microscope::MicroscopeTarget>,
    ) -> Result<(), String> {
        if self.microscope_target == target {
            return Ok(());
        }
        if let Some(target) = &target {
            notebook_protocol::microscope::document(
                &self.state.snapshot,
                &target.cell_id,
                &target.microscope_id,
            )
            .map_err(|e| e.to_string())?;
            self.save_visible_edits();
            self.state.snapshot.selected_cell_id = Some(target.cell_id.clone());
            self.selected_cells.clear();
            self.emit(NotebookCommandKind::ReadMicroscope {
                cell_id: target.cell_id.clone(),
                microscope_id: target.microscope_id.clone(),
            });
        }
        self.microscope_target = target;
        self.microscope_document = None;
        Ok(())
    }
    pub fn show_microscope(
        &mut self,
        doc: Option<notebook_protocol::microscope::MicroscopeDocument>,
    ) -> Result<(), String> {
        if let Some(doc) = &doc {
            let expected = notebook_protocol::microscope::document(
                &self.state.snapshot,
                &doc.cell_id,
                &doc.microscope.id,
            )
            .map_err(|e| e.to_string())?;
            notebook_protocol::microscope::validate_document(doc, &expected)
                .map_err(|e| e.to_string())?;
            let focus = doc.walkthrough.as_ref().map(|w| {
                self.microscope_target
                    .as_ref()
                    .filter(|t| t.cell_id == doc.cell_id && t.microscope_id == doc.microscope.id)
                    .and_then(|t| t.focus.clone())
                    .filter(|f| notebook_protocol::microscope::validate_focus(w, f).is_ok())
                    .unwrap_or_default()
            });
            self.state.snapshot.selected_cell_id = Some(doc.cell_id.clone());
            self.selected_cells.clear();
            self.microscope_target = Some(notebook_protocol::microscope::MicroscopeTarget {
                cell_id: doc.cell_id.clone(),
                microscope_id: doc.microscope.id.clone(),
                revision: doc.microscope.revision,
                focus,
            });
        } else {
            self.microscope_target = None;
        }
        self.microscope_document = doc;
        Ok(())
    }
    pub fn accept_microscope(&mut self, doc: notebook_protocol::microscope::MicroscopeDocument) {
        if self
            .microscope_target
            .as_ref()
            .is_some_and(|t| t.cell_id == doc.cell_id && t.microscope_id == doc.microscope.id)
        {
            let _ = self.show_microscope(Some(doc));
        }
    }
    pub fn new(state: NotebookState) -> Self {
        let editors = state
            .snapshot
            .cells
            .iter()
            .map(|c| (c.id.clone(), c.source.clone()))
            .collect();
        let rendered_markdown = state
            .snapshot
            .cells
            .iter()
            .filter(|cell| cell.cell_type == CellType::Markdown)
            .map(|cell| cell.id.clone())
            .collect();
        Self {
            state,
            outbound: VecDeque::new(),
            editors,
            dirty_editors: HashSet::new(),
            external_command_active: false,
            read_only: false,
            scroll_fraction: 0.0,
            follow_scroll: None,
            scroll_extent: 0.0,
            completion_suggestions: HashMap::new(),
            completion_selection: HashMap::new(),
            pending_caret_positions: HashMap::new(),
            pending_completions: BTreeMap::new(),
            pending_inspections: BTreeMap::new(),
            pending_execution_cells: BTreeMap::new(),
            pending_execution_sources: BTreeMap::new(),
            scroll_to_output: None,
            inspections: HashMap::new(),
            caret_byte_positions: HashMap::new(),
            completion_due: HashMap::new(),
            dragging_cell: None,
            rendered_markdown,
            edit_mode: false,
            pending_editor_focus: None,
            cell_clipboard: Vec::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            suppress_history: false,
            collapsed_cells: HashSet::new(),
            agent_highlights: HashMap::new(),
            reduced_motion: false,
            capture_target: None,
            capture_region: None,
            captured_cell: None,
            microscope_capture_frames: None,
            workspace_visible: true,
            workspace_toggle_requested: false,
            follow_toggle_requested: false,
            diagnostics_toggle_requested: false,
            following_driver: false,
            host_status: "Connecting…".into(),
            checkpoints_supported: true,
            microscope_target: None,
            microscope_document: None,
            graphics: graphics::GraphicsSurface::default(),
            playground_requested: None,
            microscope_delete: None,
            walkthrough_scroll_to_focus: false,
            output_views: HashMap::new(),
            hidden_line_numbers: HashSet::new(),
            find_open: false,
            find_query: String::new(),
            replace_query: String::new(),
            autosave_due: None,
            selected_cells: HashSet::new(),
            rename_open: false,
            rename_path: String::new(),
            restart_confirmation: false,
            markdown_cache: CommonMarkCache::default(),
            math_cache: Arc::new(Mutex::new(MathRenderCache::default())),
            markdown_link: None,
        }
    }
    pub fn cell_view(&mut self, id: &str, action: &str, value: &str) -> Result<(), String> {
        if id.len() > 128 || !self.state.snapshot.cells.iter().any(|cell| cell.id == id) {
            return Err("Unknown cell ID".into());
        }
        match (action, value) {
            ("highlight", color) => {
                let color = match color {
                    "blue" => Color32::from_rgb(45, 105, 145),
                    "blue-light" => Color32::from_rgb(91, 145, 181),
                    "blue-deep" => Color32::from_rgb(30, 78, 115),
                    _ => return Err("Invalid highlight color".into()),
                };
                if self.agent_highlights.len() >= 128 && !self.agent_highlights.contains_key(id) {
                    return Err("Highlight limit reached; clear a highlight first".into());
                }
                self.agent_highlights.insert(id.to_owned(), color);
            }
            ("clear_highlight", "") => {
                self.agent_highlights.remove(id);
            }
            ("cell", "true") => {
                self.collapsed_cells.insert(id.to_owned());
            }
            ("cell", "false") => {
                self.collapsed_cells.remove(id);
            }
            ("output", mode) => {
                let mode = match mode {
                    "expanded" => OutputViewMode::Expanded,
                    "windowed" => OutputViewMode::Windowed,
                    "collapsed" => OutputViewMode::Collapsed,
                    _ => return Err("Invalid output mode".into()),
                };
                self.set_output_view(id, mode);
            }
            ("capture", "") => {
                self.captured_cell = None;
                self.capture_region = None;
                self.capture_target = Some((id.to_owned(), 30));
            }
            _ => return Err("Invalid view operation".into()),
        }
        Ok(())
    }
    pub fn apply_completion(&mut self, command_id: Uuid, completion: CompletionReply) {
        if let Some(cell_id) = self.pending_completions.remove(&command_id) {
            self.completion_selection.insert(cell_id.clone(), 0);
            self.completion_suggestions.insert(cell_id, completion);
        }
    }
    pub fn apply_inspection(&mut self, command_id: Uuid, inspection: InspectionReply) {
        if let Some(cell_id) = self.pending_inspections.remove(&command_id) {
            if inspection.found && !inspection.text.is_empty() {
                self.inspections.insert(cell_id, inspection.text);
            } else {
                self.inspections.remove(&cell_id);
            }
        }
    }
    pub fn drain_commands(&mut self) -> Vec<NotebookCommand> {
        self.outbound.drain(..).collect()
    }
    pub fn external_commands_ready(&self) -> bool {
        self.dirty_editors.is_empty()
            && !matches!(
                self.state.sync_state,
                SyncState::Dirty | SyncState::Executing
            )
    }
    pub fn finish_command(&mut self, command_id: Uuid) -> Option<String> {
        let cell_id = self.pending_execution_cells.remove(&command_id);
        self.pending_execution_sources.remove(&command_id);
        cell_id
    }
    pub fn reveal_output(&mut self, cell_id: &str) {
        if self.state.snapshot.notebook.workspace == "temporary"
            && self
                .state
                .snapshot
                .cells
                .iter()
                .any(|cell| cell.id == cell_id && !cell.outputs.is_empty())
        {
            self.output_views.remove(cell_id);
            self.scroll_to_output = Some(cell_id.to_owned());
        }
    }
    pub fn replace_state(&mut self, state: NotebookState) {
        let previous_markdown: HashSet<_> = self
            .state
            .snapshot
            .cells
            .iter()
            .filter(|cell| cell.cell_type == CellType::Markdown)
            .map(|cell| cell.id.clone())
            .collect();
        let selected = self.state.snapshot.selected_cell_id.clone();
        self.state = state;
        if self.microscope_target.as_ref().is_some_and(|t| {
            notebook_protocol::microscope::document(
                &self.state.snapshot,
                &t.cell_id,
                &t.microscope_id,
            )
            .is_err()
        }) {
            self.microscope_target = None;
            self.microscope_document = None;
        }
        if let Some(selected) = selected
            && self
                .state
                .snapshot
                .cells
                .iter()
                .any(|cell| cell.id == selected)
        {
            self.state.snapshot.selected_cell_id = Some(selected);
        }
        let current_markdown: HashSet<_> = self
            .state
            .snapshot
            .cells
            .iter()
            .filter(|cell| cell.cell_type == CellType::Markdown)
            .map(|cell| cell.id.clone())
            .collect();
        self.rendered_markdown
            .retain(|cell_id| current_markdown.contains(cell_id));
        self.rendered_markdown
            .extend(current_markdown.difference(&previous_markdown).cloned());
        let current_cells = self
            .state
            .snapshot
            .cells
            .iter()
            .map(|cell| cell.id.as_str())
            .collect::<HashSet<_>>();
        self.output_views
            .retain(|cell_id, _| current_cells.contains(cell_id.as_str()));
        self.agent_highlights
            .retain(|cell_id, _| current_cells.contains(cell_id.as_str()));
        self.editors = self
            .state
            .snapshot
            .cells
            .iter()
            .map(|c| (c.id.clone(), c.source.clone()))
            .collect();
        self.dirty_editors.clear();
    }
    fn emit(&mut self, mut kind: NotebookCommandKind) {
        if self.state.snapshot.notebook.workspace == "temporary"
            && !matches!(
                &kind,
                NotebookCommandKind::ExecuteCell { .. }
                    | NotebookCommandKind::Complete { .. }
                    | NotebookCommandKind::Inspect { .. }
                    | NotebookCommandKind::InterruptKernel
                    | NotebookCommandKind::Query { .. }
            )
            && !matches!(&kind, NotebookCommandKind::ModifyCells { changes } if changes.iter().all(|c| matches!(c,CellMutation::Update { .. } | CellMutation::ClearOutputs { .. })))
        {
            return;
        }
        if self.read_only && !matches!(kind, NotebookCommandKind::ReadMicroscope { .. }) {
            return;
        }
        if !self.suppress_history
            && let NotebookCommandKind::ModifyCells { changes } = &kind
            && let Some(inverse) = inverse_cell_changes(&self.state.snapshot.cells, changes)
        {
            self.undo_stack.push(inverse);
            self.redo_stack.clear();
        }
        let command_id = Uuid::new_v4();
        if let NotebookCommandKind::ModifyCells { changes } = &mut kind {
            anchor_cell_changes(&self.state.snapshot.cells, changes);
        }
        if let NotebookCommandKind::ExecuteCell { cell_id } = &kind {
            self.pending_execution_cells
                .insert(command_id, cell_id.clone());
            if let Some(cell) = self
                .state
                .snapshot
                .cells
                .iter()
                .find(|cell| &cell.id == cell_id)
            {
                self.pending_execution_sources
                    .insert(command_id, self.editor_source(cell));
            }
        }
        let timeout_ms = if self.state.snapshot.kernel.name.starts_with("julia")
            && matches!(
                kind,
                NotebookCommandKind::ExecuteCell { .. } | NotebookCommandKind::ExecuteCode { .. }
            ) {
            120_000
        } else {
            30_000
        };
        self.outbound.push_back(NotebookCommand {
            protocol_version: PROTOCOL_VERSION,
            command_id,
            idempotency_key: Uuid::new_v4().to_string(),
            expected_revision: Some(self.state.snapshot.revision),
            timeout_ms,
            kind,
        });
    }
    fn request_completion(&mut self, cell: &Cell) {
        let code = self.editor_source(cell);
        let cursor_pos = self
            .caret_byte_positions
            .get(&cell.id)
            .copied()
            .unwrap_or(code.len())
            .min(code.len());
        let command_id = Uuid::new_v4();
        self.pending_completions.insert(command_id, cell.id.clone());
        self.outbound.push_back(NotebookCommand {
            protocol_version: PROTOCOL_VERSION,
            command_id,
            idempotency_key: Uuid::new_v4().to_string(),
            expected_revision: Some(self.state.snapshot.revision),
            timeout_ms: 10_000,
            kind: NotebookCommandKind::Complete { cursor_pos, code },
        });
    }
    fn request_inspection(&mut self, cell: &Cell) {
        let code = self.editor_source(cell);
        let cursor_pos = self
            .caret_byte_positions
            .get(&cell.id)
            .copied()
            .unwrap_or(code.len())
            .min(code.len());
        let command_id = Uuid::new_v4();
        self.pending_inspections.insert(command_id, cell.id.clone());
        self.outbound.push_back(NotebookCommand {
            protocol_version: PROTOCOL_VERSION,
            command_id,
            idempotency_key: Uuid::new_v4().to_string(),
            expected_revision: Some(self.state.snapshot.revision),
            timeout_ms: 10_000,
            kind: NotebookCommandKind::Inspect { code, cursor_pos },
        });
    }
    fn close_completion(&mut self, cell_id: &str) {
        self.completion_suggestions.remove(cell_id);
        self.completion_selection.remove(cell_id);
    }
    fn accept_completion(&mut self, cell_id: &str) {
        let Some(completion) = self.completion_suggestions.get(cell_id).cloned() else {
            return;
        };
        let index = self
            .completion_selection
            .get(cell_id)
            .copied()
            .unwrap_or_default()
            .min(completion.matches.len().saturating_sub(1));
        let Some(value) = completion.matches.get(index) else {
            self.close_completion(cell_id);
            return;
        };
        if let Some((_, editor)) = self.editors.iter_mut().find(|(id, _)| id == cell_id) {
            let start = completion.cursor_start.min(editor.len());
            let end = completion.cursor_end.min(editor.len()).max(start);
            if editor.is_char_boundary(start) && editor.is_char_boundary(end) {
                editor.replace_range(start..end, value);
                let caret = editor[..start + value.len()].chars().count();
                self.pending_caret_positions
                    .insert(cell_id.to_owned(), caret);
                self.dirty_editors.insert(cell_id.to_owned());
            }
        }
        self.close_completion(cell_id);
    }
    fn editor_source(&self, cell: &Cell) -> String {
        self.editors
            .iter()
            .find(|(id, _)| id == &cell.id)
            .map_or_else(|| cell.source.clone(), |(_, source)| source.clone())
    }
    fn flush_editor(&mut self, cell: &Cell) {
        if self.dirty_editors.remove(&cell.id) {
            self.emit(NotebookCommandKind::ModifyCells {
                changes: vec![CellMutation::Update {
                    cell_id: cell.id.clone(),
                    source: Some(self.editor_source(cell)),
                    metadata: None,
                    cell_type: Some(cell.cell_type.clone()),
                }],
            });
        }
    }
    pub fn active_context(&self) -> serde_json::Value {
        let selected = self.selected_cell().map(|(index, cell)| {
            if cell.cell_type == CellType::Markdown
                && self
                    .state
                    .snapshot
                    .cells
                    .get(index + 1)
                    .is_some_and(|code| combines_with_markdown(code, &cell.id))
            {
                (index + 1, self.state.snapshot.cells[index + 1].clone())
            } else {
                (index, cell)
            }
        });
        let selection = selected.as_ref().map(|(index, cell)| {
            let execution = self
                .pending_execution_cells
                .iter()
                .find(|(_, cell_id)| *cell_id == &cell.id)
                .and_then(|(command_id, _)| self.pending_execution_sources.get(command_id))
                .map(|source| {
                    serde_json::json!({
                        "status": if matches!(self.state.sync_state, SyncState::Executing)
                            || self.state.snapshot.kernel.state == KernelState::Busy
                        { "running" } else { "queued" },
                        "source": source,
                    })
                });
            serde_json::json!({
                "cell_id": cell.id,
                "cell_index": index,
                "mode": if self.edit_mode { "edit" } else { "command" },
                "draft": {
                    "source": self.editor_source(cell),
                    "dirty": self.dirty_editors.contains(&cell.id),
                },
                "execution": execution,
            })
        });
        let microscope = self.microscope_target.as_ref().map(|target| {
            serde_json::json!({
                "cell_id": target.cell_id,
                "microscope_id": target.microscope_id,
                "revision": target.revision,
                "focus": target.focus,
                "loaded": self.microscope_document.is_some(),
                "walkthrough": self.walkthrough_context(),
            })
        });
        serde_json::json!({
            "view": if microscope.is_some() { "microscope" } else { "notebook" },
            "notebook": {
                "path": self.state.snapshot.notebook.path,
                "revision": self.state.snapshot.revision,
            },
            "selection": selection,
            "scroll_fraction": self.scroll_fraction,
            "microscope": microscope,
            "playground": null,
        })
    }
    pub fn capture_microscope_step(&mut self) -> Result<(), String> {
        if self.microscope_target.is_none() || self.microscope_document.is_none() {
            return Err("Open a microscope step before capturing it".into());
        }
        self.captured_cell = None;
        self.microscope_capture_frames = Some(2);
        Ok(())
    }
    fn selected_cell(&self) -> Option<(usize, Cell)> {
        let selected = self.state.snapshot.selected_cell_id.as_ref()?;
        self.state
            .snapshot
            .cells
            .iter()
            .position(|cell| &cell.id == selected)
            .map(|index| (index, self.state.snapshot.cells[index].clone()))
    }

    fn selected_markdown_group_code(&self) -> Option<(usize, Cell)> {
        let (index, cell) = self.selected_cell()?;
        if cell.cell_type == CellType::Code
            && index > 0
            && self.state.snapshot.cells[index - 1].cell_type == CellType::Markdown
        {
            Some((index, cell))
        } else if cell.cell_type == CellType::Markdown
            && self
                .state
                .snapshot
                .cells
                .get(index + 1)
                .is_some_and(|code| combines_with_markdown(code, &cell.id))
        {
            Some((index + 1, self.state.snapshot.cells[index + 1].clone()))
        } else {
            None
        }
    }

    fn set_selected_markdown_group(&mut self, grouped: bool) {
        let Some((cell_index, cell)) = self.selected_markdown_group_code() else {
            return;
        };
        let Some(metadata) = cell.metadata.as_object().cloned() else {
            return;
        };
        let mut metadata = metadata;
        if grouped {
            let markdown_cell_id = self.state.snapshot.cells[cell_index - 1].id.clone();
            metadata.insert(
                MARKDOWN_GROUP_METADATA_KEY.into(),
                serde_json::json!({
                    "schema_version": 1,
                    "markdown_cell_id": markdown_cell_id,
                }),
            );
        } else {
            metadata.remove(MARKDOWN_GROUP_METADATA_KEY);
        }
        self.state.snapshot.selected_cell_id = Some(cell.id.clone());
        self.emit(NotebookCommandKind::ModifyCells {
            changes: vec![CellMutation::Update {
                cell_id: cell.id,
                source: None,
                metadata: Some(serde_json::Value::Object(metadata)),
                cell_type: None,
            }],
        });
    }
    fn save_visible_edits(&mut self) {
        for cell in self.state.snapshot.cells.clone() {
            self.flush_editor(&cell);
        }
    }
    fn execute_selected(&mut self) {
        if let Some((_, cell)) = self.selected_cell() {
            self.flush_editor(&cell);
            if cell.cell_type == CellType::Code {
                self.emit(NotebookCommandKind::ExecuteCell { cell_id: cell.id });
            } else if cell.cell_type == CellType::Markdown {
                self.rendered_markdown.insert(cell.id);
                self.pending_editor_focus = None;
                self.edit_mode = false;
            }
        }
    }
    fn execute_cell(&mut self, index: usize, cell: &Cell) {
        if cell.cell_type != CellType::Code {
            return;
        }
        self.select_cell(index, false);
        self.flush_editor(cell);
        self.emit(NotebookCommandKind::ExecuteCell {
            cell_id: cell.id.clone(),
        });
    }
    fn execute_selected_and_advance(&mut self, always_insert: bool) {
        let Some((index, _)) = self.selected_cell() else {
            return;
        };
        self.execute_selected();
        if !always_insert && let Some(next) = self.state.snapshot.cells.get(index + 1) {
            self.selected_cells.clear();
            self.state.snapshot.selected_cell_id = Some(next.id.clone());
            self.edit_mode = false;
            return;
        }
        let id = self.insert_cell(index + 1, CellType::Code, String::new());
        self.selected_cells.clear();
        self.state.snapshot.selected_cell_id = Some(id.clone());
        self.pending_editor_focus = always_insert.then_some(id);
        self.edit_mode = always_insert;
    }
    fn move_selected(&mut self, index: usize) {
        if let Some((current, cell)) = self.selected_cell()
            && current != index
        {
            self.flush_editor(&cell);
            self.emit(NotebookCommandKind::ModifyCells {
                changes: vec![CellMutation::Move {
                    cell_id: cell.id,
                    index,
                }],
            });
        }
    }
    fn set_selected_cell_type(&mut self, cell_type: CellType) {
        if let Some((_, cell)) = self.selected_cell()
            && cell.cell_type != cell_type
        {
            self.flush_editor(&cell);
            let source = self.editor_source(&cell);
            self.emit(NotebookCommandKind::ModifyCells {
                changes: vec![CellMutation::Update {
                    cell_id: cell.id,
                    source: Some(source),
                    metadata: None,
                    cell_type: Some(cell_type),
                }],
            });
        }
    }
    fn delete_selected(&mut self) {
        let cells = self.selected_cells_in_order();
        if !cells.is_empty() {
            for cell in &cells {
                self.flush_editor(cell);
            }
            self.emit(NotebookCommandKind::ModifyCells {
                changes: cells
                    .into_iter()
                    .map(|cell| CellMutation::Delete { cell_id: cell.id })
                    .collect(),
            });
            self.selected_cells.clear();
        }
    }
    fn duplicate_selected(&mut self) {
        if let Some((index, cell)) = self.selected_cell() {
            self.flush_editor(&cell);
            self.insert_cell(index + 1, cell.cell_type.clone(), self.editor_source(&cell));
        }
    }
    fn copy_selected(&mut self) {
        self.cell_clipboard = self.selected_cells_in_order();
    }
    fn cut_selected(&mut self) {
        self.copy_selected();
        self.delete_selected();
    }
    fn selected_cells_in_order(&self) -> Vec<Cell> {
        let active = self.state.snapshot.selected_cell_id.as_deref();
        self.state
            .snapshot
            .cells
            .iter()
            .filter(|cell| {
                if self.selected_cells.is_empty() {
                    Some(cell.id.as_str()) == active
                } else {
                    self.selected_cells.contains(&cell.id)
                }
            })
            .cloned()
            .collect()
    }
    fn select_cell(&mut self, index: usize, extend: bool) {
        let Some(cell) = self.state.snapshot.cells.get(index) else {
            return;
        };
        let id = cell.id.clone();
        let selected_type = cell.cell_type.clone();
        if extend {
            let anchor = self
                .state
                .snapshot
                .selected_cell_id
                .as_ref()
                .and_then(|selected| {
                    self.state
                        .snapshot
                        .cells
                        .iter()
                        .position(|cell| &cell.id == selected)
                })
                .unwrap_or(index);
            self.selected_cells.clear();
            for cell in &self.state.snapshot.cells[anchor.min(index)..=anchor.max(index)] {
                if cell.cell_type == selected_type {
                    self.selected_cells.insert(cell.id.clone());
                }
            }
        } else {
            self.selected_cells.clear();
            self.selected_cells.insert(id.clone());
        }
        self.state.snapshot.selected_cell_id = Some(id);
    }
    fn paste_below_selected(&mut self) {
        if self.cell_clipboard.is_empty() {
            return;
        }
        let start = self
            .selected_cell()
            .map_or(self.state.snapshot.cells.len(), |(index, _)| index + 1);
        let changes = self
            .cell_clipboard
            .iter()
            .cloned()
            .enumerate()
            .map(|(offset, mut cell)| {
                cell.id = Uuid::new_v4().to_string();
                cell.execution_count = None;
                cell.outputs.clear();
                CellMutation::Insert {
                    index: start + offset,
                    cell,
                }
            })
            .collect();
        self.emit(NotebookCommandKind::ModifyCells { changes });
    }
    fn replay_history(&mut self, undo: bool) {
        let changes = if undo {
            self.undo_stack.pop()
        } else {
            self.redo_stack.pop()
        };
        let Some(changes) = changes else { return };
        let inverse =
            inverse_cell_changes(&self.state.snapshot.cells, &changes).unwrap_or_default();
        self.suppress_history = true;
        self.emit(NotebookCommandKind::ModifyCells { changes });
        self.suppress_history = false;
        if undo {
            self.redo_stack.push(inverse);
        } else {
            self.undo_stack.push(inverse);
        }
    }
    fn clear_selected_outputs(&mut self) {
        if let Some((_, cell)) = self.selected_cell()
            && cell.cell_type == CellType::Code
        {
            self.emit(NotebookCommandKind::ModifyCells {
                changes: vec![CellMutation::ClearOutputs { cell_id: cell.id }],
            });
        }
    }
    fn clear_all_outputs(&mut self) {
        let changes = self
            .state
            .snapshot
            .cells
            .iter()
            .filter(|cell| cell.cell_type == CellType::Code)
            .map(|cell| CellMutation::ClearOutputs {
                cell_id: cell.id.clone(),
            })
            .collect::<Vec<_>>();
        if !changes.is_empty() {
            self.emit(NotebookCommandKind::ModifyCells { changes });
        }
    }
    fn cycle_selected_output_view(&mut self) {
        let Some((_, cell)) = self.selected_cell() else {
            return;
        };
        if cell.cell_type != CellType::Code || cell.outputs.is_empty() {
            return;
        }
        let next = self.output_view(&cell.id).next();
        if next == OutputViewMode::Expanded {
            self.output_views.remove(&cell.id);
        } else {
            self.output_views.insert(cell.id, next);
        }
    }

    fn output_view(&self, cell_id: &str) -> OutputViewMode {
        self.output_views.get(cell_id).copied().unwrap_or_default()
    }

    fn set_output_view(&mut self, cell_id: &str, mode: OutputViewMode) {
        if mode == OutputViewMode::Expanded {
            self.output_views.remove(cell_id);
        } else {
            self.output_views.insert(cell_id.to_owned(), mode);
        }
    }
    fn find_next(&mut self) {
        if self.find_query.is_empty() {
            return;
        }
        let start = self.selected_cell().map_or(0, |(index, _)| index + 1);
        let len = self.state.snapshot.cells.len();
        for offset in 0..len {
            let index = (start + offset) % len;
            let cell = &self.state.snapshot.cells[index];
            if self.editor_source(cell).contains(&self.find_query) {
                let id = cell.id.clone();
                self.state.snapshot.selected_cell_id = Some(id.clone());
                self.selected_cells.clear();
                self.collapsed_cells.remove(&id);
                if cell.cell_type == CellType::Markdown {
                    self.rendered_markdown.remove(&id);
                }
                self.pending_editor_focus = Some(id);
                self.edit_mode = true;
                break;
            }
        }
    }
    fn replace_all(&mut self) {
        if self.find_query.is_empty() {
            return;
        }
        let mut changes = Vec::new();
        for cell in self.state.snapshot.cells.clone() {
            let source = self.editor_source(&cell);
            if source.contains(&self.find_query) {
                let replacement = source.replace(&self.find_query, &self.replace_query);
                if let Some((_, editor)) = self.editors.iter_mut().find(|(id, _)| id == &cell.id) {
                    editor.clone_from(&replacement);
                }
                changes.push(CellMutation::Update {
                    cell_id: cell.id,
                    source: Some(replacement),
                    metadata: None,
                    cell_type: None,
                });
            }
        }
        if !changes.is_empty() {
            self.emit(NotebookCommandKind::ModifyCells { changes });
        }
    }
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        if self.state.snapshot.notebook.workspace == "temporary" {
            ui.horizontal(|ui| {
                ui.heading("Playground");
                if toolbar_icon_button(ui, !self.read_only, ToolbarIcon::Run, "Run playground") {
                    self.execute_selected();
                }
                if toolbar_icon_button(
                    ui,
                    !self.read_only,
                    ToolbarIcon::Stop,
                    "Interrupt playground",
                ) {
                    self.emit(NotebookCommandKind::InterruptKernel);
                }
                ui.label("Temporary · edits and outputs are discarded on exit");
            });
            return;
        }
        let idle = !matches!(
            self.state.sync_state,
            SyncState::Dirty | SyncState::Executing
        );
        let selected = self.selected_cell();
        let selected_type = selected.as_ref().map(|(_, cell)| cell.cell_type.clone());
        let markdown_group = self.selected_markdown_group_code().map(|(index, cell)| {
            combines_with_markdown(&cell, &self.state.snapshot.cells[index - 1].id)
        });
        ui.vertical(|ui| {
            ui.horizontal_wrapped(|ui| {
                if self.read_only {
                    ui.disable();
                }
                ui.menu_button("File", |ui| {
                    if ui
                        .add_enabled(idle, egui::Button::new("Save Notebook"))
                        .clicked()
                    {
                        self.save_visible_edits();
                        ui.close();
                    }
                    if ui.button("Rename Notebook…").clicked() {
                        self.rename_path = self.state.snapshot.notebook.path.clone();
                        self.rename_open = true;
                        ui.close();
                    }
                    if ui.button("Download Notebook").clicked() {
                        self.save_visible_edits();
                        self.emit(NotebookCommandKind::DownloadNotebook);
                        ui.close();
                    }
                    if ui
                        .add_enabled(idle && self.checkpoints_supported, egui::Button::new("Create Checkpoint"))
                        .on_disabled_hover_text(if self.checkpoints_supported {
                            "Wait for the current operation to finish before creating a checkpoint."
                        } else {
                            "Checkpoints are unavailable in browser mode. Export the workspace for a backup."
                        })
                        .clicked()
                    {
                        self.save_visible_edits();
                        self.emit(NotebookCommandKind::CreateCheckpoint);
                        ui.close();
                    }
                });
                ui.menu_button("Edit", |ui| {
                    if ui.button("Find and Replace…").clicked() {
                        self.find_open = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            !self.undo_stack.is_empty(),
                            egui::Button::new("Undo Cell Operation"),
                        )
                        .clicked()
                    {
                        self.replay_history(true);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            !self.redo_stack.is_empty(),
                            egui::Button::new("Redo Cell Operation"),
                        )
                        .clicked()
                    {
                        self.replay_history(false);
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(selected.is_some(), egui::Button::new("Cut Cell"))
                        .clicked()
                    {
                        self.cut_selected();
                        ui.close();
                    }
                    if ui
                        .add_enabled(selected.is_some(), egui::Button::new("Copy Cell"))
                        .clicked()
                    {
                        self.copy_selected();
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            !self.cell_clipboard.is_empty(),
                            egui::Button::new("Paste Cell Below"),
                        )
                        .clicked()
                    {
                        self.paste_below_selected();
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            idle && selected.is_some(),
                            egui::Button::new("Duplicate Cell"),
                        )
                        .clicked()
                    {
                        self.duplicate_selected();
                        ui.close();
                    }
                    if ui
                        .add_enabled(idle && selected.is_some(), egui::Button::new("Delete Cell"))
                        .clicked()
                    {
                        self.delete_selected();
                        ui.close();
                    }
                });
                ui.menu_button("View", |ui| {
                    if let Some((_, cell)) = selected.as_ref() {
                        let collapsed = self.collapsed_cells.contains(&cell.id);
                        if ui
                            .button(if collapsed {
                                "Expand Cell"
                            } else {
                                "Collapse Cell"
                            })
                            .clicked()
                        {
                            if collapsed {
                                self.collapsed_cells.remove(&cell.id);
                            } else {
                                self.collapsed_cells.insert(cell.id.clone());
                            }
                            ui.close();
                        }
                        if cell.cell_type == CellType::Code {
                            let output_view = self.output_view(&cell.id);
                            ui.add_enabled_ui(!cell.outputs.is_empty(), |ui| {
                                ui.label("Output View");
                                for (mode, label) in [
                                    (OutputViewMode::Expanded, "Fully Open"),
                                    (OutputViewMode::Windowed, "Scroll Latest"),
                                    (OutputViewMode::Collapsed, "Collapsed"),
                                ] {
                                    if ui.selectable_label(output_view == mode, label).clicked() {
                                        self.set_output_view(&cell.id, mode);
                                        ui.close();
                                    }
                                }
                            });
                            if ui.button("Cycle Output View (O)").clicked() {
                                self.cycle_selected_output_view();
                                ui.close();
                            }
                            let hidden = self.hidden_line_numbers.contains(&cell.id);
                            if ui
                                .button(if hidden {
                                    "Show Line Numbers"
                                } else {
                                    "Hide Line Numbers"
                                })
                                .clicked()
                            {
                                if hidden {
                                    self.hidden_line_numbers.remove(&cell.id);
                                } else {
                                    self.hidden_line_numbers.insert(cell.id.clone());
                                }
                                ui.close();
                            }
                        }
                        ui.separator();
                    }
                    if ui
                        .add_enabled(self.edit_mode, egui::Button::new("Command Mode"))
                        .clicked()
                    {
                        self.edit_mode = false;
                        ui.close();
                    }
                });
                ui.menu_button("Insert", |ui| {
                    if ui
                        .add_enabled(idle, egui::Button::new("Insert Cell Above"))
                        .clicked()
                    {
                        let index = selected.as_ref().map_or(0, |(index, _)| *index);
                        self.insert_cell(index, CellType::Code, String::new());
                        ui.close();
                    }
                    if ui
                        .add_enabled(idle, egui::Button::new("Insert Cell Below"))
                        .clicked()
                    {
                        self.insert_cell_after_selection(CellType::Code);
                        ui.close();
                    }
                });
                ui.menu_button("Cell", |ui| {
                    ui.menu_button("Cell Type", |ui| {
                        for (label, cell_type) in [
                            ("Code", CellType::Code),
                            ("Markdown", CellType::Markdown),
                            ("Raw", CellType::Raw),
                        ] {
                            if ui
                                .selectable_label(selected_type.as_ref() == Some(&cell_type), label)
                                .clicked()
                            {
                                self.set_selected_cell_type(cell_type);
                                ui.close();
                            }
                        }
                    });
                    ui.separator();
                    if ui
                        .add_enabled(
                            idle && markdown_group.is_some(),
                            egui::Button::new(if markdown_group == Some(true) {
                                "Ungroup Markdown and Code"
                            } else {
                                "Group with Previous Markdown"
                            }),
                        )
                        .on_disabled_hover_text(
                            "Select a code cell immediately following Markdown",
                        )
                        .clicked()
                    {
                        self.set_selected_markdown_group(markdown_group != Some(true));
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            selected_type.as_ref() == Some(&CellType::Code),
                            egui::Button::new("Clear Selected Output"),
                        )
                        .clicked()
                    {
                        self.clear_selected_outputs();
                        ui.close();
                    }
                    if ui.button("Clear All Outputs").clicked() {
                        self.clear_all_outputs();
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            idle && selected.is_some(),
                            egui::Button::new("Duplicate Cell"),
                        )
                        .clicked()
                    {
                        self.duplicate_selected();
                        ui.close();
                    }
                    if ui
                        .add_enabled(idle && selected.is_some(), egui::Button::new("Delete Cell"))
                        .clicked()
                    {
                        self.delete_selected();
                        ui.close();
                    }
                });
                ui.menu_button("Run", |ui| {
                    if ui
                        .add_enabled(
                            idle && selected_type.as_ref() == Some(&CellType::Code),
                            egui::Button::new("Run Selected Cell"),
                        )
                        .clicked()
                    {
                        self.execute_selected();
                        ui.close();
                    }
                    if ui
                        .add_enabled(idle, egui::Button::new("Run All Cells"))
                        .clicked()
                    {
                        self.save_visible_edits();
                        for cell in self.state.snapshot.cells.clone() {
                            if cell.cell_type == CellType::Code {
                                self.emit(NotebookCommandKind::ExecuteCell { cell_id: cell.id });
                            }
                        }
                        ui.close();
                    }
                });
                ui.menu_button("Kernel", |ui| {
                    if ui.button("Interrupt Kernel").clicked() {
                        self.emit(NotebookCommandKind::InterruptKernel);
                        ui.close();
                    }
                    if ui
                        .add_enabled(idle, egui::Button::new("Restart Kernel"))
                        .clicked()
                    {
                        self.restart_confirmation = true;
                        ui.close();
                    }
                });
                ui.menu_button("Help", |ui| {
                    ui.label("Enter: edit · Esc: command");
                    ui.label("A/B: insert above/below");
                    ui.label("Cmd/Ctrl+Enter: run cell");
                    ui.label("Tab: complete code");
                });
            });
            ui.horizontal_wrapped(|ui| {
                if self.read_only {
                    if toolbar_icon_button(
                        ui,
                        true,
                        ToolbarIcon::Follow(self.following_driver),
                        if self.following_driver {
                            "Stop following driver"
                        } else {
                            "Follow driver"
                        },
                    ) {
                        self.follow_toggle_requested = true;
                    }
                    ui.disable();
                }
                if toolbar_icon_button(ui, idle, ToolbarIcon::Save, "Save notebook") {
                    self.save_visible_edits();
                }
                if toolbar_icon_button(ui, idle, ToolbarIcon::Add, "Insert cell below") {
                    self.insert_cell_after_selection(CellType::Code);
                }
                ui.separator();
                let selected_index = selected.as_ref().map(|(index, _)| *index);
                if toolbar_icon_button(
                    ui,
                    idle && selected_index.is_some_and(|index| index > 0),
                    ToolbarIcon::Up,
                    "Move selected cell up",
                ) && let Some(index) = selected_index
                {
                    self.move_selected(index - 1);
                }
                if toolbar_icon_button(
                    ui,
                    idle && selected_index
                        .is_some_and(|index| index + 1 < self.state.snapshot.cells.len()),
                    ToolbarIcon::Down,
                    "Move selected cell down",
                ) && let Some(index) = selected_index
                {
                    self.move_selected(index + 1);
                }
                ui.separator();
                if toolbar_icon_button(
                    ui,
                    idle && selected_type.as_ref() == Some(&CellType::Code),
                    ToolbarIcon::Run,
                    "Run selected cell",
                ) {
                    self.execute_selected();
                }
                if toolbar_icon_button(ui, true, ToolbarIcon::Stop, "Interrupt kernel") {
                    self.emit(NotebookCommandKind::InterruptKernel);
                }
                if toolbar_icon_button(ui, idle, ToolbarIcon::Restart, "Restart kernel") {
                    self.restart_confirmation = true;
                }
                ui.separator();
                egui::ComboBox::from_id_salt("selected-cell-type")
                    .selected_text(match selected_type.as_ref() {
                        Some(CellType::Code) => "Code",
                        Some(CellType::Markdown) => "Markdown",
                        Some(CellType::Raw) => "Raw",
                        None => "Cell type",
                    })
                    .show_ui(ui, |ui| {
                        for (label, cell_type) in [
                            ("Code", CellType::Code),
                            ("Markdown", CellType::Markdown),
                            ("Raw", CellType::Raw),
                        ] {
                            if ui
                                .selectable_label(selected_type.as_ref() == Some(&cell_type), label)
                                .clicked()
                            {
                                self.set_selected_cell_type(cell_type);
                                ui.close();
                            }
                        }
                    });
                ui.separator();
                ui.label(if self.edit_mode { "Edit" } else { "Command" });
                if self.state.sync_state == SyncState::Disconnected
                    && ui
                        .add_enabled(idle, egui::Button::new("Reconnect"))
                        .clicked()
                {
                    self.emit(NotebookCommandKind::Reconnect);
                }
            });
            if self.find_open {
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.label("Find");
                    ui.add(
                        TextEdit::singleline(&mut self.find_query)
                            .desired_width(180.0)
                            .hint_text("Search notebook"),
                    );
                    ui.label("Replace");
                    ui.add(
                        TextEdit::singleline(&mut self.replace_query)
                            .desired_width(180.0)
                            .hint_text("Replacement"),
                    );
                    if ui.button("Next").clicked() {
                        self.find_next();
                    }
                    if ui.button("Replace All").clicked() {
                        self.replace_all();
                    }
                    if ui.button("Close").clicked() {
                        self.find_open = false;
                    }
                });
            }
            if self.rename_open {
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.label("Notebook name");
                    ui.add(
                        TextEdit::singleline(&mut self.rename_path)
                            .desired_width(260.0)
                            .hint_text("notebook.ipynb"),
                    );
                    if ui.button("Rename").clicked() && !self.rename_path.trim().is_empty() {
                        self.emit(NotebookCommandKind::RenameNotebook {
                            path: self.rename_path.trim().to_owned(),
                        });
                        self.rename_open = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.rename_open = false;
                    }
                });
            }
            if self.restart_confirmation {
                egui::Frame::new()
                    .fill(Color32::from_rgb(255, 248, 225))
                    .inner_margin(Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Restarting clears all variables in the current kernel.");
                            if ui.button("Cancel").clicked() {
                                self.restart_confirmation = false;
                            }
                            if ui
                                .add_enabled(idle, egui::Button::new("Restart kernel"))
                                .clicked()
                            {
                                self.restart_confirmation = false;
                                self.emit(NotebookCommandKind::RestartKernel);
                            }
                        });
                    });
            }
        });
    }
    fn insert_cell_after_selection(&mut self, cell_type: CellType) {
        if let Some(selected) = self
            .state
            .snapshot
            .cells
            .iter()
            .find(|cell| Some(&cell.id) == self.state.snapshot.selected_cell_id.as_ref())
            .cloned()
        {
            self.flush_editor(&selected);
        }
        let index = self
            .state
            .snapshot
            .selected_cell_id
            .as_ref()
            .and_then(|id| {
                self.state
                    .snapshot
                    .cells
                    .iter()
                    .position(|cell| &cell.id == id)
            })
            .map_or(self.state.snapshot.cells.len(), |index| index + 1);
        self.insert_cell(index, cell_type, String::new());
    }
    fn insert_cell(&mut self, index: usize, cell_type: CellType, source: String) -> String {
        let cell = Cell {
            id: Uuid::new_v4().to_string(),
            cell_type,
            source,
            metadata: serde_json::json!({}),
            execution_count: None,
            outputs: vec![],
        };
        let id = cell.id.clone();
        self.emit(NotebookCommandKind::ModifyCells {
            changes: vec![CellMutation::Insert { index, cell }],
        });
        id
    }
    fn status(&mut self, ui: &mut egui::Ui) {
        let temporary = self.state.snapshot.notebook.workspace == "temporary";
        let (label, color) = if !self.dirty_editors.is_empty() {
            (
                if temporary {
                    "Edit pending"
                } else {
                    "Autosave pending"
                },
                Color32::from_rgb(239, 108, 0),
            )
        } else {
            match self.state.sync_state {
                SyncState::Synchronized => (
                    if temporary { "Ready" } else { "Saved" },
                    Color32::from_rgb(46, 125, 50),
                ),
                SyncState::Dirty => ("Pending", Color32::from_rgb(239, 108, 0)),
                SyncState::Executing => ("Running", Color32::from_rgb(2, 119, 189)),
                SyncState::Disconnected => ("Disconnected", Color32::from_rgb(198, 40, 40)),
                SyncState::Error => ("Action required", Color32::from_rgb(198, 40, 40)),
            }
        };
        let status_row = ui.horizontal_top(|ui| {
            let width = (ui.available_width() - 32.0).max(0.0);
            let status = ui.allocate_ui_with_layout(
                egui::vec2(width, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_min_width(width);
                    ui.horizontal_wrapped(|ui| {
                        if !temporary
                            && toolbar_icon_button(
                                ui,
                                true,
                                ToolbarIcon::Workspace,
                                if self.workspace_visible {
                                    "Hide workspace explorer"
                                } else {
                                    "Show workspace explorer"
                                },
                            )
                        {
                            self.workspace_toggle_requested = true;
                        }
                        ui.label(if self.read_only {
                            "Observer · Read-only"
                        } else {
                            "Driver"
                        });
                        ui.separator();
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 4.0, color);
                        ui.label(label);
                        ui.separator();
                        ui.label(format!(
                            "Kernel: {} · {:?}",
                            self.state.snapshot.kernel.display_name,
                            self.state.snapshot.kernel.state
                        ));
                        ui.separator();
                        ui.label(format!("Revision {}", self.state.snapshot.revision));
                        for item in self.host_status.split(" · ") {
                            ui.separator();
                            ui.label(item);
                        }
                    });
                    if let Some(error) = &self.state.last_error {
                        ui.colored_label(
                            Color32::from_rgb(183, 28, 28),
                            format!(
                                "{} — {}",
                                error.message,
                                if error.retryable {
                                    "Retry or reconnect."
                                } else {
                                    "Edit the request and try again."
                                }
                            ),
                        );
                    }
                },
            );
            ui.allocate_ui_with_layout(
                egui::vec2(24.0, status.response.rect.height().max(24.0)),
                egui::Layout::bottom_up(egui::Align::Max),
                |ui| {
                    if diagnostics_button(ui).clicked() {
                        self.diagnostics_toggle_requested = true;
                    }
                },
            );
        });
        if status_row.response.rect.bottom() > ui.clip_rect().bottom() + 0.5 {
            // Bottom panels position with their previous height, then measure wrapping.
            ui.ctx().request_discard("status panel height changed");
        }
    }
    fn cell(&mut self, ui: &mut egui::Ui, index: usize, cell: Cell, grouped: bool) {
        let selected = self.selected_cells.contains(&cell.id)
            || self.state.snapshot.selected_cell_id.as_deref() == Some(cell.id.as_str());
        let mut collapsed = self.collapsed_cells.contains(&cell.id);
        let output_view = self.output_view(&cell.id);
        let frame = egui::Frame::new()
            .fill(if grouped {
                Color32::TRANSPARENT
            } else if selected {
                Color32::from_rgb(250, 253, 255)
            } else {
                Color32::WHITE
            })
            .stroke(Stroke::new(
                if grouped {
                    0.0
                } else if selected {
                    2.0
                } else {
                    1.0
                },
                if selected {
                    Color32::from_rgb(45, 105, 145)
                } else {
                    Color32::from_gray(210)
                },
            ))
            .corner_radius(if grouped {
                CornerRadius::ZERO
            } else {
                CornerRadius::same(3)
            })
            .inner_margin(Margin::same(10));
        let frame_response = frame.show(ui, |ui| {
            if self.external_command_active {
                ui.disable();
            }
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                let idle = !self.read_only
                    && !matches!(
                        self.state.sync_state,
                        SyncState::Dirty | SyncState::Executing
                    );
                let (toggle, output_choice) = cell_visibility_control(
                    ui,
                    collapsed,
                    cell.cell_type == CellType::Code && !cell.outputs.is_empty(),
                    output_view,
                );
                if toggle {
                    collapsed = !collapsed;
                    if collapsed {
                        self.collapsed_cells.insert(cell.id.clone());
                    } else {
                        self.collapsed_cells.remove(&cell.id);
                    }
                }
                if let Some(mode) = output_choice {
                    self.set_output_view(&cell.id, mode);
                }
                let drag = ui
                    .add_enabled(
                        idle,
                        drag_handle().min_size(egui::vec2(28.0, 28.0) * control_scale(ui)),
                    )
                    .on_hover_text("Drag to reorder cell");
                paint_drag_hand(ui, &drag);
                if drag.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                }
                if drag.drag_started() {
                    self.dragging_cell = Some(cell.id.clone());
                    self.state.snapshot.selected_cell_id = Some(cell.id.clone());
                    self.selected_cells.clear();
                }
                if drag.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                }
                let running = self
                    .pending_execution_cells
                    .values()
                    .any(|cell_id| cell_id == &cell.id);
                if cell.cell_type == CellType::Code
                    && cell_run_button(ui, idle && !running, running).clicked()
                {
                    self.execute_cell(index, &cell);
                }
                execution_status_icon(ui, cell_has_completed_execution(&cell), running);
                let prompt = ui.add(
                    egui::Label::new(
                        RichText::new(match cell.cell_type {
                            CellType::Code => format!(
                                "In [{}]",
                                cell.execution_count.map_or(" ".into(), |v| v.to_string())
                            ),
                            CellType::Markdown => "Markdown".into(),
                            CellType::Raw => "Raw".into(),
                        })
                        .monospace()
                        .color(Color32::from_rgb(75, 85, 92)),
                    )
                    .sense(egui::Sense::click()),
                );
                if prompt.clicked() {
                    self.select_cell(index, ui.input(|input| input.modifiers.shift));
                }
                let microscopes = notebook_protocol::microscope::list(&cell).unwrap_or_default();
                if !microscopes.is_empty() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let menu = ui.menu_button("        ", |ui| {
                            for item in microscopes {
                                ui.horizontal(|ui| {
                                    if ui.button(&item.title).clicked() {
                                        let _ = self.open_microscope(Some(
                                            notebook_protocol::microscope::MicroscopeTarget {
                                                cell_id: cell.id.clone(),
                                                microscope_id: item.id.clone(),
                                                revision: item.revision,
                                                focus: None,
                                            },
                                        ));
                                        ui.close();
                                    }
                                    if toolbar_icon_button(
                                        ui,
                                        !self.read_only,
                                        ToolbarIcon::Trash,
                                        "Delete microscope",
                                    ) {
                                        self.microscope_delete =
                                            Some((cell.id.clone(), item.clone()));
                                        ui.close();
                                    }
                                });
                            }
                        });
                        paint_microscope(
                            ui,
                            egui::Rect::from_center_size(
                                menu.response.rect.center(),
                                egui::vec2(18.0, 18.0) * control_scale(ui),
                            ),
                        );
                        let menu_label = format!(
                            "Microscopes ({})",
                            notebook_protocol::microscope::list(&cell)
                                .unwrap_or_default()
                                .len()
                        );
                        menu.response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                ui.is_enabled(),
                                &menu_label,
                            )
                        });
                        menu.response.on_hover_text(menu_label);
                    });
                }
            });
            if !collapsed {
                if cell.cell_type == CellType::Markdown {
                    let rendered = self.read_only || self.rendered_markdown.contains(&cell.id);
                    if rendered {
                        let source = self.editor_source(&cell);
                        let rendered = rendered_markdown_response(
                            ui,
                            &cell.id,
                            &source,
                            &mut self.markdown_cache,
                            &self.math_cache,
                        );
                        if let Some(url) = rendered.clicked_link {
                            self.markdown_link = Some((
                                url,
                                ui.ctx()
                                    .pointer_latest_pos()
                                    .unwrap_or(rendered.response.rect.center()),
                            ));
                        }
                        let rendered_response =
                            rendered.response.on_hover_text(if self.read_only {
                                "Read-only while another collaborator drives"
                            } else {
                                "Double-click to edit Markdown"
                            });
                        self.apply_rendered_markdown_interaction(
                            &cell.id,
                            rendered_response.clicked(),
                            rendered_response.double_clicked(),
                        );
                    } else if ui.button("Render Markdown").clicked() {
                        self.rendered_markdown.insert(cell.id.clone());
                        self.edit_mode = false;
                    }
                }
                if self.read_only && cell.cell_type != CellType::Markdown {
                    ui.add(
                        egui::Label::new(RichText::new(&cell.source).monospace())
                            .wrap()
                            .selectable(true),
                    );
                }
                let show_editor = !self.read_only
                    && (cell.cell_type != CellType::Markdown
                        || !self.rendered_markdown.contains(&cell.id));
                let edited = show_editor
                    .then(|| {
                        let editor = self
                            .editors
                            .iter_mut()
                            .find(|(id, _)| id == &cell.id)
                            .map(|(_, source)| source)
                            .unwrap();
                        let response = if cell.cell_type == CellType::Code {
                            let mut output = CodeEditor::default()
                                .id_source(format!("code-editor-{}", cell.id))
                                .with_rows(editor.lines().count().clamp(2, 18))
                                .with_fontsize(14.0)
                                .with_theme(ColorTheme {
                                    selection: "#d2e8f6",
                                    ..ColorTheme::GITHUB_LIGHT
                                })
                                .with_syntax(kernel_syntax(&self.state.snapshot.kernel.name))
                                .with_numlines(!self.hidden_line_numbers.contains(&cell.id))
                                .vscroll(false)
                                .show(ui, editor);
                            if let Some(range) = output.state.cursor.char_range() {
                                let char_index = range.primary.index;
                                let byte_index = editor
                                    .char_indices()
                                    .nth(char_index)
                                    .map_or(editor.len(), |(index, _)| index);
                                self.caret_byte_positions
                                    .insert(cell.id.clone(), byte_index);
                            }
                            if let Some(caret) = self.pending_caret_positions.remove(&cell.id) {
                                output
                                    .state
                                    .cursor
                                    .set_char_range(Some(CCursorRange::one(CCursor::new(caret))));
                                output.state.store(ui.ctx(), output.response.id);
                                output.response.request_focus();
                            }
                            output.response
                        } else {
                            ui.add_sized(
                                [
                                    ui.available_width(),
                                    editor.lines().count().clamp(2, 18) as f32 * 20.0 + 12.0,
                                ],
                                TextEdit::multiline(editor)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY)
                                    .code_editor(),
                            )
                        };
                        if response.changed() {
                            self.dirty_editors.insert(cell.id.clone());
                            self.autosave_due = Some(ui.input(|input| input.time) + 1.2);
                            ui.ctx().request_repaint_after(Duration::from_millis(1_200));
                            if cell.cell_type == CellType::Code {
                                self.completion_due
                                    .insert(cell.id.clone(), ui.input(|input| input.time) + 0.3);
                                ui.ctx().request_repaint_after(Duration::from_millis(300));
                            }
                        }
                        if self.pending_editor_focus.as_deref() == Some(cell.id.as_str()) {
                            response.request_focus();
                            self.pending_editor_focus = None;
                        }
                        if response.gained_focus() || response.clicked() {
                            self.state.snapshot.selected_cell_id = Some(cell.id.clone());
                            self.selected_cells.clear();
                            self.selected_cells.insert(cell.id.clone());
                            self.edit_mode = true;
                        } else if response.has_focus()
                            && self.state.snapshot.selected_cell_id.as_deref()
                                != Some(cell.id.as_str())
                        {
                            response.surrender_focus();
                        }
                        if response.has_focus() && ui.input(|input| input.key_pressed(Key::Escape))
                        {
                            response.surrender_focus();
                            self.edit_mode = false;
                        }
                        (response.lost_focus() && self.dirty_editors.remove(&cell.id))
                            .then(|| editor.clone())
                    })
                    .flatten();
                if let Some(source) = edited {
                    self.emit(NotebookCommandKind::ModifyCells {
                        changes: vec![CellMutation::Update {
                            cell_id: cell.id.clone(),
                            source: Some(source),
                            metadata: None,
                            cell_type: Some(cell.cell_type.clone()),
                        }],
                    });
                }
                if let Some(completion) = self.completion_suggestions.get(&cell.id).cloned() {
                    let selected = self
                        .completion_selection
                        .get(&cell.id)
                        .copied()
                        .unwrap_or_default();
                    let mut accepted = None;
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(ui.available_width().min(420.0));
                        ui.label(
                            RichText::new("Code completions")
                                .small()
                                .color(Color32::from_rgb(83, 99, 107)),
                        );
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .show(ui, |ui| {
                                for (index, value) in completion.matches.iter().take(12).enumerate()
                                {
                                    if completion_row(ui, value, index == selected).clicked() {
                                        accepted = Some(index);
                                    }
                                }
                            });
                        ui.label(COMPLETION_KEY_HELP);
                    });
                    if let Some(index) = accepted {
                        self.completion_selection.insert(cell.id.clone(), index);
                        self.accept_completion(&cell.id);
                    }
                }
                if let Some(inspection) = self.inspections.get(&cell.id).cloned() {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Object help").strong());
                            if ui.small_button("Close").clicked() {
                                self.inspections.remove(&cell.id);
                            }
                        });
                        egui::ScrollArea::vertical()
                            .max_height(240.0)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(RichText::new(inspection).monospace()).wrap(),
                                );
                            });
                    });
                }
                let output_start = ui.cursor().min;
                match output_view {
                    OutputViewMode::Collapsed if !cell.outputs.is_empty() => {
                        let summary = output_collapse_summary(cell.outputs.len());
                        let response = egui::Frame::new()
                            .fill(Color32::from_rgb(248, 250, 251))
                            .inner_margin(Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(summary)
                                            .monospace()
                                            .color(Color32::from_rgb(83, 99, 107)),
                                    )
                                    .sense(egui::Sense::click()),
                                )
                            })
                            .inner;
                        if response
                            .on_hover_text("Click to show this output in a scroll window")
                            .clicked()
                        {
                            self.set_output_view(&cell.id, OutputViewMode::Windowed);
                        }
                    }
                    OutputViewMode::Windowed => {
                        egui::ScrollArea::vertical()
                            .id_salt(("output-window", &cell.id))
                            .max_height(220.0)
                            .auto_shrink([false, false])
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                for output in &cell.outputs {
                                    render_output(ui, output);
                                }
                            });
                    }
                    OutputViewMode::Expanded | OutputViewMode::Collapsed => {
                        for output in &cell.outputs {
                            render_output(ui, output);
                        }
                    }
                }
                let output_rect = egui::Rect::from_min_max(
                    output_start,
                    egui::pos2(ui.max_rect().right(), ui.min_rect().bottom()),
                );
                if self.scroll_to_output.as_deref() == Some(cell.id.as_str())
                    && !cell.outputs.is_empty()
                {
                    ui.scroll_to_rect(output_rect, Some(egui::Align::Min));
                    self.scroll_to_output = None;
                }
                // Observe rather than capture the click: output links, text selection,
                // and scrollbars keep their own interaction behavior.
                if !cell.outputs.is_empty()
                    && ui.rect_contains_pointer(output_rect)
                    && ui.input(|input| input.pointer.primary_clicked())
                {
                    self.select_cell(index, false);
                    self.edit_mode = false;
                    self.pending_editor_focus = None;
                }
            }
        });
        let rect = frame_response.response.rect;
        if ui.rect_contains_pointer(rect) && ui.input(|input| input.pointer.primary_clicked()) {
            self.agent_highlights.remove(&cell.id);
        }
        if let Some(color) = self.agent_highlights.get(&cell.id) {
            let width = if self.reduced_motion {
                2.5
            } else {
                ui.ctx().request_repaint_after(Duration::from_millis(33));
                2.5 + ui.input(|input| (input.time * std::f64::consts::PI).sin()) as f32 * 0.75
            };
            ui.painter().rect_stroke(
                rect.shrink(4.0),
                2.0,
                Stroke::new(width, *color),
                egui::StrokeKind::Inside,
            );
        }
        if let Some((target, remaining)) = &mut self.capture_target
            && *target == cell.id
        {
            let rect = frame_response.response.rect;
            if *remaining == 30 {
                ui.scroll_to_rect_animation(
                    rect,
                    Some(egui::Align::Min),
                    egui::style::ScrollAnimation::none(),
                );
            }
            if *remaining > 0 {
                *remaining -= 1;
                ui.ctx().request_repaint();
            } else {
                let visible = rect.intersect(ui.clip_rect());
                self.capture_region = Some((visible, ui.ctx().pixels_per_point(), visible != rect));
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                        "notebook-cell",
                    )));
                self.capture_target = None;
            }
        }
        if let Some(dragged_id) = self.dragging_cell.clone()
            && dragged_id != cell.id
            && ui.rect_contains_pointer(frame_response.response.rect)
            && let Some(pointer) = ui.ctx().pointer_latest_pos()
        {
            let before = pointer.y < frame_response.response.rect.center().y;
            let line_y = if before {
                frame_response.response.rect.top()
            } else {
                frame_response.response.rect.bottom()
            };
            ui.painter().line_segment(
                [
                    egui::pos2(frame_response.response.rect.left(), line_y),
                    egui::pos2(frame_response.response.rect.right(), line_y),
                ],
                Stroke::new(3.0, Color32::from_rgb(45, 105, 145)),
            );
            if ui.input(|input| input.pointer.any_released()) {
                let source_index = self
                    .state
                    .snapshot
                    .cells
                    .iter()
                    .position(|candidate| candidate.id == dragged_id);
                if let Some(source_index) = source_index {
                    let target_index = move_index_for_drop(source_index, index, before);
                    if target_index != source_index {
                        if let Some(source) = self.state.snapshot.cells.get(source_index).cloned() {
                            self.flush_editor(&source);
                        }
                        self.emit(NotebookCommandKind::ModifyCells {
                            changes: vec![CellMutation::Move {
                                cell_id: dragged_id,
                                index: target_index,
                            }],
                        });
                    }
                }
                self.dragging_cell = None;
            }
        }
    }

    /// Follow presentation only: never focus an editor or emit a notebook command.
    pub fn follow_selection(&mut self, cell_id: Option<&str>) {
        if !self.read_only {
            return;
        }
        if let Some(id) = cell_id
            && !self.state.snapshot.cells.iter().any(|cell| cell.id == id)
        {
            return;
        }
        self.selected_cells.clear();
        self.state.snapshot.selected_cell_id = cell_id.map(str::to_owned);
    }

    fn apply_rendered_markdown_interaction(
        &mut self,
        cell_id: &str,
        clicked: bool,
        double_clicked: bool,
    ) {
        if clicked || double_clicked {
            self.selected_cells.clear();
            self.state.snapshot.selected_cell_id = Some(cell_id.to_owned());
            self.edit_mode = false;
            self.pending_editor_focus = None;
        }
        if double_clicked {
            self.begin_editing_cell(cell_id);
        }
    }

    fn begin_editing_cell(&mut self, cell_id: &str) {
        if self.read_only {
            return;
        }
        self.rendered_markdown.remove(cell_id);
        self.pending_editor_focus = Some(cell_id.to_owned());
        self.edit_mode = true;
    }
}

impl eframe::App for NotebookEguiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        for event in ctx.input(|input| input.events.clone()) {
            if let egui::Event::Screenshot {
                image, user_data, ..
            } = event
                && user_data
                    .data
                    .as_ref()
                    .and_then(|data| data.downcast_ref::<&str>())
                    == Some(&"notebook-cell")
                && let Some((rect, scale, clipped)) = self.capture_region.take()
            {
                let x = ((rect.left().max(0.0) * scale) as usize).min(image.size[0]);
                let y = ((rect.top().max(0.0) * scale) as usize).min(image.size[1]);
                let right = ((rect.right().max(0.0) * scale) as usize).min(image.size[0]);
                let bottom = ((rect.bottom().max(0.0) * scale) as usize).min(image.size[1]);
                let width = right.saturating_sub(x);
                let height = bottom.saturating_sub(y);
                if width > 0 && height > 0 && width.saturating_mul(height) <= 4_000_000 {
                    let mut rgba = Vec::with_capacity(width * height * 4);
                    for row in y..bottom {
                        for pixel in
                            &image.pixels[row * image.size[0] + x..row * image.size[0] + right]
                        {
                            rgba.extend_from_slice(&pixel.to_array());
                        }
                    }
                    self.captured_cell = Some(serde_json::json!({"width":width,"height":height,"clipped":clipped,"rgba":base64::engine::general_purpose::STANDARD.encode(rgba)}).to_string());
                }
            }
        }
        let compact_controls = ctx.screen_rect().width() < 600.0;
        let scale = desktop_control_scale(ctx.screen_rect().width());
        ctx.style_mut(|style| {
            style.visuals = egui::Visuals::light();
            style.spacing.item_spacing = egui::vec2(8.0, 8.0);
            style.spacing.interact_size.y = 30.0 * scale;
            style.visuals.selection.bg_fill = Color32::from_rgb(210, 232, 246);
        });
        install_data_image_loader(ctx);
        egui_extras::install_image_loaders(ctx);
        if let Some((url, position)) = self.markdown_link.clone() {
            egui::Area::new(egui::Id::new("markdown-link-menu"))
                .order(egui::Order::Foreground)
                .fixed_pos(position)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(360.0);
                        ui.label(RichText::new(&url).small()).on_hover_text(&url);
                        if ui.button("Open link in new tab").clicked() {
                            ctx.open_url(egui::OpenUrl::new_tab(url.clone()));
                            self.markdown_link = None;
                        }
                        if ui.small_button("Cancel").clicked() {
                            self.markdown_link = None;
                        }
                    });
                });
        }
        if let Some((cell_id, item)) = self.microscope_delete.clone() {
            egui::Window::new("Delete microscope?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .default_width(400.0)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Delete “{}” and its content file? This cannot be undone.",
                        item.title
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.microscope_delete = None;
                        }
                        if ui
                            .add_enabled(!self.read_only, egui::Button::new("Delete microscope"))
                            .clicked()
                        {
                            self.emit(NotebookCommandKind::DeleteMicroscope {
                                cell_id,
                                microscope_id: item.id,
                            });
                            self.microscope_delete = None;
                        }
                    });
                });
        }
        self.microscope_shortcuts(ctx);
        if let Some(target) = self.microscope_target.clone() {
            egui::TopBottomPanel::top("microscope-toolbar").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if self.read_only
                        && toolbar_icon_button(
                            ui,
                            true,
                            ToolbarIcon::Follow(self.following_driver),
                            if self.following_driver {
                                "Stop following driver"
                            } else {
                                "Follow driver"
                            },
                        )
                    {
                        self.follow_toggle_requested = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let back = egui::Button::new(
                            RichText::new("Back to notebook").color(Color32::from_rgb(145, 40, 40)),
                        )
                        .fill(Color32::from_rgb(255, 245, 245))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(190, 75, 75)));
                        if ui.add(back).on_hover_text("Backspace").clicked() {
                            let _ = self.open_microscope(None);
                        }
                    });
                    if let Ok(doc) = notebook_protocol::microscope::document(
                        &self.state.snapshot,
                        &target.cell_id,
                        &target.microscope_id,
                    ) {
                        ui.heading(doc.microscope.title);
                    }
                });
            });
            egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status(ui));
            egui::CentralPanel::default().show(ctx,|ui| {
                if let Some(doc) = self.microscope_document.clone() {
                    if let Some(w) = doc.walkthrough { self.walkthrough_ui(ui, &w); }
                    else {
                        ui.heading("This microscope has no walkthrough yet");
                        ui.label("Ask an agent to set up a walkthrough with code, annotations and an explanation for each step.");
                    }
                } else if let Some(error) = &self.state.last_error {
                    ui.colored_label(Color32::from_rgb(198,40,40),&error.message);
                    if ui.button("Retry loading microscope").clicked() {
                        self.emit(NotebookCommandKind::ReadMicroscope {cell_id:target.cell_id,microscope_id:target.microscope_id});
                    }
                } else {
                    ui.spinner();
                    ui.label("Loading microscope…");
                }
            });
            return;
        }
        if !self.read_only {
            let now = ctx.input(|input| input.time);
            if self.autosave_due.is_some_and(|deadline| deadline <= now) {
                self.autosave_due = None;
                self.save_visible_edits();
            }
            let due = self
                .completion_due
                .iter()
                .filter(|(_, deadline)| **deadline <= now)
                .map(|(cell_id, _)| cell_id.clone())
                .collect::<Vec<_>>();
            for cell_id in due {
                self.completion_due.remove(&cell_id);
                if self.edit_mode
                    && let Some(cell) = self
                        .state
                        .snapshot
                        .cells
                        .iter()
                        .find(|cell| cell.id == cell_id)
                        .cloned()
                {
                    self.request_completion(&cell);
                }
            }
            let selected_cell_id = self.state.snapshot.selected_cell_id.clone();
            let completion_open = selected_cell_id
                .as_ref()
                .is_some_and(|id| self.completion_suggestions.contains_key(id));
            if completion_open && let Some(cell_id) = selected_cell_id.as_ref() {
                let count = self
                    .completion_suggestions
                    .get(cell_id)
                    .map_or(0, |completion| completion.matches.len().min(12));
                if count > 0 && ctx.input(|input| input.key_pressed(Key::ArrowDown)) {
                    ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::ArrowDown));
                    let selected = self
                        .completion_selection
                        .entry(cell_id.clone())
                        .or_default();
                    *selected = (*selected + 1).min(count - 1);
                }
                if count > 0 && ctx.input(|input| input.key_pressed(Key::ArrowUp)) {
                    ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::ArrowUp));
                    let selected = self
                        .completion_selection
                        .entry(cell_id.clone())
                        .or_default();
                    *selected = selected.saturating_sub(1);
                }
                if ctx.input(|input| input.key_pressed(Key::Enter) || input.key_pressed(Key::Tab)) {
                    ctx.input_mut(|input| {
                        input.consume_key(egui::Modifiers::NONE, Key::Enter);
                        input.consume_key(egui::Modifiers::NONE, Key::Tab);
                    });
                    self.accept_completion(cell_id);
                } else if ctx.input(|input| input.key_pressed(Key::Escape)) {
                    ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape));
                    self.close_completion(cell_id);
                }
            } else if self.edit_mode
                && ctx.input(|input| input.modifiers.shift && input.key_pressed(Key::Tab))
                && let Some(cell) = selected_cell_id
                    .as_ref()
                    .and_then(|id| self.state.snapshot.cells.iter().find(|cell| &cell.id == id))
                    .filter(|cell| cell.cell_type == CellType::Code)
                    .cloned()
            {
                ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, Key::Tab));
                self.request_inspection(&cell);
            } else if self.edit_mode
                && ctx.input(|input| input.key_pressed(Key::Tab))
                && ctx.input(|input| !input.modifiers.shift)
                && let Some(cell) = selected_cell_id
                    .as_ref()
                    .and_then(|id| self.state.snapshot.cells.iter().find(|cell| &cell.id == id))
                    .filter(|cell| cell.cell_type == CellType::Code)
                    .cloned()
            {
                ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Tab));
                self.request_completion(&cell);
            }
            if !completion_open && ctx.input(|input| input.key_pressed(Key::Escape)) {
                self.edit_mode = false;
            }
            if !completion_open
                && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, Key::Enter))
            {
                self.execute_selected();
            } else if !completion_open
                && ctx.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, Key::Enter))
            {
                ctx.memory_mut(|memory| {
                    if let Some(id) = memory.focused() {
                        memory.surrender_focus(id);
                    }
                });
                self.execute_selected_and_advance(false);
            } else if !completion_open
                && ctx.input_mut(|input| input.consume_key(egui::Modifiers::ALT, Key::Enter))
            {
                self.execute_selected_and_advance(true);
            }
            let command_mode = !self.edit_mode;
            if command_mode
                && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Enter))
                && let Some((_, cell)) = self.selected_cell()
            {
                self.begin_editing_cell(&cell.id);
            }
            if command_mode
                && ctx.input(|input| input.key_pressed(Key::ArrowDown) || input.key_pressed(Key::J))
                && let Some((index, _)) = self.selected_cell()
                && let Some(next) = self.state.snapshot.cells.get(index + 1)
            {
                self.state.snapshot.selected_cell_id = Some(next.id.clone());
                self.selected_cells.clear();
            }
            if command_mode
                && ctx.input(|input| input.key_pressed(Key::ArrowUp) || input.key_pressed(Key::K))
                && let Some((index, _)) = self.selected_cell()
                && index > 0
            {
                self.state.snapshot.selected_cell_id =
                    Some(self.state.snapshot.cells[index - 1].id.clone());
                self.selected_cells.clear();
            }
            if command_mode && ctx.input(|input| input.key_pressed(Key::A)) {
                let index = self
                    .state
                    .snapshot
                    .selected_cell_id
                    .as_ref()
                    .and_then(|id| {
                        self.state
                            .snapshot
                            .cells
                            .iter()
                            .position(|cell| &cell.id == id)
                    })
                    .unwrap_or(0);
                self.insert_cell(index, CellType::Code, String::new());
            }
            if command_mode && ctx.input(|input| input.key_pressed(Key::B)) {
                self.insert_cell_after_selection(CellType::Code);
            }
            if command_mode && ctx.input(|input| input.key_pressed(Key::X)) {
                self.cut_selected();
            }
            if command_mode && ctx.input(|input| input.key_pressed(Key::C)) {
                self.copy_selected();
            }
            if command_mode && ctx.input(|input| input.key_pressed(Key::V)) {
                self.paste_below_selected();
            }
            if command_mode && ctx.input(|input| input.key_pressed(Key::Z)) {
                if ctx.input(|input| input.modifiers.shift) {
                    self.replay_history(false);
                } else {
                    self.replay_history(true);
                }
            }
            if command_mode && ctx.input(|input| input.key_pressed(Key::S)) {
                self.save_visible_edits();
            }
            if command_mode && ctx.input(|input| input.key_pressed(Key::M)) {
                self.set_selected_cell_type(CellType::Markdown);
            }
            if command_mode && ctx.input(|input| input.key_pressed(Key::Y)) {
                self.set_selected_cell_type(CellType::Code);
            }
            if command_mode && ctx.input(|input| input.key_pressed(Key::R)) {
                self.set_selected_cell_type(CellType::Raw);
            }
            if command_mode && ctx.input(|input| input.key_pressed(Key::O)) {
                self.cycle_selected_output_view();
            }
        }
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            self.toolbar(ui);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status(ui));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(247, 247, 247))
                    .inner_margin(if compact_controls {
                        Margin::symmetric(8, 8)
                    } else {
                        Margin::symmetric(16, 12)
                    }),
            )
            .show(ctx, |ui| {
                let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if let Some(fraction) = self.follow_scroll {
                    scroll = scroll.vertical_scroll_offset(fraction * self.scroll_extent);
                }
                let output = scroll.show(ui, |ui| {
                    ui.set_width(notebook_document_width(ui.available_width()));
                    if self.state.snapshot.cells.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(80.0);
                            ui.heading("A quiet page, ready for work");
                            ui.label("Add a code or Markdown cell to begin.");
                        });
                    }
                    let cells = self.state.snapshot.cells.clone();
                    let mut index = 0;
                    while index < cells.len() {
                        let grouped = index + 1 < cells.len()
                            && cells[index].cell_type == CellType::Markdown
                            && combines_with_markdown(&cells[index + 1], &cells[index].id);
                        if grouped {
                            let selected = [&cells[index], &cells[index + 1]].iter().any(|cell| {
                                self.selected_cells.contains(&cell.id)
                                    || self.state.snapshot.selected_cell_id.as_deref()
                                        == Some(cell.id.as_str())
                            });
                            egui::Frame::new()
                                .fill(if selected {
                                    Color32::from_rgb(250, 253, 255)
                                } else {
                                    Color32::WHITE
                                })
                                .stroke(Stroke::new(
                                    if selected { 2.0 } else { 1.0 },
                                    if selected {
                                        Color32::from_rgb(45, 105, 145)
                                    } else {
                                        Color32::from_gray(210)
                                    },
                                ))
                                .corner_radius(CornerRadius::same(3))
                                .show(ui, |ui| {
                                    self.cell(ui, index, cells[index].clone(), true);
                                    ui.separator();
                                    self.cell(ui, index + 1, cells[index + 1].clone(), true);
                                });
                            index += 2;
                        } else {
                            self.cell(ui, index, cells[index].clone(), false);
                            index += 1;
                        }
                        ui.add_space(10.0);
                    }
                    if ui.input(|input| input.pointer.any_released()) {
                        self.dragging_cell = None;
                    }
                });
                self.scroll_extent = (output.content_size.y - output.inner_rect.height()).max(0.0);
                self.scroll_fraction = if self.scroll_extent > 0.0 {
                    (output.state.offset.y / self.scroll_extent).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if self.scroll_extent > 0.0
                    && self
                        .follow_scroll
                        .is_some_and(|target| (target - self.scroll_fraction).abs() > 0.001)
                {
                    ui.ctx().request_repaint();
                }
            });
    }
}

fn render_output(ui: &mut egui::Ui, output: &CellOutput) {
    if let CellOutput::Rich { mime, data } = output
        && matches!(mime.as_str(), "image/png" | "image/svg+xml")
    {
        egui::Frame::new()
            .fill(Color32::from_rgb(248, 250, 251))
            .inner_margin(Margin::same(8))
            .show(ui, |ui| match decode_rich_image(mime, data) {
                Ok(bytes) => {
                    let dimensions = match mime.as_str() {
                        "image/png" => png_dimensions(&bytes)
                            .map(|(width, height)| (width as f32, height as f32)),
                        "image/svg+xml" => svg_dimensions(&bytes),
                        _ => None,
                    };
                    let mut image = egui::Image::from_bytes(rich_image_uri(mime, data), bytes)
                        .alt_text("Notebook graph output");
                    if let Some((width, height)) = dimensions {
                        let scale = (ui.available_width() / width).min(1.0);
                        image = image.fit_to_exact_size(egui::vec2(width * scale, height * scale));
                    }
                    ui.add(image.max_width(ui.available_width()));
                }
                Err(_) => {
                    ui.colored_label(
                        Color32::from_rgb(198, 40, 40),
                        "Image output could not be decoded. Re-run the cell to regenerate it.",
                    );
                }
            });
        return;
    }
    if let CellOutput::Rich { mime, data } = output
        && mime == "text/html"
    {
        let text = sanitized_html_text(data);
        egui::Frame::new()
            .fill(Color32::from_rgb(248, 250, 251))
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(text).monospace()).wrap());
            });
        return;
    }
    let text = match output {
        CellOutput::Text { text } | CellOutput::Stream { text, .. } => text.clone(),
        CellOutput::Error {
            name,
            message,
            traceback,
        } => format!("{name}: {message}\n{}", traceback.join("\n")),
        CellOutput::Rich { mime, data } => format!("[{mime}]\n{data}"),
    };
    egui::Frame::new()
        .fill(Color32::from_rgb(248, 250, 251))
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.add(egui::Label::new(ansi_layout_job(&text, Color32::from_rgb(38, 50, 56))).wrap());
        });
}

fn sanitized_html_text(html: &str) -> String {
    let normalized = html
        .replace("</td>", "\t")
        .replace("</th>", "\t")
        .replace("</tr>", "\n")
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n");
    let mut text = String::with_capacity(normalized.len().min(262_144));
    let mut inside_tag = false;
    for character in normalized.chars() {
        match character {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag
                && (character == '\n' || character == '\t' || !character.is_control()) =>
            {
                text.push(character);
            }
            _ => {}
        }
        if text.len() >= 262_144 {
            break;
        }
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn ansi_layout_job(text: &str, base_color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let mut color = base_color;
    let mut buffer = String::new();
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            append_output_run(&mut job, &mut buffer, color);
            match characters.next() {
                Some('[') => {
                    let mut parameters = String::new();
                    for sequence_character in characters.by_ref() {
                        if ('@'..='~').contains(&sequence_character) {
                            if sequence_character == 'm' {
                                color = ansi_foreground(&parameters, color, base_color);
                            }
                            break;
                        }
                        if parameters.len() < 64 {
                            parameters.push(sequence_character);
                        }
                    }
                }
                Some(']') => {
                    while let Some(sequence_character) = characters.next() {
                        if sequence_character == '\u{7}' {
                            break;
                        }
                        if sequence_character == '\u{1b}' && characters.next_if_eq(&'\\').is_some()
                        {
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
        } else if character == '\n' || character == '\t' || !character.is_control() {
            buffer.push(character);
        }
    }
    append_output_run(&mut job, &mut buffer, color);
    job
}

fn append_output_run(job: &mut LayoutJob, buffer: &mut String, color: Color32) {
    if buffer.is_empty() {
        return;
    }
    job.append(
        buffer,
        0.0,
        TextFormat {
            font_id: FontId::monospace(14.0),
            color,
            ..Default::default()
        },
    );
    buffer.clear();
}

fn ansi_foreground(parameters: &str, current: Color32, base: Color32) -> Color32 {
    let mut color = current;
    let parameters = if parameters.is_empty() {
        vec![0]
    } else {
        parameters
            .split(';')
            .filter_map(|parameter| parameter.parse::<u8>().ok())
            .collect()
    };
    for parameter in parameters {
        color = match parameter {
            0 | 39 => base,
            30 => Color32::from_rgb(38, 50, 56),
            31 => Color32::from_rgb(198, 40, 40),
            32 => Color32::from_rgb(46, 125, 50),
            33 => Color32::from_rgb(154, 103, 0),
            34 => Color32::from_rgb(21, 101, 192),
            35 => Color32::from_rgb(123, 31, 162),
            36 => Color32::from_rgb(0, 121, 107),
            37 => Color32::from_rgb(83, 99, 107),
            90 => Color32::from_rgb(97, 112, 119),
            91 => Color32::from_rgb(211, 47, 47),
            92 => Color32::from_rgb(56, 142, 60),
            93 => Color32::from_rgb(175, 112, 0),
            94 => Color32::from_rgb(25, 118, 210),
            95 => Color32::from_rgb(142, 36, 170),
            96 => Color32::from_rgb(0, 137, 123),
            97 => Color32::from_rgb(38, 50, 56),
            _ => color,
        };
    }
    color
}

struct RenderedMarkdown {
    response: egui::Response,
    clicked_link: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum MarkdownBlock<'a> {
    Text(&'a str),
    Table(Vec<Vec<String>>),
}

fn table_cells(line: &str) -> Vec<String> {
    let line = line.trim().trim_matches('|');
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for character in line.chars() {
        if character == '|' && !escaped {
            cells.push(cell.trim().to_owned());
            cell.clear();
        } else {
            cell.push(character);
        }
        escaped = character == '\\' && !escaped;
        if character != '\\' {
            escaped = false;
        }
    }
    cells.push(cell.trim().to_owned());
    cells
}

fn table_separator(line: &str) -> bool {
    let cells = table_cells(line);
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let trimmed = cell.trim().trim_matches(':');
            trimmed.len() >= 3 && trimmed.chars().all(|character| character == '-')
        })
}

fn markdown_blocks(source: &str) -> Vec<MarkdownBlock<'_>> {
    let lines = source.split_inclusive('\n').collect::<Vec<_>>();
    let mut blocks = Vec::new();
    let mut line = 0;
    let mut text_start = 0;
    let mut byte_offset = 0;
    let offsets = lines
        .iter()
        .map(|item| {
            let offset = byte_offset;
            byte_offset += item.len();
            offset
        })
        .collect::<Vec<_>>();
    while line + 1 < lines.len() {
        if lines[line].contains('|') && table_separator(lines[line + 1]) {
            if text_start < offsets[line] {
                blocks.push(MarkdownBlock::Text(&source[text_start..offsets[line]]));
            }
            let mut rows = vec![table_cells(lines[line])];
            line += 2;
            while line < lines.len() && !lines[line].trim().is_empty() && lines[line].contains('|')
            {
                rows.push(table_cells(lines[line]));
                line += 1;
            }
            blocks.push(MarkdownBlock::Table(rows));
            text_start = offsets.get(line).copied().unwrap_or(source.len());
            continue;
        }
        line += 1;
    }
    if text_start < source.len() {
        blocks.push(MarkdownBlock::Text(&source[text_start..]));
    }
    if blocks.is_empty() {
        blocks.push(MarkdownBlock::Text(source));
    }
    blocks
}

fn show_markdown_fragment(
    ui: &mut egui::Ui,
    source: &str,
    cache: &mut CommonMarkCache,
    render_math: &egui_commonmark::RenderMathFn,
) -> Option<String> {
    let links = pulldown_cmark::Parser::new_ext(source, pulldown_cmark::Options::all())
        .filter_map(|event| match event {
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link { dest_url, .. }) => {
                Some(dest_url.to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    for link in &links {
        cache.add_link_hook(link);
    }
    CommonMarkViewer::new()
        .explicit_image_uri_scheme(true)
        .max_image_width(Some(ui.available_width().max(1.0) as usize))
        .render_math_fn(Some(render_math))
        .show(ui, cache, source);
    links
        .into_iter()
        .find(|link| cache.get_link_hook(link) == Some(true))
}

fn rendered_markdown_response(
    ui: &mut egui::Ui,
    cell_id: &str,
    source: &str,
    cache: &mut CommonMarkCache,
    math_cache: &Arc<Mutex<MathRenderCache>>,
) -> RenderedMarkdown {
    let math_cache = Arc::clone(math_cache);
    let render_math = move |ui: &mut egui::Ui, math: &str, inline: bool| {
        math_cache
            .lock()
            .expect("markdown math cache mutex poisoned")
            .show(ui, math, inline);
    };
    let mut clicked_link = None;
    let rendered = ui.scope(|ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
        for block in markdown_blocks(source) {
            match block {
                MarkdownBlock::Text(text) => {
                    clicked_link = clicked_link
                        .or_else(|| show_markdown_fragment(ui, text, cache, &render_math));
                }
                MarkdownBlock::Table(rows) => {
                    let columns = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
                    let spacing = ui.spacing().item_spacing.x * (columns.saturating_sub(1) as f32);
                    let column_width =
                        ((ui.available_width() - spacing) / columns as f32).max(80.0);
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        for (row_index, row) in rows.into_iter().enumerate() {
                            egui::Frame::new()
                                .fill(if row_index % 2 == 0 {
                                    Color32::from_rgb(248, 249, 250)
                                } else {
                                    Color32::WHITE
                                })
                                .inner_margin(Margin::symmetric(6, 4))
                                .show(ui, |ui| {
                                    ui.horizontal_top(|ui| {
                                        for column in 0..columns {
                                            let mut value =
                                                row.get(column).cloned().unwrap_or_default();
                                            if row_index == 0 {
                                                value = format!("**{value}**");
                                            }
                                            ui.allocate_ui_with_layout(
                                                egui::vec2(column_width, 0.0),
                                                egui::Layout::top_down(egui::Align::Min),
                                                |ui| {
                                                    ui.set_min_width(column_width);
                                                    ui.set_max_width(column_width);
                                                    clicked_link = clicked_link.or_else(|| {
                                                        show_markdown_fragment(
                                                            ui,
                                                            &value,
                                                            cache,
                                                            &render_math,
                                                        )
                                                    });
                                                },
                                            );
                                        }
                                    });
                                });
                        }
                    });
                }
            }
        }
    });
    let response = ui.interact(
        rendered.response.rect,
        ui.make_persistent_id(("rendered-markdown", cell_id)),
        egui::Sense::click(),
    );
    RenderedMarkdown {
        response,
        clicked_link,
    }
}

#[derive(Default)]
struct MathRenderCache {
    textures: HashMap<u64, Result<(egui::TextureHandle, egui::Vec2), String>>,
}

impl MathRenderCache {
    fn show(&mut self, ui: &mut egui::Ui, latex: &str, inline: bool) {
        let mut hasher = DefaultHasher::new();
        latex.hash(&mut hasher);
        inline.hash(&mut hasher);
        let key = hasher.finish();
        self.textures.entry(key).or_insert_with(|| {
            render_math_formula(latex, inline).map(|image| {
                let size = egui::vec2(
                    image.width() as f32 / MATH_PIXELS_PER_POINT,
                    image.height() as f32 / MATH_PIXELS_PER_POINT,
                );
                let texture = ui.ctx().load_texture(
                    format!("markdown-math-{key:016x}"),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                (texture, size)
            })
        });
        match self.textures.get(&key) {
            Some(Ok((texture, size))) if inline => {
                let (allocation_size, paint_offset) = inline_math_layout(*size);
                let (rect, _) = ui.allocate_exact_size(allocation_size, egui::Sense::hover());
                ui.painter().image(
                    texture.id(),
                    egui::Rect::from_min_size(rect.min + paint_offset, *size),
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            Some(Ok((texture, size))) => {
                ui.vertical_centered(|ui| {
                    ui.add(egui::Image::new((texture.id(), *size)));
                });
            }
            Some(Err(message)) => {
                ui.colored_label(
                    Color32::from_rgb(198, 40, 40),
                    format!("Math could not be rendered: {message}"),
                );
            }
            None => {}
        }
    }
}

const MATH_PIXELS_PER_POINT: f32 = 2.0;
const MATH_RASTER_PADDING_POINTS: f32 = 4.0;
const INLINE_MATH_RASTER_PADDING_POINTS: f32 = 1.0;
const INLINE_MATH_DESCENDER_RESERVE_POINTS: f32 = 4.0;

fn inline_math_layout(texture_size: egui::Vec2) -> (egui::Vec2, egui::Vec2) {
    (
        texture_size + egui::vec2(0.0, INLINE_MATH_DESCENDER_RESERVE_POINTS),
        egui::Vec2::ZERO,
    )
}

const MATH_PREAMBLE: &str = r#"
#let mitexmathbf(it) = math.bold(math.upright(it))
#let mitexsqrt(..args) = {
  if args.pos().len() == 1 { $sqrt(#args.pos().at(0))$ }
  else if args.pos().len() == 2 { $root(#args.pos().at(0), #args.pos().at(1))$ }
}
#let mitexdisplay(it) = math.display(it)
#let mitexinline(it) = math.inline(it)
#let mitexscript(it) = math.script(it)
#let mitexsscript(it) = math.sscript(it)
#let mitexbold(it) = math.bold(math.upright(it))
#let mitexupright(it) = math.upright(it)
#let mitexitalic(it) = math.italic(it)
#let mitexsans(it) = math.sans(it)
#let mitexnot(it) = math.cancel(angle: 20deg, it)
#let mitexlabel(it) = none
#let mitexcaption(it) = none
#let pmatrix = math.mat.with(delim: "(")
#let bmatrix = math.mat.with(delim: "[")
#let Bmatrix = math.mat.with(delim: "{")
#let vmatrix = math.mat.with(delim: "|")
#let Vmatrix = math.mat.with(delim: "||")
#let aligned(..args) = args.pos().first()
#let gathered(..args) = args.pos().first()
#let mitexunderbrace(it) = math.underbrace(it)
#let mitexoverbrace(it) = math.overbrace(it)
#let stackrel(top, base) = math.attach(base, t: top)
#let overset(top, base) = math.attach(base, t: top)
#let textmath(it) = text(it)
#let textbf(it) = text(weight: "bold", it)
#let textit(it) = text(style: "italic", it)
#let textrm(it) = text(it)
#let tfrac(num, denom) = math.inline(math.frac(num, denom))
#let dfrac(num, denom) = math.display(math.frac(num, denom))
#let boxed(it) = box(stroke: 0.6pt, inset: (x: 4pt, y: 3pt), $it$)
#let negthinspace = h(-0.16667em)
#let xrightarrow(label) = $attach(arrow.r.long, t: #label)$
#let xleftarrow(label) = $attach(arrow.l.long, t: #label)$
#let ket(it) = $bar.v #it angle.r$
#let bra(it) = $angle.l #it bar.v$
#let braket(left, right) = $angle.l #left bar.v #right angle.r$
"#;

static MATH_FONTS: LazyLock<Vec<typst::text::Font>> = LazyLock::new(|| {
    let mut searcher = typst_kit::fonts::Fonts::searcher();
    searcher
        .include_system_fonts(false)
        .include_embedded_fonts(true);
    searcher
        .search()
        .fonts
        .iter()
        .filter_map(|slot| slot.get())
        .collect()
});

fn render_math_formula(latex: &str, inline: bool) -> Result<egui::ColorImage, String> {
    let typst_math = mitex::convert_math(latex, None).map_err(|error| error.to_string())?;
    let equation = if inline {
        format!("${typst_math}$")
    } else {
        format!("$ {typst_math} $")
    };
    let text_size = 16;
    let source = format!(
        "{MATH_PREAMBLE}\n#set page(width: auto, height: auto, margin: 8pt, fill: none)\n#set text(size: {text_size}pt, fill: black)\n{equation}"
    );
    let engine = typst_as_lib::TypstEngine::builder()
        .main_file(source)
        .fonts(MATH_FONTS.iter().cloned())
        .build();
    let compiled = engine.compile::<typst::layout::PagedDocument>();
    let document = compiled
        .output
        .map_err(|diagnostics| format!("{diagnostics}"))?;
    let page = document
        .pages
        .first()
        .ok_or_else(|| "typesetter returned no page".to_owned())?;
    let pixmap = typst_render::render(page, MATH_PIXELS_PER_POINT);
    let rendered_width = pixmap.width() as usize;
    let rendered_height = pixmap.height() as usize;
    if rendered_width == 0 || rendered_height == 0 {
        return Err("typesetter returned an empty image".into());
    }
    let rendered_pixels: Vec<_> = pixmap
        .data()
        .chunks_exact(4)
        .map(|rgba| Color32::from_rgba_premultiplied(rgba[0], rgba[1], rgba[2], rgba[3]))
        .collect();
    let mut min_x = rendered_width;
    let mut min_y = rendered_height;
    let mut max_x = 0;
    let mut max_y = 0;
    for (index, pixel) in rendered_pixels.iter().enumerate() {
        if pixel.a() == 0 {
            continue;
        }
        let x = index % rendered_width;
        let y = index / rendered_width;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    if min_x > max_x || min_y > max_y {
        return Err("typesetter returned transparent math".into());
    }
    let content_width = max_x - min_x + 1;
    let content_height = max_y - min_y + 1;
    let padding_points = if inline {
        INLINE_MATH_RASTER_PADDING_POINTS
    } else {
        MATH_RASTER_PADDING_POINTS
    };
    let padding = (padding_points * MATH_PIXELS_PER_POINT).ceil() as usize;
    let width = content_width + padding * 2;
    let height = content_height + padding * 2;
    let mut pixels = vec![Color32::TRANSPARENT; width * height];
    for row in 0..content_height {
        let source_start = (min_y + row) * rendered_width + min_x;
        let target_start = (row + padding) * width + padding;
        pixels[target_start..target_start + content_width]
            .copy_from_slice(&rendered_pixels[source_start..source_start + content_width]);
    }
    Ok(egui::ColorImage {
        size: [width, height],
        pixels,
        source_size: egui::vec2(width as f32, height as f32),
    })
}

fn completion_row(ui: &mut egui::Ui, value: &str, selected: bool) -> egui::Response {
    let button = egui::Button::new(RichText::new(value).monospace())
        .selected(selected)
        .min_size(egui::vec2(ui.available_width(), 28.0));
    let response = ui.add(button);
    if selected {
        response.scroll_to_me(None);
    }
    response
}

fn move_index_for_drop(source_index: usize, hovered_index: usize, before: bool) -> usize {
    let boundary = if before {
        hovered_index
    } else {
        hovered_index + 1
    };
    boundary.saturating_sub(usize::from(source_index < boundary))
}

// Capture positional intent when the UI emits it; resolve IDs only at preparation.
fn anchor_cell_changes(cells: &[Cell], changes: &mut [CellMutation]) {
    let mut ids: Vec<String> = cells.iter().map(|cell| cell.id.clone()).collect();
    for change in changes {
        match change.clone() {
            CellMutation::Insert { index, cell } => {
                let index = index.min(ids.len());
                if let Some(anchor) = ids.get(index).or_else(|| ids.last()) {
                    *change = CellMutation::InsertRelative {
                        anchor_cell_id: anchor.clone(),
                        after: index == ids.len(),
                        cell: cell.clone(),
                    };
                }
                ids.insert(index, cell.id);
            }
            CellMutation::Move { cell_id, index } => {
                if let Some(current) = ids.iter().position(|id| id == &cell_id) {
                    ids.remove(current);
                    let index = index.min(ids.len());
                    if let Some(anchor) = ids.get(index).or_else(|| ids.last()) {
                        *change = CellMutation::MoveRelative {
                            cell_id: cell_id.clone(),
                            anchor_cell_id: anchor.clone(),
                            after: index == ids.len(),
                        };
                    }
                    ids.insert(index, cell_id);
                }
            }
            CellMutation::Delete { cell_id } => ids.retain(|id| id != &cell_id),
            _ => {}
        }
    }
}

fn inverse_cell_changes(cells: &[Cell], changes: &[CellMutation]) -> Option<Vec<CellMutation>> {
    let mut working = cells.to_vec();
    let mut inverse = Vec::with_capacity(changes.len());
    for change in changes {
        match change {
            CellMutation::Insert { index, cell } => {
                working.insert((*index).min(working.len()), cell.clone());
                inverse.push(CellMutation::Delete {
                    cell_id: cell.id.clone(),
                });
            }
            CellMutation::Update {
                cell_id,
                source,
                metadata,
                cell_type,
            } => {
                let cell = working.iter_mut().find(|cell| &cell.id == cell_id)?;
                inverse.push(CellMutation::Update {
                    cell_id: cell_id.clone(),
                    source: source.as_ref().map(|_| cell.source.clone()),
                    metadata: metadata.as_ref().map(|_| cell.metadata.clone()),
                    cell_type: cell_type.as_ref().map(|_| cell.cell_type.clone()),
                });
                if let Some(source) = source {
                    cell.source.clone_from(source);
                }
                if let Some(metadata) = metadata {
                    cell.metadata.clone_from(metadata);
                }
                if let Some(cell_type) = cell_type {
                    cell.cell_type.clone_from(cell_type);
                }
            }
            CellMutation::Delete { cell_id } => {
                let index = working.iter().position(|cell| &cell.id == cell_id)?;
                let cell = working.remove(index);
                inverse.push(CellMutation::Insert { index, cell });
            }
            CellMutation::Move { cell_id, index } => {
                let previous = working.iter().position(|cell| &cell.id == cell_id)?;
                let cell = working.remove(previous);
                working.insert((*index).min(working.len()), cell);
                inverse.push(CellMutation::Move {
                    cell_id: cell_id.clone(),
                    index: previous,
                });
            }
            CellMutation::ClearOutputs { .. }
            | CellMutation::InsertRelative { .. }
            | CellMutation::MoveRelative { .. } => return None,
        }
    }
    inverse.reverse();
    Some(inverse)
}

fn notebook_document_width(available_width: f32) -> f32 {
    available_width.clamp(0.0, 1120.0)
}

#[derive(Clone, Copy)]
enum ToolbarIcon {
    Left,
    Right,
    Workspace,
    Follow(bool),
    Save,
    Add,
    Up,
    Down,
    Run,
    Stop,
    Restart,
    Trash,
}

fn desktop_control_scale(width: f32) -> f32 {
    (width / 800.0).clamp(0.85, 1.0)
}

fn control_scale(ui: &egui::Ui) -> f32 {
    desktop_control_scale(ui.ctx().screen_rect().width())
}

fn paint_microscope(ui: &egui::Ui, rect: egui::Rect) {
    let p = |x: f32, y: f32| {
        egui::pos2(
            rect.left() + x * rect.width(),
            rect.top() + y * rect.height(),
        )
    };
    let stroke = Stroke::new(1.5, ui.visuals().text_color());
    for (a, b) in [
        ((0.15, 0.95), (0.9, 0.95)),
        ((0.5, 0.95), (0.5, 0.75)),
        ((0.15, 0.65), (0.65, 0.65)),
        ((0.25, 0.1), (0.55, 0.4)),
        ((0.4, 0.0), (0.7, 0.3)),
        ((0.25, 0.1), (0.4, 0.0)),
        ((0.55, 0.4), (0.7, 0.3)),
    ] {
        ui.painter()
            .line_segment([p(a.0, a.1), p(b.0, b.1)], stroke);
    }
    ui.painter().add(egui::Shape::line(
        vec![
            p(0.7, 0.4),
            p(0.85, 0.5),
            p(0.85, 0.7),
            p(0.7, 0.8),
            p(0.5, 0.8),
        ],
        stroke,
    ));
}

fn diagnostics_button(ui: &mut egui::Ui) -> egui::Response {
    let response = ui
        .add_sized([24.0, 24.0], egui::Button::new(""))
        .on_hover_text("Open diagnostics");
    let rect = response.rect.shrink(5.0);
    let stroke = Stroke::new(1.5, ui.visuals().text_color());
    ui.painter()
        .rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
    let x = rect.left();
    let y = rect.center().y;
    ui.painter().add(egui::Shape::line(
        vec![
            egui::pos2(x, y),
            egui::pos2(x + 3.0, y),
            egui::pos2(x + 5.0, y - 4.0),
            egui::pos2(x + 8.0, y + 4.0),
            egui::pos2(x + 10.0, y),
            egui::pos2(rect.right(), y),
        ],
        stroke,
    ));
    response
}

fn toolbar_icon_button(ui: &mut egui::Ui, enabled: bool, icon: ToolbarIcon, tooltip: &str) -> bool {
    let scale = control_scale(ui);
    let response = ui
        .add_enabled(
            enabled,
            egui::Button::new("")
                .selected(matches!(icon, ToolbarIcon::Follow(true)))
                .min_size(egui::vec2(30.0, 28.0) * scale),
        )
        .on_hover_text(tooltip);
    let rect = response.rect.shrink(7.0 * scale);
    let color = if enabled {
        ui.visuals().widgets.inactive.fg_stroke.color
    } else {
        ui.visuals().weak_text_color()
    };
    let stroke = Stroke::new(1.6 * scale, color);
    let painter = ui.painter();
    match icon {
        ToolbarIcon::Left | ToolbarIcon::Right => {
            let (tip, tail) = if matches!(icon, ToolbarIcon::Left) {
                (rect.left(), rect.right())
            } else {
                (rect.right(), rect.left())
            };
            painter.add(egui::Shape::line(
                vec![
                    egui::pos2(tail, rect.top()),
                    egui::pos2(tip, rect.center().y),
                    egui::pos2(tail, rect.bottom()),
                ],
                stroke,
            ));
        }
        ToolbarIcon::Follow(_) => {
            let center = rect.center();
            let radius = rect.width().min(rect.height()) * 0.32;
            painter.circle_stroke(center, radius, stroke);
            painter.circle_filled(center, 1.8, color);
            for (a, b) in [
                (
                    egui::pos2(center.x, rect.top()),
                    egui::pos2(center.x, center.y - radius),
                ),
                (
                    egui::pos2(center.x, center.y + radius),
                    egui::pos2(center.x, rect.bottom()),
                ),
                (
                    egui::pos2(rect.left(), center.y),
                    egui::pos2(center.x - radius, center.y),
                ),
                (
                    egui::pos2(center.x + radius, center.y),
                    egui::pos2(rect.right(), center.y),
                ),
            ] {
                painter.line_segment([a, b], stroke);
            }
        }
        ToolbarIcon::Workspace => {
            painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
            let x = rect.left() + rect.width() * 0.35;
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                stroke,
            );
        }
        ToolbarIcon::Save => {
            painter.rect_stroke(rect, 1.0, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(rect.left() + 3.0, rect.top()),
                    egui::pos2(rect.left() + 3.0, rect.center().y - 1.0),
                ],
                stroke,
            );
            painter.rect_stroke(
                egui::Rect::from_min_max(
                    egui::pos2(rect.left() + 3.0, rect.center().y + 1.0),
                    egui::pos2(rect.right() - 3.0, rect.bottom()),
                ),
                0.0,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        ToolbarIcon::Add => {
            painter.line_segment(
                [
                    egui::pos2(rect.center().x, rect.top()),
                    egui::pos2(rect.center().x, rect.bottom()),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.left(), rect.center().y),
                    egui::pos2(rect.right(), rect.center().y),
                ],
                stroke,
            );
        }
        ToolbarIcon::Up | ToolbarIcon::Down => {
            let direction = if matches!(icon, ToolbarIcon::Up) {
                -1.0
            } else {
                1.0
            };
            let tip = egui::pos2(rect.center().x, rect.center().y + direction * 6.0);
            let base_y = rect.center().y - direction * 2.0;
            painter.line_segment([tip, egui::pos2(rect.left(), base_y)], stroke);
            painter.line_segment([tip, egui::pos2(rect.right(), base_y)], stroke);
            painter.line_segment(
                [
                    egui::pos2(rect.center().x, base_y),
                    egui::pos2(rect.center().x, rect.center().y - direction * 6.0),
                ],
                stroke,
            );
        }
        ToolbarIcon::Run => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    rect.left_top(),
                    egui::pos2(rect.right(), rect.center().y),
                    rect.left_bottom(),
                ],
                color,
                Stroke::NONE,
            ));
        }
        ToolbarIcon::Stop => {
            painter.rect_filled(rect.shrink(2.0), 0.0, color);
        }
        ToolbarIcon::Restart => {
            painter.circle_stroke(rect.center(), rect.width() * 0.38, stroke);
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(rect.left(), rect.top() + 1.0),
                    egui::pos2(rect.left() + 7.0, rect.top()),
                    egui::pos2(rect.left() + 3.0, rect.top() + 7.0),
                ],
                color,
                Stroke::NONE,
            ));
        }
        ToolbarIcon::Trash => {
            let body = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 2.0, rect.top() + 4.0),
                egui::pos2(rect.right() - 2.0, rect.bottom()),
            );
            painter.rect_stroke(body, 1.0, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(rect.left(), rect.top() + 2.0),
                    egui::pos2(rect.right(), rect.top() + 2.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.center().x - 3.0, rect.top()),
                    egui::pos2(rect.center().x + 3.0, rect.top()),
                ],
                stroke,
            );
            for fraction in [0.38, 0.62] {
                let x = body.left() + body.width() * fraction;
                painter.line_segment(
                    [
                        egui::pos2(x, body.top() + 3.0),
                        egui::pos2(x, body.bottom() - 3.0),
                    ],
                    stroke,
                );
            }
        }
    }
    response.clicked()
}

fn cell_visibility_control(
    ui: &mut egui::Ui,
    collapsed: bool,
    has_outputs: bool,
    selected: OutputViewMode,
) -> (bool, Option<OutputViewMode>) {
    let mut toggle = false;
    let mut chosen = None;
    let divider = ui.visuals().widgets.noninteractive.bg_stroke;
    egui::Frame::new()
        .fill(ui.visuals().widgets.inactive.weak_bg_fill)
        .stroke(divider)
        .corner_radius(CornerRadius::same(3))
        .inner_margin(Margin::ZERO)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let collapse = cell_collapse_button(ui, collapsed);
                toggle = collapse.clicked();
                if !has_outputs {
                    return;
                }
                ui.painter().line_segment(
                    [collapse.rect.right_top(), collapse.rect.right_bottom()],
                    divider,
                );
                ui.add_space(4.0);
                ui.add_enabled_ui(!(collapsed ^ toggle), |ui| {
                    for mode in [
                        OutputViewMode::Expanded,
                        OutputViewMode::Windowed,
                        OutputViewMode::Collapsed,
                    ] {
                        let response = output_view_button(ui, mode, selected == mode);
                        if response.clicked() {
                            chosen = Some(mode);
                        }
                        if mode != OutputViewMode::Collapsed {
                            let rect = response.rect;
                            ui.painter().line_segment(
                                [
                                    egui::pos2(rect.right(), rect.top() + 3.0),
                                    egui::pos2(rect.right(), rect.bottom() - 3.0),
                                ],
                                divider,
                            );
                        }
                    }
                });
            });
        });
    (toggle, chosen)
}

fn cell_collapse_button(ui: &mut egui::Ui, collapsed: bool) -> egui::Response {
    let scale = control_scale(ui);
    let label = if collapsed {
        "Expand entire cell"
    } else {
        "Collapse entire cell"
    };
    let response = ui
        .add(
            egui::Button::new("")
                .selected(collapsed)
                .stroke(Stroke::NONE)
                .corner_radius(CornerRadius::ZERO)
                .min_size(egui::vec2(28.0, 28.0) * scale),
        )
        .on_hover_text(label);
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Checkbox,
            response.enabled(),
            collapsed,
            label,
        )
    });
    let rect = egui::Rect::from_center_size(response.rect.center(), egui::vec2(18.0, 18.0) * scale);
    let stroke = Stroke::new(1.4, ui.style().interact(&response).fg_stroke.color);
    ui.painter()
        .rect_stroke(rect, 1.0, stroke, egui::StrokeKind::Inside);
    let center = rect.center();
    for direction in [-1.0, 1.0] {
        let edge = if collapsed { 2.0 } else { 5.0 };
        let tip = if collapsed { 5.0 } else { 2.0 };
        ui.painter().add(egui::Shape::line(
            vec![
                center + egui::vec2(-4.0, direction * edge),
                center + egui::vec2(0.0, direction * tip),
                center + egui::vec2(4.0, direction * edge),
            ],
            stroke,
        ));
    }
    response
}

fn output_view_button(ui: &mut egui::Ui, mode: OutputViewMode, selected: bool) -> egui::Response {
    let scale = control_scale(ui);
    let tooltip = match mode {
        OutputViewMode::Expanded => "Show full output",
        OutputViewMode::Windowed => "Show scrollable output pinned to latest",
        OutputViewMode::Collapsed => "Hide output",
    };
    let response = ui
        .add(
            egui::Button::new("")
                .selected(selected)
                .stroke(Stroke::NONE)
                .corner_radius(CornerRadius::ZERO)
                .min_size(egui::vec2(28.0, 28.0) * scale),
        )
        .on_hover_text(tooltip);
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::RadioButton,
            response.enabled(),
            selected,
            tooltip,
        )
    });
    let rect = egui::Rect::from_center_size(response.rect.center(), egui::vec2(18.0, 18.0) * scale);
    let color = ui.style().interact(&response).fg_stroke.color;
    let stroke = Stroke::new(1.4, color);
    match mode {
        OutputViewMode::Expanded => {
            for fraction in [0.0, 0.5, 1.0] {
                let y = egui::lerp(rect.top()..=rect.bottom(), fraction);
                ui.painter().line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    stroke,
                );
            }
        }
        OutputViewMode::Windowed => {
            ui.painter()
                .rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
            let track_x = rect.right() - 2.5;
            ui.painter().line_segment(
                [
                    egui::pos2(track_x, rect.top() + 2.0),
                    egui::pos2(track_x, rect.bottom() - 2.0),
                ],
                Stroke::new(1.0, color),
            );
            ui.painter().line_segment(
                [
                    egui::pos2(track_x, rect.center().y),
                    egui::pos2(track_x, rect.bottom() - 2.0),
                ],
                Stroke::new(2.2, color),
            );
        }
        OutputViewMode::Collapsed => {
            let center = rect.center();
            ui.painter().line_segment(
                [rect.left_top(), egui::pos2(center.x, center.y - 1.0)],
                stroke,
            );
            ui.painter().line_segment(
                [rect.right_top(), egui::pos2(center.x, center.y - 1.0)],
                stroke,
            );
            ui.painter().line_segment(
                [rect.left_bottom(), egui::pos2(center.x, center.y + 1.0)],
                stroke,
            );
            ui.painter().line_segment(
                [rect.right_bottom(), egui::pos2(center.x, center.y + 1.0)],
                stroke,
            );
        }
    }
    response
}

fn drag_handle() -> egui::Button<'static> {
    egui::Button::new("")
        .sense(egui::Sense::drag())
        .min_size(egui::vec2(28.0, 28.0))
}

fn paint_drag_hand(ui: &egui::Ui, response: &egui::Response) {
    let scale = control_scale(ui);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            response.enabled(),
            "Drag to reorder cell",
        )
    });
    let origin = response.rect.center() - egui::vec2(10.0, 10.0) * scale;
    // Open-hand outline in a 20-point square; independent of font glyph coverage.
    let outline = [
        (5.0, 11.0),
        (5.0, 5.0),
        (6.0, 4.0),
        (7.0, 5.0),
        (7.0, 9.0),
        (7.0, 2.0),
        (8.0, 1.0),
        (9.0, 2.0),
        (9.0, 8.0),
        (9.0, 1.0),
        (10.0, 0.5),
        (11.0, 1.0),
        (11.0, 8.0),
        (11.0, 3.0),
        (12.0, 2.0),
        (13.0, 3.0),
        (13.0, 10.0),
        (14.0, 7.0),
        (15.0, 6.0),
        (16.0, 7.0),
        (16.0, 12.0),
        (14.0, 17.0),
        (12.0, 19.0),
        (8.0, 19.0),
        (5.0, 16.0),
        (2.0, 11.0),
        (2.0, 10.0),
        (3.0, 9.0),
        (5.0, 11.0),
    ];
    let color = ui.style().interact(response).fg_stroke.color;
    ui.painter().add(egui::Shape::line(
        outline
            .into_iter()
            .map(|(x, y)| origin + egui::vec2(x, y) * scale)
            .collect(),
        Stroke::new(1.3, color),
    ));
}

fn cell_run_button(ui: &mut egui::Ui, enabled: bool, running: bool) -> egui::Response {
    let scale = control_scale(ui);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(28.0, 28.0) * scale,
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let visuals = ui.style().interact(&response);
    ui.painter()
        .rect_filled(rect, CornerRadius::same(2), visuals.weak_bg_fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(2),
        visuals.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let center = rect.center();
    if running {
        ui.ctx().request_repaint_after(Duration::from_millis(80));
        let start = ui.input(|input| input.time as f32) * 4.0;
        let radius = 6.0;
        let points = (0..=12)
            .map(|step| {
                let angle = start + std::f32::consts::TAU * step as f32 / 18.0;
                center + egui::vec2(angle.cos(), angle.sin()) * radius
            })
            .collect::<Vec<_>>();
        ui.painter().add(egui::Shape::line(
            points,
            Stroke::new(2.0, Color32::from_rgb(2, 119, 189)),
        ));
        response.on_hover_text("This cell is running")
    } else {
        let color = if enabled {
            Color32::from_rgb(38, 50, 56)
        } else {
            Color32::from_gray(150)
        };
        ui.painter().add(egui::Shape::convex_polygon(
            vec![
                center + egui::vec2(-4.0, -6.0) * scale,
                center + egui::vec2(6.0, 0.0) * scale,
                center + egui::vec2(-4.0, 6.0) * scale,
            ],
            color,
            Stroke::NONE,
        ));
        response.on_hover_text(if enabled {
            "Run this cell"
        } else {
            "Wait for the current notebook operation to finish"
        })
    }
}

fn execution_status_icon(ui: &mut egui::Ui, completed: bool, running: bool) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(14.0, 28.0), egui::Sense::hover());
    if running {
        ui.painter()
            .circle_filled(rect.center(), 3.5, Color32::from_rgb(2, 119, 189));
        response.on_hover_text("Cell execution in progress");
    } else if completed {
        let center = rect.center();
        let stroke = Stroke::new(1.8, Color32::from_rgb(46, 125, 50));
        ui.painter().line_segment(
            [
                egui::pos2(center.x - 4.0, center.y),
                egui::pos2(center.x - 1.0, center.y + 3.0),
            ],
            stroke,
        );
        ui.painter().line_segment(
            [
                egui::pos2(center.x - 1.0, center.y + 3.0),
                egui::pos2(center.x + 5.0, center.y - 4.0),
            ],
            stroke,
        );
        response.on_hover_text("Cell execution completed");
    }
}

fn output_collapse_summary(count: usize) -> String {
    match count {
        1 => "… 1 output hidden — click to expand".into(),
        count => format!("… {count} outputs hidden — click to expand"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notebook_protocol::{KernelIdentity, NotebookIdentity, NotebookSnapshot};
    #[test]
    fn microscope_navigation_is_local_single_target_and_ignores_late_loads() {
        use notebook_protocol::microscope::{self, MicroscopeTarget};
        let mut app = app();
        for id in ["micro01", "micro02"] {
            microscope::prepare(
                &mut app.state.snapshot,
                &NotebookCommandKind::CreateMicroscope {
                    cell_id: "code".into(),
                    microscope_id: id.into(),
                    title: id.into(),
                    walkthrough: serde_json::from_value(serde_json::json!({"title":id,"steps":[{"id":"one","title":"One","description":"Example $42$.","code":"42"}]})).unwrap(),
                },
            )
            .unwrap();
        }
        app.read_only = true;
        let first = microscope::document(&app.state.snapshot, "code", "micro01").unwrap();
        let second = microscope::document(&app.state.snapshot, "code", "micro02").unwrap();
        app.open_microscope(Some(MicroscopeTarget {
            cell_id: "code".into(),
            microscope_id: "micro01".into(),
            revision: 0,
            focus: None,
        }))
        .unwrap();
        assert!(matches!(
            app.drain_commands()[0].kind,
            NotebookCommandKind::ReadMicroscope { .. }
        ));
        app.show_microscope(Some(second.clone())).unwrap();
        app.accept_microscope(first.clone());
        assert_eq!(app.microscope_document, Some(second));
        app.open_microscope(None).unwrap();
        app.accept_microscope(first);
        assert!(app.microscope_target.is_none());
        assert!(app.microscope_document.is_none());
        assert!(app.drain_commands().is_empty());
        assert_eq!(
            microscope::list(&app.state.snapshot.cells[0])
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn active_context_tracks_live_selection_and_mode() {
        let mut app = app();
        app.state.snapshot.selected_cell_id = Some("code".into());
        app.edit_mode = true;
        app.editors[0].1 = "print('draft')".into();
        app.dirty_editors.insert("code".into());
        let context = app.active_context();
        assert_eq!(context["view"], "notebook");
        assert_eq!(context["notebook"]["path"], "completion.ipynb");
        assert_eq!(context["selection"]["cell_id"], "code");
        assert_eq!(context["selection"]["cell_index"], 0);
        assert_eq!(context["selection"]["mode"], "edit");
        assert_eq!(context["selection"]["draft"]["source"], "print('draft')");
        assert_eq!(context["selection"]["draft"]["dirty"], true);
        app.emit(NotebookCommandKind::ExecuteCell {
            cell_id: "code".into(),
        });
        assert_eq!(
            app.active_context()["selection"]["execution"]["status"],
            "queued"
        );
        assert_eq!(
            app.active_context()["selection"]["execution"]["source"],
            "print('draft')"
        );
        app.state.snapshot.selected_cell_id = None;
        app.edit_mode = false;
        assert!(app.active_context()["selection"].is_null());
        assert_eq!(app.drain_commands().len(), 1);
    }

    fn app() -> NotebookEguiApp {
        let snapshot = NotebookSnapshot {
            protocol_version: PROTOCOL_VERSION,
            schema_version: 1,
            notebook: NotebookIdentity {
                path: "completion.ipynb".into(),
                workspace: "local".into(),
            },
            kernel: KernelIdentity {
                name: "python3".into(),
                display_name: "Python 3".into(),
                session_id: None,
                state: KernelState::Idle,
            },
            revision: 1,
            cells: vec![Cell {
                id: "code".into(),
                cell_type: CellType::Code,
                source: "value.bi".into(),
                metadata: serde_json::json!({}),
                execution_count: None,
                outputs: vec![],
            }],
            selected_cell_id: Some("code".into()),
        };
        NotebookEguiApp::new(NotebookState::new(snapshot).unwrap())
    }

    #[test]
    fn markdown_shift_enter_renders_and_advances() {
        let mut app = app();
        let mut markdown = app.state.snapshot.cells[0].clone();
        markdown.id = "markdown".into();
        markdown.cell_type = CellType::Markdown;
        app.state.snapshot.cells.insert(0, markdown);
        app.editors.push(("markdown".into(), "# Edited".into()));
        app.dirty_editors.insert("markdown".into());
        app.select_cell(0, false);
        app.begin_editing_cell("markdown");
        app.execute_selected_and_advance(false);
        assert!(app.rendered_markdown.contains("markdown"));
        assert_eq!(app.state.snapshot.selected_cell_id.as_deref(), Some("code"));
        assert_eq!(app.selected_cells_in_order().len(), 1);
        assert!(!app.edit_mode);
        assert!(
            app.drain_commands()
                .iter()
                .all(|c| !matches!(c.kind, NotebookCommandKind::ExecuteCell { .. }))
        );
    }

    #[test]
    fn agent_highlights_are_bounded_local_and_dismissed_on_click() {
        let mut app = app();
        let before = app.state.clone();
        assert!(app.cell_view("code", "highlight", "red").is_err());
        assert!(app.cell_view("missing", "highlight", "blue").is_err());
        app.cell_view("code", "highlight", "blue-deep").unwrap();
        assert_eq!(app.agent_highlights.len(), 1);
        assert_eq!(app.state, before);
        app.cell_view("code", "clear_highlight", "").unwrap();
        assert!(app.agent_highlights.is_empty());
        app.cell_view("code", "highlight", "blue").unwrap();
        let ctx = egui::Context::default();
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let cell = app.state.snapshot.cells[0].clone();
        let _ = ctx.run(input.clone(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.cell(ui, 0, cell.clone(), false));
        });
        input.events = vec![
            egui::Event::PointerMoved(egui::pos2(50.0, 30.0)),
            egui::Event::PointerButton {
                pos: egui::pos2(50.0, 30.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        let _ = ctx.run(input.clone(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.cell(ui, 0, cell.clone(), false));
        });
        input.events = vec![egui::Event::PointerButton {
            pos: egui::pos2(50.0, 30.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }];
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.cell(ui, 0, cell.clone(), false));
        });
        assert!(app.agent_highlights.is_empty());
    }

    #[test]
    fn desktop_controls_shrink_smoothly_without_a_mobile_jump() {
        let widths = [1200.0, 800.0, 799.0, 601.0, 600.0, 599.0, 400.0];
        for pair in widths.windows(2) {
            assert!(desktop_control_scale(pair[1]) <= desktop_control_scale(pair[0]));
        }
        assert_eq!(desktop_control_scale(1200.0), 1.0);
        assert_eq!(desktop_control_scale(400.0), 0.85);
        assert!((desktop_control_scale(601.0) - desktop_control_scale(599.0)).abs() < 0.01);
    }

    #[test]
    fn diagnostics_remains_clickable_at_bottom_right_when_status_wraps() {
        for width in [1024.0, 599.0, 400.0] {
            let mut app = app();
            app.host_status = "Connected · WebMCP ready".into();
            let ctx = egui::Context::default();
            let mut input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 600.0),
                )),
                ..Default::default()
            };
            let draw = |ctx: &egui::Context, app: &mut NotebookEguiApp| {
                egui::TopBottomPanel::bottom("status").show(ctx, |ui| app.status(ui));
            };
            let _ = ctx.run(input.clone(), |ctx| draw(ctx, &mut app));
            let pos = egui::pos2(width - 20.0, 584.0);
            for pressed in [true, false] {
                input.events = vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        pressed,
                        button: egui::PointerButton::Primary,
                        modifiers: egui::Modifiers::NONE,
                    },
                ];
                let _ = ctx.run(input.clone(), |ctx| draw(ctx, &mut app));
            }
            assert!(app.diagnostics_toggle_requested, "width={width}");
        }
    }

    #[test]
    fn clicking_output_selects_its_cell_in_all_output_modes() {
        for mode in [
            OutputViewMode::Expanded,
            OutputViewMode::Windowed,
            OutputViewMode::Collapsed,
        ] {
            let mut app = app();
            app.state.snapshot.cells[0].outputs = vec![CellOutput::Text {
                text: "Output to select".into(),
            }];
            let mut other = app.state.snapshot.cells[0].clone();
            other.id = "markdown".into();
            other.cell_type = CellType::Markdown;
            other.outputs.clear();
            app.state.snapshot.cells.push(other);
            app.select_cell(1, false);
            app.edit_mode = true;
            app.set_output_view("code", mode);
            let ctx = egui::Context::default();
            let cell = app.state.snapshot.cells[0].clone();
            let mut input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            app.capture_target = Some(("code".into(), 0));
            let _ = ctx.run(input.clone(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    app.cell(ui, 0, cell.clone(), false);
                });
            });
            let bottom = app.capture_region.as_ref().unwrap().0.bottom();
            let position = egui::pos2(40.0, bottom - 16.0);
            for pressed in [true, false] {
                input.events = vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ];
                let _ = ctx.run(input.clone(), |ctx| {
                    egui::CentralPanel::default()
                        .show(ctx, |ui| app.cell(ui, 0, cell.clone(), false));
                });
            }
            assert_eq!(
                app.state.snapshot.selected_cell_id.as_deref(),
                Some("code"),
                "{mode:?}"
            );
            assert!(!app.selected_cells.contains("markdown"));
            assert!(!app.edit_mode);
            assert!(app.drain_commands().is_empty());
        }
    }

    #[test]
    fn ui_inserts_capture_id_anchors_and_shift_selection_does_not_mix_types() {
        let mut app = app();
        let mut markdown = app.state.snapshot.cells[0].clone();
        markdown.id = "markdown".into();
        markdown.cell_type = CellType::Markdown;
        app.state.snapshot.cells.push(markdown);
        app.insert_cell(1, CellType::Code, String::new());
        assert!(
            matches!(&app.drain_commands()[0].kind, NotebookCommandKind::ModifyCells { changes } if matches!(&changes[0], CellMutation::InsertRelative { anchor_cell_id, after: false, .. } if anchor_cell_id == "markdown"))
        );
        app.select_cell(0, false);
        app.select_cell(1, true);
        assert_eq!(app.selected_cells.len(), 1);
        assert!(app.selected_cells.contains("markdown"));
    }

    #[test]
    fn clicking_markdown_clears_previous_code_selection() {
        let mut app = app();
        let mut markdown = app.state.snapshot.cells[0].clone();
        markdown.id = "markdown".into();
        markdown.cell_type = CellType::Markdown;
        app.state.snapshot.cells.push(markdown);
        app.select_cell(0, false);
        app.apply_rendered_markdown_interaction("markdown", true, false);
        assert!(!app.selected_cells.contains("code"));
        assert_eq!(app.selected_cells_in_order()[0].id, "markdown");
    }

    #[test]
    fn followed_selection_is_read_only_bounded_to_existing_cells_and_command_free() {
        let mut app = app();
        app.follow_selection(None);
        assert_eq!(app.state.snapshot.selected_cell_id.as_deref(), Some("code"));
        app.read_only = true;
        app.follow_selection(None);
        assert!(app.state.snapshot.selected_cell_id.is_none());
        app.follow_selection(Some("missing"));
        assert!(app.state.snapshot.selected_cell_id.is_none());
        app.follow_selection(Some("code"));
        assert_eq!(app.state.snapshot.selected_cell_id.as_deref(), Some("code"));
        assert_eq!(app.state.snapshot.revision, 1);
        assert!(app.drain_commands().is_empty());
    }

    #[test]
    fn observer_cannot_enter_edit_mode_or_emit_commands() {
        let mut app = app();
        app.read_only = true;
        app.edit_mode = false;
        let before = app.state.clone();
        app.begin_editing_cell("code");
        app.emit(NotebookCommandKind::ExecuteCell {
            cell_id: "code".into(),
        });
        assert!(!app.edit_mode);
        assert!(app.drain_commands().is_empty());
        assert_eq!(app.state, before);
        app.cell_view("code", "cell", "true").unwrap();
        assert!(app.collapsed_cells.contains("code"));
    }

    #[test]
    fn selected_completion_replaces_kernel_cursor_range() {
        let mut app = app();
        let cell = app.state.snapshot.cells[0].clone();
        app.request_completion(&cell);
        let command_id = app.drain_commands()[0].command_id;
        app.apply_completion(
            command_id,
            CompletionReply {
                matches: vec!["bit_count".into(), "bit_length".into()],
                cursor_start: 6,
                cursor_end: 8,
            },
        );
        app.completion_selection.insert("code".into(), 1);

        app.accept_completion("code");

        assert_eq!(app.editors[0].1, "value.bit_length");
        assert!(app.dirty_editors.contains("code"));
        assert!(!app.completion_suggestions.contains_key("code"));
        assert_eq!(app.pending_caret_positions.get("code"), Some(&16));
    }

    #[test]
    fn drop_index_accounts_for_removing_the_dragged_cell() {
        assert_eq!(move_index_for_drop(0, 2, true), 1);
        assert_eq!(move_index_for_drop(0, 2, false), 2);
        assert_eq!(move_index_for_drop(3, 1, true), 1);
        assert_eq!(move_index_for_drop(3, 1, false), 2);
    }

    #[test]
    fn drag_handle_is_a_non_selectable_button() {
        let context = egui::Context::default();
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                ui.horizontal(|ui| {
                    let drag = ui.add(drag_handle());
                    paint_drag_hand(ui, &drag);
                    let run = cell_run_button(ui, true, false);
                    assert!(drag.sense.senses_drag());
                    assert!(!drag.sense.senses_click());
                    assert_eq!(drag.rect.size(), egui::vec2(28.0, 28.0));
                    assert!(drag.rect.right() < run.rect.left());
                });
            });
        });
    }

    #[test]
    fn collapsed_visibility_control_keeps_output_segments_and_alignment() {
        let context = egui::Context::default();
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                let expanded = ui
                    .horizontal(|ui| {
                        cell_visibility_control(ui, false, true, OutputViewMode::Windowed);
                    })
                    .response
                    .rect;
                let collapsed = ui
                    .horizontal(|ui| {
                        assert_eq!(
                            cell_visibility_control(ui, true, true, OutputViewMode::Windowed),
                            (false, None)
                        );
                    })
                    .response
                    .rect;
                assert_eq!(expanded.size(), collapsed.size());
                ui.add_enabled_ui(false, |ui| {
                    let mode = output_view_button(ui, OutputViewMode::Windowed, true);
                    assert!(!mode.enabled());
                    assert!(!mode.clicked());
                    assert_eq!(mode.rect.size(), egui::vec2(28.0, 28.0));
                });
                let collapse = cell_collapse_button(ui, false);
                assert_eq!(collapse.rect.size(), egui::vec2(28.0, 28.0));
            });
        });
    }

    #[test]
    fn notebook_document_width_tracks_narrow_frames() {
        assert_eq!(notebook_document_width(720.0), 720.0);
        assert_eq!(notebook_document_width(1440.0), 1120.0);
        assert_eq!(notebook_document_width(-1.0), 0.0);
    }

    #[test]
    fn markdown_is_rendered_by_default() {
        let mut snapshot = app().state.snapshot;
        snapshot.cells[0].cell_type = CellType::Markdown;

        let app = NotebookEguiApp::new(NotebookState::new(snapshot).unwrap());

        assert!(app.rendered_markdown.contains("code"));
    }

    #[test]
    fn julia_kernel_uses_julia_keywords() {
        let syntax = kernel_syntax("julia-course-1.10");
        assert_eq!(syntax.language, "Julia");
        assert!(syntax.keywords.contains("function"));
        assert_eq!(syntax.comment_multiline, ["#=", "=#"]);
        assert_eq!(kernel_syntax("python3").language, "Python");
    }

    #[test]
    fn tool_view_changes_preserve_notebook_and_validate_targets() {
        let mut app = app();
        let before = app.state.clone();
        app.cell_view("code", "cell", "true").unwrap();
        assert!(app.collapsed_cells.contains("code"));
        app.cell_view("code", "output", "windowed").unwrap();
        assert_eq!(app.output_view("code"), OutputViewMode::Windowed);
        app.cell_view("code", "cell", "false").unwrap();
        assert!(!app.collapsed_cells.contains("code"));
        assert!(app.cell_view("missing", "capture", "").is_err());
        assert!(app.cell_view("code", "output", "unknown").is_err());
        assert_eq!(app.state, before);
        assert!(app.drain_commands().is_empty());
    }

    #[test]
    fn output_view_cycles_three_local_states_and_preserves_outputs() {
        let mut app = app();
        app.state.snapshot.cells[0].outputs = vec![CellOutput::Text { text: "42".into() }];

        assert_eq!(app.output_view("code"), OutputViewMode::Expanded);
        app.cycle_selected_output_view();
        assert_eq!(app.output_view("code"), OutputViewMode::Windowed);
        assert_eq!(app.state.snapshot.cells[0].outputs.len(), 1);

        app.cycle_selected_output_view();
        assert_eq!(app.output_view("code"), OutputViewMode::Collapsed);
        app.cycle_selected_output_view();
        assert_eq!(app.output_view("code"), OutputViewMode::Expanded);
        assert_eq!(app.state.snapshot.cells[0].outputs.len(), 1);
    }

    #[test]
    fn output_collapse_summary_handles_singular_and_plural() {
        assert_eq!(
            output_collapse_summary(1),
            "… 1 output hidden — click to expand"
        );
        assert_eq!(
            output_collapse_summary(3),
            "… 3 outputs hidden — click to expand"
        );
    }

    #[test]
    fn refresh_preserves_editing_and_renders_new_markdown() {
        let mut snapshot = app().state.snapshot;
        snapshot.cells[0].cell_type = CellType::Markdown;
        let mut app = NotebookEguiApp::new(NotebookState::new(snapshot.clone()).unwrap());
        app.rendered_markdown.remove("code");

        snapshot.revision += 1;
        snapshot.cells.push(Cell {
            id: "new-markdown".into(),
            cell_type: CellType::Markdown,
            source: "## New".into(),
            metadata: serde_json::json!({}),
            execution_count: None,
            outputs: vec![],
        });
        app.replace_state(NotebookState::new(snapshot).unwrap());

        assert!(!app.rendered_markdown.contains("code"));
        assert!(app.rendered_markdown.contains("new-markdown"));
    }

    #[test]
    fn rendered_markdown_senses_double_clicks() {
        let context = egui::Context::default();
        let mut senses_click = false;
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                let mut cache = CommonMarkCache::default();
                let math_cache = Arc::new(Mutex::new(MathRenderCache::default()));
                senses_click =
                    rendered_markdown_response(ui, "markdown", "# Hello", &mut cache, &math_cache)
                        .response
                        .sense
                        .senses_click();
            });
        });

        assert!(senses_click);
    }

    #[test]
    fn markdown_tables_are_split_into_bounded_cells() {
        let source = "Before\n\n| Study | Useful representation |\n| --- | --- |\n| Power flow | A long explanation that must wrap |\n\nAfter";
        let blocks = markdown_blocks(source);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], MarkdownBlock::Text("Before\n\n"));
        assert_eq!(
            blocks[1],
            MarkdownBlock::Table(vec![
                vec!["Study".into(), "Useful representation".into()],
                vec![
                    "Power flow".into(),
                    "A long explanation that must wrap".into()
                ],
            ])
        );
        assert_eq!(blocks[2], MarkdownBlock::Text("\nAfter"));
    }

    #[test]
    fn markdown_table_separator_requires_real_dashes() {
        assert!(table_separator("| :--- | ---: |"));
        assert!(!table_separator("| Study | Description |"));
    }

    #[test]
    fn markdown_math_is_typeset_into_an_image() {
        for formula in [
            r"\frac{1}{2}x^2",
            r"U_f\ket{x} = (-1)^{s \cdot x}\ket{x}",
            r"\ket{\psi}=\sum_x a_x \ket{x}",
        ] {
            let image = render_math_formula(formula, false).unwrap();

            assert!(image.width() > 1);
            assert!(image.height() > 1);
            assert!(image.pixels.iter().any(|pixel| pixel.a() > 0));
        }
    }

    #[test]
    fn inline_markdown_math_is_tighter_than_display_math() {
        let inline = render_math_formula(r"x^2", true).unwrap();
        let display = render_math_formula(r"x^2", false).unwrap();

        assert!(inline.height() < display.height());
        assert!(inline.width() < display.width());
    }

    #[test]
    fn inline_markdown_math_is_optically_centered_with_the_text_run() {
        let context = egui::Context::default();
        let output = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                let mut cache = CommonMarkCache::default();
                let math_cache = Arc::new(Mutex::new(MathRenderCache::default()));
                rendered_markdown_response(
                    ui,
                    "inline-math",
                    r"Before $x^2$ after",
                    &mut cache,
                    &math_cache,
                );
            });
        });
        let text_bounds = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(_) => Some(clipped.shape.visual_bounding_rect()),
                _ => None,
            })
            .reduce(|left, right| left.union(right))
            .expect("surrounding text shape");
        let math_bounds = output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Mesh(mesh) if mesh.texture_id != egui::TextureId::default() => {
                    Some(clipped.shape.visual_bounding_rect())
                }
                _ => None,
            })
            .expect("inline math texture");
        let center_delta = math_bounds.center().y - text_bounds.center().y;

        assert!(
            center_delta.abs() <= 1.0,
            "inline math is not centered with text: delta={center_delta}, math={math_bounds:?}, text={text_bounds:?}"
        );
    }

    #[test]
    fn inline_math_descender_reserve_keeps_paint_inside_its_allocated_row() {
        let texture_size = egui::vec2(20.0, 14.0);
        let (allocation_size, paint_offset) = inline_math_layout(texture_size);

        assert!(paint_offset.y + texture_size.y <= allocation_size.y);
    }

    #[test]
    fn markdown_math_preserves_padding_around_matrix_glyphs() {
        let image = render_math_formula(
            r"H = \frac{1}{\sqrt{2}} \begin{pmatrix} 1 & 1 \\ 1 & -1 \end{pmatrix}",
            true,
        )
        .unwrap();
        let width = image.width();
        let height = image.height();
        let top = (0..width).any(|x| image.pixels[x].a() > 0);
        let bottom = (0..width).any(|x| image.pixels[(height - 1) * width + x].a() > 0);
        let left = (0..height).any(|y| image.pixels[y * width].a() > 0);
        let right = (0..height).any(|y| image.pixels[y * width + width - 1].a() > 0);

        assert!(
            !(top || bottom || left || right),
            "math glyphs touch bitmap boundary: top={top}, bottom={bottom}, left={left}, right={right}"
        );
    }

    #[test]
    fn rendered_markdown_does_not_clip_math_texture() {
        let context = egui::Context::default();
        let output = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                let mut cache = CommonMarkCache::default();
                let math_cache = Arc::new(Mutex::new(MathRenderCache::default()));
                rendered_markdown_response(
                    ui,
                    "math",
                    r"$H = \frac{1}{\sqrt{2}} \begin{pmatrix} 1 & 1 \\ 1 & -1 \end{pmatrix}$",
                    &mut cache,
                    &math_cache,
                );
            });
        });
        let clipped_math = output.shapes.iter().any(|clipped| {
            matches!(&clipped.shape, egui::Shape::Mesh(mesh) if {
                let bounds = clipped.shape.visual_bounding_rect();
                !clipped.clip_rect.contains_rect(bounds)
                    && mesh.texture_id != egui::TextureId::default()
            })
        });

        assert!(
            !clipped_math,
            "rendered math exceeds its egui clip rectangle"
        );
    }

    #[test]
    fn markdown_base64_image_uri_loads_without_network() {
        let context = egui::Context::default();
        install_data_image_loader(&context);
        egui_extras::install_image_loaders(&context);
        let uri = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

        assert!(matches!(
            context.try_load_bytes(uri),
            Ok(egui::load::BytesPoll::Ready { .. })
        ));
    }

    #[test]
    fn clicking_rendered_markdown_selects_without_editing() {
        let mut app = app();
        app.state.snapshot.selected_cell_id = None;

        app.apply_rendered_markdown_interaction("code", true, false);

        assert_eq!(app.state.snapshot.selected_cell_id.as_deref(), Some("code"));
        assert!(!app.edit_mode);
        assert!(app.pending_editor_focus.is_none());
    }

    #[test]
    fn double_clicking_rendered_markdown_selects_and_edits() {
        let mut app = app();
        app.rendered_markdown.insert("code".into());

        app.apply_rendered_markdown_interaction("code", true, true);

        assert_eq!(app.state.snapshot.selected_cell_id.as_deref(), Some("code"));
        assert!(app.edit_mode);
        assert_eq!(app.pending_editor_focus.as_deref(), Some("code"));
        assert!(!app.rendered_markdown.contains("code"));
    }

    #[test]
    fn enter_on_selected_rendered_markdown_reveals_the_editor() {
        let mut app = app();
        app.state.snapshot.cells[0].cell_type = CellType::Markdown;
        app.rendered_markdown.insert("code".into());

        app.begin_editing_cell("code");

        assert!(app.edit_mode);
        assert_eq!(app.pending_editor_focus.as_deref(), Some("code"));
        assert!(!app.rendered_markdown.contains("code"));
    }

    #[test]
    fn selected_completion_scrolls_into_view() {
        let context = egui::Context::default();
        let mut scroll_id = egui::Id::NULL;
        for _ in 0..2 {
            let _ = context.run(egui::RawInput::default(), |context| {
                egui::CentralPanel::default().show(context, |ui| {
                    scroll_id = ui.make_persistent_id(egui::Id::new("completion-scroll-test"));
                    egui::ScrollArea::vertical()
                        .id_salt("completion-scroll-test")
                        .max_height(60.0)
                        .show(ui, |ui| {
                            for index in 0..12 {
                                completion_row(ui, &format!("item-{index}"), index == 11);
                            }
                        });
                });
            });
        }

        let state = egui::scroll_area::State::load(&context, scroll_id).unwrap();
        assert!(state.offset.y > 0.0);
    }

    #[test]
    fn completion_key_help_avoids_unbundled_arrow_glyphs() {
        assert!(!COMPLETION_KEY_HELP.contains(['↑', '↓']));
        assert_eq!(
            COMPLETION_KEY_HELP,
            "Up/Down select · Enter or Tab apply · Esc close"
        );
    }

    #[test]
    fn ansi_kernel_output_is_styled_without_control_glyphs() {
        let base = Color32::from_rgb(38, 50, 56);
        let job = ansi_layout_job("\u{1b}[31mSyntaxError\u{1b}[0m", base);

        assert_eq!(job.text, "SyntaxError");
        assert!(job.text.chars().all(|character| !character.is_control()));
        assert!(job.sections.iter().any(|section| {
            section.format.color == Color32::from_rgb(198, 40, 40)
                && &job.text[section.byte_range.clone()] == "SyntaxError"
        }));
    }

    #[test]
    fn rich_png_output_is_decoded_for_the_egui_bytes_loader() {
        let bytes = decode_rich_image("image/png", "iVBORw0KGgo=").unwrap();

        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn png_dimensions_reserve_plot_space_before_texture_loading() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&715_u32.to_be_bytes());
        png.extend_from_slice(&397_u32.to_be_bytes());

        assert_eq!(png_dimensions(&png), Some((715, 397)));
    }

    #[test]
    fn svg_view_box_reserves_plot_space_before_texture_loading() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" viewBox="0 0 720 420"><circle r="10"/></svg>"#;

        assert_eq!(svg_dimensions(svg), Some((720.0, 420.0)));
    }

    #[test]
    fn svg_numeric_dimensions_take_precedence_over_view_box() {
        let svg = br#"<svg height='240px' viewBox='0 0 720 420' width='320px'></svg>"#;

        assert_eq!(svg_dimensions(svg), Some((320.0, 240.0)));
    }

    #[test]
    fn executed_code_cell_has_completion_status() {
        let mut cell = app().state.snapshot.cells[0].clone();
        assert!(!cell_has_completed_execution(&cell));

        cell.execution_count = Some(1);

        assert!(cell_has_completed_execution(&cell));
    }

    #[test]
    fn markdown_group_metadata_is_versioned_and_code_only() {
        let mut code = app().state.snapshot.cells[0].clone();
        code.metadata = serde_json::json!({
            "keep": true,
            MARKDOWN_GROUP_METADATA_KEY: {
                "schema_version": 1,
                "markdown_cell_id": "markdown"
            }
        });
        assert!(combines_with_markdown(&code, "markdown"));
        assert!(!combines_with_markdown(&code, "other"));
        assert_eq!(code.metadata["keep"], true);

        code.metadata[MARKDOWN_GROUP_METADATA_KEY] = serde_json::json!({"schema_version": 2});
        assert!(!combines_with_markdown(&code, "markdown"));
        code.cell_type = CellType::Markdown;
        code.metadata[MARKDOWN_GROUP_METADATA_KEY] =
            serde_json::json!({"schema_version": 1, "markdown_cell_id": "markdown"});
        assert!(!combines_with_markdown(&code, "markdown"));
    }

    #[test]
    fn grouped_markdown_selection_is_publicly_the_code_cell_and_can_ungroup() {
        let mut app = app();
        let mut markdown = app.state.snapshot.cells[0].clone();
        markdown.id = "markdown".into();
        markdown.cell_type = CellType::Markdown;
        markdown.metadata = serde_json::json!({});
        let mut code = app.state.snapshot.cells[0].clone();
        code.metadata = serde_json::json!({
            "keep": true,
            MARKDOWN_GROUP_METADATA_KEY: {
                "schema_version": 1,
                "markdown_cell_id": "markdown"
            }
        });
        app.state.snapshot.cells = vec![markdown, code];
        app.state.snapshot.selected_cell_id = Some("markdown".into());

        assert_eq!(app.active_context()["selection"]["cell_id"], "code");
        app.set_selected_markdown_group(false);
        let commands = app.drain_commands();
        assert_eq!(app.state.snapshot.selected_cell_id.as_deref(), Some("code"));
        let NotebookCommandKind::ModifyCells { changes } = &commands[0].kind else {
            panic!("expected metadata update");
        };
        let CellMutation::Update { metadata, .. } = &changes[0] else {
            panic!("expected cell update");
        };
        assert_eq!(metadata.as_ref().unwrap()["keep"], true);
        assert!(
            metadata
                .as_ref()
                .unwrap()
                .get(MARKDOWN_GROUP_METADATA_KEY)
                .is_none()
        );
    }

    #[test]
    fn single_cell_execution_tracks_progress_until_result_arrives() {
        let mut app = app();
        let cell = app.state.snapshot.cells[0].clone();

        app.execute_cell(0, &cell);
        let command = app.drain_commands().pop().unwrap();

        assert_eq!(
            app.pending_execution_cells.get(&command.command_id),
            Some(&"code".to_owned())
        );
        assert!(matches!(
            command.kind,
            NotebookCommandKind::ExecuteCell { cell_id } if cell_id == "code"
        ));

        app.finish_command(command.command_id);
        assert!(app.pending_execution_cells.is_empty());
    }

    #[test]
    fn completed_output_follow_is_scoped_to_temporary_playgrounds() {
        let mut app = app();
        app.state.snapshot.cells[0].outputs = vec![CellOutput::Text { text: "42".into() }];

        app.reveal_output("code");
        assert!(app.scroll_to_output.is_none());

        app.state.snapshot.notebook.workspace = "temporary".into();
        app.reveal_output("code");
        assert_eq!(app.scroll_to_output.as_deref(), Some("code"));
    }

    #[test]
    fn structural_inverse_restores_deleted_cell() {
        let cells = app().state.snapshot.cells;
        let inverse = inverse_cell_changes(
            &cells,
            &[CellMutation::Delete {
                cell_id: "code".into(),
            }],
        )
        .unwrap();

        assert!(matches!(
            &inverse[0],
            CellMutation::Insert { index: 0, cell } if cell.id == "code"
        ));
    }

    #[test]
    fn html_output_is_reduced_to_safe_table_text() {
        let text = sanitized_html_text(
            "<table><tr><th>Name</th><th>Value</th></tr><tr><td>x</td><td>42</td></tr></table><script>alert(1)</script>",
        );

        assert!(!text.contains('<'));
        assert!(text.contains("Name\tValue"));
        assert!(text.contains("x\t42"));
    }
}
