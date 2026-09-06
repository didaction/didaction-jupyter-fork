# `jupyterlite-web-mcp` design review

This review considers ideas from
[`alliecatowo/jupyterlite-web-mcp`](https://github.com/alliecatowo/jupyterlite-web-mcp)
at commit `5e3af3a63141088a66614a61a332d9307d0ab87f`. It is an implementation
reference, not a dependency. The upstream project is MIT licensed; copying
substantial code would require preserving its copyright and license notice.

## Shared foundation

Both projects expose the live browser notebook through WebMCP and keep WebMCP
as a thin adapter over notebook operations. Both use the notebook currently in
the tab, the same in-browser kernel and bounded structured results. Didaction's
single command gateway and Rust reconciliation remain the correct authority;
adopting an upstream idea must not create a second mutation or execution path.

## Ideas worth adopting

1. **Cell-source compare-and-swap.** Reads return a deterministic source hash;
   replacement and deletion require that hash. This is more precise than a
   notebook-wide revision when a human and agent touch different cells. Add it
   to the protocol rather than implementing it only in WebMCP.
2. **Truthful activity and provenance.** Show an agent action only while a tool
   is actually running, retain a bounded action history without arguments or
   results, and identify agent-produced edits/outputs in the notebook UI. The
   existing redacted `CallHistory` is a useful base.
3. **Output selection with a fingerprint.** Let a person select rendered output
   text for an agent, but return it only with a bounded fingerprint so consumers
   can detect replacement by `clear_output` or `update_display_data`.
4. **Human-controlled access.** Notebook- and cell-level `write`, `read` and
   `none` controls are a strong fit for shared human/agent editing. Enforcement
   must live at the common command boundary so egui, WebMCP and future hosts
   cannot disagree.
5. **Reviewable proposals.** An optional human-controlled propose mode could
   stage a source edit with accept/deny controls. Acceptance must revalidate the
   expected source hash and then call the same normal mutation command.
6. **Anchored review threads.** Notebook metadata can store bounded comments
   attached to a cell, source range or output fingerprint. This is valuable but
   secondary to source-hash concurrency and access controls.

## Ideas not to copy directly

- Do not import the upstream 22-tool surface wholesale. Didaction already has a
  deliberately task-oriented surface for notebooks, microscopes and
  playgrounds; similar operations should be consolidated behind existing tools.
- Do not mutate a JupyterLab shared model from WebMCP handlers. Didaction's
  handlers must continue through `command-gateway.ts`, Rust validation and the
  selected transport.
- Do not infer agent presence from WebMCP availability. Registration proves only
  that tools are exposed; an in-flight invocation is the only reliable activity
  signal currently available to the page.
- Do not adopt JupyterLab DOM selectors as authority. UI markers may be cosmetic,
  but state and permissions belong in validated models and metadata.

## Suggested sequence

1. Add per-cell source hashes and conditional writes/deletes with conflict tests.
2. Surface bounded, redacted invocation activity and mutation provenance.
3. Add human-controlled access at the shared command boundary.
4. Prototype output selection and proposal review before expanding the public
   tool catalog.
5. Consider review threads only after their metadata lifecycle and export rules
   are specified.

Each slice needs native and browser-local tests proving that human egui actions
and WebMCP calls still share one validated command path.
