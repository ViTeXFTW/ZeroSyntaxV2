#!/usr/bin/env python3
"""End-to-end smoke test: drive the zerosyntax-lsp binary over stdio.

Sends initialize -> initialized -> didOpen (a Weapon block with a bad value and
an unknown field), then asserts we get publishDiagnostics, completions, and
semantic tokens back. Also drives INCREMENTAL didChange deltas (multi-change
batches, CRLF documents, EOF edits) and asserts the resulting diagnostics are
identical to a full-text baseline of the same final text. Exits non-zero on
failure.
"""
import json
import base64
import os
import subprocess
import sys
import threading
import queue
import struct
import urllib.parse


def frame(obj: dict) -> bytes:
    body = json.dumps(obj).encode("utf-8")
    return b"Content-Length: %d\r\n\r\n%s" % (len(body), body)


def reader(stream, q: "queue.Queue"):
    buf = bytearray()
    while True:
        chunk = stream.read1(65536) if hasattr(stream, "read1") else stream.read(65536)
        if not chunk:
            q.put(None)
            return
        buf.extend(chunk)
        while True:
            sep = buf.find(b"\r\n\r\n")
            if sep == -1:
                break
            header = buf[:sep].decode("ascii", "replace")
            length = 0
            for line in header.split("\r\n"):
                if line.lower().startswith("content-length:"):
                    length = int(line.split(":", 1)[1].strip())
            start = sep + 4
            if len(buf) < start + length:
                break
            body = bytes(buf[start : start + length])
            del buf[: start + length]
            try:
                q.put(json.loads(body.decode("utf-8")))
            except Exception as e:  # noqa
                q.put({"_parse_error": str(e)})


def line_reader(stream, lines):
    for line in iter(stream.readline, b""):
        lines.append(line.decode("utf-8", "replace").rstrip())


def main() -> int:
    exe = sys.argv[1]

    # A throwaway workspace with an *uppercase* .INI definition file: the
    # scan must index it case-insensitively (real game data mixes casing).
    import pathlib
    import tempfile

    workspace = pathlib.Path(tempfile.mkdtemp(prefix="zerosyntax-e2e-"))
    (workspace / "Images.INI").write_text("MappedImage TestScanImage\nEnd\n")

    def w3d_chunk(kind, payload):
        return struct.pack("<II", kind, len(payload)) + payload

    mesh_header = bytearray(116)
    mesh_header[8:16] = b"Triangle"
    mesh_header[24:31] = b"Preview"
    preview_w3d = w3d_chunk(0, b"".join([
        w3d_chunk(0x1F, mesh_header),
        w3d_chunk(0x02, struct.pack("<9f", -1, 0, 0, 1, 0, 0, 0, 0, 1)),
        w3d_chunk(0x03, struct.pack("<9f", 0, -1, 0, 0, -1, 0, 0, -1, 0)),
        w3d_chunk(0x0D, struct.pack("<6f", 0, 0, 1, 0, .5, 1)),
        w3d_chunk(0x20, struct.pack("<4I4f", 0, 1, 2, 0, 0, -1, 0, 0)),
        w3d_chunk(0x30, w3d_chunk(0x31, w3d_chunk(0x32, b"Preview.tga\0"))),
        w3d_chunk(0x38, w3d_chunk(0x48, w3d_chunk(0x49, struct.pack("<I", 0)))),
    ]))
    (workspace / "Preview.w3d").write_bytes(preview_w3d)
    preview_tga = bytes([0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 2, 0, 32, 0x20])
    preview_tga += bytes([
        0, 0, 255, 255, 0, 255, 0, 255,
        255, 0, 0, 255, 255, 255, 255, 255,
    ])
    (workspace / "Preview.tga").write_bytes(preview_tga)
    base = pathlib.Path(tempfile.mkdtemp(prefix="zerosyntax-e2e-base-"))
    (base / "Base.ini").write_text("MappedImage HotBaseImage\nEnd\n")
    (base / "HotSound.wav").write_bytes(b"")
    (base / "HotTexture.dds").write_bytes(b"")

    def w3d_pivot(name):
        payload = name.encode("ascii") + b"\0" * (60 - len(name))
        return struct.pack("<II", 0x00000102, len(payload)) + payload

    (base / "A.w3d").write_bytes(w3d_pivot("Bone01"))
    (base / "B.w3d").write_bytes(w3d_pivot("Other"))
    archived_text = (
        "CommandButton ArchivedButton\n"
        "  Command = UNIT_BUILD\n"
        "End\n"
        "CommandSet ArchivedSet\n"
        "  1 = ArchivedButton\n"
        "End\n"
    )
    archive = base / "Base Cache #.big"

    def write_big(path, entries):
        data_offset = 0x10 + sum(8 + len(name.encode("ascii")) + 1 for name in entries)
        archive_size = data_offset + sum(len(data) for data in entries.values())
        data = bytearray(b"BIGF")
        data.extend(struct.pack(">III", archive_size, len(entries), 0))
        offset = data_offset
        for name, content in entries.items():
            encoded_name = name.encode("ascii")
            data.extend(struct.pack(">II", offset, len(content)))
            data.extend(encoded_name + b"\0")
            offset += len(content)
        for content in entries.values():
            data.extend(content)
        path.write_bytes(data)

    archive_entry = "Data/INI/Archived.ini"
    write_big(archive, {archive_entry: archived_text.encode("utf-8")})
    root_uri = workspace.as_uri()

    # vscode-languageclient appends this conventional transport flag. The
    # second server below remains a bare invocation so both entry paths stay pinned.
    proc = subprocess.Popen(
        [exe, "--stdio"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
        env={**os.environ, "RUST_LOG": "zerosyntax_lsp=debug"},
    )
    q: "queue.Queue" = queue.Queue()
    threading.Thread(target=reader, args=(proc.stdout, q), daemon=True).start()
    stderr_lines = []
    stderr_thread = threading.Thread(
        target=line_reader, args=(proc.stderr, stderr_lines), daemon=True
    )
    stderr_thread.start()
    server_requests = []
    indexing_begins = []
    progress_reports = []
    indexing_ends = []
    log_messages = []

    def send(obj):
        proc.stdin.write(frame(obj))
        proc.stdin.flush()

    def wait_for(pred, what, timeout=15.0):
        import time

        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                msg = q.get(timeout=deadline - time.time())
            except queue.Empty:
                break
            if msg is None:
                break
            assert "_parse_error" not in msg, msg
            if msg.get("method") in {
                "client/registerCapability",
                "client/unregisterCapability",
                "window/workDoneProgress/create",
            } and "id" in msg:
                server_requests.append(msg)
                send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
            if (msg.get("method") == "$/progress"
                    and msg.get("params", {}).get("value", {}).get("kind") == "begin"):
                indexing_begins.append(msg)
            if (msg.get("method") == "$/progress"
                    and msg.get("params", {}).get("value", {}).get("kind") == "report"):
                progress_reports.append(msg)
            if (msg.get("method") == "$/progress"
                    and msg.get("params", {}).get("value", {}).get("kind") == "end"):
                indexing_ends.append(msg)
            if msg.get("method") == "window/logMessage":
                log_messages.append(msg)
            if pred(msg):
                return msg
        print(f"TIMEOUT waiting for {what}", file=sys.stderr)
        return None

    runtime_settings = {
        "format": {"enable": False},
        "preview": {"enable": True, "imageWidth": 240, "zoomPercent": 150},
        "progress": {"mode": "indexing"},
        "baseIniRoots": [],
        "schema": {"path": ""},
        "analysis": {
            "modelMemberStrictness": "compatible",
            "allowPercentagesWithoutSign": False,
            "mapOrderingDiagnostics": True,
            "debounceMs": 50,
        },
    }

    def configure():
        send({"jsonrpc": "2.0", "method": "workspace/didChangeConfiguration",
              "params": {"settings": {"zerosyntax": runtime_settings}}})

    # 1) initialize with dynamic formatting and progress support.
    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"capabilities": {
                         "textDocument": {"formatting": {"dynamicRegistration": True}},
                         "window": {"workDoneProgress": True},
                     }, "workspaceFolders": None, "rootUri": root_uri,
                     "initializationOptions": {
                         "format": {"enable": False},
                         "preview": {"enable": True, "imageWidth": 240, "zoomPercent": 150},
                         "progress": {"mode": "indexing"},
                         "analysis": {"debounceMs": 50},
                     }}})
    init = wait_for(lambda m: m.get("id") == 1 and "result" in m, "initialize result")
    assert init, "no initialize result"
    caps = init["result"]["capabilities"]
    assert "completionProvider" in caps, "missing completionProvider"
    assert caps["completionProvider"].get("resolveProvider") is True, \
        "model previews require completionItem/resolve"
    assert "semanticTokensProvider" in caps, "missing semanticTokensProvider"
    sync = caps.get("textDocumentSync")
    assert sync == 2, f"expected INCREMENTAL sync (2), got {sync!r}"
    # We offered no positionEncodings, so the server must stay on the baseline.
    assert caps.get("positionEncoding", "utf-16") == "utf-16", caps.get("positionEncoding")
    assert "documentFormattingProvider" not in caps, \
        "dynamic clients must not receive a static formatting capability"
    print("OK: initialize advertised capabilities (incremental sync, utf-16)")

    send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    ready = wait_for(
        lambda m: m.get("method") == "window/logMessage"
        and "language server ready" in m.get("params", {}).get("message", ""),
        "startup logging",
    )
    assert ready and ready["params"]["type"] == 3, ready
    startup_logs = [message["params"]["message"] for message in log_messages]
    assert any("initializing v" in message for message in startup_logs), startup_logs
    assert any(
        "indexing started (reason=startup" in message for message in startup_logs
    ), startup_logs
    completed = next(
        message
        for message in startup_logs
        if "indexing completed (reason=startup" in message
    )
    assert "INI files" in completed and "W3D models" in completed, completed
    startup_begin = indexing_begins[0]["params"]["value"]
    assert startup_begin["title"] == "Starting ZeroSyntax", startup_begin
    startup_progress = indexing_ends[0]["params"]["value"]["message"]
    assert startup_progress.startswith("Ready"), startup_progress
    assert "INI files" in startup_progress or "no indexable game data" in startup_progress
    phase_messages = [
        message["params"]["value"].get("message", "")
        for message in progress_reports
    ]
    assert any("Discovering workspace" in message for message in phase_messages), phase_messages
    assert any("Activating workspace index" in message for message in phase_messages), phase_messages
    assert any("Finalizing workspace state" in message for message in phase_messages), phase_messages
    print("OK: startup emits initialization, indexing, and ready logs")

    # 2) didOpen with a Weapon block: bad bool + unknown field.
    uri = "file:///test/Weapon.ini"
    text = "Weapon AK47\n  ScaleWeaponSpeed = Maybe\n  Bogus = 1\nEnd\n"
    send({"jsonrpc": "2.0", "method": "textDocument/didOpen",
          "params": {"textDocument": {"uri": uri, "languageId": "generals-ini",
                                       "version": 1, "text": text}}})

    diag = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == uri,
        "publishDiagnostics",
    )
    assert diag, "no diagnostics published"
    codes = [d.get("code") for d in diag["params"]["diagnostics"]]
    print("OK: diagnostics:", codes)
    assert "bad-bool" in codes, f"expected bad-bool in {codes}"
    assert "unknown-field" in codes, f"expected unknown-field in {codes}"

    # 3) completion inside the block on a fresh field position.
    #    Put cursor after "Weapon AK47\n  " (line 1, char 2) by editing to a blank line.
    text2 = "Weapon AK47\n  \nEnd\n"
    send({"jsonrpc": "2.0", "method": "textDocument/didChange",
          "params": {"textDocument": {"uri": uri, "version": 2},
                     "contentChanges": [{"text": text2}]}})
    wait_for(lambda m: m.get("method") == "textDocument/publishDiagnostics", "diags v2")
    send({"jsonrpc": "2.0", "id": 2, "method": "textDocument/completion",
          "params": {"textDocument": {"uri": uri},
                     "position": {"line": 1, "character": 2}}})
    comp = wait_for(lambda m: m.get("id") == 2 and "result" in m, "completion result")
    assert comp, "no completion result"
    items = comp["result"]
    if isinstance(items, dict):
        items = items.get("items", [])
    labels = [i["label"] for i in items]
    assert "PrimaryDamage" in labels, f"expected PrimaryDamage in {labels[:10]}..."
    print(f"OK: completion returned {len(labels)} items incl. field names")

    # 3b) Model completion stays small until resolve, then carries a PNG preview.
    preview_uri = (workspace / "Preview.ini").as_uri()
    preview_text = (
        "Object PreviewObject\n"
        "  Draw = W3DModelDraw ModuleTag_01\n"
        "    DefaultConditionState\n"
        "      Model = \n"
        "    End\n"
        "  End\n"
        "End\n"
    )
    send({"jsonrpc": "2.0", "method": "textDocument/didOpen",
          "params": {"textDocument": {"uri": preview_uri, "languageId": "generals-ini",
                                       "version": 1, "text": preview_text}}})
    wait_for(lambda m: m.get("method") == "textDocument/publishDiagnostics"
             and m["params"]["uri"] == preview_uri, "preview diagnostics")
    send({"jsonrpc": "2.0", "id": 100, "method": "textDocument/completion",
          "params": {"textDocument": {"uri": preview_uri},
                     "position": {"line": 3, "character": len("      Model = ")}}})
    preview_completion = wait_for(
        lambda m: m.get("id") == 100 and "result" in m, "model completion")
    preview_items = preview_completion["result"]
    if isinstance(preview_items, dict):
        preview_items = preview_items.get("items", [])
    preview_item = next(item for item in preview_items if item["label"] == "Preview")
    assert "documentation" not in preview_item, "preview rendered eagerly"
    assert preview_item.get("data", {}).get("zerosyntax") == "w3d-model-preview"
    send({"jsonrpc": "2.0", "id": 101, "method": "completionItem/resolve",
          "params": preview_item})
    resolved = wait_for(
        lambda m: m.get("id") == 101 and "result" in m, "model completion resolve")
    markdown = resolved["result"]["documentation"]["value"]
    assert len(markdown) < 100_000, "VS Code truncates longer Markdown payloads"
    assert "|width=240)" in markdown
    encoded_png = markdown.split("data:image/png;base64,", 1)[1].split("|", 1)[0]
    assert base64.b64decode(encoded_png).startswith(b"\x89PNG\r\n\x1a\n")
    print("OK: model completion resolves lazily to a textured PNG preview")

    # 4) semantic tokens (full + range; the server must advertise range).
    assert caps["semanticTokensProvider"].get("range") is True, "range tokens not advertised"
    send({"jsonrpc": "2.0", "id": 3, "method": "textDocument/semanticTokens/full",
          "params": {"textDocument": {"uri": uri}}})
    sem = wait_for(lambda m: m.get("id") == 3 and "result" in m, "semantic tokens result")
    assert sem and sem["result"], "no semantic tokens"
    data = sem["result"]["data"]
    assert len(data) % 5 == 0 and len(data) > 0, "malformed semantic token data"
    print(f"OK: semantic tokens returned {len(data)//5} tokens")

    # The whole-document range must encode exactly the same data as full.
    send({"jsonrpc": "2.0", "id": 4, "method": "textDocument/semanticTokens/range",
          "params": {"textDocument": {"uri": uri},
                     "range": {"start": {"line": 0, "character": 0},
                               "end": {"line": 3, "character": 0}}}})
    sem_r = wait_for(lambda m: m.get("id") == 4 and "result" in m, "range tokens result")
    assert sem_r and sem_r["result"], "no range semantic tokens"
    assert sem_r["result"]["data"] == data, "range(whole doc) differs from full"
    print("OK: semanticTokens/range(whole doc) == full")

    # 5) incremental deltas must produce the same diagnostics as the full text.
    def norm(diags):
        return sorted(
            (d.get("code"), d["range"]["start"]["line"], d["range"]["start"]["character"],
             d["range"]["end"]["line"], d["range"]["end"]["character"], d["message"])
            for d in diags
        )

    def open_doc(doc_uri, doc_text, version=1):
        send({"jsonrpc": "2.0", "method": "textDocument/didOpen",
              "params": {"textDocument": {"uri": doc_uri, "languageId": "generals-ini",
                                          "version": version, "text": doc_text}}})
        msg = wait_for(
            lambda m: m.get("method") == "textDocument/publishDiagnostics"
            and m["params"]["uri"] == doc_uri,
            f"diagnostics for {doc_uri}",
        )
        assert msg, f"no diagnostics for {doc_uri}"
        return msg["params"]

    def change_doc(doc_uri, version, changes):
        send({"jsonrpc": "2.0", "method": "textDocument/didChange",
              "params": {"textDocument": {"uri": doc_uri, "version": version},
                         "contentChanges": changes}})
        msg = wait_for(
            lambda m: m.get("method") == "textDocument/publishDiagnostics"
            and m["params"]["uri"] == doc_uri
            and m["params"].get("version") == version,
            f"diagnostics v{version} for {doc_uri}",
        )
        assert msg, f"no v{version} diagnostics for {doc_uri}"
        return msg["params"]

    # A rapid edit burst must update the parse used by completion immediately,
    # while whole-document diagnostics coalesce to the latest version.
    burst_uri = "file:///test/burst.ini"
    burst_tail = "".join(
        f"Weapon Burst{i}\n  ScaleWeaponSpeed = Maybe\nEnd\n" for i in range(500)
    )
    burst_initial = "Weapon BurstHead\n  \nEnd\n" + burst_tail
    open_doc(burst_uri, burst_initial)
    for version, character in enumerate("Prim", start=2):
        column = version
        send({"jsonrpc": "2.0", "method": "textDocument/didChange",
              "params": {"textDocument": {"uri": burst_uri, "version": version},
                         "contentChanges": [{
                             "range": {
                                 "start": {"line": 1, "character": column},
                                 "end": {"line": 1, "character": column},
                             },
                             "text": character,
                         }]}})
    send({"jsonrpc": "2.0", "id": 6, "method": "textDocument/completion",
          "params": {"textDocument": {"uri": burst_uri},
                     "position": {"line": 1, "character": 6}}})

    published_before_completion = []

    def completion_or_burst_diag(message):
        if (message.get("method") == "textDocument/publishDiagnostics"
                and message["params"]["uri"] == burst_uri):
            published_before_completion.append(message["params"].get("version"))
            return True
        return message.get("id") == 6 and "result" in message

    burst_completion = wait_for(completion_or_burst_diag, "burst completion")
    assert burst_completion and burst_completion.get("id") == 6, (
        f"diagnostics blocked completion: versions {published_before_completion}"
    )
    burst_items = burst_completion["result"]
    if isinstance(burst_items, dict):
        burst_items = burst_items.get("items", [])
    burst_labels = [item["label"] for item in burst_items]
    assert "PrimaryDamage" in burst_labels, "completion did not use the latest burst parse"

    burst_versions = []

    def latest_burst_diag(message):
        if (message.get("method") != "textDocument/publishDiagnostics"
                or message["params"]["uri"] != burst_uri):
            return False
        burst_versions.append(message["params"].get("version"))
        return message["params"].get("version") == 5

    burst_diag = wait_for(latest_burst_diag, "latest burst diagnostics")
    assert burst_diag, "no diagnostics after burst"
    assert burst_versions == [5], f"expected only latest diagnostics, got {burst_versions}"
    burst_final = "Weapon BurstHead\n  Prim\nEnd\n" + burst_tail
    burst_baseline = open_doc("file:///test/burst-baseline.ini", burst_final)
    assert norm(burst_diag["params"]["diagnostics"]) == norm(burst_baseline["diagnostics"]), \
        "debounced burst diagnostics differ from a full-text baseline"
    print("OK: completion beats debounced diagnostics; burst publishes latest version only")

    # RemoveModule tags navigate to the matching module tag on the same object.
    module_map_uri = "file:///test/remove-module/map.ini"
    open_doc(
        module_map_uri,
        "Object GotoTank\n"
        "  Behavior = DestroyDie ModuleTag_Target\n"
        "  End\n"
        "  RemoveModule ModuleTag_Target\n"
        "End\n",
    )
    send({"jsonrpc": "2.0", "id": 30, "method": "textDocument/definition",
          "params": {"textDocument": {"uri": module_map_uri},
                     "position": {"line": 3, "character": 16}}})
    definition = wait_for(
        lambda m: m.get("id") == 30 and "result" in m,
        "RemoveModule definition result",
    )
    assert definition and definition["result"], "module tag did not resolve"
    targets = definition["result"]
    if isinstance(targets, dict):
        targets = [targets]
    assert targets[0]["uri"] == module_map_uri, targets
    assert targets[0]["range"]["start"]["line"] == 1, targets
    print("OK: RemoveModule tag resolves to its module definition")

    # A RemoveModule completion identifies the module that owns each tag.
    send({"jsonrpc": "2.0", "id": 69, "method": "textDocument/completion",
          "params": {"textDocument": {"uri": module_map_uri},
                     "position": {"line": 3, "character": len("  RemoveModule ")}}})
    module_completion = wait_for(
        lambda m: m.get("id") == 69 and "result" in m,
        "RemoveModule completion result",
    )
    module_items = module_completion["result"]
    if isinstance(module_items, dict):
        module_items = module_items.get("items", [])
    target_item = next(item for item in module_items if item["label"] == "ModuleTag_Target")
    assert target_item["documentation"]["kind"] == "markdown", target_item
    assert target_item["documentation"]["value"].startswith("```ini\n"), target_item
    assert "Behavior = DestroyDie ModuleTag_Target" in target_item["documentation"]["value"]
    print("OK: RemoveModule completion previews its defining module")

    send({"jsonrpc": "2.0", "id": 70, "method": "textDocument/references",
          "params": {"textDocument": {"uri": module_map_uri},
                     "position": {"line": 3, "character": 16},
                     "context": {"includeDeclaration": True}}})
    tag_refs = wait_for(
        lambda m: m.get("id") == 70 and "result" in m,
        "RemoveModule references result",
    )
    assert sorted(location["range"]["start"]["line"] for location in tag_refs["result"]) == [1, 3]
    send({"jsonrpc": "2.0", "id": 71, "method": "textDocument/rename",
          "params": {"textDocument": {"uri": module_map_uri},
                     "position": {"line": 1, "character": 30},
                     "newName": "ModuleTag_Renamed"}})
    tag_rename = wait_for(
        lambda m: m.get("id") == 71 and "result" in m,
        "module tag rename result",
    )
    tag_edits = tag_rename["result"]["changes"][module_map_uri]
    assert len(tag_edits) == 2 and all(
        edit["newText"] == "ModuleTag_Renamed" for edit in tag_edits
    ), tag_edits
    print("OK: module tag references and rename include declarations and removals")

    cases = [
        # (name, initial text, [(range, newText)], final text)
        ("value edit + field insert (multi-change batch)",
         "Weapon AK47\n  PrimaryDamage = 50.0\nEnd\n",
         [({"start": {"line": 1, "character": 18}, "end": {"line": 1, "character": 22}},
           "Maybe"),
          ({"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 0}},
           "  Bogus = 1\n")],
         "Weapon AK47\n  PrimaryDamage = Maybe\n  Bogus = 1\nEnd\n"),
        ("CRLF document edit",
         "Weapon M16\r\n  PrimaryDamage = 25.0\r\nEnd\r\n",
         [({"start": {"line": 1, "character": 18}, "end": {"line": 1, "character": 22}},
           "Nope")],
         "Weapon M16\r\n  PrimaryDamage = Nope\r\nEnd\r\n"),
        ("append block at EOF",
         "Weapon Pistol\nEnd\n",
         [({"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 0}},
           "Weapon Rifle\n  ScaleWeaponSpeed = Maybe\nEnd\n")],
         "Weapon Pistol\nEnd\nWeapon Rifle\n  ScaleWeaponSpeed = Maybe\nEnd\n"),
        ("delete spanning lines",
         "Weapon A\n  Bogus = 1\n  Bogus2 = 2\nEnd\n",
         [({"start": {"line": 1, "character": 0}, "end": {"line": 3, "character": 0}},
           "")],
         "Weapon A\nEnd\n"),
        ("delete an End line (splice fallback path)",
         "Weapon A\n  PrimaryDamage = 1\nEnd\nWeapon B\nEnd\n",
         [({"start": {"line": 2, "character": 0}, "end": {"line": 3, "character": 0}},
           "")],
         "Weapon A\n  PrimaryDamage = 1\nWeapon B\nEnd\n"),
        # >8 changes takes the server's bulk path (one full reparse instead of
        # per-change incremental) — the formatter-on-save shape.
        ("bulk change batch (full-parse path)",
         "Weapon A\nEnd\n",
         [({"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 0}},
           f"  Bogus{i} = 1\n") for i in range(1, 10)],
         "Weapon A\n" + "".join(f"  Bogus{i} = 1\n" for i in range(9, 0, -1)) + "End\n"),
    ]
    for i, (name, initial, changes, final) in enumerate(cases):
        inc_uri = f"file:///test/inc{i}.ini"
        base_uri = f"file:///test/base{i}.ini"
        open_doc(inc_uri, initial)
        got = change_doc(inc_uri, 2, [{"range": rng, "text": txt} for rng, txt in changes])
        want = open_doc(base_uri, final)
        assert norm(got["diagnostics"]) == norm(want["diagnostics"]), (
            f"[{name}] incremental diagnostics diverge from full-text baseline:\n"
            f"  incremental: {norm(got['diagnostics'])}\n"
            f"  baseline:    {norm(want['diagnostics'])}"
        )
        assert got.get("version") == 2, f"[{name}] missing/wrong version stamp"
        print(f"OK: incremental == baseline: {name}")

    # 6) workspace-scan references: the uppercase Images.INI was indexed, so
    #    `ButtonImage = TestScanImage` resolves and completion offers it.
    scan_uri = "file:///test/scan.ini"
    scan = open_doc(scan_uri, "Object ScanTest\n  ButtonImage = \nEnd\n")
    send({"jsonrpc": "2.0", "id": 7, "method": "textDocument/completion",
          "params": {"textDocument": {"uri": scan_uri},
                     "position": {"line": 1, "character": 16}}})
    comp = wait_for(lambda m: m.get("id") == 7 and "result" in m, "scan completion")
    items = comp["result"]
    if isinstance(items, dict):
        items = items.get("items", [])
    labels = [i["label"] for i in items]
    assert "TestScanImage" in labels, f"expected TestScanImage from .INI scan, got {labels[:10]}"
    scan2 = change_doc(scan_uri, 2, [
        {"range": {"start": {"line": 1, "character": 16}, "end": {"line": 1, "character": 16}},
         "text": "TestScanImage"}])
    codes = [d.get("code") for d in scan2["diagnostics"]]
    assert "unresolved-reference" not in codes, f"scan-indexed image should resolve: {codes}"
    print("OK: workspace scan indexed uppercase .INI (completion + resolution)")

    # 7) Phase-6 LSP breadth: outline, folding, workspace symbols, references,
    #    rename — exercised over a doc that defines and references an Upgrade.
    assert caps.get("documentSymbolProvider"), "missing documentSymbolProvider"
    assert caps.get("foldingRangeProvider"), "missing foldingRangeProvider"
    assert caps.get("workspaceSymbolProvider"), "missing workspaceSymbolProvider"
    assert caps.get("referencesProvider"), "missing referencesProvider"
    assert caps.get("renameProvider"), "missing renameProvider"

    p6_uri = "file:///test/phase6.ini"
    p6_text = ("Upgrade Upgrade_E2EPhase6\n  BuildTime = 1.0\nEnd\n"
               "Object Phase6Tank\n"
               "  Behavior = WeaponSetUpgrade ModuleTag_01\n"
               "    TriggeredBy = Upgrade_E2EPhase6\n"
               "  End\n"
               "End\n")
    open_doc(p6_uri, p6_text)

    send({"jsonrpc": "2.0", "id": 10, "method": "textDocument/documentSymbol",
          "params": {"textDocument": {"uri": p6_uri}}})
    syms = wait_for(lambda m: m.get("id") == 10 and "result" in m, "documentSymbol")
    names = [s["name"] for s in syms["result"]]
    assert names == ["Upgrade_E2EPhase6", "Phase6Tank"], names
    kids = [c["name"] for c in syms["result"][1].get("children", [])]
    assert "WeaponSetUpgrade" in kids, kids
    print("OK: documentSymbol returns nested outline")

    send({"jsonrpc": "2.0", "id": 11, "method": "textDocument/foldingRange",
          "params": {"textDocument": {"uri": p6_uri}}})
    folds = wait_for(lambda m: m.get("id") == 11 and "result" in m, "foldingRange")
    spans = {(f["startLine"], f["endLine"]) for f in folds["result"]}
    assert (0, 2) in spans and (3, 7) in spans and (4, 6) in spans, spans
    print("OK: foldingRange folds blocks and modules")

    send({"jsonrpc": "2.0", "id": 12, "method": "workspace/symbol",
          "params": {"query": "e2ephase6"}})
    wsym = wait_for(lambda m: m.get("id") == 12 and "result" in m, "workspace/symbol")
    assert any(s["name"] == "Upgrade_E2EPhase6" for s in wsym["result"]), wsym["result"][:3]
    print("OK: workspace/symbol matches case-insensitive substring")

    # references from the definition name (line 0 "Upgrade_E2EPhase6"),
    # including the declaration: expect the TriggeredBy site + the def.
    send({"jsonrpc": "2.0", "id": 13, "method": "textDocument/references",
          "params": {"textDocument": {"uri": p6_uri},
                     "position": {"line": 0, "character": 12},
                     "context": {"includeDeclaration": True}}})
    refs = wait_for(lambda m: m.get("id") == 13 and "result" in m, "references")
    lines = sorted(r["range"]["start"]["line"] for r in refs["result"])
    assert lines == [0, 5], f"expected def line 0 + site line 5, got {lines}"
    print("OK: references finds the TriggeredBy site and the declaration")

    # rename from the *reference* end (line 5) must edit both occurrences.
    send({"jsonrpc": "2.0", "id": 14, "method": "textDocument/rename",
          "params": {"textDocument": {"uri": p6_uri},
                     "position": {"line": 5, "character": 20},
                     "newName": "Upgrade_E2ERenamed"}})
    ren = wait_for(lambda m: m.get("id") == 14 and "result" in m, "rename")
    edits = ren["result"]["changes"][p6_uri]
    assert len(edits) == 2 and all(e["newText"] == "Upgrade_E2ERenamed" for e in edits), edits
    # An invalid new name (embedded space) must be rejected.
    send({"jsonrpc": "2.0", "id": 15, "method": "textDocument/rename",
          "params": {"textDocument": {"uri": p6_uri},
                     "position": {"line": 5, "character": 20},
                     "newName": "two words"}})
    bad = wait_for(lambda m: m.get("id") == 15, "rename rejection")
    assert "error" in bad, f"expected error for invalid name, got {bad}"
    print("OK: rename edits definition + references; invalid names rejected")

    # 8) Every runtime option hot-reloads without reopening documents.
    percent_uri = "file:///test/percent.ini"
    percent = open_doc(percent_uri, "Armor HotArmor\n  Armor = ARMOR_PIERCING 2\nEnd\n")
    assert "bad-percent" in [d.get("code") for d in percent["diagnostics"]]
    runtime_settings["analysis"]["debounceMs"] = 300
    configure()
    send({"jsonrpc": "2.0", "method": "textDocument/didChange",
          "params": {"textDocument": {"uri": percent_uri, "version": 2},
                     "contentChanges": [{"text":
                         "Armor HotArmor2\n  Armor = ARMOR_PIERCING 2\nEnd\n"}]}})
    runtime_settings["analysis"]["allowPercentagesWithoutSign"] = True
    configure()
    percent = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == percent_uri,
        "bare-percentage enable diagnostics",
    )
    assert "bad-percent" not in [d.get("code") for d in percent["params"]["diagnostics"]]
    delayed = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == percent_uri
        and m["params"].get("version") == 2,
        "pre-reload delayed diagnostics",
        timeout=2.0,
    )
    assert "bad-percent" not in [d.get("code") for d in delayed["params"]["diagnostics"]]

    runtime_settings["preview"] = {"imageWidth": 320, "zoomPercent": 200}
    configure()
    send({"jsonrpc": "2.0", "id": 102, "method": "completionItem/resolve",
          "params": preview_item})
    resized = wait_for(
        lambda m: m.get("id") == 102 and "result" in m, "resized model preview")
    resized_markdown = resized["result"]["documentation"]["value"]
    resized_png = resized_markdown.split("data:image/png;base64,", 1)[1].split("|", 1)[0]
    assert "|width=320)" in resized_markdown
    assert resized_png != encoded_png, "zoom change reused the previous preview"
    print("OK: model preview size and zoom hot-reload")

    runtime_settings["preview"]["enable"] = False
    configure()
    send({"jsonrpc": "2.0", "id": 103, "method": "completionItem/resolve",
          "params": resolved["result"]})
    disabled = wait_for(
        lambda m: m.get("id") == 103 and "result" in m, "disabled model preview")
    assert "documentation" not in disabled["result"]
    print("OK: model preview can be disabled without restarting")

    runtime_settings["analysis"]["allowPercentagesWithoutSign"] = False
    configure()
    percent = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == percent_uri,
        "bare-percentage disable diagnostics",
    )
    assert "bad-percent" in [d.get("code") for d in percent["params"]["diagnostics"]]

    map_uri = "file:///test/map.ini"
    map_text = ("CommandSet HotSet\n  1 = Command_HotLate\nEnd\n"
                "CommandButton Command_HotLate\n  Command = UNIT_BUILD\nEnd\n")
    map_diag = open_doc(map_uri, map_text)
    assert "map-forward-reference" in [d.get("code") for d in map_diag["diagnostics"]]
    runtime_settings["analysis"]["mapOrderingDiagnostics"] = False
    configure()
    map_diag = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == map_uri,
        "map-ordering disable diagnostics",
    )
    assert "map-forward-reference" not in [d.get("code") for d in map_diag["params"]["diagnostics"]]
    runtime_settings["analysis"]["mapOrderingDiagnostics"] = True
    configure()
    map_diag = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == map_uri,
        "map-ordering enable diagnostics",
    )
    assert "map-forward-reference" in [d.get("code") for d in map_diag["params"]["diagnostics"]]
    print("OK: percentage and map-ordering diagnostics hot-toggle")

    runtime_settings["analysis"]["debounceMs"] = 0
    configure()
    percent = change_doc(percent_uri, 3, [{"text":
        "Armor HotArmor3\n  Armor = ARMOR_PIERCING 2\nEnd\n"}])
    assert percent.get("version") == 3
    print("OK: debounce hot-reloads and publishes the current document version")

    progress_before = len(indexing_begins)
    runtime_settings["baseIniRoots"] = [str(base), str(archive)]
    configure()
    wait_for(
        lambda m: m.get("method") == "$/progress"
        and m.get("params", {}).get("value", {}).get("kind") == "end",
        "base-root indexing",
    )
    reindexed = next(
        (
            message
            for message in reversed(log_messages)
            if "indexing completed (reason=configuration_changed"
            in message.get("params", {}).get("message", "")
        ),
        None,
    )
    assert reindexed and "audio files" in reindexed["params"]["message"], reindexed
    assert len(indexing_begins) == progress_before + 1, (
        f"expected one base-root scan, got {len(indexing_begins) - progress_before}"
    )

    asset_uri = "file:///test/hot-assets.ini"
    open_doc(asset_uri, ("Object HotAssetObject\n  ButtonImage = \nEnd\n"
                         "DialogEvent HotDialog\n  Filename = \nEnd\n"
                         "MappedImage HotMapped\n  Texture = \nEnd\n"))
    request_id = 30

    def completion_labels(doc_uri, line, character):
        nonlocal request_id
        request_id += 1
        send({"jsonrpc": "2.0", "id": request_id,
              "method": "textDocument/completion",
              "params": {"textDocument": {"uri": doc_uri},
                         "position": {"line": line, "character": character}}})
        result = wait_for(lambda m: m.get("id") == request_id and "result" in m,
                          f"completion {request_id}")
        items = result["result"]
        if isinstance(items, dict):
            items = items.get("items", [])
        return [item["label"] for item in items]

    assert "HotBaseImage" in completion_labels(asset_uri, 1, 16), \
        "loose base INI definition missing"
    assert "HotSound.wav" in completion_labels(asset_uri, 4, 13), \
        "base audio asset missing"
    assert "HotTexture.tga" in completion_labels(asset_uri, 7, 12), \
        "base texture asset missing"

    archive_path = archive.as_posix()
    if not archive_path.startswith("/"):
        archive_path = "/" + archive_path
    archived_uri = "big://" + urllib.parse.quote(
        f"{archive_path}!/{archive_entry}", safe="/:!"
    )
    send({"jsonrpc": "2.0", "id": 34, "method": "zerosyntax/readVirtualFile",
          "params": {"uri": archived_uri}})
    canonical_virtual_file = wait_for(
        lambda m: m.get("id") == 34 and "result" in m,
        "canonical virtual file",
    )
    assert canonical_virtual_file["result"] == archived_text, (
        archived_uri, canonical_virtual_file["result"]
    )
    workspace_ref_uri = "file:///test/archive-reference.ini"
    workspace_ref = open_doc(
        workspace_ref_uri,
        "Object ArchiveUser\n  CommandSet = ArchivedSet\nEnd\n",
    )
    assert "unresolved-reference" not in [
        diagnostic.get("code") for diagnostic in workspace_ref["diagnostics"]
    ], workspace_ref["diagnostics"]
    send({"jsonrpc": "2.0", "id": 35, "method": "textDocument/definition",
          "params": {"textDocument": {"uri": workspace_ref_uri},
                     "position": {"line": 1, "character": 20}}})
    archived_definition = wait_for(
        lambda m: m.get("id") == 35 and "result" in m,
        "workspace definition into BIG archive",
    )
    assert archived_definition["result"] == [{
        "uri": archived_uri,
        "range": {
            "start": {"line": 3, "character": 11},
            "end": {"line": 3, "character": 22},
        },
    }], archived_definition["result"]

    parsed = urllib.parse.urlsplit(archived_uri)
    vscode_path = urllib.parse.unquote(parsed.path)
    if len(vscode_path) > 2 and vscode_path[2] == ":":
        vscode_path = vscode_path[:1] + vscode_path[1].lower() + vscode_path[2:]
    vscode_uri = "big:" + urllib.parse.quote(vscode_path, safe="/")
    send({"jsonrpc": "2.0", "id": 36, "method": "zerosyntax/readVirtualFile",
          "params": {"uri": vscode_uri}})
    virtual_file = wait_for(
        lambda m: m.get("id") == 36 and "result" in m,
        "VS Code-encoded virtual file",
    )
    assert virtual_file["result"] == archived_text, virtual_file["result"]

    send({"jsonrpc": "2.0", "id": 37, "method": "zerosyntax/readVirtualFile",
          "params": {"uri": archived_uri.replace("Archived.ini", "Unknown.ini")}})
    unknown_virtual = wait_for(
        lambda m: m.get("id") == 37 and "result" in m,
        "unknown virtual file",
    )
    assert unknown_virtual["result"] is None
    send({"jsonrpc": "2.0", "id": 38, "method": "zerosyntax/readVirtualFile",
          "params": {"uri": "big:///C%3A/Game%FF/Base.big!/Data/INI/Archived.ini"}})
    malformed_virtual = wait_for(
        lambda m: m.get("id") == 38 and "result" in m,
        "malformed virtual file",
    )
    assert malformed_virtual["result"] is None

    send({"jsonrpc": "2.0", "method": "textDocument/didOpen",
          "params": {"textDocument": {"uri": vscode_uri, "languageId": "generals-ini",
                                      "version": 1, "text": archived_text}}})
    virtual_diag = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == archived_uri,
        "virtual document diagnostics",
    )
    assert virtual_diag
    send({"jsonrpc": "2.0", "id": 39, "method": "textDocument/definition",
          "params": {"textDocument": {"uri": vscode_uri},
                     "position": {"line": 4, "character": 10}}})
    nested_definition = wait_for(
        lambda m: m.get("id") == 39 and "result" in m,
        "definition from inside BIG archive",
    )
    assert nested_definition["result"] == [{
        "uri": archived_uri,
        "range": {
            "start": {"line": 0, "character": 14},
            "end": {"line": 0, "character": 28},
        },
    }], nested_definition["result"]
    send({"jsonrpc": "2.0", "method": "textDocument/didClose",
          "params": {"textDocument": {"uri": vscode_uri}}})
    print("OK: BIG definitions open through encoded read-only URIs and navigate")

    model_uri = "file:///test/hot-model.ini"
    model_text = ("Object HotModelObject\n"
                  "  Draw = W3DModelDraw ModuleTag_Draw\n"
                  "    DefaultConditionState\n"
                  "      Model = A\n"
                  "      Model = B\n"
                  "      HideSubObject = Bone01\n"
                  "    End\n  End\nEnd\n")
    model_diag = open_doc(model_uri, model_text)
    assert "unknown-model-member" not in [d.get("code") for d in model_diag["diagnostics"]]
    runtime_settings["analysis"]["modelMemberStrictness"] = "strict"
    configure()
    model_diag = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == model_uri,
        "strict model-member diagnostics",
    )
    assert "unknown-model-member" in [d.get("code") for d in model_diag["params"]["diagnostics"]]
    runtime_settings["analysis"]["modelMemberStrictness"] = "compatible"
    configure()
    model_diag = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == model_uri,
        "compatible model-member diagnostics",
    )
    assert "unknown-model-member" not in [d.get("code") for d in model_diag["params"]["diagnostics"]]
    print("OK: base roots add definitions/assets/models; strictness republishes")

    runtime_settings["baseIniRoots"] = []
    configure()
    wait_for(
        lambda m: m.get("method") == "$/progress"
        and m.get("params", {}).get("value", {}).get("kind") == "end",
        "base-root removal indexing",
    )
    assert "HotBaseImage" not in completion_labels(asset_uri, 1, 16)
    assert "HotSound.wav" not in completion_labels(asset_uri, 4, 13)
    assert "HotTexture.tga" not in completion_labels(asset_uri, 7, 12)
    print("OK: removing a base root removes definitions, audio, and textures")

    custom_uri = "file:///test/custom-open.ini"
    custom = open_doc(custom_uri, "TestBlock HotCustom\n  CustomOnly = Yes\nEnd\n")
    assert "unknown-block" in [d.get("code") for d in custom["diagnostics"]]
    custom_schema = pathlib.Path(__file__).parent / "fixtures" / "custom-schema.json"
    runtime_settings["schema"]["path"] = str(custom_schema.resolve())
    configure()
    custom = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == custom_uri,
        "custom-schema diagnostics",
    )
    assert "unknown-block" not in [d.get("code") for d in custom["params"]["diagnostics"]]
    runtime_settings["schema"]["path"] = str(workspace / "missing-schema.json")
    configure()
    schema_warning_log = wait_for(
        lambda m: m.get("method") == "window/logMessage"
        and m.get("params", {}).get("type") == 2
        and "custom schema could not be loaded" in m.get("params", {}).get("message", ""),
        "invalid-schema warning log",
    )
    schema_warning_popup = wait_for(
        lambda m: m.get("method") == "window/showMessage"
        and m.get("params", {}).get("type") == 2
        and "built-in schema" in m.get("params", {}).get("message", ""),
        "invalid-schema warning popup",
    )
    assert schema_warning_log and schema_warning_popup
    custom = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == custom_uri,
        "invalid-schema fallback diagnostics",
    )
    assert "unknown-block" in [d.get("code") for d in custom["params"]["diagnostics"]]
    runtime_settings["schema"]["path"] = ""
    configure()
    custom = wait_for(
        lambda m: m.get("method") == "textDocument/publishDiagnostics"
        and m["params"]["uri"] == custom_uri,
        "embedded-schema diagnostics",
    )
    assert "unknown-block" in [d.get("code") for d in custom["params"]["diagnostics"]]
    print("OK: schema hot-reload logs invalid fallback and reparses open documents")

    runtime_settings["format"]["enable"] = True
    configure()
    registered = wait_for(
        lambda m: m.get("method") == "client/registerCapability",
        "dynamic formatting registration",
    )
    assert registered["params"]["registrations"][0]["id"] == "zerosyntax-formatting"
    runtime_settings["format"]["enable"] = False
    configure()
    unregistered = wait_for(
        lambda m: m.get("method") == "client/unregisterCapability",
        "dynamic formatting unregistration",
    )
    assert unregistered["params"]["unregisterations"][0]["id"] == "zerosyntax-formatting"
    send({"jsonrpc": "2.0", "id": 29, "method": "textDocument/formatting",
          "params": {"textDocument": {"uri": percent_uri},
                     "options": {"tabSize": 2, "insertSpaces": True}}})
    disabled = wait_for(lambda m: m.get("id") == 29, "disabled dynamic formatting")
    assert disabled.get("result") is None
    runtime_settings["format"]["enable"] = True
    configure()
    wait_for(lambda m: m.get("method") == "client/registerCapability",
             "dynamic formatting re-registration")
    wait_for(
        lambda m: m.get("method") == "window/logMessage"
        and "settings updated (format.enable)" in m.get("params", {}).get("message", ""),
        "dynamic formatting settings log",
    )

    send({"jsonrpc": "2.0", "id": 30, "method": "workspace/executeCommand",
          "params": {"command": "zerosyntax.rebuildIndexCache", "arguments": []}})
    rebuild_started = wait_for(
        lambda m: m.get("method") == "window/logMessage"
        and "indexing started (reason=manual_cache_rebuild"
        in m.get("params", {}).get("message", ""),
        "manual cache rebuild start log",
    )
    rebuild_completed = wait_for(
        lambda m: m.get("method") == "window/logMessage"
        and "indexing completed (reason=manual_cache_rebuild"
        in m.get("params", {}).get("message", ""),
        "manual cache rebuild completion log",
    )
    rebuild = wait_for(lambda m: m.get("id") == 30 and "result" in m,
                       "manual cache rebuild response")
    assert rebuild_started and rebuild_completed and rebuild["result"]["rebuilt"] is True
    assert indexing_begins[-1]["params"]["value"]["title"] == "Rebuilding ZeroSyntax index"
    print("OK: manual cache rebuild logs its reason and completion")

    requests_before = len(server_requests)
    progress_before = len(indexing_begins)
    logs_before = len(log_messages)
    configure()
    import time
    time.sleep(0.25)
    while not q.empty():
        pending = q.get_nowait()
        assert "_parse_error" not in pending, pending
        if pending.get("method") == "window/logMessage":
            log_messages.append(pending)
        assert pending.get("method") not in {
            "client/registerCapability", "client/unregisterCapability"
        }, pending
        assert not (pending.get("method") == "$/progress"
                    and pending.get("params", {}).get("value", {}).get("kind") == "begin"), pending
    assert len(server_requests) == requests_before
    assert len(indexing_begins) == progress_before
    assert len(log_messages) == logs_before
    print("OK: formatting hot-registers; identical settings are a no-op")

    # Progress mode can suppress indexing UI without suppressing lifecycle logs.
    runtime_settings["progress"]["mode"] = "off"
    configure()
    wait_for(
        lambda m: m.get("method") == "window/logMessage"
        and "settings updated (progress.mode)" in m.get("params", {}).get("message", ""),
        "progress mode off",
    )
    progress_before = len(indexing_begins)
    runtime_settings["baseIniRoots"] = [str(base)]
    configure()
    hidden_reindex = wait_for(
        lambda m: m.get("method") == "window/logMessage"
        and "indexing completed (reason=configuration_changed"
        in m.get("params", {}).get("message", ""),
        "hidden base-root indexing",
    )
    assert hidden_reindex
    assert len(indexing_begins) == progress_before

    # Verbose mode adds progress for settings-driven whole-document refreshes.
    runtime_settings["progress"]["mode"] = "verbose"
    runtime_settings["analysis"]["mapOrderingDiagnostics"] = False
    configure()
    verbose_refresh = wait_for(
        lambda m: m.get("method") == "$/progress"
        and m.get("params", {}).get("value", {}).get("kind") == "begin"
        and m["params"]["value"].get("title") == "Refreshing ZeroSyntax diagnostics",
        "verbose diagnostic refresh",
    )
    assert verbose_refresh
    wait_for(
        lambda m: m.get("method") == "$/progress"
        and m.get("params", {}).get("value", {}).get("kind") == "end",
        "verbose diagnostic refresh completion",
    )
    runtime_settings["progress"]["mode"] = "indexing"
    runtime_settings["analysis"]["mapOrderingDiagnostics"] = True
    runtime_settings["baseIniRoots"] = []
    configure()
    wait_for(
        lambda m: m.get("method") == "window/logMessage"
        and "indexing completed (reason=configuration_changed"
        in m.get("params", {}).get("message", ""),
        "restore base-root configuration",
    )
    print("OK: progress mode supports off, indexing, and verbose behavior")

    # 9) Phase-6 batch 2: semanticTokens delta, formatting, code actions.
    assert caps["semanticTokensProvider"]["full"] == {"delta": True}, \
        caps["semanticTokensProvider"]["full"]
    assert caps.get("codeActionProvider"), "missing codeActionProvider"

    # full (grab the resultId) -> edit -> delta must splice, not resend all.
    send({"jsonrpc": "2.0", "id": 20, "method": "textDocument/semanticTokens/full",
          "params": {"textDocument": {"uri": p6_uri}}})
    full = wait_for(lambda m: m.get("id") == 20 and "result" in m, "tokens full")
    rid = full["result"]["resultId"]
    assert rid, "full response carries no resultId"
    change_doc(p6_uri, 2, [
        {"range": {"start": {"line": 1, "character": 14}, "end": {"line": 1, "character": 17}},
         "text": "2.5"}])
    send({"jsonrpc": "2.0", "id": 21, "method": "textDocument/semanticTokens/full/delta",
          "params": {"textDocument": {"uri": p6_uri}, "previousResultId": rid}})
    delta = wait_for(lambda m: m.get("id") == 21 and "result" in m, "tokens delta")
    assert "edits" in delta["result"], f"expected a delta, got {list(delta['result'])}"
    edits = delta["result"]["edits"]
    assert len(edits) == 1 and len(edits[0].get("data", [])) < len(full["result"]["data"]), \
        "delta should splice less than the full token stream"
    # A bogus previousResultId falls back to a full response.
    send({"jsonrpc": "2.0", "id": 22, "method": "textDocument/semanticTokens/full/delta",
          "params": {"textDocument": {"uri": p6_uri}, "previousResultId": "no-such-id"}})
    fallback = wait_for(lambda m: m.get("id") == 22 and "result" in m, "delta fallback")
    assert "data" in fallback["result"], "stale id must fall back to full tokens"
    print("OK: semanticTokens/full/delta splices (and falls back on stale id)")

    # formatting: a misindented doc comes back normalized to scope depth.
    # Nearby per-line edits are coalesced server-side, so verify by applying
    # the edits rather than pinning their shapes.
    fmt_uri = "file:///test/fmt.ini"
    fmt_src = "Object FmtTank\nMaxHealth = 1\n      Behavior = AutoHealBehavior ModuleTag_01\nHealingAmount = 5\n      End\nEnd\n"
    open_doc(fmt_uri, fmt_src)
    send({"jsonrpc": "2.0", "id": 23, "method": "textDocument/formatting",
          "params": {"textDocument": {"uri": fmt_uri},
                     "options": {"tabSize": 2, "insertSpaces": True}}})
    fmt = wait_for(lambda m: m.get("id") == 23 and "result" in m, "formatting")

    def pos_off(text, pos):  # ASCII docs: utf-16 char == byte offset
        lines = text.split("\n")
        return sum(len(l) + 1 for l in lines[:pos["line"]]) + pos["character"]

    fmt_doc = fmt_src
    for e in sorted(fmt["result"], key=lambda e: pos_off(fmt_src, e["range"]["start"]),
                    reverse=True):
        s, t = pos_off(fmt_src, e["range"]["start"]), pos_off(fmt_src, e["range"]["end"])
        fmt_doc = fmt_doc[:s] + e["newText"] + fmt_doc[t:]
    assert fmt_doc == ("Object FmtTank\n  MaxHealth = 1\n"
                       "  Behavior = AutoHealBehavior ModuleTag_01\n"
                       "    HealingAmount = 5\n  End\nEnd\n"), fmt_doc
    assert len(fmt["result"]) == 1, \
        f"adjacent reindent edits should coalesce, got {len(fmt['result'])}"
    print("OK: formatting normalizes indentation to scope depth (coalesced)")

    # code actions: a misspelled enum member offers did-you-mean; an
    # unterminated block offers the missing End.
    ca_uri = "file:///test/actions.ini"
    open_doc(ca_uri, "Locomotor FixLoco\n  Appearance = TREDS\n")
    send({"jsonrpc": "2.0", "id": 24, "method": "textDocument/codeAction",
          "params": {"textDocument": {"uri": ca_uri},
                     "range": {"start": {"line": 0, "character": 0},
                               "end": {"line": 2, "character": 0}},
                     "context": {"diagnostics": []}}})
    ca = wait_for(lambda m: m.get("id") == 24 and "result" in m, "codeAction")
    titles = [a["title"] for a in ca["result"]]
    assert any("TREADS" in t for t in titles), titles
    assert any("`End`" in t for t in titles), titles
    fix = next(a for a in ca["result"] if "TREADS" in a["title"])
    new_texts = [e["newText"] for e in fix["edit"]["changes"][ca_uri]]
    assert new_texts == ["TREADS"], new_texts
    print("OK: code actions offer did-you-mean + missing End quickfixes")

    # 9) shutdown
    send({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
    wait_for(lambda m: m.get("id") == 99, "shutdown")
    send({"jsonrpc": "2.0", "method": "exit", "params": None})
    try:
        proc.wait(timeout=5)
    except Exception:
        proc.kill()
    stderr_thread.join(timeout=2)
    developer_logs = "\n".join(stderr_lines)
    assert "workspace scan completed" in developer_logs, developer_logs
    assert "document changed" in developer_logs, developer_logs
    assert "document diagnostics published" in developer_logs, developer_logs
    assert "parse_strategy=" in developer_logs, developer_logs
    assert uri in developer_logs, developer_logs
    assert workspace.name in developer_logs, developer_logs
    assert "ScaleWeaponSpeed = Maybe" not in developer_logs, developer_logs
    assert "Bogus = 1" not in developer_logs, developer_logs
    print("OK: RUST_LOG debug records decisions and paths without document contents")

    # 10) a default-initialized server (no initializationOptions) must not
    #     advertise formatting and must answer the request with null.
    proc2 = subprocess.Popen(
        [exe],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        bufsize=0,
    )
    q2: "queue.Queue" = queue.Queue()
    threading.Thread(target=reader, args=(proc2.stdout, q2), daemon=True).start()

    def send2(obj):
        proc2.stdin.write(frame(obj))
        proc2.stdin.flush()

    def wait_for2(pred, what, timeout=15.0):
        import time

        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                msg = q2.get(timeout=deadline - time.time())
            except queue.Empty:
                break
            if msg is None:
                break
            if pred(msg):
                return msg
        print(f"TIMEOUT waiting for {what}", file=sys.stderr)
        return None

    send2({"jsonrpc": "2.0", "id": 1, "method": "initialize",
           "params": {"capabilities": {}, "workspaceFolders": None, "rootUri": None}})
    init2 = wait_for2(lambda m: m.get("id") == 1 and "result" in m, "default initialize")
    assert init2, "no default initialize result"
    caps2 = init2["result"]["capabilities"]
    assert "documentFormattingProvider" not in caps2, \
        "formatting must be off by default (no initializationOptions)"
    send2({"jsonrpc": "2.0", "method": "initialized", "params": {}})
    send2({"jsonrpc": "2.0", "method": "textDocument/didOpen",
           "params": {"textDocument": {"uri": fmt_uri, "languageId": "generals-ini",
                                       "version": 1, "text": "Object T\nMaxHealth = 1\nEnd\n"}}})
    send2({"jsonrpc": "2.0", "id": 2, "method": "textDocument/formatting",
           "params": {"textDocument": {"uri": fmt_uri},
                      "options": {"tabSize": 2, "insertSpaces": True}}})
    fmt2 = wait_for2(lambda m: m.get("id") == 2, "disabled formatting response")
    assert fmt2 and fmt2.get("result") is None, \
        f"disabled formatting must return null, got {fmt2}"
    print("OK: formatting is opt-in (capability withheld + request answers null)")
    send2({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
    wait_for2(lambda m: m.get("id") == 99, "shutdown 2")
    send2({"jsonrpc": "2.0", "method": "exit", "params": None})
    try:
        proc2.wait(timeout=5)
    except Exception:
        proc2.kill()

    print("\nALL E2E CHECKS PASSED")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except AssertionError as e:
        print("E2E FAILURE:", e, file=sys.stderr)
        sys.exit(1)
