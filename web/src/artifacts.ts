import { readWorkspaceZip, ZIP_LIMIT } from "./workspace-zip";

/** Workspace file management is separate from notebook execution, with one bounded adapter. */
export interface ArtifactRequest {
  path: string;
  kind: "directory" | "notebook" | "file";
  content_base64?: string;
}
export interface ArtifactTransport {
  create(request: ArtifactRequest): Promise<void>;
}
export const MAX_ARTIFACT_BYTES = 1_000_000;
export function artifactPath(directory: string, name: string): string {
  const path = directory ? `${directory}/${name}` : name;
  if (
    !name ||
    name.includes("/") ||
    path.length > 512 ||
    /[\\%?#:\x00-\x1f]/.test(path) ||
    path.split("/").some((p) => !p || p.startsWith("."))
  )
    throw new Error(
      "Choose a name without slashes, hidden prefixes, or path control characters.",
    );
  return path;
}
export async function uploadRequest(
  directory: string,
  file: File,
): Promise<ArtifactRequest> {
  if (file.size > MAX_ARTIFACT_BYTES)
    throw new Error("Files must be 1 MB or smaller.");
  const path = artifactPath(directory, file.name);
  const bytes = new Uint8Array(await file.arrayBuffer());
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return {
    path,
    kind: file.name.endsWith(".ipynb") ? "notebook" : "file",
    content_base64: btoa(binary),
  };
}

function nestedArtifactPath(directory: string, relative: string): string {
  return relative
    .split("/")
    .reduce((parent, name) => artifactPath(parent, name), directory);
}

/** Expand a bounded ZIP into the same create-only requests used by ordinary uploads. */
export async function uploadRequests(
  directory: string,
  file: File,
): Promise<ArtifactRequest[]> {
  if (!file.name.toLowerCase().endsWith(".zip"))
    return [await uploadRequest(directory, file)];
  if (file.size > ZIP_LIMIT)
    throw new Error("ZIP files must be 20 MB or smaller.");
  const entries = await readWorkspaceZip(await file.arrayBuffer());
  if (!entries.length)
    throw new Error("ZIP contains no importable workspace files.");

  const directories = new Set<string>();
  for (const entry of entries) {
    const parts = entry.path.split("/");
    const limit = entry.directory ? parts.length : parts.length - 1;
    for (let index = 1; index <= limit; index++)
      directories.add(parts.slice(0, index).join("/"));
  }
  const requests: ArtifactRequest[] = [...directories]
    .sort((left, right) => {
      const depth = left.split("/").length - right.split("/").length;
      return depth || left.localeCompare(right);
    })
    .map((path) => ({
      path: nestedArtifactPath(directory, path),
      kind: "directory",
    }));
  for (const entry of entries) {
    if (entry.directory) continue;
    let binary = "";
    for (const byte of entry.bytes) binary += String.fromCharCode(byte);
    requests.push({
      path: nestedArtifactPath(directory, entry.path),
      kind: entry.path.toLowerCase().endsWith(".ipynb") ? "notebook" : "file",
      content_base64: btoa(binary),
    });
  }
  return requests;
}
export class HttpArtifactTransport implements ArtifactTransport {
  constructor(private readonly headers: () => Record<string, string>) {}
  async create(request: ArtifactRequest): Promise<void> {
    const response = await fetch("/api/v1/artifacts", {
      method: "POST",
      headers: { ...this.headers(), "content-type": "application/json" },
      body: JSON.stringify(request),
      signal: AbortSignal.timeout(65000),
    });
    if (!response.ok) {
      const value = await response.json().catch(() => null);
      throw new Error(
        value?.message ??
          "Upload was not confirmed. Refresh before retrying; check the file size and gateway support.",
      );
    }
  }
}
