import { expect, test } from "@playwright/test";
import { readFile } from "node:fs/promises";
import { readWorkspaceZip } from "../../web/src/workspace-zip";
import { waveGraphics } from "../fixtures/graphics";
import {
  installMicroscopeTools,
  microscopeCall,
} from "../fixtures/microscope-tools";

test("browser workspace export bundles notebooks and microscopes; explorer stays compact", async ({
  page,
  browser,
}) => {
  await page.setViewportSize({ width: 1280, height: 900 });
  await installMicroscopeTools(page);
  await page.goto("/");
  await page.getByRole("button", { name: "Open demo workspace" }).click();
  await expect(page.locator("#connection-status")).toContainText(
    "WebMCP ready",
  );
  const scope = {
    notebook_path: "didaction-runtime-tour.ipynb",
    cell_id: "614e0f26-970e-438f-85a4-68e4db687acc",
  };
  const result = await microscopeCall(page, "create_microscope", {
    ...scope,
    title: "Export demo",
    walkthrough: {
      title: "Export demo",
      steps: [
        {
          id: "one",
          title: "One",
          code: "42",
          description: "A saved explanation",
          graphics_regions: [
            {
              id: "waves",
              bounds: { x: 500, y: 100, width: 450, height: 400 },
              ...waveGraphics,
              artifact: "waves.ts",
            },
          ],
        },
      ],
    },
  });
  expect(result.isError).toBe(false);
  await expect(
    page
      .getByRole("button", { name: "didaction-runtime-tour.ipynb" })
      .locator(".microscope-count"),
  ).toHaveText("4");
  await expect(page.locator("#notebook-files li")).toHaveCount(2);
  const downloadPromise = page.waitForEvent("download");
  await page
    .getByRole("button", { name: "Export workspace", exact: true })
    .click();
  const download = await downloadPromise;
  const bytes = await readFile((await download.path())!);
  const entries = await readWorkspaceZip(Uint8Array.from(bytes).buffer);
  const notebook = JSON.parse(
    new TextDecoder().decode(
      entries.find((e) => e.path === scope.notebook_path)!.bytes,
    ),
  );
  expect(notebook.nbformat).toBe(4);
  const graphics = entries.find((e) => e.path.endsWith(".waves.ts"));
  expect(new TextDecoder().decode(graphics!.bytes)).toBe(waveGraphics.source);
  expect(graphics!.path).toContain(
    `${result.structuredContent.microscope_id}.waves.ts`,
  );
  await expect(page.locator("#notebook-name")).toHaveText(
    "didaction-runtime-tour.ipynb",
  );
  await expect(page.locator("#browser-home")).toBeVisible();
  expect(
    entries.some((e) =>
      e.path.endsWith(String(result.structuredContent.microscope_id)),
    ),
  ).toBe(true);
  for (const width of [1280, 846]) {
    await page.setViewportSize({ width, height: 900 });
    await page.screenshot({ path: `.runtime/workspace-controls-${width}.png` });
  }
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.mouse.click(270, 102);
  await page.screenshot({ path: ".runtime/browser-checkpoint-disabled.png" });
  // Restore into independent browser storage, not over the original workspace.
  const restoredContext = await browser.newContext();
  try {
    const restored = await restoredContext.newPage();
    await installMicroscopeTools(restored);
    await restored.goto(new URL("/", page.url()).href);
    await expect(
      restored.getByRole("heading", { name: "Choose a workspace" }),
    ).toBeVisible();
    await restored.locator("#browser-zip").setInputFiles({
      name: "workspace.zip",
      mimeType: "application/zip",
      buffer: bytes,
    });
    await expect(restored.locator("#connection-status")).toContainText(
      "WebMCP ready",
    );
    const read = await microscopeCall(restored, "read_microscope", {
      ...scope,
      microscope_id: result.structuredContent.microscope_id,
    });
    expect(read.isError).toBe(false);
    expect(JSON.stringify(read)).toContain("A saved explanation");
  } finally {
    await restoredContext.close();
  }
  await page.keyboard.press("Escape");
  page.once("dialog", (dialog) => dialog.accept());
  await page
    .getByRole("button", { name: "Choose another environment" })
    .click();
  await expect(
    page.getByRole("heading", { name: "Choose a workspace and kernel" }),
  ).toBeVisible();
  await expect(page.locator("#browser-home")).toBeHidden();
  await page.getByRole("button", { name: "Continue saved workspace" }).click();
  await expect(page.locator("#connection-status")).toContainText(
    "WebMCP ready",
  );
  expect(
    (
      await microscopeCall(page, "read_microscope", {
        ...scope,
        microscope_id: result.structuredContent.microscope_id,
      })
    ).isError,
  ).toBe(false);
});
