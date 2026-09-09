"""Regression for #66: navigate into and within BIG files after a cache restart."""
import os
from pathlib import Path
import queue
import struct
import subprocess
import tempfile
import threading
import time
import urllib.parse


def check_big_cache(exe, frame, reader):
    exe = str(Path(exe).resolve())
    with tempfile.TemporaryDirectory(prefix="zerosyntax-big-navigation-") as temp:
        root = Path(temp)
        archive = root / "Base Cache #.big"
        text = (
            "; Archived implementation\r\n"
            "CommandButton CachedButton\r\n  Command = UNIT_BUILD\r\nEnd\r\n"
            "CommandSet CachedSet\r\n  1 = CachedButton\r\nEnd\r\n"
        )
        name = b"Data\\INI\\Cached.ini\0"
        content = text.encode("utf-8")
        offset = 16 + 8 + len(name)
        archive.write_bytes(
            b"BIGF" + struct.pack(">III", offset + len(content), 1, 0)
            + struct.pack(">II", offset, len(content)) + name + content
        )
        configured_path = str(archive)
        if os.name == "nt":
            # A common spelling in baseIniRoots. Clients canonicalize the drive
            # differently; the index and source-text cache must still agree.
            configured_path = configured_path[0].lower() + configured_path[1:]
        env = {**os.environ, "LOCALAPPDATA": str(root / "cache"),
               "XDG_CACHE_HOME": str(root / "cache")}
        source_uri = (root / "Reference.ini").as_uri()

        for warm in (False, True):
            proc = subprocess.Popen(
                [exe], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, bufsize=0, env=env,
            )
            messages = queue.Queue()
            thread = threading.Thread(target=reader, args=(proc.stdout, messages), daemon=True)
            thread.start()

            def send(method, params, request_id=None):
                message = {"jsonrpc": "2.0", "method": method}
                if params is not None:
                    message["params"] = params
                if request_id is not None:
                    message["id"] = request_id
                proc.stdin.write(frame(message))
                proc.stdin.flush()

            def wait_for(predicate):
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    try:
                        message = messages.get(timeout=max(0.001, deadline - time.monotonic()))
                    except queue.Empty:
                        break
                    assert message is not None, "BIG navigation server exited early"
                    assert "_parse_error" not in message, message
                    if predicate(message):
                        return message
                raise AssertionError("timed out waiting for BIG navigation server")

            def request(method, params, request_id):
                send(method, params, request_id)
                response = wait_for(lambda m: m.get("id") == request_id)
                assert "error" not in response, response
                return response["result"]

            def open_doc(uri, document_text):
                send("textDocument/didOpen", {"textDocument": {
                    "uri": uri, "languageId": "generals-ini", "version": 1,
                    "text": document_text,
                }})
                diagnostics = wait_for(
                    lambda m: m.get("method") == "textDocument/publishDiagnostics"
                )
                assert not diagnostics["params"]["diagnostics"], diagnostics

            def definition(uri, line, character, request_id):
                return request("textDocument/definition", {
                    "textDocument": {"uri": uri},
                    "position": {"line": line, "character": character},
                }, request_id)

            try:
                request("initialize", {
                    "capabilities": {}, "rootUri": None,
                    "initializationOptions": {"baseIniRoots": [configured_path]},
                }, 1)
                send("initialized", {})
                indexed = wait_for(lambda m: m.get("method") == "window/logMessage"
                                   and "indexing completed" in m["params"]["message"])
                expected = "1 cached, 0 reparsed" if warm else "0 cached, 1 reparsed"
                assert expected in indexed["params"]["message"], indexed

                open_doc(source_uri, "Object CacheUser\n  CommandSet = CachedSet\nEnd\n")
                locations = definition(source_uri, 1, 19, 2)
                assert locations and len(locations) == 1, (warm, locations)
                target = locations[0]
                assert target["uri"].startswith("big:"), target
                assert target["range"] == {
                    "start": {"line": 4, "character": 11},
                    "end": {"line": 4, "character": 20},
                }, target
                assert request("zerosyntax/readVirtualFile", {"uri": target["uri"]}, 3) == text

                # Reproduce VS Code's URI serialization, then navigate from
                # the virtual document to another definition in the archive.
                path = urllib.parse.unquote(urllib.parse.urlsplit(target["uri"]).path)
                if len(path) > 2 and path[2] == ":":
                    path = path[:1] + path[1].lower() + path[2:]
                client_uri = "big:" + urllib.parse.quote(path, safe="/")
                assert request("zerosyntax/readVirtualFile", {"uri": client_uri}, 4) == text
                open_doc(client_uri, text)
                nested = definition(client_uri, 5, 10, 5)
                assert nested == [{"uri": target["uri"], "range": {
                    "start": {"line": 1, "character": 14},
                    "end": {"line": 1, "character": 26},
                }}], nested
                request("shutdown", None, 6)
                send("exit", None)
                proc.stdin.close()
                assert proc.wait(timeout=5) == 0
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait(timeout=5)
                thread.join(timeout=2)
                proc.stdin.close()
                proc.stdout.close()
            print(f"OK: BIG definition navigation with {'warm' if warm else 'cold'} cache")


if __name__ == "__main__":
    import sys
    from e2e import frame, reader

    check_big_cache(sys.argv[1], frame, reader)
