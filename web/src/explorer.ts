/** Accessible workspace navigation; notebook editing remains in egui/WASM. */
import {
  artifactPath,
  uploadRequests,
  type ArtifactTransport,
} from "./artifacts";
import { downloadWorkspace, writeWorkspaceZip } from "./workspace-export";

type ExplorerEntry = { name: string; path: string; type: string };
export function visibleWorkspaceEntries(entries: ExplorerEntry[]) {
  const counts = new Map<string, number>();
  const visible = entries.filter((entry) => {
    const match =
      entry.type === "file" &&
      /^(.*\.ipynb)\.[0-9a-f]{7}\.[a-z0-9]{7}(\.[A-Za-z0-9][A-Za-z0-9_-]*\.ts)?$/.exec(
        entry.path,
      );
    if (
      !match ||
      !entries.some((e) => e.type === "notebook" && e.path === match[1])
    )
      return true;
    if (!match[2]) counts.set(match[1]!, (counts.get(match[1]!) ?? 0) + 1);
    return false;
  });
  return visible.map((entry) => ({
    ...entry,
    microscopeCount: counts.get(entry.path) ?? 0,
  }));
}
export function installExplorer(
  current: string,
  assertSaved: () => void,
  open?: (path: string) => Promise<void>,
  listDirectory?: (path: string) => Promise<{
    directory: string;
    entries: { name: string; path: string; type: string }[];
  }>,
  artifacts?: ArtifactTransport,
  canWrite: () => boolean = () => false,
  exportWorkspace?: () => Promise<import("./workspace-zip").WorkspaceEntry[]>,
): void {
  const panel = document.querySelector<HTMLElement>("#file-explorer")!;
  const list = document.querySelector<HTMLUListElement>("#notebook-files")!;
  const status = document.querySelector<HTMLElement>("#explorer-status")!;
  const crumb = document.querySelector<HTMLElement>("#folder-path")!;
  const up = document.querySelector<HTMLButtonElement>("#folder-up")!;
  const toggle = document.querySelector<HTMLButtonElement>("#explorer-toggle")!;
  let directory = current.split("/").slice(0, -1).join("/");
  let generation = 0;
  toggle.onclick = () => {
    panel.hidden = !panel.hidden;
    toggle.setAttribute("aria-expanded", String(!panel.hidden));
    toggle.title = panel.hidden
      ? "Show workspace explorer"
      : "Hide workspace explorer";
    window.dispatchEvent(new Event("resize"));
    window.dispatchEvent(new Event("workspace-visibility"));
  };
  async function load(path: string) {
    const request = ++generation;
    status.textContent = "Loading folder…";
    list.replaceChildren();
    try {
      const response = listDirectory
        ? undefined
        : await fetch(
            `/api/v1/notebooks?directory=${encodeURIComponent(path)}`,
          );
      if (response && !response.ok)
        throw new Error("Folder unavailable. Use Up or Refresh to retry.");
      const data = (
        listDirectory ? await listDirectory(path) : await response!.json()
      ) as {
        directory: string;
        entries: { name: string; path: string; type: string }[];
      };
      if (request !== generation) return;
      directory = data.directory;
      crumb.textContent = directory ? `Workspace / ${directory}` : "Workspace";
      crumb.title = crumb.textContent;
      up.disabled = !directory;
      const visible = visibleWorkspaceEntries(data.entries);
      for (const entry of visible) {
        const item = document.createElement("li");
        const button = document.createElement("button");
        button.type = "button";
        button.setAttribute("aria-label", entry.name);
        const icon = document.createElementNS(
          "http://www.w3.org/2000/svg",
          "svg",
        );
        icon.setAttribute("viewBox", "0 0 24 24");
        icon.setAttribute("aria-hidden", "true");
        const shape = document.createElementNS(icon.namespaceURI, "path");
        shape.setAttribute(
          "d",
          entry.type === "directory"
            ? "M3 6h7l2 2h9v12H3Z"
            : "M5 3h10l4 4v14H5Z M9 11h6 M9 15h6",
        );
        icon.append(shape);
        const label = document.createElement("span");
        label.textContent = entry.name;
        button.append(icon, label);
        if (entry.type === "notebook") {
          const count = document.createElement("span");
          count.className = "microscope-count";
          count.title = `${entry.microscopeCount} microscopes`;
          count.setAttribute("aria-label", count.title);
          button.setAttribute("aria-description", count.title);
          const scopeIcon = icon.cloneNode(false) as SVGElement;
          const path = document.createElementNS(icon.namespaceURI, "path");
          path.setAttribute(
            "d",
            "M4 21h16 M12 21v-4 M3 15h12 M6 3l3-2 7 7-3 3Z M16 10c6 2 5 9-4 9",
          );
          scopeIcon.append(path);
          count.append(scopeIcon, String(entry.microscopeCount));
          button.append(count);
        }
        button.title = entry.path;
        if (
          entry.path ===
          (new URL(location.href).searchParams.get("notebook") ?? current)
        )
          button.setAttribute("aria-current", "page");
        button.onclick = async () => {
          if (entry.type === "directory") {
            void load(entry.path);
            return;
          }
          if (entry.type === "file") {
            status.textContent = `${entry.name} is a workspace artifact, not a notebook. It is available to the kernel.`;
            return;
          }
          try {
            assertSaved();
            if (open) {
              await open(entry.path);
              return;
            }
            const url = new URL(location.href);
            url.searchParams.set("notebook", entry.path);
            location.assign(url.href);
          } catch {
            status.textContent =
              "Save your edits and wait for execution to finish before opening another notebook.";
          }
        };
        item.append(button);
        list.append(item);
      }
      status.textContent = visible.length
        ? `${visible.length} workspace items`
        : "This folder is empty. Create a notebook or upload a file.";
    } catch (error) {
      if (request === generation)
        status.textContent =
          error instanceof Error
            ? error.message
            : "Unable to load folder. Refresh to retry.";
    }
  }
  up.onclick = () => void load(directory.split("/").slice(0, -1).join("/"));
  window.addEventListener(
    "workspace-files-changed",
    () => void load(directory),
  );
  if (exportWorkspace) {
    const button = document.createElement("button");
    button.id = "workspace-export";
    button.textContent = "Export workspace";
    button.title =
      "Download saved notebooks, folders, artifacts and microscopes as a ZIP. Temporary kernel files are excluded.";
    status.before(button);
    button.onclick = async () => {
      button.disabled = true;
      status.textContent = "Preparing workspace ZIP…";
      try {
        assertSaved();
        downloadWorkspace(writeWorkspaceZip(await exportWorkspace()));
        status.textContent =
          "Workspace exported. Temporary kernel files and variables are not included.";
      } catch (error) {
        status.textContent =
          error instanceof Error ? error.message : "Export failed. Retry.";
      } finally {
        button.disabled = false;
      }
    };
  }
  document.querySelector<HTMLButtonElement>("#folder-refresh")!.onclick = () =>
    void load(directory);
  const form = document.querySelector<HTMLFormElement>("#artifact-create")!;
  const name = document.querySelector<HTMLInputElement>("#artifact-name")!;
  const kind = document.querySelector<HTMLSelectElement>("#artifact-kind")!;
  const upload = document.querySelector<HTMLInputElement>("#artifact-upload")!;
  const controls =
    document.querySelector<HTMLFieldSetElement>("#artifact-controls")!;
  let busy = false;
  const updateAccess = () => {
    controls.disabled = busy || !artifacts || !canWrite();
    controls.title = !artifacts
      ? "Workspace uploads require the native server runtime"
      : !canWrite()
        ? "Only the workspace driver can create or upload files"
        : "Create or upload in the displayed folder (1 MB per file; 20 MB per ZIP)";
  };
  const observer = new MutationObserver(updateAccess);
  observer.observe(document.querySelector("#driver-status")!, {
    attributes: true,
    childList: true,
  });
  window.addEventListener("pagehide", () => observer.disconnect(), {
    once: true,
  });
  async function write(action: () => Promise<void>) {
    if (busy) return;
    try {
      if (!artifacts || !canWrite())
        throw new Error(
          "Only the workspace driver can write files in server mode.",
        );
      busy = true;
      updateAccess();
      status.textContent = "Saving workspace item…";
      await action();
      await load(directory);
    } catch (error) {
      status.textContent =
        error instanceof Error
          ? error.message
          : "Save was not confirmed. Refresh before retrying.";
    } finally {
      busy = false;
      updateAccess();
    }
  }
  form.onsubmit = (event) => {
    event.preventDefault();
    const destination = directory;
    void write(async () => {
      const type = kind.value as "notebook" | "directory" | "file";
      const filename =
        type === "notebook" && !name.value.endsWith(".ipynb")
          ? `${name.value}.ipynb`
          : name.value;
      if (!name.value.trim()) throw new Error("Enter a name first.");
      await artifacts!.create({
        path: artifactPath(destination, filename),
        kind: type,
      });
      name.value = "";
    });
  };
  upload.onchange = () => {
    const files = Array.from(upload.files ?? []);
    const destination = directory;
    void write(async () => {
      for (const file of files)
        for (const request of await uploadRequests(destination, file))
          await artifacts!.create(request);
    }).finally(() => {
      upload.value = "";
    });
  };
  updateAccess();
  void load(directory);
}
