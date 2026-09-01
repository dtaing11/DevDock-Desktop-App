#!/usr/bin/env python3
"""A minimal language server, for testing the client against a real process.

Speaks just enough LSP to exercise every path the client uses: the
initialize handshake, a server-to-client request (which the client must
answer or a real server would stall), document sync, diagnostics, and each
feature request.
"""
import json
import sys


def read_message():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.decode("utf-8").strip()
        if not line:
            break
        if line.lower().startswith("content-length:"):
            length = int(line.split(":", 1)[1].strip())
    if length is None:
        return None
    return json.loads(sys.stdin.buffer.read(length).decode("utf-8"))


def send(payload):
    body = json.dumps(payload).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()


def rng(l1, c1, l2, c2):
    return {"start": {"line": l1, "character": c1}, "end": {"line": l2, "character": c2}}


def main():
    while True:
        message = read_message()
        if message is None:
            return
        method = message.get("method")
        mid = message.get("id")
        params = message.get("params") or {}

        if method == "initialize":
            send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {
                "textDocumentSync": 1,
                "hoverProvider": True,
                "definitionProvider": True,
                "referencesProvider": True,
                "documentSymbolProvider": True,
                "documentFormattingProvider": True,
                "renameProvider": {"prepareProvider": True},
                "completionProvider": {"triggerCharacters": ["."]},
            }}})
        elif method == "initialized":
            # A server-to-client request: the client must answer this.
            send({"jsonrpc": "2.0", "id": 9001, "method": "workspace/configuration",
                  "params": {"items": [{"section": "fake"}, {"section": "fake2"}]}})
        elif method == "textDocument/didOpen":
            uri = params["textDocument"]["uri"]
            send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": rng(0, 3, 0, 8),
                    "severity": 1,
                    "code": "E0001",
                    "source": "fake",
                    "message": "something is wrong",
                }],
            }})
        elif method == "textDocument/didChange":
            uri = params["textDocument"]["uri"]
            # Second version is clean, so the test can watch diagnostics clear.
            send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                  "params": {"uri": uri, "diagnostics": []}})
        elif method == "textDocument/hover":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "contents": {"kind": "markdown", "value": "fn greet() -> String"}}})
        elif method == "textDocument/definition":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "targetUri": params["textDocument"]["uri"],
                "targetSelectionRange": rng(4, 0, 4, 5)}})
        elif method == "textDocument/references":
            send({"jsonrpc": "2.0", "id": mid, "result": [
                {"uri": params["textDocument"]["uri"], "range": rng(1, 2, 1, 7)},
                {"uri": params["textDocument"]["uri"], "range": rng(6, 0, 6, 5)}]})
        elif method == "textDocument/documentSymbol":
            send({"jsonrpc": "2.0", "id": mid, "result": [{
                "name": "App", "kind": 23, "range": rng(0, 0, 9, 0),
                "selectionRange": rng(0, 7, 0, 10),
                "children": [{"name": "run", "kind": 6, "range": rng(2, 4, 4, 5),
                              "selectionRange": rng(2, 11, 2, 14)}]}]})
        elif method == "textDocument/completion":
            send({"jsonrpc": "2.0", "id": mid, "result": {"isIncomplete": False, "items": [
                {"label": "greet", "kind": 3, "detail": "fn() -> String", "sortText": "0"},
                {"label": "grumble", "kind": 3, "sortText": "1",
                 "textEdit": {"range": rng(0, 0, 0, 2), "newText": "grumble()"}}]}})
        elif method == "textDocument/formatting":
            send({"jsonrpc": "2.0", "id": mid, "result": [
                {"range": rng(0, 0, 0, 4), "newText": "FMT"}]})
        elif method == "textDocument/prepareRename":
            send({"jsonrpc": "2.0", "id": mid, "result": {"range": rng(0, 3, 0, 8)}})
        elif method == "textDocument/rename":
            uri = params["textDocument"]["uri"]
            send({"jsonrpc": "2.0", "id": mid, "result": {"documentChanges": [{
                "textDocument": {"uri": uri, "version": 2},
                "edits": [{"range": rng(0, 3, 0, 8), "newText": params["newName"]}]}]}})
        elif method == "unsupported/method":
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32601, "message": "method not found"}})
        elif method == "shutdown":
            send({"jsonrpc": "2.0", "id": mid, "result": None})
        elif method == "exit":
            return


if __name__ == "__main__":
    main()
