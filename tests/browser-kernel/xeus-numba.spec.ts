import { expect, test } from "@playwright/test";
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import {
  installMicroscopeTools,
  microscopeCall,
} from "../fixtures/microscope-tools";

test("real xeus Numba compiles numerical code and displays its output", async ({
  page,
}) => {
  const candidate = process.env.DIDACTION_XEUS_TEST_ASSETS;
  test.skip(
    !existsSync(resolve(candidate ?? "web/public/xeus", "worker.js")),
    "Run pnpm prepare:xeus first",
  );
  if (candidate) {
    await page.route("**/xeus/**", async (route) => {
      const relative = new URL(route.request().url()).pathname.split(
        "/xeus/",
      )[1]!;
      if (relative.includes("..")) throw new Error("Invalid test asset path");
      await route.fulfill({
        path: resolve(candidate, relative),
        headers: {
          "Cross-Origin-Embedder-Policy": "require-corp",
          "Cross-Origin-Resource-Policy": "same-origin",
        },
      });
    });
  }
  await page.setViewportSize({ width: 1280, height: 1000 });
  await installMicroscopeTools(page);
  await page.goto("/?kernel=xeus-python-019");
  await page.getByRole("button", { name: "Open demo workspace" }).click();
  await expect(page.locator("#connection-status")).toContainText(
    "WebMCP ready",
  );
  const result = await microscopeCall(page, "insert_execute_code_cell", {
    notebook_path: "didaction-runtime-tour.ipynb",
    index: 0,
    source: JSON.parse(
      readFileSync("examples/xeus-numba.ipynb", "utf8"),
    ).cells[1].source.join(""),
  });
  const cell = await microscopeCall(page, "read_cell", {
    notebook_path: "didaction-runtime-tour.ipynb",
    cell_id: result.structuredContent.cell_id,
  });
  expect(result, JSON.stringify({ result, cell })).toMatchObject({
    isError: false,
  });
  const output = JSON.stringify(result.structuredContent);
  expect(output).toContain("Compiled result: 285.0");
  expect(output).toContain("Nopython signatures: 1");
  const plot = await microscopeCall(page, "insert_execute_code_cell", {
    notebook_path: "didaction-runtime-tour.ipynb",
    index: 1,
    source: JSON.parse(
      readFileSync("examples/xeus-numba.ipynb", "utf8"),
    ).cells[2].source.join(""),
  });
  expect(plot).toMatchObject({ isError: false });
  expect(JSON.stringify(plot.structuredContent)).toContain("image/png");
  await page.waitForTimeout(250);
  await page.screenshot({ path: ".runtime/xeus-numba.png", fullPage: true });
});
