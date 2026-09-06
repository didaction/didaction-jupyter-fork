import { expect, test } from "@playwright/test";

test("browser transport renames notebooks that own microscope sidecars", async ({
  page,
}) => {
  await page.goto("/");
  await page.getByRole("button", { name: "Open demo workspace" }).click();
  const outcome = await page.evaluate(async () => {
    const { NotebookApplication, microscopeDocument } = await import(
      String("/pkg/notebook_wasm.js")
    );
    const { BrowserNotebookTransport, initialBrowserSnapshot } = await import(
      String("/src/browser-transport.ts")
    );
    const saved = initialBrowserSnapshot("owned.ipynb");
    saved.cells = [
      {
        id: "code",
        cell_type: "code",
        source: "42",
        metadata: {
          didaction_microscopes: {
            schema_version: 1,
            items: [{ id: "abc1234", title: "Owned view" }],
          },
        },
        execution_count: null,
        outputs: [],
      },
    ];
    const { IndexedNotebookStore } = await import(
      String("/src/browser-store.ts")
    );
    const store = new IndexedNotebookStore(crypto.randomUUID());
    await store.write("owned.ipynb", saved);
    const identity = JSON.parse(
      microscopeDocument(JSON.stringify(saved), "code", "abc1234", undefined),
    );
    identity.document.walkthrough = {
      title: "Owned view",
      steps: [
        { id: "one", title: "One", code: "42", description: "Forty two" },
      ],
    };
    await store.commitMicroscope(
      saved,
      identity.path,
      JSON.stringify(identity.document),
    );
    const kernel = {
      request: async () => ({}),
      interrupt: () => {},
      restart: async () => {},
      close: () => {},
    };
    const transport = new BrowserNotebookTransport(
      "owned.ipynb",
      store,
      kernel,
      (snapshot: string) => new NotebookApplication(snapshot),
    );
    const command = (type: string, fields: Record<string, unknown> = {}) => ({
      protocol_version: 1,
      command_id: crypto.randomUUID(),
      idempotency_key: crypto.randomUUID(),
      timeout_ms: 30000,
      type,
      ...fields,
    });
    const setup = await transport.setup(
      command("setup", {
        path: "owned.ipynb",
        kernel: "pyodide-314",
        create: false,
      }),
    );
    const result = await transport.rename(
      command("rename_notebook", {
        path: "renamed.ipynb",
        expected_revision: 0,
      }),
    );
    const artifacts = await store.artifacts();
    const old = await store.read("owned.ipynb");
    const renamed = await store.read("renamed.ipynb");
    await store.close();
    return {
      setup,
      result,
      old,
      renamed,
      artifacts: artifacts.map((file: { path: string; bytes: Uint8Array }) => ({
        path: file.path,
        document: JSON.parse(new TextDecoder().decode(file.bytes)),
      })),
    };
  });
  expect(outcome.setup.error).toBeUndefined();
  expect(outcome.setup.snapshot.cells).toHaveLength(1);
  expect(outcome.result.error).toBeUndefined();
  expect(outcome.result.snapshot.notebook.path).toBe("renamed.ipynb");
  expect(outcome.old).toBeUndefined();
  expect(outcome.renamed.notebook.path).toBe("renamed.ipynb");
  expect(outcome.artifacts).toHaveLength(1);
  expect(outcome.artifacts[0].path).toMatch(/^renamed\.ipynb\./);
  expect(outcome.artifacts[0].document.notebook_path).toBe("renamed.ipynb");
});

test("browser transport validates in WASM and preserves state across storage failures and duplicate execution", async ({
  page,
}) => {
  await page.goto("/");
  await page.getByRole("button", { name: "Open demo workspace" }).click();
  await expect(page.locator("#connection-status")).toContainText(
    "Browser kernel",
  );
  const outcome = await page.evaluate(async () => {
    const { NotebookApplication } = await import(
      String("/pkg/notebook_wasm.js")
    );
    const { BrowserNotebookTransport } = await import(
      String("/src/browser-transport.ts")
    );
    let saved: unknown;
    let failSave = false;
    let executions = 0;
    const store = {
      read: async () => saved,
      write: async (_path: string, snapshot: unknown) => {
        if (failSave) throw new Error("Injected storage failure");
        saved = structuredClone(snapshot);
      },
      rename: async () => {},
      list: async () => ({ directory: "", entries: [] }),
    };
    const kernel = {
      request: async () => {
        executions++;
        return { execution_count: executions };
      },
      interrupt: () => {},
      restart: async () => {},
      close: () => {},
    };
    const transport = new BrowserNotebookTransport(
      "contract.ipynb",
      store,
      kernel,
      (snapshot: string) => new NotebookApplication(snapshot),
    );
    const command = (type: string, fields: Record<string, unknown> = {}) => ({
      protocol_version: 1,
      command_id: crypto.randomUUID(),
      idempotency_key: crypto.randomUUID(),
      timeout_ms: 30000,
      type,
      ...fields,
    });
    await transport.setup(
      command("setup", {
        path: "contract.ipynb",
        kernel: "pyodide-314",
        create: true,
      }),
    );
    const insert = command("modify_cells", {
      expected_revision: 0,
      changes: [
        {
          operation: "insert",
          index: 0,
          cell: {
            id: "code",
            cell_type: "code",
            source: "42",
            metadata: {},
            execution_count: null,
            outputs: [],
          },
        },
      ],
    });
    const inserted = await transport.modifyCells(insert);
    const duplicateInsert = await transport.modifyCells(insert);
    failSave = true;
    const failed = await transport.modifyCells(
      command("modify_cells", {
        expected_revision: 1,
        changes: [{ operation: "update", cell_id: "code", source: "99" }],
      }),
    );
    failSave = false;
    const restored = await transport.query(command("query", { query: "full" }));
    const execute = command("execute_cell", {
      cell_id: "code",
      expected_revision: 1,
    });
    const executed = await transport.execute(execute);
    const repeated = await transport.execute(execute);
    failSave = true;
    const uncertain = command("execute_cell", {
      cell_id: "code",
      expected_revision: 2,
    });
    await transport.execute(uncertain);
    await transport.execute(uncertain);
    failSave = false;
    const invalid = await transport.query(
      command("query", { protocol_version: 99, query: "full" }),
    );
    const unsupported = await transport.execute(
      command("execute_code", { code: "42" }),
    );
    return {
      inserted,
      duplicateInsert,
      failed,
      restored,
      executed,
      repeated,
      executions,
      invalid,
      unsupported,
    };
  });
  expect(outcome.duplicateInsert).toEqual(outcome.inserted);
  expect(outcome.failed.error).toBeTruthy();
  expect(outcome.restored.snapshot.cells[0].source).toBe("42");
  expect(outcome.restored.snapshot.revision).toBe(1);
  expect(outcome.repeated).toEqual(outcome.executed);
  expect(outcome.executions).toBe(2);
  expect(outcome.invalid.error).toBeTruthy();
  expect(outcome.unsupported.error.code).toBe("unsupported_operation");
});
