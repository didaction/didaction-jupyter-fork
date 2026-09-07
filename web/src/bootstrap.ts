import init, {
  mountNotebook,
  NotebookApplication,
  wasmBuildInfo,
  validateWalkthroughFocus,
} from "../pkg/notebook_wasm";
import { CallHistory, installDiagnostics } from "./diagnostics";
import { GraphicsController } from "./graphics";
import {
  CommandGateway,
  createQueuedNotebookDispatcher,
} from "./command-gateway";
import { GatewayNotebookTransport } from "./gateway-client";
import type { NotebookCommand, NotebookSnapshot } from "./types";
import { installWebMcp } from "./webmcp";
import { NotebookTools, type Transaction } from "./notebook-tools";
import { installExplorer } from "./explorer";
import { HttpArtifactTransport } from "./artifacts";
import { WorkspaceTools, type OpenNotebook } from "./workspace-tools";
import { NotebookCollaboration } from "./collaboration";
import { FollowController } from "./follow";
import { PlaygroundController } from "./playground";
import type {
  BrowserWorkspace,
  LocalNotebookConnection,
} from "./browser-workspace";

let browserWorkspace: BrowserWorkspace | undefined;

interface NotebookContext extends OpenNotebook {
  tickPlayground(following: boolean): void;
  followMicroscope(target: unknown, current: () => boolean): Promise<void>;
  connection: NotebookCollaboration | LocalNotebookConnection;
  scrollFraction(): number;
  followSelection(cellId: string | null): void;
  followScroll(fraction: number | null): void;
  isActive(): boolean;
  hostStatus(following: boolean, text: string): void;
  takeFollowToggle(): boolean;
  takeDiagnosticsToggle(): boolean;
}
const openContexts = new Set<NotebookContext>();

const boundedPng = (
  source: HTMLCanvasElement,
  maxBase64Bytes = 750_000,
): { data: string; width: number; height: number; downscaled: boolean } => {
  let canvas = source;
  let data = canvas.toDataURL("image/png").split(",")[1] ?? "";
  let downscaled = false;
  while (
    data.length > maxBase64Bytes &&
    (canvas.width > 128 || canvas.height > 128)
  ) {
    const ratio = Math.min(
      0.85,
      Math.max(0.35, Math.sqrt(maxBase64Bytes / data.length) * 0.9),
    );
    const next = document.createElement("canvas");
    next.width = Math.max(128, Math.floor(canvas.width * ratio));
    next.height = Math.max(128, Math.floor(canvas.height * ratio));
    const context = next.getContext("2d");
    if (!context) throw new Error("PNG capture canvas is unavailable");
    context.drawImage(canvas, 0, 0, next.width, next.height);
    canvas = next;
    data = canvas.toDataURL("image/png").split(",")[1] ?? "";
    downscaled = true;
  }
  if (!data || data.length > maxBase64Bytes)
    throw new Error("Microscope capture cannot fit the image transport limit");
  return { data, width: canvas.width, height: canvas.height, downscaled };
};

const status = document.querySelector<HTMLOutputElement>("#connection-status")!;
const fatal = document.querySelector<HTMLElement>("#fatal-error")!;
const command = (
  type: string,
  values: Record<string, unknown> = {},
): NotebookCommand => ({
  protocol_version: 1,
  command_id: crypto.randomUUID(),
  idempotency_key: crypto.randomUUID(),
  expected_revision: null,
  timeout_ms: 30_000,
  type,
  ...values,
});

async function createContext(
  startup: { path: string; kernel: string },
  create = false,
): Promise<NotebookContext> {
  const notebookLease = browserWorkspace
    ? await browserWorkspace.acquire(startup.path)
    : undefined;
  const collaboration =
    import.meta.env.VITE_NOTEBOOK_RUNTIME === "browser"
      ? new (await import("./browser-workspace")).LocalNotebookConnection(
          startup.path,
        )
      : new NotebookCollaboration(startup.path, (path) => {
          const connection = [...openContexts].find(
            (context) => context.path?.() === path,
          )?.connection;
          return connection instanceof NotebookCollaboration
            ? connection
            : undefined;
        });
  await collaboration.join();
  let readOnly = !collaboration.state?.is_driver;
  const transport =
    browserWorkspace?.transport(startup.path) ??
    new GatewayNotebookTransport(
      "/api/v1/commands",
      startup.path,
      () => collaboration.headers(),
      (path) => collaboration.rename(path),
    );
  const inFlight = collaboration.state?.snapshot;
  const setup =
    (inFlight?.kernel as { state?: string } | undefined)?.state === "busy"
      ? { snapshot: inFlight, error: null }
      : await transport.setup(
          command("setup", {
            path: startup.path,
            kernel: startup.kernel,
            create: create && !readOnly,
          }),
        );
  if (setup.error || !setup.snapshot) {
    await collaboration.close();
    notebookLease?.release();
    throw new Error(
      setup.error?.message ?? "Gateway returned no notebook snapshot",
    );
  }
  const snapshot = setup.snapshot as NotebookSnapshot;
  const wasm = new NotebookApplication(JSON.stringify(snapshot));
  const gateway = new CommandGateway(wasm, transport);
  let mounted: Awaited<ReturnType<typeof mountNotebook>> | undefined;
  let graphics: GraphicsController | undefined;
  let collapsedExplorerForMicroscope = false;
  const collapseExplorerForMicroscope = () => {
    if (!mounted || collapsedExplorerForMicroscope) return;
    const active = JSON.parse(mounted.activeContext());
    const explorer = document.querySelector<HTMLElement>("#file-explorer")!;
    if (active.microscope && !explorer.hidden) {
      document.querySelector<HTMLButtonElement>("#explorer-toggle")!.click();
      collapsedExplorerForMicroscope = true;
    } else if (!active.microscope) {
      collapsedExplorerForMicroscope = false;
    }
  };
  const motionPreference = matchMedia("(prefers-reduced-motion: reduce)");
  const syncMotion = () => mounted?.setReducedMotion(motionPreference.matches);
  const dispatchEguiCommand = createQueuedNotebookDispatcher(
    gateway,
    () =>
      Number(
        (JSON.parse(wasm.publicSnapshot()) as { snapshot: NotebookSnapshot })
          .snapshot.revision,
      ),
    (command) => {
      if (
        ["create_microscope", "delete_microscope", "rename_notebook"].includes(
          command.type,
        )
      )
        window.dispatchEvent(new Event("workspace-files-changed"));
      if (command.type !== "rename_notebook") return;
      const path = JSON.parse(wasm.publicSnapshot()).snapshot.notebook
        .path as string;
      if (mounted) {
        const url = new URL(location.href);
        url.searchParams.set("notebook", path);
        history.replaceState(null, "", url);
      }
      if (browserWorkspace) collaboration.rename(path);
    },
  );
  let incomingSnapshot: NotebookSnapshot | undefined;
  let reconciliationQueued = false;
  void collaboration.watch(
    (state) => {
      // Own commands reconcile through their existing result path. Observers use
      // validated full snapshots, including clear_output and display replacements.
      if (
        state.snapshot &&
        state.origin !== state.client_id &&
        (readOnly || !state.is_driver)
      ) {
        incomingSnapshot = state.snapshot;
        if (!reconciliationQueued) {
          reconciliationQueued = true;
          void dispatchEguiCommand
            .transaction(async () => {
              const incoming = incomingSnapshot!;
              incomingSnapshot = undefined;
              const current = JSON.parse(wasm.publicSnapshot())
                .snapshot as NotebookSnapshot;
              if (incoming.revision >= current.revision) {
                wasm.replaceSnapshot(JSON.stringify(incoming));
                mounted?.applyExternalResult(
                  JSON.stringify({
                    protocol_version: 1,
                    command_id: crypto.randomUUID(),
                    idempotency_key: crypto.randomUUID(),
                    base_revision: null,
                    committed_revision: incoming.revision,
                    snapshot: incoming,
                    error: null,
                  }),
                  false,
                );
              }
            })
            .catch(() => {
              readOnly = true;
              mounted?.setReadOnly(true);
            })
            .finally(() => {
              reconciliationQueued = false;
            });
        }
      }
      readOnly = !state.is_driver;
      mounted?.setReadOnly(readOnly);
      if (mounted)
        status.textContent =
          document.documentElement.dataset.webmcp === "available"
            ? "Connected · WebMCP ready"
            : "Connected · WebMCP unavailable";
    },
    () => {
      readOnly = true;
      mounted?.setReadOnly(true);
      if (mounted) status.textContent = "Reconnecting · Read-only";
    },
  );
  const syncWorkspaceVisibility = () =>
    mounted?.setWorkspaceVisible(
      !document.querySelector<HTMLElement>("#file-explorer")!.hidden,
    );
  window.addEventListener("workspace-visibility", syncWorkspaceVisibility);
  const externalExecute = async (serialized: string) => {
    try {
      const result = await gateway.execute(serialized, (progress) =>
        mounted?.applyExternalResult(JSON.stringify(progress), true),
      );
      mounted?.applyExternalResult(result, false);
      if (
        !JSON.parse(result).error &&
        ["create_microscope", "delete_microscope"].includes(
          JSON.parse(serialized).type,
        )
      )
        window.dispatchEvent(new Event("workspace-files-changed"));
      return result;
    } catch (error) {
      const command = JSON.parse(serialized) as NotebookCommand;
      mounted?.applyExternalResult(
        JSON.stringify({
          protocol_version: 1,
          command_id: command.command_id,
          idempotency_key: command.idempotency_key,
          base_revision: null,
          committed_revision: null,
          snapshot: null,
          error: {
            code: "transport_error",
            message:
              "Tool command failed; reconnect and inspect notebook before retrying",
            retryable: true,
          },
        }),
        false,
      );
      throw error;
    }
  };
  const transaction: Transaction = (task) =>
    dispatchEguiCommand.transaction(async () => {
      mounted?.assertExternalReady();
      mounted?.setExternalBusy(true);
      try {
        return await task(externalExecute);
      } finally {
        mounted?.setExternalBusy(false);
      }
    });
  const interruptExecute = async (serialized: string) => {
    // Out-of-band interruption cannot advance a running command's validator revision.
    const validation = new NotebookApplication(
      JSON.stringify(
        (JSON.parse(wasm.publicSnapshot()) as { snapshot: NotebookSnapshot })
          .snapshot,
      ),
    );
    try {
      return await new CommandGateway(validation, transport).execute(
        serialized,
      );
    } finally {
      validation.dispose();
    }
  };
  const playground = new PlaygroundController({
    stopFollowing: () =>
      document.querySelector<HTMLButtonElement>("#follow-driver")?.click(),
    valid: (cellId, id, revision) => {
      const active = mounted && JSON.parse(mounted.activeContext()).microscope;
      const saved = JSON.parse(wasm.publicSnapshot())
        .snapshot.cells.find((cell: { id: string }) => cell.id === cellId)
        ?.metadata.didaction_microscopes?.items.find(
          (item: { id: string }) => item.id === id,
        );
      return (
        active?.cell_id === cellId &&
        active?.microscope_id === id &&
        saved &&
        (saved.revision ?? 0) === revision
      );
    },
    path: () => startup.path,
    headers: () => collaboration.headers(),
    canWrite: () => !readOnly,
    document: (cellId, microscopeId) =>
      transaction(async (execute) => {
        const result = JSON.parse(
          await execute(
            JSON.stringify(
              command("read_microscope", {
                cell_id: cellId,
                microscope_id: microscopeId,
              }),
            ),
          ),
        );
        if (result.error || !result.microscope)
          throw new Error(result.error?.message ?? "Microscope unavailable");
        return result.microscope;
      }),
    enter: (doc, index) => {
      mounted?.showMicroscope(JSON.stringify(doc));
      mounted?.focusWalkthrough(
        JSON.stringify({ step_index: index, annotation_id: null }),
      );
    },
  });
  const playgroundTool = async (
    name: string,
    args: Record<string, unknown>,
  ) => {
    if (!mounted) throw new Error("Open this notebook first");
    if (name === "open_playground")
      await playground.open(
        String(args.cell_id),
        String(args.microscope_id),
        Number(args.step_index),
      );
    if (name === "close_playground") await playground.close();
    if (name === "execute_playground")
      await playground.execute(args.source as string | undefined);
    const value = { ok: true, snapshot: playground.snapshot() };
    return {
      content: [{ type: "text" as const, text: JSON.stringify(value) }],
      structuredContent: value,
      isError: false,
    };
  };
  const tools = new NotebookTools(
    transaction,
    () =>
      (JSON.parse(wasm.publicSnapshot()) as { snapshot: NotebookSnapshot })
        .snapshot,
    () => {},
    interruptExecute,
    (name, args) =>
      [
        "open_playground",
        "close_playground",
        "read_playground",
        "execute_playground",
      ].includes(name)
        ? playgroundTool(name, args)
        : dispatchEguiCommand.transaction(async () => {
            if (!mounted)
              throw new Error(
                "Select this notebook with open_notebook before using view tools",
              );
            const id = args.cell_id as string;
            if (
              [
                "open_microscope",
                "close_microscope",
                "focus_microscope_step",
                "focus_microscope_annotation",
                "clear_microscope_focus",
              ].includes(name)
            ) {
              if (name === "close_microscope") mounted.showMicroscope("null");
              else if (name === "clear_microscope_focus") {
                const active = JSON.parse(mounted.activeContext());
                if (
                  active.microscope?.cell_id !== id ||
                  active.microscope?.microscope_id !== args.microscope_id ||
                  !active.microscope?.walkthrough
                )
                  throw new Error(
                    "Open this microscope's walkthrough before clearing focus",
                  );
                mounted.focusWalkthrough(
                  JSON.stringify({
                    step_index: active.microscope.walkthrough.step_index,
                    annotation_id: null,
                  }),
                );
              } else {
                mounted.assertExternalReady();
                const response = JSON.parse(
                  await externalExecute(
                    JSON.stringify(
                      command("read_microscope", {
                        cell_id: id,
                        microscope_id: args.microscope_id,
                        expected_revision: JSON.parse(wasm.publicSnapshot())
                          .snapshot.revision,
                      }),
                    ),
                  ),
                );
                if (response.error || !response.microscope)
                  throw new Error(
                    response.error?.message ?? "Microscope could not be loaded",
                  );
                const focus = name.startsWith("focus_microscope_")
                  ? JSON.stringify({
                      step_index: args.step_index,
                      annotation_id: args.annotation_id ?? null,
                    })
                  : undefined;
                if (focus)
                  validateWalkthroughFocus(
                    JSON.stringify(response.microscope),
                    focus,
                  );
                mounted.showMicroscope(JSON.stringify(response.microscope));
                if (focus) mounted.focusWalkthrough(focus);
              }
              const result = {
                ok: true,
                view: JSON.parse(mounted.activeContext()),
              };
              return {
                content: [
                  { type: "text" as const, text: JSON.stringify(result) },
                ],
                structuredContent: result,
                isError: false,
              };
            }
            if (!["capture_cell", "capture_microscope_step"].includes(name)) {
              syncMotion();
              mounted.cellView(
                id,
                name === "highlight_cell"
                  ? "highlight"
                  : name === "clear_cell_highlight"
                    ? "clear_highlight"
                    : name === "set_cell_visibility"
                      ? "cell"
                      : "output",
                name === "highlight_cell"
                  ? String(args.color ?? "blue")
                  : name === "clear_cell_highlight"
                    ? ""
                    : String(
                        name === "set_cell_visibility"
                          ? args.collapsed
                          : args.mode,
                      ),
              );
              const result = { ok: true, cell_id: id, ...args };
              return {
                content: [
                  { type: "text" as const, text: JSON.stringify(result) },
                ],
                structuredContent: result,
                isError: false,
              };
            }
            let captureCellId = id;
            let captureMicroscopeId = args.microscope_id as string | undefined;
            if (name === "capture_microscope_step") {
              const active = JSON.parse(mounted.activeContext());
              if (!active.microscope?.loaded)
                throw new Error("Open a microscope before capturing it");
              captureCellId = active.microscope.cell_id;
              captureMicroscopeId = active.microscope.microscope_id;
              mounted.captureMicroscopeStep();
            } else {
              mounted.cellView(id, "capture", "");
            }
            const deadline = performance.now() + 10000;
            while (performance.now() < deadline) {
              await new Promise((resolve) => setTimeout(resolve, 50));
              const raw = mounted.takeCellCapture();
              if (!raw) continue;
              const capture = JSON.parse(raw) as {
                width: number;
                height: number;
                rgba: string;
                clipped: boolean;
              };
              const canvas = document.createElement("canvas");
              canvas.width = capture.width;
              canvas.height = capture.height;
              const pixels = Uint8ClampedArray.from(
                atob(capture.rgba),
                (byte) => byte.charCodeAt(0),
              );
              canvas
                .getContext("2d")!
                .putImageData(
                  new ImageData(pixels, capture.width, capture.height),
                  0,
                  0,
                );
              const encoded = boundedPng(canvas);
              const result = {
                ok: true,
                cell_id: captureCellId,
                ...(name === "capture_microscope_step"
                  ? { microscope_id: captureMicroscopeId }
                  : {}),
                width: encoded.width,
                height: encoded.height,
                source_width: capture.width,
                source_height: capture.height,
                downscaled: encoded.downscaled,
                clipped: capture.clipped,
              };
              return {
                content: [
                  { type: "text" as const, text: JSON.stringify(result) },
                  {
                    type: "image" as const,
                    mimeType: "image/png" as const,
                    data: encoded.data,
                  },
                ],
                structuredContent: result,
                isError: false,
              };
            }
            throw new Error(
              "Cell capture timed out; keep the notebook tab visible",
            );
          }),
  );
  const deactivate = () => {
    graphics?.dispose();
    graphics = undefined;
    if (!mounted) return;
    void playground.close().catch(() => playground.dispose());
    motionPreference.removeEventListener("change", syncMotion);
    mounted.dispose();
    mounted = undefined;
    const old = document.querySelector<HTMLCanvasElement>("#notebook-canvas")!;
    const fresh = old.cloneNode(false) as HTMLCanvasElement;
    old.replaceWith(fresh);
    document.querySelector<HTMLElement>("#notebook-shell")!.hidden = true;
  };
  const context: NotebookContext = {
    tickPlayground: (following) => {
      collapseExplorerForMicroscope();
      playground.setFollowing(following && !!mounted);
      const index = mounted?.takePlaygroundRequest();
      if (index !== undefined && index !== null) {
        const target = JSON.parse(mounted!.activeContext()).microscope;
        if (target)
          void playground
            .open(target.cell_id, target.microscope_id, index)
            .catch((error) => {
              status.textContent = String(error);
            });
      }
    },
    followMicroscope: async (target, current) => {
      const requested = target as {
        cell_id: string;
        microscope_id: string;
        focus?: { step_index: number; annotation_id?: string | null };
        revision?: number;
      } | null;
      if (!mounted || !current()) return;
      const active = JSON.parse(mounted.activeContext()).microscope;
      if (!requested) {
        if (!active) return;
        mounted.showMicroscope("null");
        return;
      }
      if (
        active?.cell_id === requested.cell_id &&
        active?.microscope_id === requested.microscope_id &&
        (active?.revision ?? 0) === (requested.revision ?? 0) &&
        active.loaded
      ) {
        if (requested.focus)
          mounted.focusWalkthrough(JSON.stringify(requested.focus));
        return;
      }
      // View events can arrive before the notebook snapshot announcing creation.
      // Refresh through the command queue before resolving the referenced document.
      await transaction(async (execute) => {
        if (!current() || !mounted) return;
        await execute(JSON.stringify(command("query", { query: "full" })));
        if (!current() || !mounted) return;
        const response = JSON.parse(
          await execute(
            JSON.stringify(
              command("read_microscope", {
                cell_id: requested.cell_id,
                microscope_id: requested.microscope_id,
              }),
            ),
          ),
        );
        if (response.error || !response.microscope)
          throw new Error("Microscope unavailable");
        if (current() && mounted) {
          mounted.showMicroscope(JSON.stringify(response.microscope));
          if (requested.focus)
            mounted.focusWalkthrough(JSON.stringify(requested.focus));
        }
      });
    },
    tools,
    connection: collaboration,
    isActive: () => mounted !== undefined,
    hostStatus: (following, text) => mounted?.setHostStatus(following, text),
    takeFollowToggle: () => mounted?.takeFollowToggle() ?? false,
    takeDiagnosticsToggle: () => mounted?.takeDiagnosticsToggle() ?? false,
    scrollFraction: () => mounted?.scrollFraction() ?? 0,
    followSelection: (cellId) => mounted?.setFollowSelection(cellId),
    followScroll: (fraction) => mounted?.setFollowScroll(fraction),
    canWrite: () => !readOnly,
    collaboration: () => ({
      client_id: collaboration.state?.client_id ?? null,
      driver_id: collaboration.state?.driver_id ?? null,
      is_driver: !readOnly,
      clients: collaboration.state?.clients ?? [],
    }),
    changeDriver: (clientId) => collaboration.changeDriver(clientId),
    activeContext: () => {
      if (!mounted) return null;
      const context = JSON.parse(mounted.activeContext()) as Record<
        string,
        unknown
      >;
      const activePlayground = playground.activeContext();
      return activePlayground
        ? {
            ...context,
            view: "playground",
            selection: null,
            playground: activePlayground,
          }
        : context;
    },
    path: () =>
      JSON.parse(wasm.publicSnapshot()).snapshot.notebook.path as string,
    ready: () => mounted?.assertExternalReady(),
    activate: async () => {
      document.querySelector<HTMLElement>("#notebook-shell")!.hidden = false;
      mounted = await mountNotebook(
        "notebook-canvas",
        JSON.stringify(JSON.parse(wasm.publicSnapshot()).snapshot),
        dispatchEguiCommand,
        () => {
          document
            .querySelector<HTMLButtonElement>("#explorer-toggle")!
            .click();
          return !document.querySelector<HTMLElement>("#file-explorer")!.hidden;
        },
      );
      mounted.setReadOnly(readOnly);
      mounted.setCheckpointsSupported(!browserWorkspace);
      graphics = new GraphicsController(
        mounted,
        () =>
          !document.hidden &&
          document.querySelector<HTMLCanvasElement>("#notebook-canvas")?.style
            .visibility !== "hidden",
      );
      syncMotion();
      motionPreference.addEventListener("change", syncMotion);
      const url = new URL(location.href);
      syncWorkspaceVisibility();
      const activePath = JSON.parse(wasm.publicSnapshot()).snapshot.notebook
        .path as string;
      url.searchParams.set("notebook", activePath);
      history.replaceState(null, "", url);
      document
        .querySelectorAll<HTMLElement>("#notebook-files button")
        .forEach((button) => {
          if (button.title === activePath)
            button.setAttribute("aria-current", "page");
          else button.removeAttribute("aria-current");
        });
    },
    deactivate,
    dispose: () => {
      playground.dispose();
      openContexts.delete(context);
      window.removeEventListener(
        "workspace-visibility",
        syncWorkspaceVisibility,
      );
      deactivate();
      wasm.dispose();
      void transport.close();
      void collaboration.close();
      notebookLease?.release();
    },
  };
  openContexts.add(context);
  return context;
}

async function boot(): Promise<void> {
  await init();
  const callHistory = new CallHistory();
  const diagnostics = installDiagnostics(
    callHistory,
    JSON.parse(wasmBuildInfo()),
  );
  let startup: { path: string; kernel: string };
  let ownerLivenessListener: ((event: Event) => void) | undefined;
  if (import.meta.env.VITE_NOTEBOOK_RUNTIME === "browser") {
    const { BrowserWorkspace } = await import("./browser-workspace");
    browserWorkspace = new BrowserWorkspace();
    const { chooseBrowserWorkspace } = await import("./browser-launch");
    const chosen = await chooseBrowserWorkspace(
      browserWorkspace,
      new URL(location.href).searchParams.get("notebook"),
      new URL(location.href).searchParams.get("kernel"),
    );
    const browserUrl = new URL(location.href);
    const { isBrowserKernelName } = await import("./browser-kernel-profile");
    if (!isBrowserKernelName(chosen.kernel))
      throw new Error("Unsupported browser kernel");
    browserWorkspace.kernelName = chosen.kernel;
    browserUrl.searchParams.set("notebook", chosen.path);
    browserUrl.searchParams.set("kernel", chosen.kernel);
    browserUrl.searchParams.set("workspace", chosen.workspace);
    browserUrl.searchParams.delete("runtime");
    history.replaceState(null, "", browserUrl);
    startup = chosen;
    const owner = document.querySelector<HTMLOutputElement>("#driver-status")!;
    owner.textContent = "Owner · live";
    owner.title =
      "This tab owns the selected notebook and announces liveness every 30 seconds.";
    ownerLivenessListener = (event) => {
      const state = (event as CustomEvent<{ heartbeat_at: string }>).detail;
      owner.textContent = "Owner · live";
      owner.title = `This tab owns the selected notebook. Last liveness announcement: ${state.heartbeat_at}`;
    };
    window.addEventListener("browser-notebook-liveness", ownerLivenessListener);
  } else {
    const response = await fetch("/api/v1/config");
    if (!response.ok) throw new Error("Gateway configuration unavailable");
    const configuration = (await response.json()) as {
      path: string;
      kernel: string;
      workspace_label?: string;
    };
    startup = configuration;
    const workspaceSource =
      document.querySelector<HTMLOutputElement>("#workspace-source")!;
    if (configuration.workspace_label) {
      workspaceSource.hidden = false;
      workspaceSource.textContent = `Workspace: ${configuration.workspace_label}`;
      workspaceSource.title = `Notebook files are sourced from ${configuration.workspace_label}`;
    }
  }
  const selected = new URL(location.href).searchParams.get("notebook");
  if (selected) startup.path = selected;
  const initial = await createContext(startup, !selected);
  const workspace = new WorkspaceTools(
    initial.tools.listTools(),
    (path) => createContext({ path, kernel: startup.kernel }),
    async (directory) => {
      if (browserWorkspace) return browserWorkspace.store.list(directory);
      const response = await fetch(
        `/api/v1/notebooks?directory=${encodeURIComponent(directory)}`,
      );
      if (!response.ok) throw new Error("Folder unavailable");
      return response.json();
    },
  );
  await workspace.seed(startup.path, initial);
  const followButton =
    document.querySelector<HTMLButtonElement>("#follow-driver")!;
  const followStatus =
    document.querySelector<HTMLOutputElement>("#follow-status")!;
  const driverStatus =
    document.querySelector<HTMLOutputElement>("#driver-status")!;
  const activeContext = () =>
    [...openContexts].find((context) => context.isActive());
  const homeButton =
    document.querySelector<HTMLButtonElement>("#browser-home")!;
  homeButton.hidden = !browserWorkspace;
  homeButton.onclick = () => {
    try {
      for (const context of openContexts) context.ready();
      if (
        !confirm(
          "Return to the workspace chooser? Saved notebooks and files remain. All live kernel variables and temporary playgrounds in this tab will be discarded.",
        )
      )
        return;
      workspace.dispose();
      browserWorkspace?.close();
      if (ownerLivenessListener)
        window.removeEventListener(
          "browser-notebook-liveness",
          ownerLivenessListener,
        );
      location.assign(location.pathname);
    } catch (error) {
      followStatus.textContent =
        error instanceof Error
          ? error.message
          : "Save edits and wait for execution before leaving.";
    }
  };
  const permissionButton =
    document.querySelector<HTMLButtonElement>("#driver-permission")!;
  let changingPermission = false;
  permissionButton.onclick = async () => {
    const active = activeContext();
    if (!active || changingPermission) return;
    if (
      !active.canWrite?.() &&
      active.collaboration?.().driver_id !== null &&
      !confirm(
        "Take workspace control from the current driver? Their notebook becomes read-only. Active execution must finish first.",
      )
    )
      return;
    try {
      for (const context of openContexts) context.ready();
      changingPermission = true;
      permissionButton.disabled = true;
      await active.connection.setDriverPermission(
        active.canWrite?.() ? "release" : "claim",
      );
      followStatus.textContent = "";
    } catch (error) {
      followStatus.textContent =
        error instanceof Error
          ? error.message
          : "Control change failed; retry.";
    } finally {
      changingPermission = false;
    }
  };
  let anchor: NotebookContext | undefined;
  const follow = new FollowController(
    async (view, current) => {
      const allowed = () =>
        current() &&
        !!anchor &&
        !anchor.canWrite?.() &&
        anchor.collaboration?.().driver_id === view.driver_id;
      if (!allowed()) return;
      if (!(await workspace.openForFollow(view.notebook_path, allowed)))
        throw new Error("Follow target cannot be selected");
      if (allowed()) {
        await activeContext()?.followMicroscope(
          view.microscope ?? null,
          allowed,
        );
        if (!allowed()) return;
        activeContext()?.followScroll(view.scroll_fraction);
        if (view.selected_cell_id !== undefined)
          activeContext()?.followSelection(view.selected_cell_id);
        followStatus.textContent = "";
        followButton.title = `Following ${view.notebook_path}. Click to browse independently.`;
      }
    },
    () => {
      for (const context of openContexts) context.followScroll(null);
    },
    () => {
      followStatus.textContent = "Follow paused · waiting for driver";
    },
  );
  const updateFollowButton = () => {
    const isDriver = !!activeContext()?.canWrite?.();
    const active = activeContext();
    const notebookName = document.querySelector<HTMLElement>("#notebook-name")!;
    const path = active?.path?.() ?? "";
    notebookName.textContent = path;
    notebookName.title = path;
    permissionButton.hidden = !!browserWorkspace || !active;
    permissionButton.textContent = isDriver
      ? "Release driver"
      : active?.collaboration?.().driver_id
        ? "Take control"
        : "Claim driver";
    permissionButton.title = isDriver
      ? "Let another collaborator claim workspace control"
      : active?.collaboration?.().driver_id
        ? "Take workspace control; the current driver becomes read-only"
        : "Claim the vacant workspace driver role";
    permissionButton.disabled = changingPermission;
    followButton.hidden = isDriver;
    driverStatus.hidden = !isDriver;
    followButton.disabled = !follow.enabled && (!activeContext() || isDriver);
    followButton.setAttribute("aria-pressed", String(follow.enabled));
    followButton.textContent = follow.enabled
      ? "Stop following"
      : "Follow driver";
    if (!follow.enabled)
      followButton.title =
        "Opt in to the driver's notebook and scroll position";
  };
  followButton.onclick = () => {
    if (follow.enabled) {
      follow.stop();
      anchor = undefined;
      followStatus.textContent = "";
    } else {
      anchor = activeContext();
      if (!anchor || anchor.canWrite?.()) return;
      follow.start(anchor.connection);
      followStatus.textContent = "Waiting for driver’s view…";
    }
    updateFollowButton();
  };
  const followTimer = window.setInterval(() => {
    if (anchor && (!openContexts.has(anchor) || anchor.canWrite?.())) {
      follow.stop();
      anchor = undefined;
      followStatus.textContent = "";
    }
    updateFollowButton();
    const active = activeContext();
    for (const context of openContexts)
      context.tickPlayground(context === active && follow.enabled);
    if (active?.takeDiagnosticsToggle()) diagnostics.toggle();
    if (active?.takeFollowToggle()) followButton.click();
    active?.hostStatus(follow.enabled, status.textContent ?? "Connecting…");
    if (!active?.canWrite?.()) return;
    const view = active.activeContext?.();
    const selectedCell = (view?.selection as { cell_id?: string } | null)
      ?.cell_id;
    const microscopeView = view?.microscope as
      | (import("./follow").MicroscopeTarget & Record<string, unknown>)
      | null;
    const microscope = microscopeView
      ? {
          cell_id: microscopeView.cell_id,
          microscope_id: microscopeView.microscope_id,
          revision: microscopeView.revision,
          focus: microscopeView.focus,
        }
      : null;
    for (const context of openContexts) {
      if (context.canWrite?.()) {
        void context.connection.publish({
          protocol_version: 1,
          notebook_path: active.path!(),
          scroll_fraction: active.scrollFraction(),
          selected_cell_id: selectedCell ?? null,
          microscope,
        });
      }
    }
  }, 250);
  updateFollowButton();
  installExplorer(
    startup.path,
    () => {
      activeContext()?.ready?.();
    },
    async (path) => {
      const result = await workspace.callTool("open_notebook", {
        notebook_path: path,
      });
      if (result.isError) throw new Error("Unable to open notebook");
    },
    browserWorkspace ? (path) => browserWorkspace!.store.list(path) : undefined,
    browserWorkspace
      ? browserWorkspace.artifacts
      : new HttpArtifactTransport(
          () => activeContext()?.connection.headers() ?? {},
        ),
    () => activeContext()?.canWrite?.() ?? false,
    async () => {
      for (const context of openContexts) context.ready?.();
      if (browserWorkspace) return browserWorkspace.store.exportEntries();
      const response = await fetch("/api/v1/workspace-export", {
        signal: AbortSignal.timeout(65000),
      });
      if (!response.ok)
        throw new Error(
          "Workspace export unavailable. Check the gateway and export limits, then retry.",
        );
      const result = await response.json();
      return result.entries.map(
        (entry: {
          path: string;
          directory: boolean;
          content_base64: string;
        }) => ({
          path: entry.path,
          directory: entry.directory,
          bytes: Uint8Array.from(atob(entry.content_base64), (c) =>
            c.charCodeAt(0),
          ),
        }),
      );
    },
  );
  const webmcp = await installWebMcp(workspace, undefined, callHistory);
  const hasWebMcp = webmcp.available;
  status.textContent = hasWebMcp
    ? "Connected · WebMCP ready"
    : "Connected · WebMCP unavailable";
  if (browserWorkspace)
    status.textContent = `Browser kernel · ${hasWebMcp ? "WebMCP ready" : "WebMCP unavailable"}`;
  document.documentElement.dataset.webmcp = hasWebMcp
    ? "available"
    : "unavailable";
  const shell = document.querySelector<HTMLElement>("#notebook-shell")!;
  const resizeObserver = new ResizeObserver(() => {
    requestAnimationFrame(() => window.dispatchEvent(new Event("resize")));
  });
  resizeObserver.observe(shell);
  window.addEventListener(
    "beforeunload",
    () => {
      clearInterval(followTimer);
      follow.stop();
      resizeObserver.disconnect();
      webmcp.dispose();
      diagnostics.dispose();
      workspace.dispose();
      browserWorkspace?.close();
      if (ownerLivenessListener)
        window.removeEventListener(
          "browser-notebook-liveness",
          ownerLivenessListener,
        );
    },
    { once: true },
  );
}

boot().catch((error: unknown) => {
  status.textContent = "Disconnected";
  const listingStatus = document.querySelector("#explorer-status");
  fatal.hidden = false;
  const title = document.querySelector<HTMLElement>("#fatal-title")!;
  const message = document.querySelector<HTMLElement>("#fatal-message")!;
  const liveness = document.querySelector<HTMLOutputElement>(
    "#notebook-lock-liveness",
  )!;
  const retry = document.querySelector<HTMLButtonElement>("#fatal-retry")!;
  const home = document.querySelector<HTMLButtonElement>("#fatal-home")!;
  retry.onclick = () => location.reload();
  const locked = error as {
    name?: string;
    path?: string;
    liveness?: { owner_id: string; heartbeat_at: string } | null;
  };
  if (locked?.name === "NotebookLockedError") {
    title.textContent = "Notebook in use";
    message.textContent = `${locked.path ?? "This notebook"} is open in another tab. Close it there, or choose a different notebook. If you just closed the owner tab, wait 2 seconds and try again.`;
    const heartbeat = locked.liveness?.heartbeat_at;
    if (listingStatus) {
      listingStatus.textContent = heartbeat
        ? `Open in another tab · owner active at ${new Date(heartbeat).toLocaleTimeString()}`
        : "Open in another tab";
    }
    liveness.hidden = false;
    liveness.textContent = heartbeat
      ? `Owner tab ${locked.liveness!.owner_id} active at ${new Date(heartbeat).toLocaleTimeString()} · ${heartbeat}`
      : "Another tab holds the notebook lock; its last heartbeat is unavailable.";
    retry.disabled = true;
    retry.textContent = "Try again in 2s";
    window.setTimeout(() => {
      retry.disabled = false;
      retry.textContent = "Try again";
    }, 2_000);
    home.hidden = false;
    home.onclick = () => location.assign(location.pathname);
  } else {
    if (listingStatus) listingStatus.textContent = "Notebook startup failed";
    message.textContent =
      error instanceof Error ? error.message : "Unknown startup failure";
  }
});
