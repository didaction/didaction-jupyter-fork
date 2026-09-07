import type { BrowserWorkspace } from "./browser-workspace";
import { readWorkspaceZip, ZIP_LIMIT } from "./workspace-zip";
import {
  savedWorkspaces,
  rememberWorkspace,
} from "./browser-workspace-catalog";
import {
  DEFAULT_BROWSER_KERNEL,
  isBrowserKernelName,
  storedBrowserKernel,
} from "./browser-kernel-profile";

function notebookKernel(snapshot: unknown): unknown {
  if (!snapshot || typeof snapshot !== "object") return null;
  const kernel = (snapshot as { kernel?: unknown }).kernel;
  return kernel && typeof kernel === "object"
    ? (kernel as { name?: unknown }).name
    : null;
}

export async function chooseBrowserWorkspace(
  workspace: BrowserWorkspace,
  requested: string | null,
  requestedKernel: string | null,
): Promise<{ path: string; kernel: string; workspace: string }> {
  let workspaceId =
    new URL(location.href).searchParams.get("workspace") ?? "legacy";
  const savedNotebook = requested
    ? await workspace.store.read(requested)
    : undefined;
  const directKernel =
    storedBrowserKernel(notebookKernel(savedNotebook)) ??
    (requestedKernel && isBrowserKernelName(requestedKernel)
      ? requestedKernel
      : DEFAULT_BROWSER_KERNEL);
  if (!isBrowserKernelName(directKernel))
    throw new Error(
      "Unsupported browser kernel. Open / and choose a versioned Python runtime.",
    );
  if (requested && savedNotebook)
    return { path: requested, kernel: directKernel, workspace: workspaceId };
  const panel = document.querySelector<HTMLElement>("#browser-launch")!;
  const layout = document.querySelector<HTMLElement>(".workspace-layout")!;
  const message = document.querySelector<HTMLElement>(
    "#browser-launch-status",
  )!;
  const empty = document.querySelector<HTMLButtonElement>("#browser-empty")!;
  const demo = document.querySelector<HTMLButtonElement>("#browser-demo")!;
  const resume = document.querySelector<HTMLButtonElement>("#browser-resume")!;
  const file = document.querySelector<HTMLInputElement>("#browser-zip")!;
  const picker = document.querySelector<HTMLSelectElement>("#browser-saved")!;
  const saved = (await savedWorkspaces()).filter((w) => w.notebooks.length);
  for (const entry of saved) {
    const option = document.createElement("option");
    option.value = entry.id;
    option.textContent = `${entry.name} (${entry.notebooks.length} ${entry.notebooks.length === 1 ? "notebook" : "notebooks"})`;
    picker.append(option);
  }
  const contents = document.querySelector<HTMLElement>(
    "#browser-workspace-contents",
  )!;
  picker.onchange = () => {
    contents.replaceChildren();
    for (const path of saved.find((w) => w.id === picker.value)?.notebooks ??
      []) {
      const item = document.createElement("li");
      item.textContent = path;
      contents.append(item);
    }
  };
  picker.dispatchEvent(new Event("change"));
  document.querySelector<HTMLElement>("#browser-continue")!.hidden =
    !saved.length;
  panel.hidden = false;
  layout.hidden = true;
  empty.focus();
  return new Promise((resolve) => {
    let busy = false;
    async function run(action: () => Promise<string>) {
      if (busy) return;
      busy = true;
      empty.disabled = demo.disabled = resume.disabled = file.disabled = true;
      message.textContent = "Preparing browser workspace…";
      try {
        const path = await action();
        const stored = await workspace.store.read(path);
        const selectedKernel =
          storedBrowserKernel(notebookKernel(stored)) ?? directKernel;
        panel.hidden = true;
        layout.hidden = false;
        resolve({ path, kernel: selectedKernel, workspace: workspaceId });
      } catch (error) {
        message.textContent =
          error instanceof Error
            ? error.message
            : "Import failed. No files were replaced.";
      } finally {
        busy = false;
        empty.disabled =
          demo.disabled =
          resume.disabled =
          file.disabled =
            false;
        file.value = "";
      }
    }
    async function select(id: string) {
      await workspace.selectWorkspace(id);
      workspaceId = id;
    }
    empty.onclick = () =>
      void run(async () => {
        const id = crypto.randomUUID();
        const names = new Set((await savedWorkspaces()).map((w) => w.name));
        const baseName = "Untitled workspace";
        let name = baseName;
        for (let suffix = 2; names.has(name); suffix++)
          name = `${baseName} (${suffix})`;
        await rememberWorkspace({ id, name });
        await select(id);
        const path = "Untitled.ipynb";
        await workspace.artifacts.create({ kind: "notebook", path });
        return path;
      });
    demo.onclick = () =>
      void run(async () => {
        await rememberWorkspace({ id: "demo-v2", name: "Demo workspace" });
        await select("demo-v2");
        return workspace.artifacts.demo();
      });
    resume.onclick = () =>
      void run(async () => {
        const entry = saved.find((w) => w.id === picker.value);
        if (!entry) throw new Error("Select a saved workspace");
        await select(entry.id);
        return entry.notebooks[0]!;
      });
    file.onchange = () => {
      const zip = file.files?.[0];
      if (!zip) return;
      void run(async () => {
        if (zip.size > ZIP_LIMIT) throw new Error("ZIP must be at most 20 MB.");
        const entries = await readWorkspaceZip(await zip.arrayBuffer());
        if (!entries.some((e) => !e.directory && e.path.endsWith(".ipynb")))
          throw new Error(
            "ZIP needs at least one .ipynb notebook. Use the explorer to upload other files.",
          );
        const id = crypto.randomUUID();
        const baseName =
          zip.name.replace(/\.zip$/i, "").slice(0, 100) || "Imported workspace";
        const names = new Set((await savedWorkspaces()).map((w) => w.name));
        let name = baseName;
        for (let suffix = 2; names.has(name); suffix++)
          name = `${baseName} (${suffix})`;
        // Register before importing so a successful data commit is always discoverable.
        await rememberWorkspace({
          id,
          name,
        });
        await select(id);
        return (await workspace.artifacts.import(entries))[0]!;
      });
    };
  });
}
