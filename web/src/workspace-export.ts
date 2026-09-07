import type { BrowserSnapshot } from "./browser-transport";
import {
  BROWSER_KERNELS,
  DEFAULT_BROWSER_KERNEL,
} from "./browser-kernel-profile";
import type { WorkspaceEntry } from "./workspace-zip";
import { browserPath } from "./browser-store";
import { crc32, COUNT_LIMIT, ENTRY_LIMIT, ZIP_LIMIT } from "./workspace-zip";

/** Standard nbformat, not the runtime snapshot envelope. */
export function notebookBytes(snapshot: BrowserSnapshot): Uint8Array {
  const kernel = snapshot.kernel ?? {
    name: DEFAULT_BROWSER_KERNEL,
    display_name: BROWSER_KERNELS[DEFAULT_BROWSER_KERNEL].displayName,
  };
  const cells = snapshot.cells.map((cell) => {
    const base = {
      id: cell.id,
      cell_type: cell.cell_type,
      source: cell.source,
      metadata: cell.metadata,
    };
    if (cell.cell_type !== "code") return base;
    return {
      ...base,
      execution_count: cell.execution_count,
      outputs: cell.outputs.map((output) => {
        if (output.kind === "stream")
          return {
            output_type: "stream",
            name: output.name,
            text: output.text,
          };
        if (output.kind === "error")
          return {
            output_type: "error",
            ename: output.name,
            evalue: output.message,
            traceback: output.traceback,
          };
        const data =
          output.kind === "text"
            ? { "text/plain": output.text }
            : {
                [output.mime]:
                  output.mime === "image/svg+xml"
                    ? new TextDecoder().decode(
                        Uint8Array.from(atob(output.data), (c) =>
                          c.charCodeAt(0),
                        ),
                      )
                    : output.data,
              };
        return { output_type: "display_data", metadata: {}, data };
      }),
    };
  });
  return new TextEncoder().encode(
    JSON.stringify({
      nbformat: 4,
      nbformat_minor: 5,
      metadata: {
        kernelspec: {
          name: kernel.name,
          display_name: kernel.display_name,
          language: "python",
        },
      },
      cells,
    }),
  );
}

/** Stored ZIP: deterministic, bounded and round-trippable through workspace import. */
export function writeWorkspaceZip(
  entries: WorkspaceEntry[],
): Uint8Array<ArrayBuffer> {
  if (entries.length > COUNT_LIMIT)
    throw new Error("Workspace exceeds the 1,000-item export limit.");
  const names = new Set<string>();
  const prepared = entries.map((entry) => {
    browserPath(entry.path, true);
    if (
      !entry.path ||
      names.has(entry.path) ||
      entry.bytes.length > ENTRY_LIMIT ||
      (entry.directory && entry.bytes.length)
    )
      throw new Error(
        "Workspace has duplicate paths or a file exceeds the 1 MB export limit.",
      );
    names.add(entry.path);
    return {
      ...entry,
      name: new TextEncoder().encode(entry.path + (entry.directory ? "/" : "")),
    };
  });
  const size = prepared.reduce(
    (n, e) => n + 76 + e.name.length * 2 + e.bytes.length,
    22,
  );
  if (size > ZIP_LIMIT)
    throw new Error("Workspace exceeds the 20 MB ZIP export limit.");
  const bytes = new Uint8Array(size),
    view = new DataView(bytes.buffer);
  const u16 = (p: number, n: number) => view.setUint16(p, n, true);
  const u32 = (p: number, n: number) => view.setUint32(p, n, true);
  let offset = 0;
  const offsets: number[] = [];
  for (const e of prepared) {
    offsets.push(offset);
    u32(offset, 0x04034b50);
    u16(offset + 4, 20);
    u16(offset + 6, 0x800);
    u16(offset + 12, 0x21); // Valid deterministic DOS date: 1980-01-01.
    u32(offset + 14, crc32(e.bytes));
    u32(offset + 18, e.bytes.length);
    u32(offset + 22, e.bytes.length);
    u16(offset + 26, e.name.length);
    bytes.set(e.name, offset + 30);
    bytes.set(e.bytes, offset + 30 + e.name.length);
    offset += 30 + e.name.length + e.bytes.length;
  }
  const start = offset;
  prepared.forEach((e, i) => {
    u32(offset, 0x02014b50);
    u16(offset + 4, 20);
    u16(offset + 6, 20);
    u16(offset + 8, 0x800);
    u16(offset + 14, 0x21);
    u32(offset + 16, crc32(e.bytes));
    u32(offset + 20, e.bytes.length);
    u32(offset + 24, e.bytes.length);
    u16(offset + 28, e.name.length);
    u32(offset + 38, e.directory ? 16 : 0);
    u32(offset + 42, offsets[i]!);
    bytes.set(e.name, offset + 46);
    offset += 46 + e.name.length;
  });
  u32(offset, 0x06054b50);
  u16(offset + 8, entries.length);
  u16(offset + 10, entries.length);
  u32(offset + 12, offset - start);
  u32(offset + 16, start);
  return bytes;
}

export function downloadWorkspace(bytes: Uint8Array<ArrayBuffer>): void {
  const url = URL.createObjectURL(
    new Blob([bytes], { type: "application/zip" }),
  );
  const link = document.createElement("a");
  link.href = url;
  link.download = "workspace.zip";
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
