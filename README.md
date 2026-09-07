# didaction Jupyter

didaction Jupyter is a local-first notebook environment for researchers, students,
and learners who want familiar, code-first Jupyter ergonomics in an egui/WebAssembly
interface. It opens real `.ipynb` files, runs standard Jupyter kernels, renders
Markdown, math and rich outputs, and gives browser agents the same validated
notebook command path through optional WebMCP tools.

Try the browser-local build at
[didaction.github.io/didaction-jupyter](https://didaction.github.io/didaction-jupyter/).

![didaction Jupyter notebook interface](docs/assets/notebook-ui.png)

The project has two deliberately separate distributions:

| Mode               | Best for                                                                              | Kernel and storage                                                                                      |
| ------------------ | ------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| **Browser-local**  | Static hosting, demos, private local learning                                         | Pyodide or experimental xeus-python in a Worker; notebooks and imported workspace files in IndexedDB    |
| **Jupyter server** | Existing Python environments, arbitrary installed kernelspecs, persistent local files | A loopback Rust gateway connects directly to Jupyter Contents, Sessions, Kernels, and kernel WebSockets |

Both modes mount the same egui UI and route human actions and WebMCP calls through
the same typed command gateway. The browser-local build never silently connects to
the server runtime, and the server build never exposes Jupyter credentials to
browser JavaScript.

## Quickstart

### Browser-local

Requirements: Rust 1.96, the `wasm32-unknown-unknown` target, Node.js, pnpm, and
`wasm-pack`.

```bash
pnpm install --frozen-lockfile
rustup target add wasm32-unknown-unknown
pnpm build:browser
pnpm serve:browser
```

Open <http://127.0.0.1:5175>. Choose a pinned Python runtime, then create a blank
workspace, open the demo, import a bounded ZIP workspace, or continue saved work.
The chooser offers `SKILLS.md` for agents working through WebMCP. Imported
notebooks never execute automatically. See
[Browser-local runtime](docs/browser-runtime.md) for per-notebook tab locks,
persistence, ZIP bounds, kernel versions, xeus setup, and static deployment.

### Jupyter server

Additional requirements: Python 3.12 and `uv`.

```bash
uv sync --python 3.12 --frozen
pnpm install --frozen-lockfile
rustup target add wasm32-unknown-unknown
pnpm build:wasm
scripts/dev.sh
```

Open <http://127.0.0.1:5173>. The default workspace seeds
`notebook-parity-demo.ipynb` with executable completion, math, Markdown, and SVG
graph examples. The generated Jupyter token stays in the service environment.

Configure a fixed workspace, initial notebook, and kernelspec at startup:

```bash
DIDACTION_NOTEBOOK_WORKSPACE=/absolute/notebooks \
DIDACTION_NOTEBOOK_PATH=course/week-1.ipynb \
DIDACTION_KERNEL_NAME=python3 \
scripts/dev.sh
```

Paths are relative to the configured workspace and cannot traverse outside it.
List installed kernels with `uv run jupyter kernelspec list`. To use an unregistered
environment containing `ipykernel`, also set
`DIDACTION_KERNEL_PYTHON=/absolute/environment/.venv/bin/python`; the launcher
creates a repository-local kernelspec without changing user-wide configuration.

Docker is also available:

```bash
bash scripts/container.sh up
```

For a persistent per-user kernel catalog and generated Compose deployment, install
the Rust control utility and initialize it once:

```bash
cargo install --path crates/didaction-jupyter-cli
djupctl init
djupctl config
djupctl up
```

`djupctl` stores configuration, a generated Compose file, and its private
development token under `~/.config/didaction`. The TUI distinguishes server
container profiles from browser-WASM profiles; only an enabled server profile can
be selected for a Compose deployment.

Only <http://127.0.0.1:5173> is published; Jupyter remains on the private container
network. See [Container deployment](docs/container-deployment.md) for custom images,
attach mode, workspaces, and secrets.

## What works

- Familiar notebook menus and toolbar, ordered code/Markdown/raw cells, add above
  or below, delete, duplicate, drag/move, cut/copy/paste, and structural undo/redo.
- Real execution, run-and-advance, interrupt/restart/reconnect, execution counts,
  intermediate NDJSON output streaming, and persisted final notebook state.
- Kernel completion, signature help, find/replace, line numbers, keyboard command
  mode, and three-state output presentation.
- Rendered CommonMark, inline/display math, bounded base64 images, text/stream/error
  output, safe readable HTML tables, PNG, and SVG.
- A workspace explorer for folders, notebooks and artifacts, including bounded
  creation/uploads on the server and ZIP import/export in browser-local mode.
- Cell-owned **Microscopes**: multi-step visual explanations with highlighted code,
  math-capable descriptions, animated AssemblyScript RGBA graphics, hoverable
  annotations, temporary executable playground windows, and WebMCP PNG capture for
  visual feedback.
- Single-driver local collaboration in server mode, with explicit driver handoff
  and opt-in following.

The maintained status matrix is [Frontend parity](docs/frontend-parity.md). It is
the source of truth for supported, partial, and intentionally deferred behavior.

## Architecture and trust boundaries

```text
egui action ─┐
             ├─ CommandGateway → WASM validation → typed transport
WebMCP tool ─┘                                      ↓
                         browser Worker or same-origin Rust gateway
                                                   ↓
                 bounded progress/result → WASM reconciliation → egui
```

Rust/WASM does not fetch, open WebSockets, access files, or invoke Jupyter. In the
server build, TypeScript owns browser APIs while the Rust gateway owns the Jupyter
token and directly adapts Contents/Sessions/Kernels REST plus the kernel channels
WebSocket. In browser-local mode, TypeScript hosts the pinned WASM kernel Worker and
IndexedDB workspace behind the same transport interface.

The command path is intentionally singular: WebMCP cannot forward arbitrary MCP or
Jupyter calls, and UI code cannot bypass protocol validation. See
[Frontend tools](docs/frontend-tools.md), [Microscopes](docs/microscope.md), and
[Collaboration](docs/collaboration.md).

## Verification

Run the complete local suite:

```bash
scripts/check.sh
```

Focused checks:

```bash
cargo test --workspace --all-features
pnpm test
uv run pytest -q
pnpm test:browser-kernel
DIDACTION_GATEWAY_IMPLEMENTATION=rust scripts/smoke.sh
```

Pull requests run fast Rust, TypeScript, and Python checks first. Browser-local and
real-IPython acceptance run as separate CI jobs so failures identify the affected
runtime. See [Contributing](CONTRIBUTING.md).

## Security warning

Notebook execution is arbitrary code execution. This project is a single-user
local development and learning runtime—not a sandbox or authenticated remote
multi-user service. Run only trusted notebooks and kernels.

Services bind to loopback by default. Workspace paths are confined and credentials
remain server-side, but trusted kernel code still has that kernel process's file and
network permissions. WebMCP rejects shell/package-install magics and exposes no
generic forwarding method. HTML/JavaScript output, widgets/comms, terminals, and
notebook trust/signing remain unavailable. See [Security policy](SECURITY.md).

## Current limitations

- One driver per notebook per gateway process; external Jupyter editors can still
  conflict, and this is not authenticated remote collaboration.
- Browser-local Python filesystem changes made inside a Worker are temporary unless
  explicitly saved through the workspace APIs.
- Math uses a bounded local LaTeX subset. Arbitrary HTML is reduced to safe readable
  text/table output.
- Structural undo does not restore cleared kernel outputs; rerun the cell instead.
- Cell collapse and line-number preferences are browser-session state rather than
  notebook metadata.
- No ipywidgets/comms, debugger, terminal, nbextensions, trust UI, slideshow tools,
  or full JupyterLab extension/workbench compatibility.
- WebMCP and xeus-python browser support remain experimental.

## Documentation

Start with the [documentation index](docs/README.md). It separates current user and
contributor documentation from historical design investigations.

## License

Copyright 2026 didaction. Original project material is licensed under the
[Apache License, Version 2.0](LICENSE). See [NOTICE](NOTICE) and the
[third-party distribution inventory](THIRD_PARTY_LICENSES.md). Imported notebooks,
datasets, fonts, runtimes, and other third-party material retain their own terms.
