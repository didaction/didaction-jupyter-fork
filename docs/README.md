# Documentation

This index distinguishes current product contracts from historical investigations.
Use current documentation for setup and implementation decisions; investigations
record evidence and past design reasoning and may describe superseded behavior.

## Use and operate

- [Browser-local runtime](browser-runtime.md): static build, workspace persistence,
  Pyodide/xeus kernels, ZIP import/export, and hosting.
- [Container deployment](container-deployment.md): packaged server runtime, custom
  images, attach mode, configuration, and secrets.
- [Julia course runtime](julia-course.md): pinned Julia environment and smoke test.
- [Collaboration](collaboration.md): driver, observer, follow, and handoff semantics.

## Current product and contributor contracts

- [Frontend parity](frontend-parity.md): maintained capability status and deferrals.
- [Frontend tools](frontend-tools.md): WebMCP tools, bounds, and shared command path.
- [Microscopes](microscope.md): ownership, walkthroughs, annotations, and lifecycle.
- [Walkthrough graphics](walkthrough-graphics.md): AssemblyScript RGBA regions,
  compositing, worker safety, and capture.
- [Playground UI](playground-ui.md): temporary isolated experiment windows.
- [Jupyter frontend parity reference](jupyter-frontend-parity-reference.md): upstream
  behavior used to evaluate familiar notebook ergonomics.

## Historical investigations

Files under [`investigations/`](investigations/) are design records, compatibility
research, or migration notes. They are not current setup instructions:

- [Direct Jupyter protocol](investigations/direct-jupyter-protocol.md)
- [JupyterLite browser runtime](investigations/jupyterlite-browser-runtime.md)
- [`jupyterlite-web-mcp` design review](investigations/jupyterlite-web-mcp.md)
- [xeus-python browser runtime](investigations/xeus-python-browser.md)
- [Qrisp WASM compatibility](investigations/qrisp-wasm-compatibility.md)
- [Rust gateway migration](investigations/rust-runtime-migration.md)
