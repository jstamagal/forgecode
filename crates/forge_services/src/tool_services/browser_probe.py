"""forge browser_probe (T14): render a page or local HTML file in headless
Chromium (Playwright, SwiftShader for WebGL), run click/press actions, capture
PNG frames and report per-frame pixel metrics. Reads one JSON spec on stdin,
prints one JSON report on stdout. Pixel metrics are computed from the PNGs
with the standard library (zlib), so Pillow is not needed."""

import functools
import http.server
import json
import os
import re
import socketserver
import struct
import sys
import threading
import zlib
from collections import Counter
from pathlib import Path
from urllib.parse import quote

from playwright.sync_api import sync_playwright

GL_ARGS = ["--use-gl=swiftshader", "--enable-unsafe-swiftshader", "--hide-scrollbars", "--mute-audio"]
# Used when Playwright's own Chromium is not installed.
FALLBACKS = ["/opt/chrome-for-testing/chrome-linux64/chrome", "/usr/bin/google-chrome-stable",
             "/usr/bin/google-chrome", "/usr/bin/chromium", "/usr/bin/chromium-browser"]


def launch(pw):
    try:
        return pw.chromium.launch(args=GL_ARGS)
    except Exception as first:
        for candidate in FALLBACKS:
            if os.path.isfile(candidate):
                return pw.chromium.launch(executable_path=candidate, args=GL_ARGS)
        raise first


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_args):
        pass


class Server(socketserver.ThreadingMixIn, socketserver.TCPServer):
    allow_reuse_address = True
    daemon_threads = True


def serve(path):
    root = path if path.is_dir() else path.parent
    index = "" if path.is_dir() else quote(path.name)
    server = Server(("127.0.0.1", 0), functools.partial(Quiet, directory=str(root)))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, "http://127.0.0.1:%d/%s" % (server.server_address[1], index)


def decode_png(data):
    """RGB rows of an 8-bit RGB/RGBA PNG (what Chromium writes)."""
    pos, chunks, width, height, ctype = 8, [], 0, 0, 2
    while pos < len(data):
        length, kind = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + length]
        if kind == b"IHDR":
            width, height, _, ctype = struct.unpack(">IIBB", body[:10])
        elif kind == b"IDAT":
            chunks.append(body)
        pos += 12 + length
    raw = zlib.decompress(b"".join(chunks))
    bpp = 4 if ctype == 6 else 3
    stride = width * bpp
    rows, prev, i = [], bytearray(stride), 0
    for _ in range(height):
        ftype, line = raw[i], bytearray(raw[i + 1:i + 1 + stride])
        i += 1 + stride
        for x in range(stride):
            a = line[x - bpp] if x >= bpp else 0
            b = prev[x]
            c = prev[x - bpp] if x >= bpp else 0
            if ftype == 1:
                line[x] = (line[x] + a) & 255
            elif ftype == 2:
                line[x] = (line[x] + b) & 255
            elif ftype == 3:
                line[x] = (line[x] + (a + b) // 2) & 255
            elif ftype == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        rows.append(bytes(line[j] for k in range(0, stride, bpp) for j in (k, k + 1, k + 2)))
        prev = line
    return width, height, rows


def metrics(png):
    width, height, rows = decode_png(png)
    counts = Counter()
    for row in rows:
        for x in range(0, len(row), 3):
            counts[((row[x] >> 3) << 10) | ((row[x + 1] >> 3) << 5) | (row[x + 2] >> 3)] += 1
    total = max(1, width * height)
    return {"dimensions": [width, height], "dominant_color_share": round(max(counts.values()) / total, 4),
            "unique_colors": len(counts)}, (width, height, rows)


def changed_pct(before, after):
    if before is None or before[:2] != after[:2]:
        return None
    changed = 0
    for left, right in zip(before[2], after[2]):
        for x in range(0, len(left), 3):
            if max(abs(left[x] - right[x]), abs(left[x + 1] - right[x + 1]), abs(left[x + 2] - right[x + 2])) > 8:
                changed += 1
    return round(changed * 100.0 / max(1, before[0] * before[1]), 3)


def label(text):
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")[:40] or "step"


def main():
    spec = json.loads(sys.stdin.read())
    out = Path(spec["output_dir"])
    out.mkdir(parents=True, exist_ok=True)
    target = spec["target"]
    server = None
    if "://" not in target:
        server, target = serve(Path(target))
    report = {"target": spec["target"], "output_dir": str(out), "frames": [], "clicked": [],
              "console_errors": [], "page_errors": []}
    with sync_playwright() as pw:
        browser = launch(pw)
        page = browser.new_page(viewport={"width": int(spec.get("viewport_width") or 1280),
                                          "height": int(spec.get("viewport_height") or 800)})
        page.on("console", lambda m: m.type == "error" and len(report["console_errors"]) < 50
                and report["console_errors"].append(m.text[:500]))
        page.on("pageerror", lambda e: len(report["page_errors"]) < 50 and report["page_errors"].append(str(e)[:500]))
        page.goto(target, wait_until="load", timeout=60000)
        page.wait_for_timeout(int(spec.get("settle_ms") or 1200))
        report["webgl"] = page.evaluate("""() => {
            const c = document.createElement('canvas');
            return {webgl: !!c.getContext('webgl'), webgl2: !!document.createElement('canvas').getContext('webgl2')};
        }""")
        previous = None

        def capture(name):
            nonlocal previous
            path = out / ("%02d-%s.png" % (len(report["frames"]) + 1, label(name)))
            png = page.screenshot(path=str(path))
            frame, decoded = metrics(png)
            frame["path"] = str(path)
            frame["label"] = name
            frame["changed_pixels_pct"] = changed_pct(previous, decoded)
            previous = decoded
            report["frames"].append(frame)

        capture("loaded")
        for step in [s for s in (spec.get("click") or "").split(">") if s.strip()]:
            pattern = re.compile(step.strip(), re.IGNORECASE)
            target_el = page.get_by_role("button", name=pattern).or_(page.get_by_role("link", name=pattern)).first
            try:
                target_el.click(timeout=5000)
                report["clicked"].append(step.strip())
            except Exception as exc:
                report["clicked"].append("FAILED %s: %s" % (step.strip(), str(exc).splitlines()[0][:200]))
                break
            page.wait_for_timeout(300)
        for key in [k for k in (spec.get("press") or "").split(",") if k.strip()]:
            page.keyboard.press(key.strip())
        if spec.get("click") or spec.get("press"):
            capture("after-actions")
            page.wait_for_timeout(int(spec.get("hold_ms") or 1600))
            capture("held")
        browser.close()
    if server is not None:
        server.shutdown()
    print(json.dumps(report, indent=1))


main()
