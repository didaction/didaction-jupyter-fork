import { expect, test, type Page } from "@playwright/test";

async function installTools(page: Page) {
  await page.addInitScript(() => {
    const tools: Record<string, { execute(input: unknown): Promise<unknown> }> =
      {};
    Object.defineProperty(document, "modelContext", {
      configurable: true,
      value: {
        registerTool(tool: {
          name: string;
          execute(input: unknown): Promise<unknown>;
        }) {
          tools[tool.name] = tool;
        },
        unregisterTool(name: string) {
          delete tools[name];
        },
      },
    });
    Object.assign(window, { collaborationTestTools: tools });
  });
}
async function call(
  page: Page,
  notebook: string,
  name: string,
  args: Record<string, unknown> = {},
) {
  return page.evaluate(
    async ({ notebook, name, args }) => {
      const tools = (
        window as unknown as {
          collaborationTestTools: Record<
            string,
            {
              execute(input: unknown): Promise<{
                isError: boolean;
                structuredContent: Record<string, unknown>;
              }>;
            }
          >;
        }
      ).collaborationTestTools;
      return tools[name]!.execute({ notebook_path: notebook, ...args });
    },
    { notebook, name, args },
  );
}

test("one driver; observers receive intermediate output and handoff reverses permissions", async ({
  page,
  context,
}) => {
  test.skip(
    !process.env.DIDACTION_BROWSER_GATEWAY,
    "Requires real isolated gateway and kernel",
  );
  const notebook = `collaboration-${crypto.randomUUID()}.ipynb`;
  const config = await (await page.request.get("/api/v1/config")).json();
  const join = await (
    await page.request.post("/api/v1/collaboration/join", {
      headers: { "x-notebook-path": notebook },
    })
  ).json();
  const headers = {
    "x-notebook-path": notebook,
    "x-notebook-client": join.token,
  };
  const created = await (
    await page.request.post("/api/v1/commands", {
      headers,
      data: {
        protocol_version: 1,
        type: "setup",
        path: notebook,
        kernel: config.kernel,
        create: true,
        command_id: crypto.randomUUID(),
        idempotency_key: crypto.randomUUID(),
        timeout_ms: 30000,
      },
    })
  ).json();
  expect(created.error).toBeNull();
  await page.request.post("/api/v1/collaboration/leave", { headers });
  await installTools(page);
  await page.goto(`/?notebook=${notebook}`);
  await expect(page.locator("#connection-status")).toContainText(
    "WebMCP ready",
    { timeout: 60000 },
  );
  const observer = await context.newPage();
  await installTools(observer);
  await observer.goto(`/?notebook=${notebook}`);
  await expect(observer.locator("#connection-status")).toContainText(
    "WebMCP ready",
    { timeout: 60000 },
  );
  const role = async (target: Page) =>
    (await call(target, notebook, "get_collaboration")).structuredContent;
  await expect.poll(async () => (await role(page)).is_driver).toBe(true);
  await expect.poll(async () => (await role(observer)).is_driver).toBe(false);
  const denied = await call(observer, notebook, "insert_cell", {
    index: 0,
    source: "unwanted = 1",
    cell_type: "code",
  });
  expect(denied.isError).toBe(true);
  expect(JSON.stringify(denied)).toContain("not_driver");
  const inserted = await call(page, notebook, "insert_cell", {
    index: 0,
    cell_type: "code",
    source:
      "import time\nfrom IPython.display import clear_output\nprint('intermediate-visible', flush=True)\ntime.sleep(2)\nclear_output(wait=False)\ntime.sleep(0.5)\nprint('latest-visible', flush=True)\ntime.sleep(2)",
  });
  expect(inserted.isError).toBe(false);
  const cellId = inserted.structuredContent.cell_id;
  let running = true;
  const received: string[] = [];
  observer.on("response", async (response) => {
    if (!response.url().includes("/collaboration/events")) return;
    try {
      if (running) received.push(await response.text());
    } catch {
      /* page may close */
    }
  });
  const execution = call(page, notebook, "execute_cell", {
    cell_id: cellId,
  }).then((result) => {
    running = false;
    return result;
  });
  await expect
    .poll(() =>
      received.some((text) => text.includes("intermediate-visible\\n")),
    )
    .toBe(true);
  expect(running).toBe(true);
  await observer.screenshot({
    path: ".runtime/collaboration-observer-stream.png",
  });
  expect((await execution).isError).toBe(false);
  const read = await call(observer, notebook, "read_cell", { cell_id: cellId });
  expect(JSON.stringify(read)).toContain("latest-visible");
  await observer.setViewportSize({ width: 520, height: 720 });
  await observer.screenshot({
    path: ".runtime/collaboration-observer-mobile.png",
  });
  const observerId = (await role(observer)).client_id;
  expect(
    (
      await call(page, notebook, "change_notebook_driver", {
        client_id: observerId,
      })
    ).isError,
  ).toBe(false);
  await expect.poll(async () => (await role(observer)).is_driver).toBe(true);
  await expect.poll(async () => (await role(page)).is_driver).toBe(false);
  await expect(observer.locator("#follow-driver")).toBeHidden();
  await expect(observer.locator("#driver-status")).toBeVisible();
  await expect(page.locator("#follow-driver")).toBeVisible();
  await expect(page.locator("#driver-status")).toBeHidden();
  if (process.env.DIDACTION_GATEWAY_IMPLEMENTATION === "rust") {
    await expect(page.locator("#browser-home")).toBeHidden();
    await expect(
      page.getByRole("button", { name: "Take control", exact: true }),
    ).toBeVisible();
    page.once("dialog", (dialog) => dialog.accept());
    await page
      .getByRole("button", { name: "Take control", exact: true })
      .click();
    await expect.poll(async () => (await role(page)).is_driver).toBe(true);
    await expect(
      observer.getByRole("button", { name: "Take control", exact: true }),
    ).toBeVisible();
    observer.once("dialog", (dialog) => dialog.accept());
    await observer
      .getByRole("button", { name: "Take control", exact: true })
      .click();
    await expect.poll(async () => (await role(observer)).is_driver).toBe(true);
    await observer
      .getByRole("button", { name: "Release driver", exact: true })
      .click();
    await expect(
      page.getByRole("button", { name: "Claim driver", exact: true }),
    ).toBeVisible();
    await expect(
      observer.getByRole("button", { name: "Claim driver", exact: true }),
    ).toBeVisible();
    await page.screenshot({ path: ".runtime/header-claim-driver.png" });
    expect(
      (await call(observer, notebook, "delete_cell", { cell_id: cellId }))
        .isError,
    ).toBe(true);
    await page
      .getByRole("button", { name: "Claim driver", exact: true })
      .click();
    await expect.poll(async () => (await role(page)).is_driver).toBe(true);
    await page.screenshot({ path: ".runtime/header-release-driver.png" });
    await expect(
      observer.getByRole("button", { name: "Take control", exact: true }),
    ).toBeVisible();
    await page
      .getByRole("button", { name: "Release driver", exact: true })
      .click();
    await observer
      .getByRole("button", { name: "Claim driver", exact: true })
      .click();
    await expect.poll(async () => (await role(observer)).is_driver).toBe(true);
  }
  await observer.screenshot({ path: ".runtime/driver-indicator-mobile.png" });
  expect(
    (await call(page, notebook, "delete_cell", { cell_id: cellId })).isError,
  ).toBe(true);
  expect(
    (
      await call(observer, notebook, "overwrite_cell_source", {
        cell_id: cellId,
        source:
          "print('before-disconnect', flush=True)\ntime.sleep(3)\nprint('after-disconnect', flush=True)",
      })
    ).isError,
  ).toBe(false);
  const progress = (marker: string) =>
    page.waitForResponse(
      async (response) => {
        if (!response.url().includes("/collaboration/events")) return false;
        try {
          return (await response.text()).includes(`${marker}\\n`);
        } catch {
          return false;
        }
      },
      { timeout: 30000 },
    );
  const started = progress("before-disconnect");
  const detached = call(observer, notebook, "execute_cell", {
    cell_id: cellId,
  }).catch(() => null);
  await started;
  const completed = progress("after-disconnect");
  await observer.close();
  await completed;
  await detached;
  await expect
    .poll(async () => (await role(page)).is_driver, { timeout: 55000 })
    .toBe(true);
  expect(
    (await call(page, notebook, "delete_cell", { cell_id: cellId })).isError,
  ).toBe(false);
});
