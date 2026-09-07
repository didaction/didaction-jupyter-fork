import json
import logging
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any
from uuid import UUID

import httpx
import nbformat
import pytest

from services.gateway.app import main as gateway_main
from services.gateway.app.config import Settings
from services.gateway.app.jupyter_adapter import AdapterError, JupyterNotebookTransport
from services.gateway.app.models import Command
from services.gateway.app.normalize import normalize_cells
from services.gateway.app.redaction import REDACTED, RedactingFilter, redact


def make_command(kind: str, **values: object) -> Command:
    return Command.model_validate(
        {
            "protocol_version": 1,
            "command_id": UUID(int=1),
            "idempotency_key": "one",
            "timeout_ms": 1000,
            "type": kind,
            **values,
        }
    )


def test_relative_positions_follow_ids_and_reject_missing_anchors(tmp_path: Path) -> None:
    adapter = JupyterNotebookTransport(Settings(workspace=tmp_path))
    notebook = nbformat.v4.new_notebook(
        cells=[nbformat.v4.new_code_cell(id=name) for name in ["a", "c", "b"]]
    )
    adapter._apply_changes(
        notebook,
        [
            {
                "operation": "insert_relative",
                "anchor_cell_id": "b",
                "after": False,
                "cell": {"id": "x", "cell_type": "code", "source": "42"},
            }
        ],
    )
    assert [cell.id for cell in notebook.cells] == ["a", "c", "x", "b"]
    adapter._apply_changes(
        notebook,
        [{"operation": "move_relative", "cell_id": "a", "anchor_cell_id": "b", "after": True}],
    )
    assert [cell.id for cell in notebook.cells] == ["c", "x", "b", "a"]
    with pytest.raises(AdapterError):
        adapter._apply_changes(
            notebook,
            [
                {
                    "operation": "move_relative",
                    "cell_id": "a",
                    "anchor_cell_id": "deleted",
                    "after": True,
                }
            ],
        )
    assert [cell.id for cell in notebook.cells] == ["c", "x", "b", "a"]


@pytest.mark.parametrize(
    "path",
    ["../bad.ipynb", "/tmp/bad.ipynb", "a//b.ipynb", "a/./b.ipynb"],  # noqa: S108
)
def test_path_confinement(path: str, tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="path_rejected"):
        Settings(workspace=tmp_path).confined_path(path)


def test_startup_notebook_and_kernel_are_configuration(tmp_path: Path) -> None:
    settings = Settings(
        workspace=tmp_path,
        notebook_path="course/week-1.ipynb",
        kernel_name="python-custom",
    )

    assert settings.startup_notebook() == "course/week-1.ipynb"
    assert settings.kernel_name == "python-custom"


def test_allowed_origins_are_explicit_and_exact() -> None:
    settings = Settings(allowed_origins="https://notebooks.example, http://localhost:5173/")

    assert settings.origin_allowed("https://notebooks.example", "gateway.example")
    assert settings.origin_allowed("http://localhost:5173", "gateway.example")
    assert settings.origin_allowed("https://gateway.example", "gateway.example")
    assert not settings.origin_allowed("https://evil.example", "gateway.example")
    with pytest.raises(ValueError, match=r"exact HTTP\(S\) origins"):
        Settings(allowed_origins="*")


@pytest.mark.asyncio
async def test_configured_origin_receives_bounded_cors_headers(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        gateway_main,
        "settings",
        Settings(allowed_origins="https://notebooks.example"),
    )
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=gateway_main.app), base_url="http://gateway.example"
    ) as client:
        allowed = await client.get("/healthz", headers={"origin": "https://notebooks.example"})
        denied = await client.get("/healthz", headers={"origin": "https://untrusted.example"})
        preflight = await client.options(
            "/api/v1/commands",
            headers={
                "origin": "https://notebooks.example",
                "access-control-request-method": "POST",
            },
        )

    assert allowed.headers["access-control-allow-origin"] == "https://notebooks.example"
    assert allowed.headers["access-control-allow-credentials"] == "true"
    assert denied.status_code == 403
    assert preflight.status_code == 204
    assert preflight.headers["access-control-allow-methods"] == "GET, POST, OPTIONS"


@pytest.mark.asyncio
async def test_discovery_reports_startup_race_as_typed_disconnect(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    async def disconnected(*args: object, **kwargs: object) -> None:
        raise httpx.ConnectError("not ready")

    monkeypatch.setattr(httpx.AsyncClient, "request", disconnected)
    adapter = JupyterNotebookTransport(Settings(workspace=tmp_path))

    with pytest.raises(AdapterError) as error:
        await adapter.discover()

    assert error.value.code == "disconnected"
    assert error.value.retryable is True


def test_direct_mutations_preserve_stable_identity(tmp_path: Path) -> None:
    adapter = JupyterNotebookTransport(Settings(workspace=tmp_path))
    notebook = nbformat.v4.new_notebook(cells=[nbformat.v4.new_code_cell("first", id="stable")])
    adapter._apply_changes(
        notebook,
        [
            {"operation": "update", "cell_id": "stable", "source": "changed"},
            {
                "operation": "insert",
                "index": 0,
                "cell": {"id": "new", "cell_type": "markdown", "source": "# Note"},
            },
            {"operation": "move", "cell_id": "stable", "index": 0},
        ],
    )
    assert [cell.id for cell in notebook.cells] == ["stable", "new"]
    assert notebook.cells[0].source == "changed"


def test_delete_and_type_conversion(tmp_path: Path) -> None:
    adapter = JupyterNotebookTransport(Settings(workspace=tmp_path))
    notebook = nbformat.v4.new_notebook(
        cells=[
            nbformat.v4.new_code_cell("print(1)", id="a"),
            nbformat.v4.new_code_cell("remove", id="b"),
        ]
    )
    adapter._apply_changes(
        notebook,
        [
            {"operation": "update", "cell_id": "a", "cell_type": "markdown"},
            {"operation": "delete", "cell_id": "b"},
        ],
    )
    assert notebook.cells[0].cell_type == "markdown"
    assert notebook.cells[0].id == "a"
    assert len(notebook.cells) == 1


def test_clear_outputs_resets_execution_state(tmp_path: Path) -> None:
    adapter = JupyterNotebookTransport(Settings(workspace=tmp_path))
    cell = nbformat.v4.new_code_cell("1 + 1", id="a", execution_count=4)
    cell.outputs = [nbformat.v4.new_output("execute_result", data={"text/plain": "2"})]
    notebook = nbformat.v4.new_notebook(cells=[cell])

    adapter._apply_changes(notebook, [{"operation": "clear_outputs", "cell_id": "a"}])

    assert notebook.cells[0].outputs == []
    assert notebook.cells[0].execution_count is None


@pytest.mark.asyncio
async def test_execute_stream_yields_iopub_updates_and_latest_clear_state(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    adapter = JupyterNotebookTransport(Settings(workspace=tmp_path))
    notebook = nbformat.v4.new_notebook(
        cells=[nbformat.v4.new_code_cell("stream", id="stream-cell")]
    )
    saved: list[list[dict[str, object]]] = []

    class ProtocolClient:
        def execute_interactive(self, code: str, **kwargs: object) -> dict[str, object]:
            hook = kwargs["output_hook"]
            assert callable(hook)
            for message in [
                {
                    "header": {"msg_type": "stream"},
                    "content": {"name": "stdout", "text": "obsolete\n"},
                },
                {"header": {"msg_type": "clear_output"}, "content": {"wait": True}},
                {
                    "header": {"msg_type": "stream"},
                    "content": {"name": "stdout", "text": "latest\n"},
                },
                {
                    "header": {"msg_type": "display_data"},
                    "content": {
                        "data": {"image/png": "chart"},
                        "metadata": {},
                        "transient": {"display_id": "plot-1"},
                    },
                },
            ]:
                hook(message)
            return {"content": {"status": "ok", "execution_count": 7}}

    class Kernel:
        class Manager:
            client = ProtocolClient()

        _manager = Manager()

    async def read(_: str) -> Any:
        return notebook

    async def save(_: str, value: Any) -> None:
        saved.append([dict(output) for output in value.cells[0].outputs])

    async def kernel(_: str, __: str) -> Any:
        return Kernel()

    monkeypatch.setattr(adapter, "_read_notebook", read)
    monkeypatch.setattr(adapter, "_save_notebook", save)
    monkeypatch.setattr(adapter, "_ensure_kernel", kernel)
    states = [
        state
        async for state in adapter.execute_stream(
            make_command("execute_cell", cell_id="stream-cell"), "stream.ipynb"
        )
    ]

    observed = [state.cells[0].outputs for state in states]
    assert any(outputs and outputs[0].get("text") == "obsolete\n" for outputs in observed)
    assert any(outputs == [] for outputs in observed[1:])
    assert states[-1].cells[0].outputs[0]["text"] == "latest\n"
    assert states[-1].cells[0].execution_count == 7
    assert saved[-1][0]["text"] == "latest\n"
    assert all("transient" not in output for state in states for output in state.cells[0].outputs)


@pytest.mark.asyncio
async def test_stream_endpoint_emits_busy_snapshots_before_idle_final(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    first = nbformat.v4.new_notebook(cells=[nbformat.v4.new_code_cell("print('one')", id="cell")])
    first.cells[0].outputs = [nbformat.v4.new_output("stream", name="stdout", text="one\n")]
    final = nbformat.from_dict(first)
    final.cells[0].outputs = [nbformat.v4.new_output("stream", name="stdout", text="two\n")]

    class StreamingTransport:
        ready = True
        revisions = {"stream.ipynb": ("before", 1)}

        async def execute_stream(self, command: Command, path: str) -> AsyncIterator[Any]:
            yield first
            yield final

        def revision_for(self, path: str, cells: list[dict[str, Any]]) -> int:
            return 2 if cells[0]["outputs"][0]["text"] == "one\n" else 3

    monkeypatch.setattr(gateway_main, "transport", StreamingTransport())
    monkeypatch.setattr(gateway_main, "current_notebook", "stream.ipynb")
    gateway_main.result_cache.clear()
    payload = make_command("execute_cell", cell_id="cell").model_dump(mode="json")
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=gateway_main.app), base_url="http://test"
    ) as client:
        joined = (await client.post("/api/v1/collaboration/join")).json()
        response = await client.post(
            "/api/v1/commands/stream", json=payload, headers={"x-notebook-client": joined["token"]}
        )

    events = [json.loads(line) for line in response.text.splitlines()]
    assert [event["snapshot"]["kernel"]["state"] for event in events] == [
        "busy",
        "busy",
        "idle",
    ]
    assert events[0]["snapshot"]["cells"][0]["outputs"][0]["text"] == "one\n"
    assert events[-1]["snapshot"]["cells"][0]["outputs"][0]["text"] == "two\n"


def test_stale_cell_is_typed_error(tmp_path: Path) -> None:
    adapter = JupyterNotebookTransport(Settings(workspace=tmp_path))
    notebook = nbformat.v4.new_notebook()
    with pytest.raises(AdapterError, match="Cell identity is stale"):
        adapter._apply_changes(notebook, [{"operation": "delete", "cell_id": "missing"}])


def test_normalizes_text_errors_and_png() -> None:
    cells = normalize_cells(
        {
            "cells": [
                {
                    "id": "stable",
                    "cell_type": "code",
                    "source": "plot()",
                    "outputs": [
                        {"output_type": "stream", "name": "stdout", "text": "ready\n"},
                        {"output_type": "display_data", "data": {"image/png": "abc"}},
                    ],
                }
            ]
        }
    )
    assert cells[0]["id"] == "stable"
    assert cells[0]["outputs"][1] == {"kind": "rich", "mime": "image/png", "data": "abc"}


def test_normalizes_bounded_html_table_as_rich_output() -> None:
    cells = normalize_cells(
        [
            {
                "id": "a",
                "cell_type": "code",
                "source": "table",
                "outputs": [
                    {
                        "output_type": "display_data",
                        "data": {"text/html": "<table><tr><td>42</td></tr></table>"},
                    }
                ],
            }
        ]
    )

    assert cells[0]["outputs"][0] == {
        "kind": "rich",
        "mime": "text/html",
        "data": "<table><tr><td>42</td></tr></table>",
    }


def test_completion_bounds_model() -> None:
    command = make_command("complete", code="value.bi", cursor_pos=8)
    assert command.cursor_pos == 8
    with pytest.raises(ValueError):
        make_command("complete")


def test_inspection_bounds_model() -> None:
    command = make_command("inspect", code="value", cursor_pos=5)
    assert command.cursor_pos == 5
    with pytest.raises(ValueError):
        make_command("inspect")


def test_redacts_sensitive_data(caplog: pytest.LogCaptureFixture) -> None:
    assert redact({"token": "secret", "nested": {"source": "value = 42"}}) == {
        "token": REDACTED,
        "nested": {"source": REDACTED},
    }
    logger = logging.getLogger("redaction-test")
    logger.addFilter(RedactingFilter())
    with caplog.at_level(logging.INFO):
        logger.info("payload=%s", {"authorization": "secret"})
    assert "secret" not in caplog.text
