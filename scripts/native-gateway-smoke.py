"""Real native gateway contract checks. Only temporary notebooks from smoke.sh."""

import base64
import json
import os
import time
import uuid

import httpx


def command(kind: str, **kwargs: object) -> dict:
    return {
        "protocol_version": 1,
        "command_id": str(uuid.uuid4()),
        "idempotency_key": str(uuid.uuid4()),
        "expected_revision": None,
        "timeout_ms": 30000,
        "type": kind,
        **kwargs,
    }


with httpx.Client(base_url=os.environ["DIDACTION_GATEWAY_URL"], timeout=45) as client:
    path = "native-contract.ipynb"
    a = {"x-notebook-path": path}
    joined = client.post("/api/v1/collaboration/join", headers=a).json()
    a["x-notebook-client"] = joined["token"]
    assert joined["is_driver"], joined
    b = {"x-notebook-path": "other.ipynb"}
    observer = client.post("/api/v1/collaboration/join", headers=b).json()
    b["x-notebook-client"] = observer["token"]
    assert not observer["is_driver"], observer
    assert client.post("/api/v1/collaboration/claim", headers=b).status_code == 200
    assert client.post("/api/v1/collaboration/release", headers=a).status_code == 403
    assert client.post("/api/v1/collaboration/claim", headers=a).status_code == 200
    assert client.post("/api/v1/collaboration/release", headers=b).status_code == 403

    def call(kind: str, **kwargs: object) -> dict:
        value = client.post("/api/v1/commands", headers=a, json=command(kind, **kwargs)).json()
        assert not value.get("error"), value
        return value

    state = call("setup", path=path, kernel="python3", create=True)
    # Microscope metadata and its derived sidecar survive through real Contents.
    microscope_cell = state["snapshot"]["cells"][0]["id"]
    micro = {"cell_id": microscope_cell, "microscope_id": "micro01"}
    create_micro = command(
        "create_microscope",
        **micro,
        title="Closer look",
        walkthrough={
            "title": "Closer look",
            "steps": [{"id": "one", "title": "One", "code": "42", "markdown": "Explanation"}],
        },
    )
    created_micro = client.post("/api/v1/commands", headers=a, json=create_micro).json()
    assert not created_micro.get("error"), created_micro
    assert client.post("/api/v1/commands", headers=a, json=create_micro).json() == created_micro
    assert call("read_microscope", **micro)["microscope"]["microscope"]["title"] == "Closer look"
    walkthrough = {
        "title": "Explanation",
        "steps": [
            {
                "id": "first",
                "title": "Value",
                "code": "x = 42",
                "playground_code": "temporary_value = 40 + 2\nprint(temporary_value)",
                "markdown": "The value is **42**.",
                "graphics": {
                    "language": "assemblyscript-rgba-1",
                    "source": "export function render(): usize { return 0; }",
                    "description": "Source persistence test",
                    "artifact": "example.ts",
                },
                "annotations": [
                    {
                        "id": "value",
                        "start_line": 1,
                        "start_column": 1,
                        "end_column": 1,
                        "end_line": 1,
                        "text": "Assignment",
                        "color": "blue",
                    }
                ],
            }
        ],
    }
    update_micro = command("set_microscope_walkthrough", **micro, walkthrough=walkthrough)
    updated = client.post("/api/v1/commands", headers=a, json=update_micro).json()
    assert not updated.get("error"), updated
    assert updated["microscope"]["walkthrough"] == walkthrough
    assert client.post("/api/v1/commands", headers=a, json=update_micro).json() == updated
    assert call("read_microscope", **micro)["microscope"]["walkthrough"] == walkthrough
    # Temporary sessions use separate kernels and never create notebook files.
    pg_input = {**micro, "step_index": 0}
    denied_pg = client.post("/api/v1/playground", headers=b, json=pg_input)
    assert denied_pg.status_code == 403, denied_pg.text
    pg = client.post("/api/v1/playground", headers=a, json=pg_input).json()
    assert pg["snapshot"]["cells"][0]["source"] == walkthrough["steps"][0]["playground_code"]
    endpoint = f"/api/v1/playground/{pg['id']}/commands"
    executed_pg = client.post(
        endpoint, headers=a, json=command("execute_cell", cell_id="playground")
    ).json()
    assert not executed_pg.get("error"), executed_pg
    assert "42" in json.dumps(executed_pg["snapshot"]["cells"][0]["outputs"])
    completion_pg = client.post(
        endpoint, headers=a, json=command("complete", code="temporary_v", cursor_pos=11)
    ).json()
    assert "temporary_value" in completion_pg["completion"]["matches"], completion_pg
    streaming_source = (
        "from IPython.display import clear_output\nimport time\n"
        "print('early', flush=True)\ntime.sleep(0.6)\n"
        "clear_output(wait=True)\nprint('latest', flush=True)"
    )
    edit_stream = client.post(
        endpoint,
        headers=a,
        json=command(
            "modify_cells",
            changes=[
                {
                    "operation": "update",
                    "cell_id": "playground",
                    "source": streaming_source,
                    "metadata": None,
                }
            ],
        ),
    ).json()
    assert not edit_stream.get("error"), edit_stream
    frames = []
    with client.stream(
        "POST", endpoint + "/stream", headers=a, json=command("execute_cell", cell_id="playground")
    ) as response:
        for line in response.iter_lines():
            frames.append(json.loads(line))
    assert any(
        "early" in json.dumps(frame.get("snapshot", {}).get("cells", [{}])[0].get("outputs"))
        and frame["snapshot"]["kernel"]["state"] == "busy"
        for frame in frames
    ), frames
    final_outputs = json.dumps(frames[-1]["snapshot"]["cells"][0]["outputs"])
    assert "latest" in final_outputs and "early" not in final_outputs, frames[-1]
    denied_write = client.post(
        endpoint, headers=b, json=command("execute_cell", cell_id="playground")
    ).json()
    assert denied_write["error"]["code"] == "not_driver", denied_write
    assert (
        client.post("/api/v1/playground/close", headers=a, json={"id": pg["id"]}).status_code == 200
    )
    assert client.get("/api/v1/playground", headers=a).json() is None
    fresh_pg = client.post("/api/v1/playground", headers=a, json=pg_input).json()
    assert (
        client.post("/api/v1/playground/close", headers=a, json={"id": pg["id"]}).status_code == 400
    )
    assert client.get("/api/v1/playground", headers=a).json()["id"] == fresh_pg["id"]
    endpoint = f"/api/v1/playground/{fresh_pg['id']}/commands"
    edit_pg = client.post(
        endpoint,
        headers=a,
        json=command(
            "modify_cells",
            changes=[
                {
                    "operation": "update",
                    "cell_id": "playground",
                    "source": "print('temporary_value' in globals())",
                    "metadata": None,
                }
            ],
        ),
    ).json()
    assert not edit_pg.get("error"), edit_pg
    fresh_output = client.post(
        endpoint, headers=a, json=command("execute_cell", cell_id="playground")
    ).json()
    assert "False" in json.dumps(fresh_output["snapshot"]["cells"][0]["outputs"]), fresh_output
    assert (
        client.post("/api/v1/playground/close", headers=a, json={"id": fresh_pg["id"]}).status_code
        == 200
    )
    denied_update = client.post(
        "/api/v1/commands",
        headers=b,
        json=command("set_microscope_walkthrough", **micro, walkthrough=walkthrough),
    ).json()
    assert denied_update["error"]["code"] == "not_driver"
    assert (
        call("query", query="full")["snapshot"]["cells"][0]["metadata"]["didaction_microscopes"][
            "items"
        ][0]["id"]
        == "micro01"
    )
    denied = client.post(
        "/api/v1/commands",
        headers=b,
        json=command("create_microscope", **micro, title="Denied", walkthrough=walkthrough),
    ).json()
    assert denied["error"]["code"] == "not_driver", denied
    entries_micro = client.get("/api/v1/notebooks", params={"directory": ""}).json()["entries"]
    sidecar_micro = next(e["path"] for e in entries_micro if e["path"].endswith(".micro01"))
    assert sidecar_micro.startswith(path + ".")
    graphics_path = sidecar_micro + ".example.ts"
    exported = client.get("/api/v1/workspace-export").json()["entries"]
    saved_graphics = next(e for e in exported if e["path"] == graphics_path)
    assert (
        base64.b64decode(saved_graphics["content_base64"]).decode()
        == walkthrough["steps"][0]["graphics"]["source"]
    )
    walkthrough["steps"][0]["graphics"]["artifact"] = "replaced.ts"
    call("set_microscope_walkthrough", **micro, walkthrough=walkthrough)
    exported = client.get("/api/v1/workspace-export").json()["entries"]
    assert not any(e["path"] == graphics_path for e in exported)
    graphics_path = sidecar_micro + ".replaced.ts"
    assert any(e["path"] == graphics_path for e in exported)
    call("delete_microscope", **micro)
    assert not any(
        e["path"] in (sidecar_micro, graphics_path)
        for e in client.get("/api/v1/notebooks", params={"directory": ""}).json()["entries"]
    )
    assert (
        "didaction_microscopes"
        not in call("query", query="full")["snapshot"]["cells"][0]["metadata"]
    )
    # Create-only artifact API uses the same workspace driver capability.
    folder = {"path": "uploads", "kind": "directory"}
    assert client.post("/api/v1/artifacts", headers=b, json=folder).status_code == 403
    assert client.post("/api/v1/artifacts", headers=a, json=folder).json()["ok"]
    assert client.post("/api/v1/artifacts", headers=a, json=folder).is_error
    for name in ["../escape", "/absolute", "uploads/.secret", "uploads/a%2fb"]:
        assert client.post(
            "/api/v1/artifacts", headers=a, json={"path": name, "kind": "file"}
        ).is_error
    assert client.post(
        "/api/v1/artifacts", headers=a, json={"path": "uploads/nested", "kind": "directory"}
    ).json()["ok"]
    payload = base64.b64encode(b"x,y\n1,2\n").decode()
    artifact = {"path": "uploads/nested/data.csv", "kind": "file", "content_base64": payload}
    assert client.post("/api/v1/artifacts", headers=a, json=artifact).json()["ok"]
    assert client.post("/api/v1/artifacts", headers=a, json=artifact).is_error
    # Artifact bodies have a separately bounded allowance above command size.
    assert client.post(
        "/api/v1/artifacts",
        headers=a,
        json={
            "path": "uploads/large.bin",
            "kind": "file",
            "content_base64": base64.b64encode(bytes(400_000)).decode(),
        },
    ).json()["ok"]
    notebook = {"nbformat": 4, "nbformat_minor": 5, "metadata": {}, "cells": []}
    assert client.post(
        "/api/v1/artifacts",
        headers=a,
        json={
            "path": "uploads/nested/demo.ipynb",
            "kind": "notebook",
            "content_base64": base64.b64encode(json.dumps(notebook).encode()).decode(),
        },
    ).json()["ok"]
    entries = client.get("/api/v1/notebooks", params={"directory": "uploads/nested"}).json()[
        "entries"
    ]
    assert {e["name"] for e in entries} == {"data.csv", "demo.ipynb"}
    assert client.post(
        "/api/v1/artifacts",
        headers=a,
        json={"path": "uploads/bad.ipynb", "kind": "file", "content_base64": payload},
    ).is_error
    assert client.post(
        "/api/v1/artifacts",
        headers=a,
        json={"path": "uploads/big.bin", "kind": "file", "content_base64": "A" * 1_400_001},
    ).is_error
    for endpoint in ["commands", "commands/stream"]:
        denied = client.post(
            f"/api/v1/{endpoint}",
            headers=b,
            json=command("execute_cell", cell_id="x"),
        ).text
        assert "not_driver" in denied, denied
    # Public IDs are not private capabilities.
    forged = client.post(
        "/api/v1/commands",
        headers={**a, "x-notebook-client": joined["client_id"]},
        json=command("restart_kernel"),
    ).json()
    assert forged["error"]["code"] == "not_driver", forged

    cell = state["snapshot"]["cells"][0]["id"]
    change = command(
        "modify_cells", changes=[{"operation": "update", "cell_id": cell, "source": "x = 42\nx"}]
    )
    first = client.post("/api/v1/commands", headers=a, json=change).json()
    assert not first.get("error"), first
    assert client.post("/api/v1/commands", headers=a, json=change).json() == first
    changed_key = {**change, "changes": [{"operation": "delete", "cell_id": cell}]}
    assert (
        client.post("/api/v1/commands", headers=a, json=changed_key).json()["error"]["code"]
        == "duplicate_command"
    )
    stale = client.post(
        "/api/v1/commands",
        headers=a,
        json=command(
            "modify_cells", expected_revision=0, changes=[{"operation": "delete", "cell_id": cell}]
        ),
    ).json()
    assert stale["error"]["code"] == "stale_revision", stale
    state = call("execute_cell", cell_id=cell)
    assert "42" in json.dumps(state["snapshot"]["cells"][0]["outputs"])
    assert call("inspect", code="len", cursor_pos=3)["inspection"]["found"]
    call("create_checkpoint")
    exported = client.get("/api/v1/download", headers=a)
    assert exported.json()["nbformat"] == 4
    workspace_export = client.get("/api/v1/workspace-export").json()
    exported_files = {entry["path"]: entry for entry in workspace_export["entries"]}
    assert exported_files["uploads/nested"]["directory"]
    assert base64.b64decode(exported_files["uploads/nested/data.csv"]["content_base64"])
    assert json.loads(base64.b64decode(exported_files[path]["content_base64"]))["nbformat"] == 4
    assert (
        client.get(
            "/api/v1/workspace-export", headers={"origin": "https://untrusted.invalid"}
        ).status_code
        == 403
    )
    assert path in client.get("/api/v1/notebooks").text
    for bad in ["../secret", "/secret", "a%2f..%2fb", ".secret", "a\\b"]:
        response = client.get("/api/v1/notebooks", params={"directory": bad})
        assert response.status_code == 400, bad
    assert (
        client.post(
            "/api/v1/commands", headers=a, json=command("query", query="full", protocol_version=2)
        ).json()["error"]["code"]
        == "unsupported_version"
    )
    assert (
        client.post(
            "/api/v1/collaboration/join", headers={"origin": "https://untrusted.invalid"}
        ).status_code
        == 403
    )
    assert (
        client.post(
            "/api/v1/commands",
            headers={**a, "content-type": "application/json"},
            content='{"data":"' + "a" * 310000 + '"}',
        ).status_code
        == 413
    )

    joined_other = client.post(
        "/api/v1/collaboration/join", headers={**a, "x-notebook-path": "other.ipynb"}
    ).json()
    assert joined_other["is_driver"]
    view = {
        "protocol_version": 1,
        "notebook_path": "other.ipynb",
        "scroll_fraction": 0.4,
        "selected_cell_id": cell,
    }
    assert client.post(
        "/api/v1/collaboration/view",
        headers={**a, "x-notebook-target-client": a["x-notebook-client"]},
        json=view,
    ).json()["ok"]
    assert (
        client.get("/api/v1/collaboration/view", headers=b).json()["view"]["selected_cell_id"]
        == cell
    )
    assert client.post(f"/api/v1/collaboration/driver/{observer['client_id']}", headers=a).json()[
        "ok"
    ]
    assert (
        client.post("/api/v1/commands", headers=a, json=command("restart_kernel")).json()["error"][
            "code"
        ]
        == "not_driver"
    )
    assert client.post(f"/api/v1/collaboration/driver/{joined['client_id']}", headers=b).json()[
        "ok"
    ]

    # Socket departure must not cancel execution or allow premature ownership handoff.
    call(
        "modify_cells",
        changes=[
            {
                "operation": "update",
                "cell_id": cell,
                "source": (
                    "import time\nprint('before', flush=True)\n"
                    "time.sleep(1)\nprint('after', flush=True)"
                ),
            }
        ],
    )
    with client.stream(
        "POST", "/api/v1/commands/stream", headers=a, json=command("execute_cell", cell_id=cell)
    ) as response:
        next(response.iter_lines())
    time.sleep(0.2)
    handoff = client.post(f"/api/v1/collaboration/driver/{observer['client_id']}", headers=a).json()
    assert handoff["code"] == "execution_rejected", handoff
    time.sleep(1.3)
    assert "after" in json.dumps(call("query", query="full")["snapshot"]["cells"][0]["outputs"])

    # Timeout is cached and quarantines writes until an explicit restart.
    call(
        "modify_cells",
        changes=[{"operation": "update", "cell_id": cell, "source": "import time\ntime.sleep(10)"}],
    )
    timeout = command("execute_cell", cell_id=cell, timeout_ms=150)
    failure = client.post("/api/v1/commands", headers=a, json=timeout).json()
    assert failure["error"]["code"] == "timeout", failure
    assert client.post("/api/v1/commands", headers=a, json=timeout).json() == failure
    assert (
        client.post(
            "/api/v1/commands", headers=a, json=command("execute_cell", cell_id=cell)
        ).json()["error"]["code"]
        == "execution_rejected"
    )
    call("restart_kernel")
    call("modify_cells", changes=[{"operation": "update", "cell_id": cell, "source": "6 * 7"}])
    assert "42" in json.dumps(call("execute_cell", cell_id=cell)["snapshot"]["cells"][0]["outputs"])
    renamed = call("rename_notebook", path="native-renamed.ipynb")
    assert renamed["snapshot"]["notebook"]["path"] == "native-renamed.ipynb"
    a["x-notebook-path"] = "native-renamed.ipynb"
    assert not client.get("/api/v1/collaboration/events", headers=a).is_error
    call("close")
    client.post("/api/v1/collaboration/leave", headers=a)
    client.post("/api/v1/collaboration/leave", headers={**a, "x-notebook-path": "other.ipynb"})
    client.post("/api/v1/collaboration/leave", headers=b)
    assert os.environ["DIDACTION_JUPYTER_TOKEN"] not in exported.text

print(
    "Rust gateway contracts: PASS "
    "(ownership, follow, idempotency, bounds, timeout, reconnect, rename)"
)
