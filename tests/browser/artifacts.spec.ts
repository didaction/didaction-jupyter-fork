import { expect, test } from "@playwright/test";
import { zipFixture } from "../fixtures/workspace-zip";

test("driver creates nested workspace items and uploads a notebook without overwriting", async ({
  page,
  context,
}) => {
  test.skip(
    !process.env.DIDACTION_BROWSER_GATEWAY,
    "Requires isolated Rust gateway",
  );
  await page.goto("/");
  await expect(page.locator("#connection-status")).toContainText("Connected");
  const create = async (kind: string, name: string) => {
    await page.locator("#artifact-kind").selectOption(kind);
    await page.locator("#artifact-name").fill(name);
    await page.getByRole("button", { name: "Create", exact: true }).click();
  };
  await expect(page.locator("#artifact-name")).toBeEnabled();
  await create("directory", "browser-uploads");
  await page
    .getByRole("button", { name: "browser-uploads", exact: true })
    .click();
  await expect(page.locator("#folder-path")).toHaveText(
    "Workspace / browser-uploads",
  );
  await create("directory", "nested");
  await page.getByRole("button", { name: "nested", exact: true }).click();
  await create("notebook", "new-notebook");
  await expect(
    page.getByRole("button", { name: "new-notebook.ipynb", exact: true }),
  ).toBeVisible();
  await page.locator("#artifact-upload").setInputFiles([
    { name: "data.csv", mimeType: "text/csv", buffer: Buffer.from("x,y\n1,2") },
    {
      name: "uploaded.ipynb",
      mimeType: "application/json",
      buffer: Buffer.from(
        JSON.stringify({
          nbformat: 4,
          nbformat_minor: 5,
          metadata: {},
          cells: [],
        }),
      ),
    },
  ]);
  await expect(
    page.getByRole("button", { name: "uploaded.ipynb", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "data.csv", exact: true }),
  ).toBeVisible();
  await page.locator("#folder-up").click();
  await page.locator("#artifact-upload").setInputFiles({
    name: "import.zip",
    mimeType: "application/zip",
    buffer: zipFixture([
      { name: "zip-course/", text: "" },
      {
        name: "zip-course/imported.ipynb",
        text: JSON.stringify({
          nbformat: 4,
          nbformat_minor: 5,
          metadata: {},
          cells: [],
        }),
      },
      { name: "zip-course/.DS_Store", text: "ignored" },
    ]),
  });
  await page.getByRole("button", { name: "zip-course", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "imported.ipynb", exact: true }),
  ).toBeVisible();
  await page.locator("#folder-up").click();
  await page.getByRole("button", { name: "nested", exact: true }).click();
  await create("file", "data.csv");
  await expect(page.locator("#explorer-status")).toContainText(
    "already exists",
  );
  await page.screenshot({ path: ".runtime/artifacts-desktop.png" });
  const observer = await context.newPage();
  await observer.goto("/");
  await expect(observer.locator("#connection-status")).toContainText(
    "Connected",
  );
  await expect(observer.locator("#artifact-name")).toBeDisabled();
  await expect(observer.locator("#artifact-upload")).toBeDisabled();
  await observer.close();
  await page
    .getByRole("button", { name: "uploaded.ipynb", exact: true })
    .click();
  await expect(page).toHaveURL(
    /notebook=browser-uploads%2Fnested%2Fuploaded.ipynb/,
  );
});
