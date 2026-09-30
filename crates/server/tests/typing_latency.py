#!/usr/bin/env python3
"""Measure user-visible latency through the release LSP binary.

Usage: typing_latency.py EXE [LARGE_INI] [--workspace-root DIR]
"""

import argparse
import json
import math
import pathlib
import queue
import statistics
import subprocess
import sys
import tempfile
import threading
import time

sys.dont_write_bytecode = True
from e2e import frame, reader


def synthetic_document(target_lines=50_000):
    out = ["Weapon CompletionProbe\n  ProjectileObject = \nEnd\n\n"]
    lines = 4
    i = 0
    while lines < target_lines:
        if i % 2 == 0:
            out.append(
                f"Weapon PerfWeapon{i}\n"
                "  PrimaryDamage = 40.0\n"
                "  PrimaryDamageRadius = 5.0\n"
                "  SecondaryDamage = 10.0\n"
                "  SecondaryDamageRadius = 10.0\n"
                "  AttackRange = 150.0\n"
                "  MinimumAttackRange = 10.0\n"
                "  DamageType = ARMOR_PIERCING\n"
                "  DeathType = EXPLODED\n"
                "  WeaponSpeed = 600.0\n"
                f"  ProjectileObject = PerfObject{i + 1}\n"
                "  FireSound = NoSound\n"
                "  ScatterRadius = 2.5\n"
                "  AcceptableAimDelta = 5.0\n"
                "  RadiusDamageAngle = 180.0\n"
                "End\n\n"
            )
            lines += 17
        else:
            out.append(
                f"Object PerfObject{i}\n"
                "  Side = America\n"
                "  BuildCost = 900\n"
                "  BuildTime = 10.0\n"
                "  VisionRange = 150.0\n"
                "  KindOf = VEHICLE SELECTABLE\n"
                "  Draw = W3DModelDraw ModuleTag_Draw\n"
                "    ConditionState NONE\n"
                f"      Animation = PerfObject{i}.Idle\n"
                "    End\n"
                "    ConditionState REALLYDAMAGED\n"
                f"      Animation = PerfObject{i}.IdleDamaged\n"
                "    End\n"
                "  End\n"
                "  Body = ActiveBody ModuleTag_Body\n"
                "    MaxHealth = 300.0\n"
                "    InitialHealth = 300.0\n"
                "  End\n"
                "  Behavior = ArmorUpgrade ModuleTag_Armor\n"
                "    TriggeredBy = None\n"
                "  End\n"
                "End\n\n"
            )
            lines += 23
        i += 1
    return "".join(out)


def synthetic_workspace(root, file_count=200, target_lines=50_000):
    lines_per_file = math.ceil(target_lines / file_count)
    total_lines = 0
    serial = 0
    for file_number in range(file_count):
        chunks = []
        lines = 0
        while lines < lines_per_file:
            chunks.append(
                f"Object WorkspaceObject{serial}\n"
                "  Side = America\n"
                "End\n\n"
                f"Weapon WorkspaceWeapon{serial}\n"
                "  PrimaryDamage = 10.0\n"
                f"  ProjectileObject = WorkspaceObject{serial}\n"
                "End\n\n"
            )
            lines += 9
            serial += 1
        (root / f"workspace-{file_number:03}.ini").write_text(
            "".join(chunks), encoding="utf-8"
        )
        total_lines += lines
    return file_count, total_lines


def percentile_95(values):
    return sorted(values)[math.ceil(len(values) * 0.95) - 1]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("exe")
    parser.add_argument("large_ini", nargs="?")
    parser.add_argument("--workspace-root", type=pathlib.Path)
    args = parser.parse_args()

    temporary = None
    if args.large_ini:
        path = pathlib.Path(args.large_ini).resolve()
        text = path.read_text(encoding="utf-8", errors="replace")
        source = "file"
    else:
        temporary = tempfile.TemporaryDirectory(prefix="zerosyntax-performance-")
        temporary_root = pathlib.Path(temporary.name)
        path = temporary_root / "editing.ini"
        text = synthetic_document()
        path.write_text(text, encoding="utf-8")
        source = "synthetic"

    workspace_root = args.workspace_root.resolve() if args.workspace_root else None
    workspace_files = workspace_lines = 0
    if source == "synthetic" and workspace_root is None:
        workspace_root = pathlib.Path(temporary.name) / "workspace"
        workspace_root.mkdir()
        workspace_files, workspace_lines = synthetic_workspace(workspace_root)
    elif workspace_root:
        workspace_files = sum(
            1 for item in workspace_root.rglob("*") if item.is_file() and item.suffix.lower() == ".ini"
        )

    lines = text.splitlines()
    uri = path.as_uri()
    exe = pathlib.Path(args.exe)
    executable = str(exe.resolve()) if exe.exists() else args.exe
    proc = subprocess.Popen(
        [executable],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        bufsize=0,
    )
    messages: "queue.Queue" = queue.Queue()
    threading.Thread(target=reader, args=(proc.stdout, messages), daemon=True).start()
    diagnostics = []

    def send(message):
        proc.stdin.write(frame(message))
        proc.stdin.flush()

    def receive(timeout=30.0):
        message = messages.get(timeout=timeout)
        if message is None:
            raise RuntimeError("language server exited")
        if message.get("method") == "textDocument/publishDiagnostics":
            params = message["params"]
            diagnostics.append(
                (
                    time.perf_counter(),
                    params["uri"],
                    params.get("version"),
                    len(params["diagnostics"]),
                )
            )
        return message

    def wait_for(predicate, what):
        try:
            while True:
                message = receive()
                if predicate(message):
                    return message
        except queue.Empty as error:
            raise RuntimeError(f"timed out waiting for {what}") from error

    def wait_id(request_id):
        return wait_for(lambda message: message.get("id") == request_id, f"response {request_id}")

    def wait_diagnostics(version):
        try:
            while True:
                match = next(
                    (item for item in diagnostics if item[1] == uri and item[2] == version),
                    None,
                )
                if match:
                    return match
                receive()
        except queue.Empty as error:
            seen = [item[2] for item in diagnostics if item[1] == uri]
            raise RuntimeError(
                f"timed out waiting for diagnostics {version}; saw {seen}"
            ) from error

    root_uri = workspace_root.as_uri() if workspace_root else None
    send(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": None,
                "rootUri": root_uri,
                "capabilities": {"general": {"positionEncodings": ["utf-8"]}},
            },
        }
    )
    wait_id(1)
    initialized = time.perf_counter()
    send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    wait_for(
        lambda message: message.get("method") == "window/logMessage"
        and "language server ready" in message.get("params", {}).get("message", ""),
        "language server ready notification",
    )
    workspace_ready_ms = (time.perf_counter() - initialized) * 1000

    opened = time.perf_counter()
    send(
        {
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "generals-ini",
                    "version": 1,
                    "text": text,
                }
            },
        }
    )
    first_diag = wait_diagnostics(1)

    line_number, line = next(
        (i, line)
        for i, line in enumerate(lines)
        if "=" in line
        and not line.lstrip().startswith(";")
        and line.split("=", 1)[1].strip()
    )
    value_column = line.index("=") + 1
    while line[value_column].isspace():
        value_column += 1
    original = line[value_column]
    replacement = "9" if original != "9" else "8"
    position = {"line": line_number, "character": value_column}
    end = {"line": line_number, "character": value_column + 1}
    completion_line = next(
        (
            i
            for i, candidate in enumerate(lines)
            if candidate.strip() == "ProjectileObject ="
        ),
        line_number,
    )
    completion_position = {
        "line": completion_line,
        "character": len(lines[completion_line]),
    }
    version = 1
    request_id = 10
    current = original

    def content_change(character):
        return {
            "range": {"start": position, "end": end},
            "text": character,
        }

    def change():
        nonlocal version, current
        current = replacement if current == original else original
        version += 1
        send(
            {
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": [content_change(current)],
                },
            }
        )

    def bulk_change(count):
        nonlocal version, current
        changes = []
        for _ in range(count):
            current = replacement if current == original else original
            changes.append(content_change(current))
        version += 1
        send(
            {
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": changes,
                },
            }
        )

    def request(method, params):
        nonlocal request_id
        request_id += 1
        send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            }
        )
        return wait_id(request_id)

    def complete():
        request(
            "textDocument/completion",
            {
                "textDocument": {"uri": uri},
                "position": completion_position,
            },
        )

    idle = []
    for _ in range(20):
        started = time.perf_counter()
        complete()
        idle.append((time.perf_counter() - started) * 1000)

    semantic = []
    for _ in range(5):
        started = time.perf_counter()
        request(
            "textDocument/semanticTokens/full",
            {"textDocument": {"uri": uri}},
        )
        semantic.append((time.perf_counter() - started) * 1000)

    completion_metrics = {}
    diagnostic_versions = []
    names = {
        1: "single_edit_to_completion",
        4: "four_edit_burst_to_completion",
        8: "eight_edit_burst_to_completion",
        16: "sixteen_edit_burst_to_completion",
    }
    for burst in (1, 4, 8, 16):
        started = time.perf_counter()
        for _ in range(burst):
            change()
        latest = version
        complete()
        completion_metrics[names[burst]] = (time.perf_counter() - started) * 1000
        wait_diagnostics(latest)
        diagnostic_versions.append(latest)

    started = time.perf_counter()
    bulk_change(16)
    latest = version
    complete()
    bulk_ms = (time.perf_counter() - started) * 1000
    latest_diag = wait_diagnostics(latest)
    latest_diagnostics_ms = (latest_diag[0] - started) * 1000
    diagnostic_versions.append(latest)

    metrics = {
        "workspace_ready": workspace_ready_ms,
        "open_to_diagnostics": (first_diag[0] - opened) * 1000,
        "idle_completion_median": statistics.median(idle),
        "idle_completion_p95": percentile_95(idle),
        "full_semantic_tokens": statistics.median(semantic),
        **completion_metrics,
        "sixteen_change_bulk_notification": bulk_ms,
        "latest_diagnostics_after_editing": latest_diagnostics_ms,
    }
    report = {
        "workload": {
            "source": source,
            "file": str(path),
            "file_mib": round(len(text.encode("utf-8")) / 1024 / 1024, 2),
            "lines": len(lines),
            "workspace_root": str(workspace_root) if workspace_root else None,
            "workspace_files": workspace_files,
            "workspace_lines": workspace_lines or None,
            "initial_diagnostics": first_diag[3],
            "separate_edit_notifications": [1, 4, 8, 16],
            "bulk_notification_changes": 16,
            "diagnostic_versions": diagnostic_versions,
        },
        "metrics_ms": {name: round(value, 4) for name, value in metrics.items()},
    }
    print(json.dumps(report, indent=2, sort_keys=True))

    send({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
    wait_id(99)
    send({"jsonrpc": "2.0", "method": "exit", "params": None})
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    if temporary:
        temporary.cleanup()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (AssertionError, OSError, RuntimeError, queue.Empty) as error:
        print(f"typing benchmark failed: {error}", file=sys.stderr)
        sys.exit(1)
