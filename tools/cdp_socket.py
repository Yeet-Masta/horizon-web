"""A Chrome DevTools Protocol client over a plain WebSocket, standard library only.

web_profile.py drives the page through Playwright, which opens no CDP session on a worker.
This one talks to the browser's own endpoint (--remote-debugging-port) in flat mode, so it
can attach to the page's workers and run the CPU profiler on each (`worker_profiles`).
"""
import base64
import json
import os
import socket
import struct
import threading
import time
import urllib.parse
import urllib.request


class Socket:
    """One WebSocket to `url`; `send` waits for the answer to its command."""

    def __init__(self, url, timeout=30):
        parts = urllib.parse.urlparse(url)
        self.sock = socket.create_connection((parts.hostname, parts.port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall(
            (f"GET {parts.path} HTTP/1.1\r\nHost: {parts.hostname}:{parts.port}\r\n"
             "Upgrade: websocket\r\nConnection: Upgrade\r\n"
             f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            piece = self.sock.recv(4096)
            if not piece:
                raise ConnectionError("cdp_socket: the browser closed the handshake")
            head += piece
        status = head.split(b"\r\n", 1)[0]
        if b" 101 " not in status:
            raise ConnectionError(f"cdp_socket: handshake refused: {status!r}")
        self.buffer = head.split(b"\r\n\r\n", 1)[1]
        self.sock.settimeout(None)
        self.next_id = 0
        self.answers = {}
        self.events = []
        self.lock = threading.Condition()
        self.closed = False
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _exact(self, n):
        while len(self.buffer) < n:
            piece = self.sock.recv(1 << 20)
            if not piece:
                raise ConnectionError("cdp_socket: closed")
            self.buffer += piece
        out, self.buffer = self.buffer[:n], self.buffer[n:]
        return out

    def _frame(self):
        b0, b1 = self._exact(2)
        opcode, length = b0 & 0x0F, b1 & 0x7F
        if length == 126:
            (length,) = struct.unpack(">H", self._exact(2))
        elif length == 127:
            (length,) = struct.unpack(">Q", self._exact(8))
        mask = self._exact(4) if b1 & 0x80 else None
        data = self._exact(length)
        if mask:
            data = bytes(c ^ mask[i % 4] for i, c in enumerate(data))
        return b0 & 0x80, opcode, data

    def _read(self):
        try:
            message = b""
            while True:
                fin, opcode, data = self._frame()
                if opcode == 8:
                    break
                if opcode == 9:
                    self._write(10, data)
                    continue
                if opcode in (0, 1, 2):
                    message += data
                    if not fin:
                        continue
                    msg = json.loads(message)
                    message = b""
                    with self.lock:
                        if "id" in msg:
                            self.answers[msg["id"]] = msg
                        else:
                            self.events.append(msg)
                        self.lock.notify_all()
        except (OSError, ConnectionError, ValueError):
            pass
        with self.lock:
            self.closed = True
            self.lock.notify_all()

    def _write(self, opcode, payload):
        mask = os.urandom(4)
        n = len(payload)
        if n < 126:
            head = struct.pack(">BB", 0x80 | opcode, 0x80 | n)
        elif n < 1 << 16:
            head = struct.pack(">BBH", 0x80 | opcode, 0x80 | 126, n)
        else:
            head = struct.pack(">BBQ", 0x80 | opcode, 0x80 | 127, n)
        masked = bytes(c ^ mask[i % 4] for i, c in enumerate(payload))
        self.sock.sendall(head + mask + masked)

    def send(self, method, params=None, session=None, timeout=60):
        with self.lock:
            self.next_id += 1
            ident = self.next_id
        msg = {"id": ident, "method": method, "params": params or {}}
        if session:
            msg["sessionId"] = session
        self._write(1, json.dumps(msg).encode())
        deadline = time.time() + timeout
        with self.lock:
            while ident not in self.answers:
                if self.closed or time.time() > deadline:
                    raise TimeoutError(f"cdp_socket: no answer to {method}")
                self.lock.wait(0.5)
            answer = self.answers.pop(ident)
        if "error" in answer:
            raise RuntimeError(f"cdp_socket: {method}: {answer['error']}")
        return answer.get("result", {})

    def take_events(self, method):
        with self.lock:
            taken = [e for e in self.events if e.get("method") == method]
            self.events = [e for e in self.events if e.get("method") != method]
        return taken

    def close(self):
        try:
            self._write(8, b"")
            self.sock.close()
        except OSError:
            pass


def browser_socket(port):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=10) as r:
        return Socket(json.load(r)["webSocketDebuggerUrl"])


class WorkerProfiles:
    """The CPU profiler on every worker of the page at `page_url_part`, started together."""

    def __init__(self, port, page_url_part, interval_us=250):
        self.cdp = browser_socket(port)
        self.interval = interval_us
        targets = self.cdp.send("Target.getTargets")["targetInfos"]
        page = next(t for t in targets if t["type"] == "page" and page_url_part in t["url"])
        self.page_session = self.cdp.send(
            "Target.attachToTarget", {"targetId": page["targetId"], "flatten": True})["sessionId"]
        self.workers = {}
        self._auto_attach(self.page_session)

    def _auto_attach(self, session):
        self.cdp.send("Target.setAutoAttach", {"autoAttach": True, "waitForDebuggerOnStart": False,
                                               "flatten": True}, session=session)
        time.sleep(0.5)
        for event in self.cdp.take_events("Target.attachedToTarget"):
            info, sid = event["params"]["targetInfo"], event["params"]["sessionId"]
            if info["type"] != "worker" or sid in self.workers:
                continue
            self.workers[sid] = info.get("title") or info.get("url", "")
            # A worker's own workers (none so far) are attached too.
            try:
                self._auto_attach(sid)
            except (RuntimeError, TimeoutError):
                pass

    def start(self):
        """Starts the profiler on each worker still running (some end: the page's short-lived
        ones); how many."""
        running = {}
        for sid, title in self.workers.items():
            try:
                self.cdp.send("Profiler.enable", session=sid)
                self.cdp.send("Profiler.setSamplingInterval", {"interval": self.interval},
                              session=sid)
                self.cdp.send("Profiler.start", session=sid)
                running[sid] = title
            except (RuntimeError, TimeoutError):
                pass
        self.workers = running
        return len(running)

    def stop(self):
        """Each worker's profile: [(title, profile)]."""
        out = []
        for sid, title in self.workers.items():
            try:
                out.append((title, self.cdp.send("Profiler.stop", session=sid,
                                                   timeout=120)["profile"]))
            except (RuntimeError, TimeoutError) as e:
                print(f"web_profile: a worker's profile was lost ({e})")
        self.cdp.close()
        return out
