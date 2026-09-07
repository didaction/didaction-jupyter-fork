import { expect, test } from "vitest";
import { kernelFromNotebookMetadata } from "./browser-artifacts";

test("browser kernel selection follows exact notebook kernelspec metadata", () => {
  expect(
    kernelFromNotebookMetadata({ kernelspec: { name: "pyodide-027" } }),
  ).toBe("pyodide-027");
  expect(kernelFromNotebookMetadata({ kernelspec: { name: "python3" } })).toBe(
    "pyodide-314",
  );
  expect(
    kernelFromNotebookMetadata({
      kernelspec: { name: "xeus-python-019" },
    }),
  ).toBe("xeus-python-019");
});
