import { NotebookApplication } from "../pkg/notebook_wasm";
import { browserPath, IndexedNotebookStore } from "./browser-store";
import { initialBrowserSnapshot } from "./browser-transport";
import { OutputReducer } from "./browser-outputs";
import type { ArtifactRequest, ArtifactTransport } from "./artifacts";
import {
  ENTRY_LIMIT,
  ZIP_LIMIT,
  COUNT_LIMIT,
  readWorkspaceZip,
  type WorkspaceEntry,
} from "./workspace-zip";
import {
  DEFAULT_BROWSER_KERNEL,
  isBrowserKernelName,
  type BrowserKernelName,
} from "./browser-kernel-profile";

const DEMO_ARCHIVES = [
  "demos/didaction-runtime-tour.zip",
  "demos/ieee-9-bus-power-flow.zip",
] as const;

export const DEFAULT_DEMO_NOTEBOOK = "didaction-runtime-tour.ipynb";

export function kernelFromNotebookMetadata(
  metadata: Record<string, unknown>,
): BrowserKernelName {
  const kernelspec = metadata.kernelspec;
  if (!kernelspec || typeof kernelspec !== "object")
    return DEFAULT_BROWSER_KERNEL;
  const name = (kernelspec as { name?: unknown }).name;
  return typeof name === "string" && isBrowserKernelName(name)
    ? name
    : DEFAULT_BROWSER_KERNEL;
}

export function importNotebook(path: string, bytes: Uint8Array) {
  const raw = JSON.parse(
    new TextDecoder("utf-8", { fatal: true }).decode(bytes),
  );
  if (
    raw.nbformat !== 4 ||
    !Array.isArray(raw.cells) ||
    !raw.metadata ||
    typeof raw.metadata !== "object" ||
    Array.isArray(raw.metadata)
  )
    throw new Error("Expected an nbformat 4 notebook.");
  const snapshot = initialBrowserSnapshot(
    path,
    kernelFromNotebookMetadata(raw.metadata as Record<string, unknown>),
  );
  snapshot.cells = raw.cells.map((cell: Record<string, unknown>) => {
    if (
      Array.isArray(cell.source) &&
      cell.source.some((line) => typeof line !== "string")
    )
      throw new Error("Invalid notebook source");
    const reducer = new OutputReducer();
    if (cell.outputs !== undefined && !Array.isArray(cell.outputs))
      throw new Error("Invalid notebook outputs");
    for (const output of (cell.outputs ?? []) as Record<string, unknown>[]) {
      const bundle = structuredClone(output);
      if (Array.isArray(bundle.text)) bundle.text = bundle.text.join("");
      if (bundle.data && typeof bundle.data === "object")
        for (const [key, value] of Object.entries(bundle.data)) {
          if (Array.isArray(value))
            (bundle.data as Record<string, unknown>)[key] = value.join("");
        }
      reducer.apply({ type: String(output.output_type), bundle });
    }
    const metadata = structuredClone(cell.metadata ?? {}) as Record<
      string,
      unknown
    >;
    delete metadata.trusted;
    return {
      id: typeof cell.id === "string" ? cell.id : crypto.randomUUID(),
      cell_type: cell.cell_type as string,
      source: Array.isArray(cell.source)
        ? cell.source.join("")
        : (cell.source as string),
      metadata,
      execution_count: (cell.execution_count ?? null) as number | null,
      outputs: reducer.outputs,
    };
  });
  snapshot.selected_cell_id = snapshot.cells[0]?.id ?? null;
  const check = new NotebookApplication(JSON.stringify(snapshot));
  check.dispose();
  return snapshot;
}
export class BrowserArtifactTransport implements ArtifactTransport {
  constructor(readonly store: IndexedNotebookStore) {}
  async import(entries: WorkspaceEntry[]): Promise<string[]> {
    if (
      entries.length > COUNT_LIMIT ||
      entries.reduce((n, e) => n + e.bytes.length, 0) > ZIP_LIMIT
    )
      throw new Error("Workspace exceeds 1,000 items or 20 MB.");
    const items = entries.map((entry) => {
      browserPath(entry.path, true);
      if (!entry.path || entry.bytes.length > ENTRY_LIMIT)
        throw new Error("Each file must be at most 1 MB.");
      return {
        ...entry,
        snapshot:
          !entry.directory && entry.path.endsWith(".ipynb")
            ? importNotebook(entry.path, entry.bytes)
            : undefined,
      };
    });
    await this.store.importEntries(items);
    return items.filter((i) => i.snapshot).map((i) => i.path);
  }
  async create(request: ArtifactRequest): Promise<void> {
    browserPath(request.path, true);
    if ((request.content_base64?.length ?? 0) > 1_333_336)
      throw new Error("Each file must be at most 1 MB.");
    const parent = request.path.split("/").slice(0, -1).join("/");
    if (parent) {
      const grand = parent.split("/").slice(0, -1).join("/");
      if (
        !(await this.store.list(grand)).entries.some(
          (e) => e.path === parent && e.type === "directory",
        )
      )
        throw new Error("Create the parent folder first.");
    }
    let bytes =
      request.content_base64 === undefined
        ? new Uint8Array()
        : Uint8Array.from(atob(request.content_base64), (c) => c.charCodeAt(0));
    if (request.kind === "notebook" && !request.path.endsWith(".ipynb"))
      throw new Error("Notebook names must end in .ipynb.");
    if (request.kind === "directory" && request.content_base64 !== undefined)
      throw new Error("Folders cannot contain upload data.");
    if (request.kind === "notebook" && request.content_base64 === undefined)
      bytes = new TextEncoder().encode(
        JSON.stringify({
          nbformat: 4,
          nbformat_minor: 5,
          metadata: {},
          cells: [],
        }),
      );
    await this.import([
      { path: request.path, directory: request.kind === "directory", bytes },
    ]);
  }
  async demo(): Promise<string> {
    if (await this.store.read(DEFAULT_DEMO_NOTEBOOK))
      return DEFAULT_DEMO_NOTEBOOK;
    const entries: WorkspaceEntry[] = [];
    for (const archive of DEMO_ARCHIVES) {
      const response = await fetch(`${import.meta.env.BASE_URL}${archive}`);
      if (!response.ok)
        throw new Error(`Could not load bundled demo: ${archive}`);
      entries.push(...(await readWorkspaceZip(await response.arrayBuffer())));
    }
    await this.import(entries);
    return DEFAULT_DEMO_NOTEBOOK;
  }
}
