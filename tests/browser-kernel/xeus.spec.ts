import { expect, test } from "@playwright/test";
import { existsSync } from "node:fs";
import {
  installMicroscopeTools,
  microscopeCall,
} from "../fixtures/microscope-tools";

test.skip(
  !existsSync("web/public/xeus/worker.js"),
  "Optional runtime: run pnpm prepare:xeus first",
);

test("xeus worker: completion, inspection, async streaming, files, restart and interrupt", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const { WorkerKernel } = await import(String("/src/browser-kernel.ts"));
    const kernel = new WorkerKernel(
      async () => ({
        files: [
          {
            path: "lesson/data.txt",
            directory: false,
            bytes: new TextEncoder().encode("uploaded data"),
          },
        ],
        directory: "lesson",
      }),
      "xeus-python-019",
    );
    const events: { type: string; bundle?: Record<string, unknown> }[] = [];
    const execute = (code: string) =>
      kernel.request(
        "execute",
        code,
        0,
        30_000,
        (event: (typeof events)[number]) => events.push(event),
      );
    try {
      await execute(
        "import numpy, scipy, pandas, matplotlib, networkx, sympy, ipywidgets\nprint('scientific-baseline', numpy.__version__, scipy.__version__, pandas.__version__)",
      );
      await execute("value = 40 + 2\nvalue");
      const complete = await kernel.request("complete", "str.up", 6, 30_000);
      const inspect = await kernel.request("inspect", "len", 3, 30_000);
      await execute("print(open('data.txt').read())");
      let finished = false;
      let intermediate = false;
      const asyncReply = await kernel.request(
        "execute",
        "import asyncio\nfrom IPython.display import display, clear_output\nprint('first', flush=True)\nawait asyncio.sleep(0.15)\nclear_output(wait=True)\ndisplay('latest', display_id='sample')\nawait asyncio.sleep(0.15)\ndisplay('updated', display_id='sample', update=True)",
        0,
        30_000,
        (event: (typeof events)[number]) => {
          if (!finished && event.type === "stream") intermediate = true;
          events.push(event);
        },
      );
      finished = true;
      await kernel.restart();
      const missing = await execute("value");
      const running = execute("while True: pass").catch(
        (error: Error) => error.message,
      );
      setTimeout(() => kernel.interrupt(), 300);
      const interrupted = await running;
      await kernel.restart();
      const recovered = await execute("6 * 7");
      return {
        events,
        complete,
        inspect,
        intermediate,
        asyncReply,
        missing,
        interrupted,
        recovered,
      };
    } finally {
      kernel.close();
    }
  });
  expect(result.complete.matches).toContain(".upper");
  expect(result.complete.cursor_start).toBe(3);
  expect(result.inspect.found).toBe(true);
  expect(result.intermediate).toBe(true);
  expect(result.asyncReply).toMatchObject({ status: "ok" });
  expect(result.events.map((event) => event.type)).toEqual(
    expect.arrayContaining(["clear_output", "update_display_data"]),
  );
  expect(JSON.stringify(result.events)).toContain("uploaded data");
  expect(JSON.stringify(result.events)).toContain("scientific-baseline");
  expect(result.missing).toMatchObject({ status: "error" });
  expect(result.missing.ename).toContain("NameError");
  expect(result.interrupted).toContain("variables were lost");
  expect(result.recovered).toMatchObject({ status: "ok" });
});

test("real xeus worker through egui and WebMCP: execute, plot and persist", async ({
  page,
}) => {
  await page.setViewportSize({ width: 1280, height: 1000 });
  const forbidden: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (
      url.pathname.startsWith("/api/") ||
      (url.protocol.startsWith("http") && url.hostname !== "127.0.0.1")
    )
      forbidden.push(url.href);
  });
  await installMicroscopeTools(page);
  await page.goto("/?kernel=xeus-python-019");
  await page.getByRole("button", { name: "Open demo workspace" }).click();
  await expect(page.locator("#notebook-canvas")).toBeVisible();
  await expect(page.locator("#connection-status")).toContainText(
    "WebMCP ready",
  );
  const call = (name: string, args: Record<string, unknown>) =>
    microscopeCall(page, name, {
      notebook_path: "didaction-runtime-tour.ipynb",
      ...args,
    });
  const first = await call("insert_execute_code_cell", {
    index: 0,
    source: "xeus_value = 40 + 2\nxeus_value",
  });
  expect(first).toMatchObject({ isError: false });
  expect(JSON.stringify(first)).toContain("42");
  const plot = await call("insert_execute_code_cell", {
    index: 1,
    source:
      "import matplotlib.pyplot as plt\nplt.bar(['Xeus', 'Python'], [42, 26])\nplt.title('Xeus-Python in the browser')\nplt.show()",
  });
  expect(plot).toMatchObject({ isError: false });
  expect(JSON.stringify(plot)).toContain("image/png");
  expect(
    JSON.stringify(
      await call("read_cell", { cell_id: first.structuredContent.cell_id }),
    ),
  ).toContain('"execution_count":1');
  await page.waitForTimeout(500);
  await page.screenshot({ path: ".runtime/xeus-notebook.png", fullPage: true });
  await page.reload();
  await expect(page.locator("#connection-status")).toContainText(
    "WebMCP ready",
  );
  const saved = await call("read_cell", {
    cell_id: first.structuredContent.cell_id,
  });
  expect(JSON.stringify(saved)).toContain("42");
  expect(page.url()).toContain("kernel=xeus-python-019");
  expect(forbidden).toEqual([]);
});
