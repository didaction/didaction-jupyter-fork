import { expect, test } from "vitest";
import { notebookBytes, writeWorkspaceZip } from "./workspace-export";
import { readWorkspaceZip } from "./workspace-zip";
import { visibleWorkspaceEntries } from "./explorer";
import type { BrowserSnapshot } from "./browser-transport";

test("ZIP export preserves nested folders, binary artifacts and microscope sidecars", async () => {
  const entries = [
    { path: "lesson", directory: true, bytes: new Uint8Array() },
    {
      path: "lesson/demo.ipynb",
      directory: false,
      bytes: new TextEncoder().encode('{"nbformat":4}'),
    },
    {
      path: "lesson/demo.ipynb.012abcd.abc1234",
      directory: false,
      bytes: new TextEncoder().encode('{"walkthrough":{}}'),
    },
    {
      path: "lesson/data.bin",
      directory: false,
      bytes: new Uint8Array([0, 255, 128]),
    },
  ];
  expect(await readWorkspaceZip(writeWorkspaceZip(entries).buffer)).toEqual(
    entries,
  );
});

test("export fails instead of truncating unsafe or oversized workspaces", () => {
  const entry = { path: "file", directory: false, bytes: new Uint8Array() };
  expect(() => writeWorkspaceZip([{ ...entry, path: "../secret" }])).toThrow();
  expect(() => writeWorkspaceZip([entry, entry])).toThrow();
  expect(() =>
    writeWorkspaceZip([{ ...entry, bytes: new Uint8Array(1_000_001) }]),
  ).toThrow();
  expect(() =>
    writeWorkspaceZip(
      Array.from({ length: 1001 }, (_, i) => ({ ...entry, path: String(i) })),
    ),
  ).toThrow();
});

test("notebook export uses nbformat and preserves metadata, sources and output", () => {
  const snapshot = {
    cells: [
      {
        id: "code",
        cell_type: "code",
        source: "42",
        metadata: {
          didaction_microscopes: {
            schema_version: 1,
            items: [{ id: "abc1234", title: "Demo" }],
          },
        },
        execution_count: 1,
        outputs: [{ kind: "text", text: "42" }],
      },
      {
        id: "md",
        cell_type: "markdown",
        source: "Hello",
        metadata: {},
        outputs: [],
        execution_count: null,
      },
    ],
  } as BrowserSnapshot;
  const raw = JSON.parse(new TextDecoder().decode(notebookBytes(snapshot)));
  expect(raw.nbformat).toBe(4);
  expect(raw.metadata.kernelspec.name).toBe("pyodide-314");
  expect(raw.cells[0].metadata).toEqual(snapshot.cells[0]!.metadata);
  expect(raw.cells[0].outputs[0].data["text/plain"]).toBe("42");
  expect(raw.cells[1]).not.toHaveProperty("outputs");
});

test("explorer hides owned sidecars, counts per notebook, retains unrelated artifacts", () => {
  const entries = [
    { name: "demo.ipynb", path: "a/demo.ipynb", type: "notebook" },
    {
      name: "demo.ipynb.012abcd.abc1234",
      path: "a/demo.ipynb.012abcd.abc1234",
      type: "file",
    },
    {
      name: "demo.ipynb.012abcd.def5678",
      path: "a/demo.ipynb.012abcd.def5678",
      type: "file",
    },
    { name: "data.csv", path: "a/data.csv", type: "file" },
  ];
  expect(visibleWorkspaceEntries(entries)).toEqual([
    { ...entries[0], microscopeCount: 2 },
    { ...entries[3], microscopeCount: 0 },
  ]);
});
