import type { NotebookSnapshot } from "./types";
import { microscopeGraphicsArtifacts } from "../pkg/notebook_wasm";

import { browserPath } from "./browser-path";
export { browserPath } from "./browser-path";
export interface NotebookStore {
  artifacts?(): Promise<
    { path: string; directory: boolean; bytes: Uint8Array }[]
  >;
  commitMicroscope?(
    snapshot: NotebookSnapshot,
    path: string,
    content: string | null,
    previous?: string,
  ): Promise<void>;
  read(path: string): Promise<NotebookSnapshot | undefined>;
  write(path: string, snapshot: NotebookSnapshot): Promise<void>;
  rename(
    oldPath: string,
    path: string,
    snapshot: NotebookSnapshot,
  ): Promise<void>;
  list(directory: string): Promise<{
    directory: string;
    entries: { name: string; path: string; type: string }[];
  }>;
}
/** Origin-local notebooks, deliberately separate from the kernel's temporary FS. */
export class IndexedNotebookStore implements NotebookStore {
  /** Read both stores in one transaction so notebook references and sidecars agree. */
  async exportEntries() {
    const { notebookBytes } = await import("./workspace-export");
    const db = await this.db;
    return new Promise<import("./workspace-zip").WorkspaceEntry[]>(
      (resolve, reject) => {
        const tx = db.transaction(["notebooks", "artifacts"]);
        const books = tx.objectStore("notebooks").getAll();
        const files = tx.objectStore("artifacts").getAll();
        tx.oncomplete = () => {
          try {
            resolve([
              ...files.result,
              ...books.result.map(
                (book: import("./browser-transport").BrowserSnapshot) => ({
                  path: book.notebook.path,
                  directory: false,
                  bytes: notebookBytes(book),
                }),
              ),
            ]);
          } catch (error) {
            reject(error);
          }
        };
        tx.onabort = tx.onerror = () =>
          reject(new Error("Unable to read workspace for export. Retry."));
      },
    );
  }
  async commitMicroscope(
    snapshot: NotebookSnapshot,
    path: string,
    content: string | null,
    previous?: string,
  ): Promise<void> {
    browserPath(path, true);
    let priorGraphics: Record<string, string> = {};
    if (previous) {
      try {
        priorGraphics = JSON.parse(microscopeGraphicsArtifacts(previous));
      } catch (error) {
        // A deletion may target a sidecar written by an older, intentionally
        // incompatible walkthrough schema. Its derived sidecar path is still
        // owned by the validated notebook reference, so remove that file while
        // leaving unrecognized attachments untouched rather than making the
        // microscope impossible to delete.
        if (content !== null) throw error;
      }
    }
    const nextGraphics: Record<string, string> = content
      ? JSON.parse(microscopeGraphicsArtifacts(content))
      : {};
    const bytes = content === null ? null : new TextEncoder().encode(content);
    const notebook = (snapshot.notebook as { path: string }).path;
    const db = await this.db;
    return new Promise((resolve, reject) => {
      const tx = db.transaction(["notebooks", "artifacts"], "readwrite");
      const files = tx.objectStore("artifacts"),
        books = tx.objectStore("notebooks");
      const count = books.count(),
        saved = files.getAll();
      saved.onsuccess = () => {
        const existing = saved.result.find(
          (f: { path: string }) => f.path === path,
        );
        const changedPaths = new Set([
          path,
          ...Object.keys(priorGraphics),
          ...Object.keys(nextGraphics),
        ]);
        const nextFiles = Object.entries(nextGraphics).map(
          ([path, source]) => ({
            path,
            directory: false,
            bytes: new TextEncoder().encode(source),
          }),
        );
        if (bytes) nextFiles.push({ path, directory: false, bytes });
        const retained = saved.result.filter(
          (f: { path: string }) => !changedPaths.has(f.path),
        );
        for (const name of changedPaths) {
          if (name === path) continue;
          const old = saved.result.find(
            (f: { path: string }) => f.path === name,
          );
          if (
            old &&
            (old.directory ||
              priorGraphics[name] === undefined ||
              new TextDecoder().decode(old.bytes) !== priorGraphics[name])
          ) {
            tx.abort();
            return;
          }
        }
        if (
          (previous !== undefined
            ? !existing || new TextDecoder().decode(existing.bytes) !== previous
            : content !== null && existing) ||
          (content !== null &&
            (count.result + retained.length + nextFiles.length > 1000 ||
              retained.reduce(
                (n: number, f: { bytes: Uint8Array }) => n + f.bytes.length,
                0,
              ) +
                nextFiles.reduce((n, f) => n + f.bytes.length, 0) >
                20_000_000))
        ) {
          tx.abort();
          return;
        }
        for (const name of changedPaths) files.delete(name);
        for (const file of nextFiles) files.put(file, file.path);
        books.put(snapshot, browserPath(notebook));
      };
      tx.oncomplete = () => resolve();
      tx.onabort = tx.onerror = () =>
        reject(
          new Error(
            "Microscope transaction failed; notebook and file were not changed",
          ),
        );
    });
  }
  private db: Promise<IDBDatabase>;
  constructor(
    id = new URL(location.href).searchParams.get("workspace") ?? "legacy",
  ) {
    this.db = this.open(id);
  }
  async selectWorkspace(id: string): Promise<void> {
    const next = this.open(id);
    const db = await next;
    (await this.db).close();
    this.db = Promise.resolve(db);
  }
  async close(): Promise<void> {
    (await this.db).close();
  }
  private open(id: string): Promise<IDBDatabase> {
    if (!/^(legacy|demo|demo-v2|[a-f0-9-]{36})$/.test(id))
      throw new Error("Invalid browser workspace identity");
    return new Promise((resolve, reject) => {
      const request = indexedDB.open(
        id === "legacy"
          ? "didaction-browser-notebooks-v1"
          : `didaction-workspace-${id}`,
        2,
      );
      request.onupgradeneeded = () => {
        for (const name of ["notebooks", "artifacts"])
          if (!request.result.objectStoreNames.contains(name))
            request.result.createObjectStore(name);
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () =>
        reject(new Error("Browser notebook storage unavailable"));
    });
  }
  async read(path: string): Promise<NotebookSnapshot | undefined> {
    const db = await this.db;
    return new Promise((resolve, reject) => {
      const request = db
        .transaction("notebooks")
        .objectStore("notebooks")
        .get(browserPath(path));
      request.onsuccess = () => resolve(request.result);
      request.onerror = () =>
        reject(new Error("Unable to read saved notebook"));
    });
  }
  async write(path: string, snapshot: NotebookSnapshot): Promise<void> {
    const db = await this.db;
    return new Promise((resolve, reject) => {
      browserPath(path);
      const transaction = db.transaction(
        ["notebooks", "artifacts"],
        "readwrite",
      );
      const check = transaction.objectStore("artifacts").getAll();
      check.onsuccess = () => {
        if (
          check.result.some(
            (file: { path: string; directory: boolean }) =>
              file.path === path ||
              file.path.startsWith(path + "/") ||
              (!file.directory && path.startsWith(file.path + "/")),
          )
        ) {
          transaction.abort();
          return;
        }
        transaction.objectStore("notebooks").put(snapshot, path);
      };
      transaction.oncomplete = () => resolve();
      transaction.onabort = transaction.onerror = () =>
        reject(
          new Error(
            "Notebook not saved: browser storage failed or is full. Download a backup.",
          ),
        );
    });
  }
  async rename(
    oldPath: string,
    path: string,
    snapshot: NotebookSnapshot,
  ): Promise<void> {
    const db = await this.db;
    return new Promise((resolve, reject) => {
      const tx = db.transaction(["notebooks", "artifacts"], "readwrite");
      const store = tx.objectStore("notebooks");
      const check = store.get(browserPath(path));
      const files = tx.objectStore("artifacts").getAll();
      let destinationExists: boolean | undefined;
      let artifacts:
        | { path: string; directory: boolean; bytes: Uint8Array }[]
        | undefined;
      const commit = () => {
        if (destinationExists === undefined || artifacts === undefined) return;
        const ownedPrefixes = artifacts
          .filter((file) => {
            if (file.directory || !file.path.startsWith(`${oldPath}.`))
              return false;
            try {
              const document = JSON.parse(new TextDecoder().decode(file.bytes));
              const references = (
                snapshot.cells as {
                  id: string;
                  metadata: Record<string, unknown>;
                }[]
              )
                .flatMap((cell) => {
                  const microscopes = cell.metadata.didaction_microscopes as
                    | { items?: { id?: string }[] }
                    | undefined;
                  return (microscopes?.items ?? []).map((item) => ({
                    cell: cell.id,
                    id: item.id,
                  }));
                })
                .filter((reference) => reference.id);
              return (
                document.notebook_path === oldPath &&
                references.some(
                  (reference) =>
                    reference.cell === document.cell_id &&
                    reference.id === document.microscope?.id,
                )
              );
            } catch {
              return false;
            }
          })
          .map((file) => file.path);
        const moving = new Map<string, string>();
        for (const prefix of ownedPrefixes)
          for (const file of artifacts)
            if (file.path === prefix || file.path.startsWith(`${prefix}.`))
              moving.set(
                file.path,
                `${path}${file.path.slice(oldPath.length)}`,
              );
        const retained = new Set(
          artifacts
            .filter((file) => !moving.has(file.path))
            .map((file) => file.path),
        );
        if (
          destinationExists ||
          artifacts.some(
            (file: { path: string; directory: boolean }) =>
              file.path === path ||
              file.path.startsWith(path + "/") ||
              (!file.directory && path.startsWith(file.path + "/")),
          ) ||
          [...moving.values()].some((target) => retained.has(target))
        ) {
          tx.abort();
          return;
        }
        const artifactStore = tx.objectStore("artifacts");
        for (const [source, target] of moving) {
          const original = artifacts.find((file) => file.path === source)!;
          let bytes = original.bytes;
          if (ownedPrefixes.includes(source)) {
            const document = JSON.parse(new TextDecoder().decode(bytes));
            document.notebook_path = path;
            bytes = new TextEncoder().encode(JSON.stringify(document));
          }
          artifactStore.put({ ...original, path: target, bytes }, target);
          artifactStore.delete(source);
        }
        store.put(snapshot, path);
        store.delete(browserPath(oldPath));
      };
      check.onsuccess = () => {
        destinationExists = check.result !== undefined;
        commit();
      };
      files.onsuccess = () => {
        artifacts = files.result;
        commit();
      };
      tx.oncomplete = () => resolve();
      tx.onabort = tx.onerror = () =>
        reject(
          new Error("Rename failed: destination exists or storage unavailable"),
        );
    });
  }
  async list(directory: string) {
    browserPath(directory, true);
    const db = await this.db;
    const keys = await new Promise<IDBValidKey[]>((resolve, reject) => {
      const request = db
        .transaction("notebooks")
        .objectStore("notebooks")
        .getAllKeys();
      request.onsuccess = () => resolve(request.result);
      request.onerror = () =>
        reject(new Error("Unable to list browser notebooks"));
    });
    const prefix = directory ? `${directory}/` : "";
    const entries = new Map<
      string,
      { name: string; path: string; type: string }
    >();
    for (const key of keys) {
      if (typeof key !== "string" || !key.startsWith(prefix)) continue;
      const rest = key.slice(prefix.length);
      const name = rest.split("/")[0]!;
      entries.set(name, {
        name,
        path: prefix + name,
        type: rest.includes("/") ? "directory" : "notebook",
      });
    }
    for (const item of await this.artifacts()) {
      if (!item.path.startsWith(prefix)) continue;
      const rest = item.path.slice(prefix.length);
      if (!rest) continue;
      const name = rest.split("/")[0]!;
      entries.set(name, {
        name,
        path: prefix + name,
        type: rest.includes("/") || item.directory ? "directory" : "file",
      });
    }
    if (entries.size > 1000) throw new Error("Folder listing exceeds limit");
    return {
      directory,
      entries: [...entries.values()].sort((a, b) =>
        a.name.localeCompare(b.name),
      ),
    };
  }
  async artifacts(): Promise<
    { path: string; directory: boolean; bytes: Uint8Array }[]
  > {
    const db = await this.db;
    return new Promise((resolve, reject) => {
      const request = db
        .transaction("artifacts")
        .objectStore("artifacts")
        .getAll();
      request.onsuccess = () => resolve(request.result);
      request.onerror = () =>
        reject(new Error("Could not read browser workspace files"));
    });
  }
  /** Atomic create-only import: every collision or storage error rolls back the batch. */
  async importEntries(
    items: {
      path: string;
      directory: boolean;
      bytes: Uint8Array;
      snapshot?: NotebookSnapshot;
    }[],
  ): Promise<void> {
    const db = await this.db;
    return new Promise((resolve, reject) => {
      const tx = db.transaction(["notebooks", "artifacts"], "readwrite");
      const notebooks = tx.objectStore("notebooks"),
        artifacts = tx.objectStore("artifacts");
      const allBooks = notebooks.getAllKeys(),
        allFiles = artifacts.getAll();
      allFiles.onsuccess = () => {
        const totalBytes =
          allFiles.result.reduce(
            (n: number, file: { bytes: Uint8Array }) => n + file.bytes.length,
            0,
          ) +
          items.reduce(
            (n, item) => n + (item.snapshot ? 0 : item.bytes.length),
            0,
          );
        if (
          new Set(items.map((item) => item.path)).size !== items.length ||
          totalBytes > 20_000_000
        ) {
          tx.abort();
          return;
        }
        const occupied = new Map<string, boolean>(
          (allBooks.result as string[]).map((p) => [p, false]),
        );
        for (const file of allFiles.result)
          occupied.set(file.path, file.directory);
        const combined = new Map(occupied);
        for (const item of items) {
          if (
            occupied.has(item.path) &&
            !(item.directory && occupied.get(item.path))
          ) {
            tx.abort();
            return;
          }
          combined.set(item.path, item.directory);
        }
        if (combined.size > 1000) {
          tx.abort();
          return;
        }
        for (const path of combined.keys()) {
          const parts = path.split("/");
          parts.pop();
          while (parts.length) {
            const parent = parts.join("/");
            if (combined.has(parent) && !combined.get(parent)) {
              tx.abort();
              return;
            }
            parts.pop();
          }
        }
        for (const item of items) {
          if (item.snapshot) notebooks.add(item.snapshot, item.path);
          else if (!occupied.has(item.path))
            artifacts.add(
              { path: item.path, directory: item.directory, bytes: item.bytes },
              item.path,
            );
        }
      };
      tx.oncomplete = () => resolve();
      tx.onabort = tx.onerror = () =>
        reject(
          new Error(
            "Import not saved: a name already exists, paths conflict, the 1,000-item/20 MB file limit was reached, or browser storage is full. Existing files were preserved.",
          ),
        );
    });
  }
}
