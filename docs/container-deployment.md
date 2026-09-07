# Jupyter runtime + didaction sidecar

The runtime is an existing Jupyter Server image with installed kernelspecs. The
sidecar serves egui/WASM and the gateway from one origin. Transport is unchanged:
browser HTTP/NDJSON; gateway Jupyter REST/kernel WebSocket. Neither container
receives the Docker socket. The gateway has no notebook filesystem mount.

The Rust gateway image is built on pull requests and published from `main` and
version tags as `ghcr.io/didaction/didaction-jupyter-fork-gateway`. Published
images include the server-mode frontend and use immutable `sha-<commit>` tags;
`latest` follows `main`.

The gateway is now the native Rust host. The kernel runtime still uses the
configured Jupyter Server image. For an explicit Python gateway rollback, use
`docker compose -f docker-compose.yml -f deploy/compose.python.yml up --build -d`.
Rust-specific verification and conservative retry behavior are documented in
[the migration history](investigations/rust-runtime-migration.md).

## Managed launch

```bash
DIDACTION_NOTEBOOK_WORKSPACE=/absolute/path/to/notebooks \
DIDACTION_NOTEBOOK_PATH=lesson.ipynb \
DIDACTION_KERNEL_NAME=python3 \
bash scripts/container.sh up
```

Default workspace: `.runtime/notebooks`. Default notebook:
`notebook-parity-demo.ipynb`. The folder must be writable by the runtime image's
user (Jupyter Docker Stacks normally uses UID 1000); on Linux arrange folder
ownership before launch. Do not recursively change ownership of an unrelated
host directory. The notebook path is relative to the mounted folder. Runtime
code can read/write this mount, including deleting files: back it up.

The wrapper generates a credential file once under `.runtime/secrets` if none
exists. The directory is private; the file is readable by the container's
non-root user. Supply your own readable file with `DIDACTION_JUPYTER_TOKEN_FILE`.
Compose grants that credential only to runtime and gateway. It is excluded from
the image build context and never returned by browser configuration.

`DIDACTION_PORT` sets the loopback published port (default 5173).
`DIDACTION_TIMEOUT_SECONDS` sets upstream HTTP timeout.
`DIDACTION_KERNEL_MEMORY` and `DIDACTION_KERNEL_CPUS` default to 4g and 2.
The command's separate execution timeout remains bounded by the protocol.

Use `docker compose ps` for health and `bash scripts/container.sh down` to stop.
The source-build image compiles the math renderer and needs substantial Docker
VM memory (allocate at least 8 GB). On smaller VMs, use
`DIDACTION_PREBUILT_FRONTEND=1 bash scripts/container.sh up`: this builds the same
frontend with the host Rust/pnpm toolchain before packaging it into the gateway.
The gateway's readiness check probes Jupyter status, installed kernelspecs, and
Contents. A missing configured kernelspec is not reported ready. Actual kernel
launch is checked by the integration test, not repeatedly by health probes.

### Multiple server kernels with `djupctl`

`djupctl config` enables one or more server profiles and chooses the default.
`djupctl up` starts one isolated Jupyter container per enabled server profile,
mounts the same configured notebook workspace into each, and gives the gateway
a private, versioned routing manifest. Browser code never receives backend URLs
or the shared Jupyter credential.

Existing notebooks route by the conventional `metadata.kernelspec.name` field.
New notebooks record the default profile's kernelspec. A setup command may name
either a profile ID or an unambiguous kernelspec; an unavailable or ambiguous
kernelspec fails explicitly. The choice is therefore durable across gateway
restarts, while live kernel sessions remain isolated in their profile container.

## Other images / kernels

### Quantum-school demo

With the course checkout at `../summer-quantum-school26` and the gateway image
already built, run `bash scripts/quantum-school.sh up`. It builds a Qrisp 0.9.7
runtime, stops the current managed deployment, and opens the Day 1 notebook from
the course folder. Existing notebook files are preserved; kernel memory is lost
when switching deployments. Stop with `bash scripts/quantum-school.sh down`
(or `bash scripts/container.sh down`). Override `DIDACTION_NOTEBOOK_WORKSPACE`
and `DIDACTION_NOTEBOOK_PATH` to select another course folder or notebook.
Cloud-backend exercises still require your own credentials; none are bundled.

### Custom runtime

Set `DIDACTION_RUNTIME_IMAGE` to an immutable image digest and
`DIDACTION_KERNEL_NAME` to its installed kernelspec. The image must contain Python,
the `jupyter server` command, and the language kernel/dependencies. Our runtime
command replaces its default command, but preserves its entrypoint; images with
incompatible entrypoints need a small operator-owned Compose override. A bare
kernel executable is not sufficient. No packages are installed at startup.

The default image is pinned to multi-architecture manifest
`sha256:aebcf531fc77f3341568f5e37de7eb392ae48f1ae5ce9bc9cf779bd602548d17`.
Inspected Linux ARM64 packages: Jupyter Server 2.21.0, ipykernel 7.3.0,
JupyterLab 4.6.3. Host development retains the repository's separate uv.lock pins.
Re-pin an image only after running the container smoke test; other kernels may
have different completion, rich-output, and interrupt capabilities.

## Kernel environment and secrets

Non-sensitive settings can be put in a private env file and selected with
`DIDACTION_KERNEL_ENV_FILE`. It is applied only to the runtime, not the gateway.
Do not commit secrets to `deploy/kernel.env`.

Prefer files for secrets. Mount only the relevant secret files in a dedicated
directory and map their filenames to environment variables:

```bash
DIDACTION_KERNEL_SECRETS_DIR=/absolute/path/to/course-secrets \
DIDACTION_KERNEL_SECRET_ENV='{"IQM_TOKEN":"iqm-token"}' \
bash scripts/container.sh up
```

The mapping is validated and files are read by the runtime launcher. Connection
configuration cannot be overridden by this mapping. Secret values are never
stored in the image or kernelspec. However, runtime processes and notebook code
can read runtime secrets and can print or transmit them. Containers are not a
safe execution environment for hostile notebooks. Network egress is permitted.
All runtime kernels share the runtime's configured secret scope. Rotation
requires recreating the containers and restarting their kernels.

## Attach to an existing Jupyter Server

```bash
DIDACTION_JUPYTER_URL=https://jupyter.example/base/path \
DIDACTION_JUPYTER_TOKEN_FILE=/absolute/path/to/token \
DIDACTION_NOTEBOOK_PATH=lesson.ipynb \
DIDACTION_KERNEL_NAME=python3 \
docker compose -f deploy/compose.attach.yml up --build -d
```

The URL must be reachable from the container. Container localhost is not the
host machine. Standard token authentication and normal TLS verification are
supported; no disabled-verification option is provided. Custom authentication,
JupyterHub login flows, and mutual TLS are not implemented. The configured
notebook path is relative to that server's Contents root; the sidecar cannot
remap the remote server's filesystem or provision its kernels/secrets.
Stop with the same Compose file and `down`.

Only the operator configures connections. The browser cannot forward arbitrary
Jupyter requests or choose arbitrary hosts. This is still local, single-user
software, not an authenticated remote multi-user deployment.

## Verification

`scripts/check.sh` runs native/WASM/frontend/Python/browser/real-kernel checks.
After building `didaction-gateway:local`, run
`bash scripts/container-check.sh` for a real container execution/streaming,
download, and browser mount check. It uses a separate Compose project and
temporary notebook folder and stops its containers on exit.
