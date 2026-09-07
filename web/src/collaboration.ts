import type { NotebookSnapshot } from "./types";
import {
  validateFollowView,
  type FollowTransport,
  type FollowView,
  type FollowPublisher,
  type FollowPosition,
} from "./follow";

export interface CollaborationState {
  notebook_path: string;
  client_id: string;
  driver_id: string | null;
  is_driver: boolean;
  clients: string[];
  sequence: number;
  origin: string | null;
  snapshot: NotebookSnapshot | null;
}

// One private workspace capability per page, shared across notebook subscriptions.
// Serialize joins so concurrent opens/reconnects cannot create competing identities.
let workspaceToken = "";
let joinTail: Promise<unknown> = Promise.resolve();

/** HTTP adapter; ownership policy and fanout live in the gateway, not here. */
export class NotebookCollaboration implements FollowTransport, FollowPublisher {
  private token = "";
  private stopped = false;
  private controller = new AbortController();
  state?: CollaborationState;
  private published = "";
  private publishing = false;
  private publishedAt = 0;
  constructor(
    private path: string,
    private readonly resolveTarget: (
      path: string,
    ) => NotebookCollaboration | undefined = () => undefined,
  ) {}
  rename(path: string): void {
    this.path = path;
  }
  headers(): Record<string, string> {
    return {
      "x-notebook-path": encodeURIComponent(this.path),
      "x-notebook-client": this.token,
    };
  }
  async publish(position: FollowPosition): Promise<void> {
    const target =
      position.notebook_path === this.path
        ? this
        : this.resolveTarget(position.notebook_path);
    if (!target) return;
    if (
      !this.state?.is_driver ||
      !target.state?.is_driver ||
      this.publishing ||
      this.stopped
    )
      return;
    const view = {
      protocol_version: 1,
      notebook_path: target.path,
      scroll_fraction: Math.round(position.scroll_fraction * 1000) / 1000,
      selected_cell_id: position.selected_cell_id ?? null,
      microscope: position.microscope ?? null,
    };
    const key = JSON.stringify(view);
    if (key === this.published && performance.now() - this.publishedAt < 2000)
      return;
    this.publishing = true;
    try {
      const response = await fetch("/api/v1/collaboration/view", {
        method: "POST",
        headers: {
          ...this.headers(),
          "content-type": "application/json",
          "x-notebook-target-client": target.token,
        },
        body: key,
        signal: AbortSignal.any([
          this.controller.signal,
          AbortSignal.timeout(5000),
        ]),
      });
      if (response.ok) {
        this.published = key;
        this.publishedAt = performance.now();
      }
    } catch {
      /* The main membership stream owns connection recovery. */
    } finally {
      this.publishing = false;
    }
  }
  subscribe(receive: (view: FollowView | null) => void): () => void {
    const controller = new AbortController();
    void (async () => {
      let sequence = -1;
      while (!controller.signal.aborted && !this.stopped) {
        try {
          const response = await fetch(
            `/api/v1/collaboration/view?after=${sequence}`,
            {
              headers: this.headers(),
              signal: AbortSignal.any([
                controller.signal,
                this.controller.signal,
                AbortSignal.timeout(15000),
              ]),
            },
          );
          if (!response.ok) throw new Error("Follow connection unavailable");
          const text = await response.text();
          if (text.length > 2048) throw new Error("Follow event exceeds limit");
          const event = JSON.parse(text) as { sequence: number; view: unknown };
          if (controller.signal.aborted || this.stopped) return;
          receive(event.view === null ? null : validateFollowView(event.view));
          sequence = event.sequence;
        } catch {
          if (controller.signal.aborted || this.stopped) return;
          receive(null);
          await new Promise((resolve) => setTimeout(resolve, 1000));
        }
      }
    })();
    return () => controller.abort();
  }
  async join(): Promise<void> {
    const run = async () => {
      const request = () =>
        fetch("/api/v1/collaboration/join", {
          method: "POST",
          headers: {
            "x-notebook-path": encodeURIComponent(this.path),
            "x-notebook-client": workspaceToken,
          },
          signal: this.controller.signal,
        });
      let response = await request();
      if (response.status === 403 && workspaceToken) {
        workspaceToken = "";
        response = await request();
      }
      if (!response.ok)
        throw new Error("Unable to join notebook collaboration");
      const { token, ...state } =
        (await response.json()) as CollaborationState & { token: string };
      this.token = token;
      workspaceToken = token;
      this.state = state;
    };
    const task = joinTail.then(run);
    joinTail = task.catch(() => undefined);
    await task;
  }
  async changeDriver(clientId: string): Promise<void> {
    const response = await fetch(
      `/api/v1/collaboration/driver/${encodeURIComponent(clientId)}`,
      {
        method: "POST",
        headers: this.headers(),
      },
    );
    if (!response.ok)
      throw new Error(
        "Driver handoff refused; wait until idle and select a connected collaborator",
      );
  }
  async setDriverPermission(action: "claim" | "release"): Promise<void> {
    const response = await fetch(`/api/v1/collaboration/${action}`, {
      method: "POST",
      headers: this.headers(),
      signal: AbortSignal.timeout(10000),
    });
    if (!response.ok)
      throw new Error(
        "Control change refused. Wait for active execution to finish, then try again.",
      );
  }
  async watch(
    onState: (state: CollaborationState) => void,
    onDisconnect: () => void,
  ): Promise<void> {
    let sequence = -1;
    while (!this.stopped) {
      try {
        const response = await fetch(
          `/api/v1/collaboration/events?after=${sequence}`,
          {
            headers: this.headers(),
            signal: AbortSignal.any([
              this.controller.signal,
              AbortSignal.timeout(15000),
            ]),
          },
        );
        if (response.status === 403) {
          onDisconnect();
          await this.join();
          sequence = -1;
          continue;
        }
        if (!response.ok) throw new Error("Collaboration disconnected");
        const text = await response.text();
        if (text.length > 4_000_000)
          throw new Error("Collaboration snapshot exceeds limit");
        const state = JSON.parse(text) as CollaborationState;
        if (state.notebook_path) this.path = state.notebook_path;
        onState(state);
        this.state = state;
        sequence = state.sequence;
      } catch {
        if (this.stopped) return;
        onDisconnect();
        await new Promise((resolve) => setTimeout(resolve, 1000));
      }
    }
  }
  async close(): Promise<void> {
    this.stopped = true;
    this.controller.abort();
    await fetch("/api/v1/collaboration/leave", {
      method: "POST",
      headers: this.headers(),
      keepalive: true,
    }).catch(() => undefined);
  }
}
