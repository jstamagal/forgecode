"""The `tools` object a forge kernel cell calls tools through (T16).

Runs inside the kernel. Each call is one connection to the unix socket forge
serves: the request is one JSON document, the write side is shut down, and
the reply is one JSON document {"value": ...} or {"error": "..."}.
"""

import json
import socket


class ToolError(Exception):
    """The forge tool refused the call or reported a failure."""


class Tools:
    """`tools.<name>(**kwargs)` runs the forge tool `<name>` and returns its result text."""

    ToolError = ToolError

    def __init__(self, socket_path, token):
        self._socket_path = socket_path
        self._token = token

    def call(self, name, /, **kwargs):
        reply = self._request({"op": "call", "tool": name, "kwargs": kwargs})
        if reply.get("error") is not None:
            raise ToolError(reply["error"])
        return reply.get("value")

    def names(self):
        reply = self._request({"op": "names"})
        if reply.get("error") is not None:
            raise ToolError(reply["error"])
        return list(reply.get("value") or [])

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)

        def call(**kwargs):
            return self.call(name, **kwargs)

        call.__name__ = call.__qualname__ = name
        return call

    def __repr__(self):
        return "<forge tools: tools.<name>(...) calls a forge tool; tools.names() lists them>"

    def _request(self, payload):
        payload["token"] = self._token
        data = json.dumps(payload, default=str).encode("utf-8")
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
            conn.connect(self._socket_path)
            conn.sendall(data)
            conn.shutdown(socket.SHUT_WR)
            chunks = []
            while True:
                chunk = conn.recv(1 << 16)
                if not chunk:
                    break
                chunks.append(chunk)
        if not chunks:
            raise ToolError("forge closed the tool bridge without an answer")
        reply = json.loads(b"".join(chunks).decode("utf-8"))
        if not isinstance(reply, dict):
            raise ToolError("the forge tool bridge sent a malformed reply")
        return reply
