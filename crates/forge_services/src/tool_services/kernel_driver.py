"""forge kernel driver (T16): owns one Jupyter kernel and speaks JSON lines.

Requests on stdin, one JSON object per line:
  {"op": "run", "id": N, "code": "..."}       start a cell
  {"op": "interrupt"}                          SIGINT the running cell
  {"op": "shutdown"}                           stop the kernel and exit
Events on stdout, one JSON object per line:
  {"event": "ready", "python": "..."}
  {"event": "output", "id": N, "kind": "stdout"|"stderr"|"result"|"error"|"image", "text": "..."}
  {"event": "done", "id": N, "status": "ok"|"error"|"aborted", "namespace": [...], "defined": [...]}
  {"event": "dead", "message": "..."}
Only the standard library and jupyter_client are used.
"""

import base64
import json
import os
import queue
import sys
import threading
import time

from jupyter_client.manager import KernelManager

OUT = sys.stdout
LOCK = threading.Lock()


def emit(event):
    with LOCK:
        OUT.write(json.dumps(event) + "\n")
        OUT.flush()


IMAGE_DIR = os.environ.get("FORGE_KERNEL_IMAGES", "")
NAMESPACE_CODE = (
    "import types as __t, json as __j\n"
    "print('__FORGE_NS__' + __j.dumps(sorted(k for k, v in list(globals().items())"
    " if not k.startswith('_') and k not in ('tools', 'In', 'Out', 'get_ipython', 'exit', 'quit')"
    " and (isinstance(v, (__t.FunctionType, type))))))\n"
    "del __t, __j\n"
)
GLOBALS_CODE = (
    "import json as __j\n"
    "print('__FORGE_GL__' + __j.dumps(sorted(k for k in list(globals()) if not k.startswith('_')"
    " and k not in ('In', 'Out', 'get_ipython', 'exit', 'quit'))))\n"
    "del __j\n"
)


def main():
    km = KernelManager(kernel_name="python3")
    km.start_kernel(cwd=os.getcwd(), extra_arguments=["--HistoryManager.enabled=False"])
    kc = km.client()
    kc.start_channels()
    kc.wait_for_ready(timeout=60)
    images = [0]

    def quiet(code):
        """Runs code silently and returns its stdout."""
        msg_id = kc.execute(code, silent=False, store_history=False)
        out = []
        while True:
            try:
                msg = kc.get_iopub_msg(timeout=30)
            except queue.Empty:
                break
            if msg.get("parent_header", {}).get("msg_id") != msg_id:
                continue
            kind = msg["header"]["msg_type"]
            if kind == "stream":
                out.append(msg["content"]["text"])
            if kind == "status" and msg["content"]["execution_state"] == "idle":
                break
        return "".join(out)

    def marker(text, prefix):
        for line in text.splitlines():
            if line.startswith(prefix):
                try:
                    return json.loads(line[len(prefix):])
                except ValueError:
                    return []
        return []

    setup = os.environ.get("FORGE_KERNEL_SETUP", "")
    if setup:
        quiet(setup)
    emit({"event": "ready", "python": sys.executable})

    commands = queue.Queue()

    def reader():
        for line in sys.stdin:
            line = line.strip()
            if line:
                try:
                    commands.put(json.loads(line))
                except ValueError:
                    pass
        commands.put({"op": "shutdown"})

    threading.Thread(target=reader, daemon=True).start()

    def run_cell(cell_id, code):
        before = set(marker(quiet(GLOBALS_CODE), "__FORGE_GL__"))
        msg_id = kc.execute(code)
        status = "ok"
        while True:
            try:
                command = commands.get_nowait()
            except queue.Empty:
                command = None
            if command is not None:
                if command.get("op") == "interrupt":
                    km.interrupt_kernel()
                elif command.get("op") == "shutdown":
                    commands.put(command)
                    km.interrupt_kernel()
            if not km.is_alive():
                emit({"event": "dead", "message": "the kernel process died while running cell %s" % cell_id})
                return False
            try:
                msg = kc.get_iopub_msg(timeout=0.2)
            except queue.Empty:
                continue
            if msg.get("parent_header", {}).get("msg_id") != msg_id:
                continue
            kind = msg["header"]["msg_type"]
            content = msg["content"]
            if kind == "stream":
                emit({"event": "output", "id": cell_id, "kind": content["name"], "text": content["text"]})
            elif kind in ("execute_result", "display_data"):
                data = content.get("data", {})
                png = data.get("image/png")
                if png and IMAGE_DIR:
                    images[0] += 1
                    path = os.path.join(IMAGE_DIR, "cell-%s-%d.png" % (cell_id, images[0]))
                    os.makedirs(IMAGE_DIR, exist_ok=True)
                    with open(path, "wb") as fh:
                        fh.write(base64.b64decode(png))
                    emit({"event": "output", "id": cell_id, "kind": "image", "text": path})
                elif "text/plain" in data:
                    emit({"event": "output", "id": cell_id, "kind": "result", "text": data["text/plain"] + "\n"})
            elif kind == "error":
                status = "error"
                text = "\n".join(content.get("traceback", []))
                emit({"event": "output", "id": cell_id, "kind": "error", "text": text + "\n"})
            elif kind == "status" and content["execution_state"] == "idle":
                break
        namespace = marker(quiet(NAMESPACE_CODE), "__FORGE_NS__")
        after = set(marker(quiet(GLOBALS_CODE), "__FORGE_GL__"))
        emit({"event": "done", "id": cell_id, "status": status, "namespace": namespace,
              "defined": sorted(after - before)})
        return True

    while True:
        command = commands.get()
        op = command.get("op")
        if op == "run":
            if not run_cell(command.get("id"), command.get("code", "")):
                break
        elif op == "shutdown":
            break
    try:
        kc.stop_channels()
        km.shutdown_kernel(now=True)
    except Exception:
        pass


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:  # the driver's own failure reaches forge as one line
        emit({"event": "dead", "message": "%s: %s" % (type(exc).__name__, exc)})
