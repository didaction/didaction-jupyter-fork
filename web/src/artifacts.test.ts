import { test } from "vitest";
import assert from "node:assert/strict";
import { artifactPath, uploadRequest, uploadRequests } from "./artifacts";
import { zipFixture } from "../../tests/fixtures/workspace-zip";

test("artifact names stay in their displayed folder", () => {
  assert.equal(
    artifactPath("lesson/subfolder", "data.csv"),
    "lesson/subfolder/data.csv",
  );
  for (const name of [
    "",
    "../x",
    "/x",
    ".secret",
    "a%2fb",
    "a\\b",
    "a?b",
    "a:b",
  ])
    assert.throws(() => artifactPath("lesson", name));
});
test("uploads preserve binary bytes and classify notebooks", async () => {
  const file = new File([new Uint8Array([0, 255, 1])], "data.bin");
  assert.deepEqual(await uploadRequest("folder", file), {
    path: "folder/data.bin",
    kind: "file",
    content_base64: "AP8B",
  });
  assert.equal(
    (await uploadRequest("", new File(["{}"], "lesson.ipynb"))).kind,
    "notebook",
  );
  await assert.rejects(
    uploadRequest("", new File([new Uint8Array(1_000_001)], "big.bin")),
  );
});
test("ZIP uploads create implied folders before bounded files", async () => {
  const zip = zipFixture([
    {
      name: "course/lesson.ipynb",
      text: '{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[]}',
    },
    { name: "course/data.csv", text: "x,y\n1,2" },
    { name: "course/.DS_Store", text: "ignored" },
  ]);
  const requests = await uploadRequests(
    "imports",
    new File([zip], "course.zip", { type: "application/zip" }),
  );
  assert.deepEqual(
    requests.map(({ path, kind }) => ({ path, kind })),
    [
      { path: "imports/course", kind: "directory" },
      { path: "imports/course/lesson.ipynb", kind: "notebook" },
      { path: "imports/course/data.csv", kind: "file" },
    ],
  );
});
