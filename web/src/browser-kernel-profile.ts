export const BROWSER_KERNELS = {
  "pyodide-314": {
    label: "Pyodide 314.0.5 · Python 3.14",
    displayName: "Python 3.14 (Pyodide 314.0.5)",
  },
  "pyodide-027": {
    label: "Pyodide 0.27.7 · Python 3.12",
    displayName: "Python 3.12 (Pyodide 0.27.7)",
  },
  "xeus-python-019": {
    label: "xeus-python 0.19.0 · Python 3.13",
    displayName: "Python 3.13 (xeus-python 0.19.0)",
  },
} as const;

export type BrowserKernelName = keyof typeof BROWSER_KERNELS;
export const DEFAULT_BROWSER_KERNEL: BrowserKernelName = "pyodide-314";
export function isBrowserKernelName(value: string): value is BrowserKernelName {
  return value in BROWSER_KERNELS;
}

export function storedBrowserKernel(value: unknown): BrowserKernelName | null {
  return typeof value === "string" && isBrowserKernelName(value) ? value : null;
}
