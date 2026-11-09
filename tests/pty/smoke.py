#!/usr/bin/env python3
"""Offline, screen-asserted chui integration tests. Python 3.9+, POSIX, stdlib only."""
import argparse
import base64
import codecs
import errno
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unicodedata
from unittest import SkipTest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from plugins import PLUGINS, commit_fixture, copy_repository

BIN = Path(os.environ.get("TIECHUI_BIN", ROOT / "target/debug/chui")).resolve()
ESC = "\x1b"


class Screen:
    """Incremental VT subset used by chui; cells include wide-character continuations.

    This deliberately does not model terminal width-reflow or font/color rendering.
    Alternate screen saves the main buffer and cursor; 3J erases scrollback only.
    """
    def __init__(self, columns=100, rows=32, background="dark"):
        self.background = background
        self.columns, self.rows = columns, rows
        self.cells = self.blank()
        self.row = self.col = 0
        self.saved = None
        self.updating = False
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.replies = []
        self.queries = set()

    def blank(self):
        return [[" "] * self.columns for _ in range(self.rows)]

    def resize(self, columns, rows):
        def fit(cells):
            return [(line[:columns] + [" "] * columns)[:columns] for line in
                    (cells[:rows] + [[" "] * columns for _ in range(rows)])[:rows]]
        self.cells = fit(self.cells)
        if self.saved:
            cells, row, col = self.saved
            self.saved = (fit(cells), min(row, rows - 1), min(col, columns - 1))
        self.columns, self.rows = columns, rows
        self.row, self.col = min(self.row, rows - 1), min(self.col, columns - 1)

    def newline(self):
        self.row += 1
        if self.row == self.rows:
            self.cells.pop(0)
            self.cells.append([" "] * self.columns)
            self.row -= 1

    def csi(self, params, final):
        if params == "?" and final == "u":
            self.queries.add("keyboard")
            self.replies.append(ESC + "[?0u")
        elif final == "c":
            self.replies.append(ESC + "[?1;2c")
        elif params.startswith("?"):
            if params == "?2026":
                self.updating = final == "h"
            if params == "?1049":
                if final == "h" and self.saved is None:
                    self.saved = (self.cells, self.row, self.col)
                    self.cells, self.row, self.col = self.blank(), 0, 0
                elif final == "l" and self.saved is not None:
                    self.cells, self.row, self.col = self.saved
                    self.saved = None
            return
        elif final == "m":
            return
        elif not params.startswith((">", "<", "=")):
            values = [int(p or 0) for p in params.split(";") if p.isdigit() or not p]
            n = (values[0] if values else 0) or 1
            mode = values[0] if values else 0
            if final in "Hf":
                self.row = n - 1
                self.col = ((values[1] if len(values) > 1 else 1) or 1) - 1
            elif final == "A": self.row -= n
            elif final == "B": self.row += n
            elif final == "C": self.col += n
            elif final == "D": self.col -= n
            elif final == "E": self.row, self.col = self.row + n, 0
            elif final == "F": self.row, self.col = self.row - n, 0
            elif final == "G": self.col = n - 1
            elif final == "d": self.row = n - 1
            elif final == "J":
                if mode == 2:
                    self.cells = self.blank()
                elif mode in (0, 1):
                    for row in range(self.rows):
                        for col in range(self.columns):
                            if ((row, col) >= (self.row, self.col) if mode == 0 else
                                    (row, col) <= (self.row, self.col)):
                                self.cells[row][col] = " "
            elif final == "K":
                start, end = ((0, self.columns) if mode == 2 else
                              (0, self.col + 1) if mode == 1 else (self.col, self.columns))
                self.cells[self.row][start:end] = [" "] * (end - start)
            self.row = max(0, min(self.row, self.rows - 1))
            self.col = max(0, min(self.col, self.columns - 1))

    def feed(self, data):
        self.pending += self.decoder.decode(data)
        while self.pending:
            text = self.pending
            if text.startswith(ESC):
                if len(text) < 2:
                    break
                if text[1] == "[":
                    match = re.match(r"\x1b\[([0-?]*[ -/]*)([@-~])", text)
                    if not match:
                        break
                    self.csi(*match.groups())
                    used = match.end()
                elif text[1] == "]":
                    match = re.match(r"\x1b\](.*?)(?:\x07|\x1b\\)", text, re.S)
                    if not match:
                        break
                    query = match.group(1)
                    if query in ("10;?", "11;?"):
                        code = query[:2]
                        self.queries.add(code)
                        rgb = "dcdc/e1e1/ebeb" if code == "10" else "1414/1919/2828"
                        if self.background == "light":
                            rgb = "2020/2525/3030" if code == "10" else "ffff/ffff/ffff"
                        self.replies.append(ESC + "]" + code + ";rgb:" + rgb + "\x07")
                    used = match.end()
                else:
                    used = 2
                self.pending = text[used:]
                continue
            char, self.pending = text[0], text[1:]
            if char == "\r": self.col = 0
            elif char == "\n": self.newline()
            elif char == "\b": self.col = max(0, self.col - 1)
            elif char == "\t": self.col = min(self.columns - 1, (self.col // 8 + 1) * 8)
            elif char.isprintable() or unicodedata.category(char) == "Co":
                if unicodedata.combining(char):
                    self.cells[self.row][max(0, self.col - 1)] += char
                    continue
                width = 2 if unicodedata.east_asian_width(char) in "WF" else 1
                if self.col + width > self.columns:
                    self.col = 0
                    self.newline()
                self.cells[self.row][self.col] = char
                if width == 2:
                    self.cells[self.row][self.col + 1] = ""
                self.col += width

    def text(self):
        return "\n".join("".join(line).rstrip() for line in self.cells)


class Reply:
    def __init__(self, text="", tools=(), hold=False, usage=100, tail="", status=200):
        self.status = status
        self.tail = tail
        self.text, self.tools, self.usage = text, tools, usage
        self.release = threading.Event()
        if not hold:
            self.release.set()


class Mock:
    models = ["alpha", "beta"]

    def __init__(self, replies):
        self.replies = replies
        self.requests, self.errors = [], []
        mock = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                if not self.path.endswith("/models"):
                    self.send_error(404)
                    return
                body = json.dumps({"data": [{"id": model} for model in mock.models]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                index = len(mock.requests)
                mock.requests.append(request)
                if index >= len(mock.replies):
                    mock.errors.append("Unexpected provider request: " + json.dumps(request))
                    self.send_error(500)
                    return
                reply = mock.replies[index]
                if reply.status != 200:
                    self.send_response(reply.status)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(json.dumps({"error": {"message": "temporarily unavailable"}}).encode())
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()

                def event(delta, finish=None):
                    self.wfile.write(("data: " + json.dumps({"choices": [
                        {"index": 0, "delta": delta, "finish_reason": finish}]}) + "\n\n").encode())
                    self.wfile.flush()
                try:
                    event({"role": "assistant", "content": reply.text})
                    if not reply.release.wait(15):
                        mock.errors.append("Scripted reply was never released")
                        return
                    if reply.tail:
                        event({"content": reply.tail})
                    if reply.tools:
                        event({"tool_calls": [{"index": i, "id": "call_" + str(index) + "_" + str(i),
                               "type": "function", "function": {"name": name, "arguments": json.dumps(args(request) if callable(args) else args)}}
                              for i, (name, args) in enumerate(reply.tools)]})
                    event({}, "tool_calls" if reply.tools else "stop")
                    self.wfile.write(("data: " + json.dumps({"choices": [], "usage": {
                        "prompt_tokens": reply.usage, "completion_tokens": 10}}) +
                        "\n\ndata: [DONE]\n\n").encode())
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass  # Esc intentionally closes an in-flight response.

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        for reply in self.replies:
            reply.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


class Session:
    def __init__(self, fixture, *args):
        self.fixture = fixture
        self.screen = Screen(background=fixture.background)
        self.master, slave = pty.openpty()
        self.original_termios = termios.tcgetattr(slave)
        self.resize(100, 32)
        self.process = subprocess.Popen([str(BIN), *args], cwd=fixture.cwd, env=fixture.env,
                                        stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
        os.close(slave)
        self.raw = bytearray()
        fixture.sessions.append(self)

    def send(self, text):
        os.write(self.master, text.encode())

    def pump(self, duration=0.05):
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if not select.select([self.master], [], [], max(0, deadline - time.monotonic()))[0]:
                break
            try:
                data = os.read(self.master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not data:
                break
            self.raw.extend(data)
            self.screen.feed(data)
            for reply in self.screen.replies:
                self.send(reply)
            self.screen.replies.clear()

    def check(self, condition, message):
        if not condition:
            raise AssertionError(message + "\n--- screen ---\n" + self.screen.text())

    def wait(self, expected, absent=None, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump()
            text = self.screen.text()
            if not self.screen.updating and expected in text and (absent is None or absent not in text):
                return text
            if self.process.poll() is not None:
                break
        self.check(False, "Timed out waiting for %r (absent=%r)" % (expected, absent))

    def draft(self, text):
        self.send(ESC + "[200~" + text + ESC + "[201~")
        self.pump(0.08)

    def command(self, text, expected):
        self.draft(text)
        self.send("\r")
        return self.wait(expected)

    def resize(self, columns, rows):
        self.screen.resize(columns, rows)
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
        if hasattr(self, "process"):
            os.killpg(self.process.pid, signal.SIGWINCH)

    def exit(self):
        self.send("\x04")
        deadline = time.monotonic() + 5
        while self.process.poll() is None and time.monotonic() < deadline:
            self.pump()
        self.check(self.process.poll() == 0, "Ctrl+D did not exit cleanly")
        self.check(self.screen.saved is None, "Exit left alternate screen active")
        self.check(termios.tcgetattr(self.master) == self.original_termios,
                   "Exit did not restore terminal attributes")

    def close(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=3)
        os.close(self.master)


class Fixture:
    def __init__(self, name, replies, background="dark"):
        self.background = background
        temp = ROOT / ".local/pty"
        temp.mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix=name + "-", dir=temp))
        self.home, self.cwd = self.root / "home", self.root / "work"
        self.home.mkdir()
        self.cwd.mkdir()
        subprocess.run(["git", "init", "-q", str(self.cwd)], check=True,
                       env={"PATH": os.environ["PATH"], "HOME": str(self.root),
                            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull})
        self.mock = Mock(replies)
        self.sessions = []
        self.env = {"PATH": os.environ["PATH"], "HOME": str(self.root),
                    "TIECHUI_HOME": str(self.home), "TC_PTY_FAKE_KEY": "not-a-real-key",
                    "TERM": "xterm-256color", "COLORTERM": "truecolor", "LANG": "en_US.UTF-8"}
        self.config = self.home / "config.toml"
        self.config.write_text('# preserve this comment\nmodel = "fixture/alpha"\n', encoding="utf-8")
        self.providers = self.home / "providers.toml"
        self.providers.write_text('[fixture]\nwire = "openai-completions"\n'
                                  'base_url = "http://127.0.0.1:%d"\napi_key_env = "TC_PTY_FAKE_KEY"\n'
                                  'models = [%s]\n' % (self.mock.server.server_port, ", ".join(
                                      '{id = "%s", context_window = 10000, max_output = 512}' % model
                                      for model in self.mock.models)), encoding="utf-8")

    def start(self, *args):
        session = Session(self, *args)
        session.wait("Resume a previous session" if "--resume" in args else "alpha")
        return session

    def cli(self, *args):
        return subprocess.run([str(BIN), *args], cwd=self.cwd, env=self.env, check=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)

    def close(self, failed):
        for i, session in enumerate(self.sessions):
            session.close()
            if failed:
                (self.root / ("terminal-%d.raw" % i)).write_bytes(session.raw)
                (self.root / ("screen-%d.txt" % i)).write_text(session.screen.text(), encoding="utf-8")
        if hasattr(self, "plugin_pids") and self.plugin_pids.exists():
            pids = [int(pid) for pid in self.plugin_pids.read_text().split()]
            for group in pids[::2]:
                try:
                    os.killpg(group, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        self.mock.close()
        if failed:
            (self.root / "requests.json").write_text(json.dumps(self.mock.requests, indent=2), encoding="utf-8")
            print("Kept failure artifacts:", self.root, flush=True)
        else:
            shutil.rmtree(self.root)


def message_text(message):
    content = message["content"]
    return content if isinstance(content, str) else "".join(part.get("text", "") for part in content)


def startup(f):
    s = f.start()
    s.wait("›")
    s.wait("/resume past sessions")
    s.check({"keyboard", "10", "11"} <= s.screen.queries, "Terminal queries were not answered")
    s.check(s.screen.text().count("Tiechui  v") == 1, "Session header missing or repeated")
    s.exit()


def fullscreen(f):
    s = Session(f)
    s.screen.feed(b"SHELL_HISTORY\r\n")
    s.wait("alpha")
    s.check(s.screen.saved is not None, "Main chat did not enter alternate screen")
    s.check("SHELL_HISTORY" not in s.screen.text(), "Shell history leaked into fullscreen chat")
    s.check(b"\x1b[?1000h" in s.raw, "Fullscreen chat did not enable mouse capture")
    s.command("long output", "FULL_HOLD")
    s.draft("preserved draft")
    s.wait("preserved draft")
    composer_row = s.screen.row
    s.send("\x1b[5~")
    s.wait("esc latest", absent="FULL_HOLD")
    s.check(s.screen.row == composer_row, "Scrolling moved the composer")
    before = s.screen.text().splitlines()[:20]
    f.mock.replies[0].release.set()
    s.pump(0.4)
    s.check(s.screen.text().splitlines()[:20] == before, "New output moved the paused transcript")
    s.send("\x1b")
    s.wait("FULL_DONE")
    s.wait("preserved draft")
    for columns, rows in [(40, 18), (100, 32)]:
        s.resize(columns, rows)
        s.wait("preserved draft")
        s.check(s.screen.saved is not None, "Resize left fullscreen mode")
    s.send("\x0f")
    s.wait("T R A N S C R I P T")
    s.send("q")
    s.wait("preserved draft", absent="T R A N S C R I P T")
    s.check(s.screen.saved is not None, "Closing a viewer returned to the shell")
    previous = len(s.raw)
    s.send("\x1a")
    s.pump(0.2)
    os.killpg(s.process.pid, signal.SIGCONT)
    s.pump(0.3)
    s.wait("preserved draft")
    resumed = s.raw[previous:]
    s.check(b"\x1b[?1049l" in resumed and b"\x1b[?1049h" in resumed,
            "Suspend/resume did not restore and reclaim the terminal")
    s.check(b"\x1b[?1000h" in resumed, "Resume did not restore mouse capture")
    s.send("\x03")
    s.wait("alpha", absent="preserved draft")
    s.exit()
    s.check("SHELL_HISTORY" in s.screen.text(), "Exit destroyed the shell history")
    s.check("FULL_DONE" not in s.screen.text(), "Fullscreen output leaked into the shell")


def inline(f):
    s = f.start("--no-alt-screen")
    s.check(s.screen.saved is None, "Inline launch entered alternate screen")
    s.command("inline", "INLINE_DONE")
    s.send("\x0f")
    s.wait("T R A N S C R I P T")
    s.send("q")
    s.wait("INLINE_DONE", absent="T R A N S C R I P T")
    s.check(s.screen.saved is None, "Inline viewer did not restore the main buffer")
    s.exit()
    s.check("INLINE_DONE" in s.screen.text(), "Inline exit lost the native transcript")


def streaming(f):
    s = f.start()
    s.command("stream", "STREAM_PART")
    s.check("STREAM_DONE" not in s.screen.text(), "Held stream already finished")
    f.mock.replies[0].release.set()
    s.wait("STREAM_DONE")
    s.exit()


def tools(f):
    (f.cwd / "one.txt").write_text("one\n")
    (f.cwd / "two.txt").write_text("two\n")
    s = f.start()
    s.command("tools", "TOOLS_DONE")
    s.wait("Explored")
    s.wait("one.txt")
    s.wait("two.txt")
    s.wait("Ran")
    s.check(any(m["role"] == "tool" for m in f.mock.requests[-1]["messages"]), "Missing tool results")
    s.wait("+ Show details")
    row = next(i for i, line in enumerate(s.screen.text().splitlines()) if "+ Show details" in line) + 1
    s.send(f"\x1b[<0;7;{row}M\x1b[<0;7;{row}m")
    s.wait("− Hide details", absent="+ Show details")
    s.exit()


def interrupt(f):
    s = f.start()
    s.command("hold", "INTERRUPT_PART")
    s.send(ESC)
    s.wait("Conversation interrupted")
    s.draft("draft survives")
    s.wait("draft survives")
    s.send("\x03")
    s.wait("alpha", absent="draft survives")
    s.exit()


def retry_interrupt(f):
    with f.config.open("a") as config:
        config.write("\n[retry]\nbase_delay_ms=30000\nmax_delay_ms=60000\n")
    s = f.start()
    s.command("retry wait", "Retrying request")
    s.wait("esc to interrupt")
    timer = re.search(r"Retrying request \(([^•]+) • esc to interrupt\)", s.screen.text()).group(1)
    s.pump(1.2)
    s.check("Retrying request (" + timer + " • esc to interrupt)" in s.screen.text(),
            "Retry wait advanced the activity timer")
    start = time.monotonic()
    s.send(ESC)
    s.wait("Conversation interrupted", absent="Retrying request", timeout=2)
    s.check(time.monotonic() - start < 2, "Esc did not cancel retry promptly")
    s.check(len(f.mock.requests) == 1, "Cancelled retry sent another request")
    s.exit()
    s = f.start("--resume")
    s.send("\r")
    s.wait("retry wait", absent="Resume a previous session")
    s.command("continue after cancellation", "RETRY_RESUMED")
    s.check(len(f.mock.requests) == 2, "Resume repeated the failed attempt")
    s.exit()


def ask(f):
    s = f.start()
    s.command("ask questions", "Question 1/2")
    s.wait("2 unanswered")
    s.send("\t")
    s.draft("discard this note")
    s.send(ESC)
    s.wait("tab to add notes", absent="discard this note")
    s.send("\t")
    s.draft("keep it small")
    s.send("\x0e")
    s.wait("Question 2/2")
    s.send("\r")
    s.wait("Submit with unanswered questions?")
    s.send(ESC)
    s.wait("Question 1/2")
    s.wait("keep it small")
    s.send("\r")
    s.wait("Question 2/2")
    s.draft("free text answer")
    s.send("\r")
    s.wait("ASK_DONE")
    results = [m["content"] for m in f.mock.requests[-1]["messages"] if m["role"] == "tool"]
    response = json.loads(results[0])
    s.check(response == {"answers": {
        "plan": {"answers": ["Small change (Recommended)", "user_note: keep it small"]},
        "notes": {"answers": ["user_note: free text answer"]}}}, "Incorrect Ask answers")
    s.exit()
    s = f.start("--resume")
    s.send("\r")
    s.wait("ASK_DONE", absent="Resume a previous session")
    s.wait("Which implementation?")
    s.wait("answer: free text answer")
    s.check(len(f.mock.requests) == 2, "Resume issued a model request")
    s.exit()


def retry_manual(f):
    with f.config.open("a") as config:
        config.write("\n[retry]\nenabled=false\n")
    s = f.start()
    s.command("original task", "/retry retry the failed request")
    s.check(len(f.mock.requests) == 1, "Automatic retry was enabled")
    s.exit()
    s = f.start("--resume")
    s.send("\r")
    s.wait("/retry retry the failed request", absent="Resume a previous session")
    s.command("/retry extra", "Usage: /retry")
    s.check(len(f.mock.requests) == 1, "Invalid retry arguments issued a request")
    s.command("/retry", "MANUAL_RECOVERED")
    s.check(len(f.mock.requests) == 2, "Manual retry issued extra requests")
    s.check(f.mock.requests[0]["messages"] == f.mock.requests[1]["messages"],
            "Retry changed context or added a user message")
    s.command("/retry", "No failed request to retry.")
    s.check(len(f.mock.requests) == 2, "Retry replayed a successful request")
    s.exit()


def ask_killed(f):
    s = f.start()
    s.command("ask before crash", "Question 1/1")
    os.killpg(s.process.pid, signal.SIGKILL)
    s.process.wait(timeout=3)
    resumed = f.start("--resume")
    resumed.send("\r")
    resumed.wait("ask before crash", absent="Resume a previous session")
    resumed.command("continue after crash", "ASK_CRASH_RECOVERED")
    results = [m["content"] for m in f.mock.requests[-1]["messages"] if m["role"] == "tool"]
    resumed.check(results == ["No result provided"], "Pending Ask was answered or rerun after crash")
    resumed.check(len(f.mock.requests) == 2, "Resume reran the pending Ask")
    resumed.exit()


def ask_cancelled(f):
    s = f.start()
    s.command("ask then cancel", "Question 1/1")
    s.send(ESC)
    s.wait("Questions 0/1 answered (interrupted)")
    s.wait("What should we consider? (unanswered)")
    s.exit()
    resumed = f.start("--resume")
    resumed.send("\r")
    resumed.wait("Questions 0/1 answered (interrupted)", absent="Resume a previous session")
    resumed.check(len(f.mock.requests) == 1, "Cancelled Ask issued another request")
    resumed.exit()


def queue(f):
    s = f.start()
    s.command("hold queue", "QUEUE_HOLD")
    s.draft("follow one")
    s.send("\t")
    s.wait("Queued follow-up inputs")
    s.draft("follow two")
    s.send("\t")
    s.wait("follow two")
    s.draft("steer first")
    s.send("\r")
    s.wait("steer first")
    s.check(len(f.mock.requests) == 1, "Queue sent before the active reply settled")
    f.mock.replies[0].release.set()
    s.wait("QUEUE_LAST")
    users = [[message_text(m) for m in r["messages"] if m["role"] == "user"] for r in f.mock.requests]
    s.check(len(users) == 4 and users[1][-1] == "steer first" and
            users[2][-1] == "follow one" and users[3][-1] == "follow two", "Wrong queue order: " + repr(users))
    s.exit()


def paste(f):
    s = f.start()
    payload = "paste first\nsecond line\nthird line\n" + "long text " * 100
    s.draft(payload)
    s.wait("Pasted")
    s.send("\r")
    s.wait("PASTE_DONE")
    s.check(message_text(f.mock.requests[0]["messages"][-1]) == payload, "Paste payload changed")
    s.exit()


def cjk(f):
    s = f.start()
    for char in "测试中文输入":
        s.send(char)
        s.pump(0.035)
    s.wait("测试中文输入")
    s.send("\r")
    s.wait("中文回答完成")
    s.check(message_text(f.mock.requests[0]["messages"][-1]) == "测试中文输入", "CJK input changed")
    s.exit()


def resize(f):
    s = f.start()
    s.draft("resize draft")
    for columns, rows in [(50, 16), (120, 32), (80, 24)]:
        s.resize(columns, rows)
        s.pump(0.15)
        s.wait("resize draft")
        s.wait("alpha")
        s.check(any("─" * 20 in line for line in s.screen.text().splitlines()), "Composer rule missing after resize")
    s.send("\x03")
    s.wait("alpha", absent="resize draft")
    s.exit()


def viewport_reflow(f):
    s = f.start()
    s.command("reflow", "STREAM_HEAD")
    for columns, rows in [(40, 32), (100, 32), (60, 28)]:
        s.resize(columns, rows)
        s.pump(0.3)
        s.wait("STREAM_HEAD")
        s.check(s.screen.text().count("STREAM_HEAD") == 1, "Duplicated stream during resize")
        s.check(s.screen.text().count("中") == 12, "Torn CJK stream during resize")
    f.mock.replies[0].release.set()
    s.wait("STREAM_END")
    for columns in [40, 100, 60]:
        s.resize(columns, 32)
        s.pump(0.3)
        s.wait("STREAM_END")
        s.check(s.screen.text().count("STREAM_HEAD") == 1, "Duplicated finalized history")
        s.check(s.screen.text().count("STREAM_END") == 1, "Duplicated final stream tail")
        s.check(s.screen.text().count("中") == 12, "Lost finalized CJK history")
    s.draft("safe draft")
    for fragment in ["\x1b]11;", "rgb:ff/ff/ff", "\x1b\\"]:
        s.send(fragment)
        s.pump(0.03)
    s.wait("safe draft")
    s.check("rgb:" not in s.screen.text(), "Late color reply leaked into composer")
    s.send("\x03")
    s.send("\x0f")
    s.wait("T R A N S C R I P T")
    s.check(b"\x1b[?1000h" in s.raw, "Pager did not enable mouse capture")
    mouse_copy(s, "STREAM_HEAD")
    s.send("\x1b")
    s.send("q")
    s.wait("alpha", absent="T R A N S C R I P T")
    s.exit()


def copied_texts(s, start):
    return [base64.b64decode(payload, validate=True).decode("utf-8") for payload in
            re.findall(rb"\x1b\]52;c;([A-Za-z0-9+/=]*)\x1b\\", bytes(s.raw[start:]))]


def mouse_copy(s, text, enabled=True):
    s.wait(text)
    s.pump(0.15)
    row, line = next((row, "".join(cells)) for row, cells in enumerate(s.screen.cells)
                     if text in "".join(cells))
    column = line.index(text)
    start = len(s.raw)
    s.send(f"\x1b[<0;{column + 1};{row + 1}M")
    s.pump(0.1)
    s.check(b"\x1b]52;" not in s.raw[start:], "Mouse press copied before release")
    s.send(f"\x1b[<32;{column + 1 + len(text) // 2};{row + 1}M")
    s.pump(0.15)
    s.check(b"\x1b]52;" not in s.raw[start:], "Mouse drag copied before release")
    release = f"\x1b[<0;{column + 1 + len(text)};{row + 1}m"
    s.send(release)
    s.pump(0.15)
    expected = [text] if enabled else []
    s.check(copied_texts(s, start) == expected, "Release did not copy the selected text exactly once")
    s.send(release)
    s.pump(0.15)
    s.check(copied_texts(s, start) == expected, "Repeated release or redraw copied again")
    if enabled:
        s.wait("chars to clipboard")
    else:
        s.check("chars to clipboard" not in s.screen.text(), "Disabled automatic copy showed a hint")
    return start


def selection_copy(f):
    for args in [(), ("--no-alt-screen",)]:
        s = f.start(*args)
        s.command("copy mouse gesture", "COPY_ON_RELEASE")
        if args:
            s.send("\x0f")
            s.wait("T R A N S C R I P T")
        start = mouse_copy(s, "COPY_ON_RELEASE")
        s.wait("T R A N S C R I P T" if args else "alpha", absent="chars to clipboard")
        s.check(copied_texts(s, start) == ["COPY_ON_RELEASE"], "Hint expiry copied again")
        s.send(ESC)
        s.pump(0.1)
        if args:
            s.send("q")
            s.wait("alpha", absent="T R A N S C R I P T")
        s.exit()


def selection_copy_config(f):
    f.config.write_text(f.config.read_text() + "\n[tui]\ncopy_on_select = false\n")
    s = f.start()
    s.command("COPY_CONFIG_SESSION", "COPY_CONFIG_TEXT")
    start = mouse_copy(s, "COPY_CONFIG_TEXT", enabled=False)
    s.send("\r")
    s.wait("chars to clipboard")
    s.check(copied_texts(s, start) == ["COPY_CONFIG_TEXT"], "Explicit copy failed with automatic copy disabled")
    s.send(ESC)
    s.pump(0.1)
    f.config.write_text(f.config.read_text().replace("copy_on_select = false", "copy_on_select = true"))
    s.command("/reload", "Reloaded")
    mouse_copy(s, "COPY_CONFIG_TEXT")
    s.send(ESC)
    s.pump(0.1)
    s.command("/new", "New session started")
    s.command("new copy session", "COPY_NEW_TEXT")
    mouse_copy(s, "COPY_NEW_TEXT")
    s.send(ESC)
    s.pump(0.1)
    s.command("/resume", "Resume a previous session")
    s.send("COPY_CONFIG_SESSION")
    s.wait("COPY_CONFIG_SESSION")
    s.send("\r")
    s.wait("COPY_CONFIG_TEXT", absent="Resume a previous session")
    mouse_copy(s, "COPY_CONFIG_TEXT")
    s.send(ESC)
    s.pump(0.1)
    f.config.write_text(f.config.read_text().replace("copy_on_select = true", "copy_on_select = false"))
    s.command("/reload", "Reloaded")
    mouse_copy(s, "COPY_CONFIG_TEXT", enabled=False)
    s.send(ESC)
    s.pump(0.1)
    s.exit()


def theme(f):
    s = f.start()
    s.wait("alpha")
    s.command("theme history", "THEME_HISTORY")
    s.pump(0.2)
    def model_style():
        matches = re.findall(rb"\x1b\[([0-9;]+)m[^\x1b\r\n]*alpha", bytes(s.raw))
        s.check(bool(matches), "Model status style missing")
        return matches[-1]
    original = model_style()
    start = len(s.raw)
    s.command("/theme", "Select Syntax Theme")
    s.send("\x1b[B")
    s.pump(0.3)
    s.check(model_style() != original, "Theme preview did not update status colors")
    s.send("\x1b[A\x1b[B")
    s.pump(0.3)
    s.send("\x1b")
    s.wait("alpha", absent="Select Syntax Theme")
    s.pump(0.3)
    s.check(model_style() == original, "Cancel did not restore theme colors")
    s.check("theme =" not in f.config.read_text(), "Cancel saved the preview")
    s.check(b"\x1b[3J" not in s.raw[start:], "Theme preview or cancel erased scrollback")
    s.command("/theme", "Select Syntax Theme")
    s.send("\x1b[B\r")
    s.wait("alpha", absent="Select Syntax Theme")
    s.pump(0.3)
    s.check('theme = "Catppuccin Latte"' in f.config.read_text(), "Selected theme was not saved")
    s.check(b"\x1b[3J" not in s.raw[start:], "Saving a theme erased scrollback")
    if s.screen.saved is None:
        s.check(b"THEME_HISTORY" not in s.raw[start:], "Theme picker reprinted committed history")
    else:
        s.check(s.screen.text().count("THEME_HISTORY") == 1, "Theme picker lost or duplicated visible history")
    s.command("/reload", "Reloaded")
    s.pump(0.3)
    s.check(model_style() != original, "Reload lost the saved theme")
    s.exit()


def resume(f):
    alias = f.root / "linked-work"
    alias.symlink_to(f.cwd, target_is_directory=True)
    f.cli("exec", "--cwd", str(alias), "RESUME_ALPHA")
    f.cli("exec", "RESUME_BETA")
    before = {p: p.read_bytes() for p in f.home.rglob("*.jsonl")}
    s = f.start("--cwd", str(alias), "--resume")
    s.check(any(json.loads(data.splitlines()[0])["cwd"] == str(alias) for data in before.values()),
            "Session header lost the lexical cwd")
    s.wait("RESUME_ALPHA")
    s.wait("RESUME_BETA")
    s.send("ALPHA")
    s.wait("RESUME_ALPHA", absent="RESUME_BETA")
    s.send("\x05")
    s.wait("Created:")
    s.wait("ALPHA_HISTORY")
    s.send("\x14")
    s.wait("Conversation ·")
    s.wait("ALPHA_HISTORY")
    s.send(ESC)
    s.wait("Resume a previous session")
    s.send(ESC)
    s.wait("RESUME_BETA")
    s.send(ESC)
    s.wait("alpha", absent="Resume a previous session")
    s.wait("linked-work")
    s.check(all(p.read_bytes() == data for p, data in before.items()), "Browsing modified old history")
    s.command("/resume", "Resume a previous session")
    s.send("ALPHA")
    s.wait("RESUME_ALPHA", absent="RESUME_BETA")
    s.send("\r")
    s.wait("ALPHA_HISTORY", absent="Resume a previous session")
    s.wait("linked-work")
    s.command("continue resumed", "RESUMED_DONE")
    s.check("ALPHA_HISTORY" in json.dumps(f.mock.requests[-1]["messages"]), "Resumed history absent from request")
    s.exit()
    startup = f.start("--resume")
    startup.send("BETA")
    startup.wait("RESUME_BETA", absent="RESUME_ALPHA")
    startup.send("\r")
    startup.wait("BETA_HISTORY", absent="Resume a previous session")
    startup.check(len(f.mock.requests) == 3, "Picker made a provider request")
    startup.exit()


def model(f):
    s = f.start()
    s.command("/model", "enter select")
    s.wait("fixture/alpha")
    s.wait("fixture/beta")
    s.send("beta\r")
    s.wait("beta", absent="enter select")
    s.command("use beta", "BETA_DONE")
    s.check(f.mock.requests[-1]["model"] == "beta", "Model selection did not reach provider")
    s.command("/model", "enter select")
    s.send("alpha\r")
    s.wait("alpha", absent="enter select")
    s.command("use alpha", "ALPHA_DONE")
    s.check(f.mock.requests[-1]["model"] == "alpha", "Model did not switch back")
    s.exit()
    s.check('model = "fixture/alpha"' in f.config.read_text(), "Picked model was not saved")


def approval(f):
    project = f.cwd / ".agents"
    project.mkdir()
    (project / "config.toml").write_text('[[permissions.rules]]\nsurface="bash"\npattern="*"\naction="ask"\n')
    f.cli("trust", "allow")
    s = f.start()
    s.command("approval", "Would you like to run")
    s.wait("printf")
    s.send("1")
    s.wait("Would you like to run")
    # The second command has a distinct marker, so the first prompt cannot satisfy this wait.
    s.wait("DENIED_SIDE_EFFECT")
    s.send("\x1b[A\r")  # Wrap to the final choice: deny with reason.
    s.wait("differently")
    s.draft("keep the fixture safe")
    s.send("\r")
    s.wait("APPROVAL_DONE")
    results = [m["content"] for m in f.mock.requests[-1]["messages"] if m["role"] == "tool"]
    s.check(any("keep the fixture safe" in str(result) for result in results), "Denial reason did not reach provider")
    s.check((f.cwd / "allowed.txt").read_text() == "approved", "Allow once did not execute")
    s.check(not (f.cwd / "denied.txt").exists(), "Denied command executed")
    s.exit()


def consent(f):
    project = f.cwd / ".agents"
    project.mkdir()
    (project / "hooks.json").write_text(json.dumps({"hooks": {"Stop": [{"hooks": [
        {"type": "command", "command": "printf reviewed"}]}]}}))
    decisions = f.home / "trust.json"
    approvals = f.home / "hooks-trust.json"
    s = Session(f)
    s.wait("Trust this folder?")
    s.exit()
    s.check(not decisions.exists(), "Quitting saved a trust decision")
    s = Session(f)
    s.wait("Trust this folder?")
    s.send("\r")
    s.wait("Hooks need review")
    s.wait("Stop: printf reviewed")
    s.send("\x1b[B\r")  # Continue without approving.
    s.wait("alpha", absent="Hooks need review")
    s.exit()
    s.check(json.loads(decisions.read_text()) == {str(f.cwd.resolve()): True}, "Trust was not saved")
    s.check(not approvals.exists(), "Skipping approved hooks")
    s = Session(f)
    s.wait("Hooks need review")
    s.check("Trust this folder?" not in s.screen.text(), "Saved trust asked again")
    s.send("\r")
    s.wait("alpha", absent="Hooks need review")
    s.exit()
    listed = json.loads(f.cli("hooks", "list", "--json").stdout)
    s.check(listed["hooks"][0]["enabled"], "Approval did not enable the hook")
    f.start().exit()
    decisions.unlink()
    approvals.unlink()
    s = Session(f)
    s.wait("Trust this folder?")
    s.send("\x1b[B\r")  # Continue without project settings: nothing left for hooks to ask.
    s.wait("alpha", absent="Trust this folder?")
    s.check("Hooks need review" not in s.screen.text(), "Untrusted project asked about hooks")
    s.exit()
    s.check(json.loads(decisions.read_text()) == {str(f.cwd.resolve()): False}, "Denial was not saved")


def setup(f):
    f.providers.unlink()
    f.config.write_text("# preserve this comment\n", encoding="utf-8")
    s = Session(f)
    s.wait("Choose the API format")
    s.send("\x1b[B\r")  # OpenAI Chat Completions.
    s.wait("API address")
    s.send("http://127.0.0.1:%d\r" % f.mock.server.server_port)
    s.wait("Paste a key")
    s.send("\x1b[B\r")  # From an environment variable.
    s.wait("Environment variable")
    s.send("TC_PTY_MISSING_KEY\r")
    s.wait("TC_PTY_MISSING_KEY is not set")
    s.send("\x7f" * len("MISSING_KEY") + "FAKE_KEY\r")
    s.wait("2 models at")
    s.send("bt")
    s.wait("1. [ ] beta", absent="alpha")
    s.send(" \x7f\x7f ")  # Check beta, clear the search, check alpha.
    s.wait("2 checked")
    s.wait("2. [x] beta")
    s.send("\r")
    s.wait("Context window")
    s.send("\r")
    s.wait("Max output")
    s.send("\x7f" * 5 + "512\r")
    s.wait("Reasoning")
    s.send("\x1b[B\r")  # No.
    s.wait("/resume past sessions")
    s.exit()
    s.check(f.mock.requests[0]["model"] == "alpha", "The check did not use the first chosen model")
    config = f.config.read_text().splitlines()
    s.check('model = "local/alpha"' in config and "# preserve this comment" in config, "Model not selected")
    providers = f.providers.read_text()
    expected = ('[local]\nwire = "openai-completions"\nbase_url = "http://127.0.0.1:%d"\n'
                'api_key_env = "TC_PTY_FAKE_KEY"\n' % f.mock.server.server_port) + "".join(
        '\n[[local.models]]\nid = "%s"\ncontext_window = 200000\nmax_output = 512\n'
        'reasoning = false\nimages = false\n' % model for model in ["alpha", "beta"])
    s.check(providers == expected, "Unexpected providers.toml:\n" + providers)


def statusline(f):
    s = f.start()
    original = f.config.read_bytes()
    s.command("/statusline", "Configure Status Line")
    s.send("context-remaining\t")
    s.wait("[Icon]")
    s.send(ESC)
    s.wait("alpha", absent="Configure Status Line")
    s.check(f.config.read_bytes() == original, "Cancelled status selection was saved")
    s.command("/statusline", "Configure Status Line")
    s.send("context-remaining\t")
    s.wait("[Icon]")
    s.send("\r")
    s.wait("alpha", absent="Configure Status Line")
    s.command("usage", "USAGE_DONE")
    s.wait("90%")
    s.check(any(chr(code) in s.screen.text() for code in range(0xF240, 0xF245)), "Battery glyph missing")
    s.command("/reload", "Reloaded")
    s.command("/statusline", "Configure Status Line")
    s.send("context-remaining")
    s.wait("[Icon]")
    s.check("# preserve this comment" in f.config.read_text(), "Status save lost config comment")
    s.send(ESC)
    s.wait("alpha", absent="Configure Status Line")
    s.command("/trust", "unknown:")
    s.command("/trust allow", "allowed:")
    s.wait("Applies from the next session.")
    s.exit()


def context(f):
    f.providers.write_text(f.providers.read_text().replace("context_window = 10000", "context_window = 200000"))
    s = f.start()
    s.command("/context", "Context · Overview")
    s.check(s.screen.saved is not None, "Context did not enter alternate screen")
    s.check(len(f.mock.requests) == 0, "Opening context sent a probe")
    s.send("i")
    s.send(ESC + "[F")
    s.wait("No request captured")
    s.send(ESC)
    s.wait("Context · Overview", absent="Details")
    s.send(ESC)
    s.wait("›", absent="Context · Overview")
    s.command("inspect messages", "CONTEXT_DONE")
    before = json.dumps(f.mock.requests, sort_keys=True)
    s.command("/context", "Context · Overview")
    s.send("z")
    s.wait("Fit ")
    for view in ["System", "Tools", "Injections", "Messages"]:
        s.send(ESC + "[C")
        s.wait("Context · " + view)
        if view == "Tools":
            s.send("\r")
            s.wait("Definition")
            s.send(ESC)
            s.wait("Tool definitions declared", absent="Definition")
            s.send(ESC + "[F")
            s.wait("TOTAL")
    s.send("/")
    s.wait("Search ›")
    s.send("assistant\r")
    s.wait("Search: assistant")
    s.send("\r")
    s.wait("CONTEXT_DONE")
    s.send(ESC)
    s.wait("Context · Messages", absent="CONTEXT_DONE")
    s.send(ESC + "[C")
    s.wait("Context · Overview")
    s.send("c")
    s.wait("Context · Cache")
    s.send(ESC + "[F")
    s.wait("unattributed")
    s.resize(40, 20)
    s.wait("Esc back")
    s.send(ESC)
    s.wait("Context · Overview")
    s.send(ESC)
    s.wait("›", absent="Context · Overview")
    for view in ["system", "tools", "injections", "messages", "cache"]:
        s.command("/context " + view, "Context · " + view.title())
        s.send("\x03")
        s.wait("›", absent="Context · ")
    s.check(json.dumps(f.mock.requests, sort_keys=True) == before, "Inspection changed or added a model request")
    s.exit()


def btw(f):
    s = f.start()
    s.wait("›")
    s.command("MAIN_CONTEXT", "MAIN_RUNNING")
    s.command("/btw SIDE_QUESTION", "SIDE_PARTIAL")
    s.wait("Side from main thread")
    s.check(len(f.mock.requests) == 2, "Side request missing")
    side = f.mock.requests[1]
    s.check(bool(side.get("tools")), "Side request is missing tools")
    s.check(not any(tool["function"]["name"] in ("subagent", "create_goal", "compact_history") for tool in side["tools"]), "Side request exposed parent-only tools")
    system = "\n".join(message_text(m) for m in side["messages"] if m["role"] == "system")
    s.check("MAIN_CONTEXT" in system and "<main_instructions>" in system,
            "Side request lacks frozen main snapshot")
    s.draft("SIDE_DRAFT")
    s.send("\x1f")
    s.wait("Side working · ctrl+/ for side")
    f.mock.replies[0].release.set()
    s.wait("MAIN_DONE")
    s.command("/btw REJECT_WHILE_BUSY", "Side reply is still running")
    s.wait("SIDE_DRAFT")
    s.check(len(f.mock.requests) == 2, "Busy question replaced the request")
    s.send(ESC + "[47;5u")
    s.wait("ctrl+/ for side", absent="Ask a follow-up question")
    s.send(ESC + "[55;5u")
    s.wait("SIDE_DRAFT")
    s.send("\x03")  # clear, retaining the side thread
    f.mock.replies[1].release.set()
    s.wait("SIDE_DONE")
    s.send("\x13")
    s.wait("Shared with main conversation")
    s.check(len(f.mock.requests) == 2, "Sharing started a main request")
    s.send("\x04")
    s.wait("›", absent="Side from main thread")
    s.command("NEXT_MAIN_QUESTION", "NEXT_MAIN_DONE")
    shared = [message_text(m) for m in f.mock.requests[2]["messages"] if "Side conversation result" in message_text(m)]
    s.check(len(shared) == 1 and "SIDE_DONE" in shared[0], "Next request lacks the single shared answer")
    s.check(all("Side conversation result" not in message_text(m) for m in f.mock.requests[0]["messages"]), "Sharing changed the in-flight main request")
    s.command("/btw", "Ask a follow-up question")
    s.check("SIDE_QUESTION" not in s.screen.text(), "Close retained side history")
    s.send("\x04")
    s.wait("›", absent="Side from main thread")
    s.exit()


def btw_tools(f):
    f.config.write_text(f.config.read_text() + '\n[[permissions.rules]]\nsurface="*"\npattern="*"\naction="allow"\n[[permissions.rules]]\nsurface="write"\npattern="*"\naction="ask"\n')
    s = f.start()
    s.command("MAIN_CONTEXT", "MAIN_RUNNING")
    s.command("/btw Create side.txt with SIDE_FILE and read it back", "Side from main thread")
    s.wait("main needs approval")
    s.send("\x1f")
    s.wait("Would you like to make")
    s.wait("side.txt")
    s.send("1")
    s.wait("ctrl+/ for side", absent="Would you like to make")
    s.send("\x1f")
    s.wait("SIDE_TOOLS_DONE")
    s.check((f.cwd / "side.txt").read_text() == "SIDE_FILE", "Side write did not execute")
    results = [m for m in f.mock.requests[2]["messages"] if m["role"] == "tool"]
    s.check(len(results) == 2 and "SIDE_FILE" in str(results), "Side tool results did not reach its model")
    s.check(len(f.mock.requests) == 3, "Side tools unexpectedly continued main")
    s.send("\x1f")
    f.mock.replies[0].release.set()
    s.wait("MAIN_DONE")
    s.exit()


def btw_ask(f):
    s = f.start()
    s.wait("›")
    s.command("MAIN_WITH_ASK", "MAIN_WAITING")
    s.command("/btw side question", "SIDE_ANSWER")
    s.draft("SIDE_DRAFT")
    f.mock.replies[0].release.set()
    s.wait("main needs input")
    s.check("SIDE_DRAFT" in s.screen.text() and "Choose a path" not in s.screen.text(), "Main Ask replaced the side view")
    s.send("\x1f")
    s.wait("Choose a path")
    s.send("\r")
    s.wait("MAIN_RESUMED")
    s.send("\x1f")
    s.wait("SIDE_DRAFT")
    s.send("\x03\x04")
    s.wait("›", absent="Side from main thread")
    s.exit()


def goal(f):
    s = f.start()
    s.command("/goal verify local artifacts", "GOAL_FINISHED")
    s.wait("Goal achieved")
    s.pump(0.3)
    s.check(len(f.mock.requests) == 6, "Goal did not stop after completion")
    s.command("/goal", "Status: complete")
    s.check(len(f.mock.requests) == 6, "Viewing goal sent a model request")
    session_file = next((f.home / "sessions").rglob("*.jsonl"))
    entries = [json.loads(line) for line in session_file.read_text().splitlines()]
    states = [e["data"] for e in entries if e.get("custom_kind") == "tiechui.goal.state"]
    s.check(states[-1]["goal"]["status"] == "complete", "Goal completion was not saved")
    s.check(states[-1]["goal"]["tokens_used"] == 660, "Goal accounting formula changed")
    s.send(ESC + "[A")
    s.pump(0.1)
    s.check("<active_goal" not in s.screen.text(), "Hidden continuation entered input history")
    s.send("\x03")
    s.exit()


def goal_pause(f):
    s = f.start()
    s.command("/goal inspect and wait", "GOAL_CONTINUING")
    s.send(ESC)
    s.wait("Goal paused")
    f.mock.replies[2].release.set()
    s.pump(0.3)
    s.check(len(f.mock.requests) == 3, "Interrupted goal continued")
    s.command("/goal", "Status: paused")
    s.exit()


def scheduled(f):
    s = f.start()
    text = s.command("/loop 7m check LOOP_PROMPT", "LOOP_RAN")
    task = re.search(r"Scheduled ([0-9a-f]{8})", text).group(1)
    s.check("every 6 minutes" in text and "7m rounded" in text, "Interval rounding was not reported")
    s.check(any(m["role"] == "user" and message_text(m) == "check LOOP_PROMPT"
                for m in f.mock.requests[0]["messages"]), "The loop prompt was not the turn's input")
    s.wait("1 scheduled task")
    s.command("/cron-list", task + " · every 6 minutes · session")
    s.command("/cron-delete " + task, "Deleted " + task)
    s.wait("Deleted " + task, absent="1 scheduled task")
    s.command("keep a daily check", "DURABLE_DONE")
    saved = json.loads((f.cwd / ".agents/scheduled_tasks.json").read_text())
    s.check([t["prompt"] for t in saved["tasks"]] == ["DURABLE_PROMPT"], "Durable task was not saved")
    s.pump(0.3)
    s.check(len(f.mock.requests) == 3, "A deleted or future task ran")
    s.exit()


def rewind(f):
    f.config.write_text(f.config.read_text() + '\n[tui.status_line]\nitems=["model", "run-state"]\n')
    path = f.cwd / "rewind.txt"
    path.write_text("original\n")
    s = f.start()
    s.command("change the tracked file", "Yes, proceed")
    s.send("1")
    s.wait("REWIND_EDIT_DONE")
    s.wait("Ready")  # Streamed answer text can arrive before TurnCompleted.
    s.check(path.read_text() == "changed\n", "Mock edit did not change the file")
    requests = len(f.mock.requests)
    s.send(ESC)
    s.wait("esc again to rewind")
    s.wait("alpha", absent="esc again to rewind")
    s.send(ESC)
    s.pump(0.03)
    s.send(ESC)
    s.wait("Select the point you want to restore to.")
    s.send("\r")
    s.wait("Restore code and conversation")
    s.send("\r")
    s.wait("prompt returned unsent")
    s.wait("› change the tracked file")
    s.check(path.read_text() == "original\n", "Rewind did not restore the file")
    s.pump(0.2)
    s.check(len(f.mock.requests) == requests, "Rewind sent a model request")
    s.send(ESC)
    s.wait("esc again to clear input")
    s.wait("alpha", absent="esc again to clear input")
    s.send(ESC)
    s.pump(0.03)
    s.send(ESC)
    s.wait("\n›\n", absent="esc again to clear input")
    session_file = next((f.home / "sessions").rglob("*.jsonl"))
    def drafts():
        return [entry for entry in map(json.loads, session_file.read_text().splitlines())
                if entry.get("custom_kind") == "tiechui.composer.draft"]
    s.check(not drafts(), "Text-only refill wrote a draft entry")
    for character in "/reload":
        s.send(character)
        s.pump(0.03)
    s.check(not drafts(), "Typing /reload wrote draft entries")
    old_count = s.screen.text().count("REWIND_EDIT_DONE")
    start = len(s.raw)
    s.send("\r")
    s.wait("Reloaded")
    s.pump(0.2)
    if s.screen.saved is None:
        s.check(b"REWIND_EDIT_DONE" not in s.raw[start:], "Reload replayed old history")
    s.check(s.screen.text().count("REWIND_EDIT_DONE") == old_count, "Reload duplicated visible history")
    s.check(b"\x1b[3J" not in s.raw[start:], "Reload erased scrollback")
    s.check("› /reload" not in s.screen.text(), "Reload restored the submitted command")
    s.command("/rewind", "No previous message or control operation to rewind.")
    s.check(not drafts(), "Reload wrote a text draft")
    s.check(len(f.mock.requests) == requests, "Reload sent a model request")
    s.exit()


def compaction(f):
    f.config.write_text(f.config.read_text() + '\n[compaction]\nreserve_tokens=1000\nkeep_recent_tokens=40\n')
    s = f.start()
    old = "ANCIENT_HISTORY " * 100
    recent = "RECENT_HISTORY " * 30
    following = "FOLLOWING_REQUEST " * 30
    s.command(old, "FIRST_COMPACTION_TURN")
    s.command(recent, "Auto-compacting")
    s.check(len(f.mock.requests) == 3, "Threshold did not issue exactly one summary request")
    summary = f.mock.requests[2]
    s.check(not summary.get("tools"), "Summary request included tools")
    s.check("ANCIENT_HISTORY" in json.dumps(summary), "Summary missed old conversation")
    f.mock.replies[2].release.set()
    s.wait("Compacted context", absent="Auto-compacting")
    s.command(following, "AFTER_AUTO_COMPACTION")
    request = json.dumps(f.mock.requests[3])
    s.check("AUTO_CHECKPOINT" in request and "RECENT_HISTORY" in request,
            "Next request lost the summary or kept messages")
    s.check("ANCIENT_HISTORY" not in request, "Next request retained compacted messages")
    s.command("/compact preserve file paths exactly", "Compacting context")
    manual = f.mock.requests[4]
    s.check("Additional focus: preserve file paths exactly" in json.dumps(manual),
            "Manual compaction omitted custom instructions")
    s.check("<previous-summary>" in json.dumps(manual) and "AUTO_CHECKPOINT" in json.dumps(manual),
            "Repeated compaction did not update the previous summary")
    f.mock.replies[4].release.set()
    s.wait("Compacted context", absent="Compacting context")
    s.exit()
    resumed = f.start("--resume")
    resumed.send("\r")
    resumed.wait("Compacted context", absent="Resume a previous session")
    resumed.command("continue after compaction", "COMPACTION_RESUMED")
    request = json.dumps(f.mock.requests[5])
    resumed.check("MANUAL_CHECKPOINT" in request and "FOLLOWING_REQUEST" in request,
                  "Resume lost the checkpoint or retained tail")
    resumed.check("ANCIENT_HISTORY" not in request and "AUTO_CHECKPOINT" not in request,
                  "Resume included replaced history")
    resumed.exit()


RANGE_SUMMARY = "Historical report review completed: obsolete diagnostic noise was examined. Preserve current constraints and use read_history for original evidence."


def first_candidate(request):
    status = next(m for m in reversed(request["messages"]) if m["role"] == "tool")
    return json.loads(status["content"])["candidates"][0]["id"]


def range_compaction(f):
    f.config.write_text(f.config.read_text() + '\n[compaction]\nenabled=false\npreserve_recent_messages=0\npreserve_recent_tokens=0\n[compaction.output]\nenabled=false\n')
    s = f.start()
    s.command("Review the old report", "REPORT_END")
    s.command("Continue with the current task", "RANGE_COMPACTED")
    s.wait("Maintained context")
    s.check(len(f.mock.requests) == 5, "Range compaction made an extra summary request")
    status = json.dumps(f.mock.requests[2])
    s.check("candidate" in status and "summary_limit_chars" in status, "Status did not expose candidates")
    receipt = json.dumps(f.mock.requests[3])
    s.check("net_saved_tokens_estimate" in receipt, "Compaction did not return a receipt")
    request = json.dumps(f.mock.requests[3])
    s.check(request.count(RANGE_SUMMARY) == 1, "Summary was absent or duplicated")
    # Status excerpts quote the report's start, so its tail marks the compacted body.
    s.check("REPORT_END" not in request, "Compressed report remained in next request")
    cooling = json.loads(next(m for m in reversed(f.mock.requests[4]["messages"]) if m["role"] == "tool")["content"])
    s.check(cooling["maintenance"]["state"] == "cooldown" and not cooling.get("candidates"), "Status offered candidates during cooldown")
    raw = "".join(path.read_text() for path in f.home.glob("sessions/**/*.jsonl"))
    s.check("OLD_RANGE_EVIDENCE" in raw and "tiechui.compaction.v1" in raw, "Session did not retain raw history and state")
    s.command("/reload", "Reloaded")
    s.check(len(f.mock.requests) == 5, "Reload called the model")
    s.command("Continue after reload", "RANGE_RELOADED")
    request = json.dumps(f.mock.requests[5])
    s.check(request.count(RANGE_SUMMARY) == 1 and "REPORT_END" not in request, "Reload lost the range projection")
    s.command("/context cache", "compression-associated")
    s.check(len(f.mock.requests) == 6, "Context inspection called the model")
    s.send(ESC)
    s.wait("Context · Overview")
    s.send(ESC)
    s.wait("›", absent="Context · Overview")
    s.exit()


def subagents(f):
    f.config.write_text(f.config.read_text() + '\n[[permissions.rules]]\nsurface="*"\npattern="*"\naction="allow"\n[[permissions.rules]]\nsurface="write"\npattern="*"\naction="ask"\n')
    s = f.start()
    s.command("delegate two tasks", "Would you like to make")
    s.wait("child.txt")
    s.send("1")
    # The writer remains active after its edit while the other task has finished.
    deadline = time.monotonic() + 8
    while len(f.mock.requests) < 4 and time.monotonic() < deadline:
        s.pump()
    s.check(len(f.mock.requests) == 4, "Child did not continue after forwarded approval")
    s.check((f.cwd / "child.txt").read_text() == "child edit", "Approved child edit did not execute")
    s.send(ESC + "[1;3C")
    s.wait("Subagents")
    s.send(ESC + "[C")  # Any member selects the same dispatch for Ctrl+X.
    s.send("\x18")
    s.wait("− worker")
    s.send(ESC)
    s.wait("PARENT_DONE")
    s.wait("· cancelled")
    s.wait("FINISHED_RESULT")
    s.check(len(f.mock.requests) == 5, "Cancellation started an unexpected child or request")
    s.exit()


def composer_resources(f):
    # Native discovery must work without fd, rg, or git on the child's PATH.
    f.env["PATH"] = str(f.root / "no-search-programs")
    skill = f.home / "skills" / "review" / "SKILL.md"
    skill.parent.mkdir(parents=True)
    skill.write_text("---\nname: review\ndescription: Review a fixture\ndisable-model-invocation: true\n---\nSKILL_BODY_ON_SUBMIT\n")
    (f.cwd / "中文 notes.txt").write_text("FILE_BODY_NOT_INSERTED")
    (f.cwd / "ignored notes.txt").write_text("IGNORED")
    (f.cwd / ".gitignore").write_text("ignored*\n")
    (f.cwd / "src").mkdir()
    (f.cwd / "src" / "main.rs").write_text("FILE_BODY_NOT_INSERTED")
    s = f.start()
    s.draft('@notes')
    s.wait('中文 notes.txt')
    rows = s.screen.text().splitlines()
    s.check(next(i for i, r in enumerate(rows) if '> 中文 notes.txt' in r) < next(i for i, r in enumerate(rows) if '› @notes' in r), "Candidates are below the composer")
    s.send("\t")
    s.wait('› @"中文 notes.txt"')
    s.draft('$review')
    s.wait('manual')
    s.send("\t")
    s.wait('› @"中文 notes.txt" $review')
    s.check(not f.mock.requests, "Selecting references submitted the draft")
    s.send("\r")
    s.wait('RESOURCES_DONE')
    sent = json.dumps(f.mock.requests[0], ensure_ascii=False)
    s.check(str(skill) in sent and '中文 notes.txt' in sent, "References lost real paths")
    s.check('FILE_BODY_NOT_INSERTED' not in sent and 'SKILL_BODY_ON_SUBMIT' in sent, "Explicit skill was not loaded on submission")
    s.draft('@src')
    s.wait('src/')
    s.send("\t")
    s.wait('main.rs')
    s.wait('› @src/')
    s.send("\r")
    s.wait('› @src/main.rs')
    s.check(len(f.mock.requests) == 1, "Directory navigation submitted input")
    s.send("\x15")
    s.draft('@review')
    s.wait('No matches')
    s.send(ESC + '[Z')
    s.wait('$review')
    s.send("\t")
    s.wait('› $review')
    s.send("\x15")
    s.command('/skills', 'Skills')
    s.wait('manual')
    s.send(ESC)
    s.wait('›', absent='Tab/Enter insert')
    s.command('/skills', 'Skills')
    s.send("\r")
    s.wait('› $review')
    s.check(len(f.mock.requests) == 1, "Skill picker sent a provider request")
    s.send("\x15")
    s.command('/permissions show', 'Effective rules (later matches win per surface)')
    s.send(ESC)
    s.wait('›', absent='Effective rules (later matches win per surface)')
    s.exit()
    listing = f.cli('skills', 'list').stdout.decode()
    if 'review' not in listing or str(skill) not in listing:
        raise AssertionError('Headless skills list did not list loaded skills')
    # The same menu must remain usable in a short inline terminal.
    s = f.start('--no-alt-screen')
    s.resize(40, 8)
    s.pump(0.2)
    s.draft('$review')
    s.wait('> $review')
    s.wait('› $review')
    s.send("\t")
    s.send("\x15")
    s.exit()


def allow_tool(f, tool):
    """Plugin tools ask by default; these scenarios exercise the plugin, not the approval."""
    with f.config.open("a", encoding="utf-8") as config:
        config.write('[permissions]\nrules = [{surface = "%s", pattern = "*", action = "allow"}]\n' % tool)


def t2_plugins(f, marketplace=False):
    f.plugin_pids = f.root / "pids"
    package = f.root / "plugin"
    (package / ".tiechui-plugin").mkdir(parents=True)
    shutil.copy2(ROOT / "target/debug/t2-test-plugin", package / "server")
    manifest = {
        "name": "fixture", "tiechui": {"apiVersion": 1},
        "extension": {
            "command": "server", "args": [str(f.root)],
            "capabilities": ["tools", "commands", "tool.intercept", "ui.panel", "ui.notify", "status.set"],
            "contributions": {
                "tools": [{"name": "echo", "description": "Test plugin tool", "inputSchema": {"type": "object"}}],
                "commands": [{"name": "ui", "description": "Open plugin panel"}],
                "interceptors": ["before_policy"],
            },
        },
    }
    (package / ".tiechui-plugin/plugin.json").write_text(json.dumps(manifest), encoding="utf-8")
    def git(*args):
        return subprocess.run(["git", *args], cwd=package, env={**f.env,
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
            "GIT_COMMITTER_NAME": "Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid"},
            check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout.decode().strip()

    def commit(parent=None):
        for path in [".tiechui-plugin/plugin.json", "server", "marketplace.json"]:
            blob = git("hash-object", "-w", path)
            mode = "100755" if path == "server" else "100644"
            git("update-index", "--add", "--cacheinfo", mode + "," + blob + "," + path)
        tree = git("write-tree")
        revision = git("commit-tree", tree, "-m", "fixture", *(["-p", parent] if parent else []))
        git("update-ref", "refs/heads/main", revision)
        return revision

    if marketplace:
        git("init", "-q", "-b", "main")
        (package / "marketplace.json").write_text(json.dumps({
            "name": "local-market", "plugins": [{"name": "fixture", "source": {"source": "local", "path": "."}}]
        }), encoding="utf-8")
        revision = commit()
        f.cli("plugin", "marketplace", "add", str(package))
        f.cli("plugin", "add", "fixture@local-market")
    else:
        f.cli("plugin", "add", str(package))
    f.cli("trust", "allow")
    allow_tool(f, "plugin_fixture_echo")
    s = Session(f)
    s.wait("Authorize plugin fixture")
    s.check(not (f.root / "pids").exists(), "Plugin started before capability approval")
    s.send("\r")
    s.wait("fixture: ready")
    s.command("call the plugin", "T2_TOOL_DONE")
    s.check("Plugin tool result" in s.raw.decode("utf-8", "replace"), "Missing plugin tool result")
    s.command("try the denied action", "T2_DENY_DONE")
    s.check("Denied by test plugin" in s.raw.decode("utf-8", "replace"), "Missing interceptor denial")
    s.command("/plugin_fixture_ui panel", "Test plugin panel")
    s.wait("Build: 1/2")
    s.check("Markdown node" in s.screen.text(), "Missing declarative markdown")
    s.send("\x1b")
    s.wait("UI completed")
    s.command("/plugin_fixture_ui notify", "Plugin notification")
    s.wait("UI completed")
    responses = [json.loads(line) for line in (f.root / "responses").read_text().splitlines()]
    s.check(responses[-1] == {"result": None}, "Notification did not return null immediately")
    s.command("/plugin_fixture_ui status", "Fixture status")
    pids = [int(pid) for pid in (f.root / "pids").read_text().split()]
    s.exit()
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        remaining = []
        for pid in pids:
            try:
                os.kill(pid, 0)
                remaining.append(pid)
            except ProcessLookupError:
                pass
        if not remaining:
            break
        time.sleep(0.05)
    s.check(not remaining, "Plugin process group survived exit: %r" % remaining)
    f.mock.replies.extend([Reply(tools=[("plugin_fixture_echo", {"action": "notify"})]), Reply("NOTIFY_DONE")])
    result = f.cli("exec", "call the notification tool")
    s.check(b"fixture: Notice: Plugin notification" in result.stderr, "Headless notification missing from stderr")
    s.check(b"Plugin notification" not in result.stdout, "Headless notification leaked to stdout")
    s.check(b"NOTIFY_DONE" in result.stdout, "Headless notification blocked the turn")
    if marketplace:
        manifest["version"] = "2.0.0"
        (package / ".tiechui-plugin/plugin.json").write_text(json.dumps(manifest), encoding="utf-8")
        upgraded = commit(revision)
        result = f.cli("plugin", "upgrade", "fixture").stdout.decode()
        s.check(revision in result and upgraded in result, "Upgrade did not show commit change")
        s.check("M .tiechui-plugin/plugin.json" in result, "Upgrade did not show changed manifest")
        s.check("capabilities require approval" in result, "T2 upgrade omitted capability notice")
        s = Session(f)
        s.wait("Authorize plugin fixture")
        s.send("\r")
        s.wait("fixture: ready")
        s.exit()
        for extension in [None, {"command": "server", "capabilities": []}]:
            if extension is None:
                manifest.pop("extension")
            else:
                manifest["extension"] = extension
            (package / ".tiechui-plugin/plugin.json").write_text(json.dumps(manifest), encoding="utf-8")
            upgraded = commit(upgraded)
            result = f.cli("plugin", "upgrade", "fixture").stdout.decode()
            s.check("capabilities require approval" not in result, "Upgrade without capabilities requested approval")


def sdk_plugin(f, language):
    if not PLUGINS.is_dir():
        raise SkipTest("sibling tiechui-plugins repository is absent")
    if language == "typescript" and shutil.which("node") is None:
        raise SkipTest("node is not installed")
    repository = f.root / "plugins-repository"
    copy_repository(repository)
    name = "sdk-" + language
    package = repository / "plugins" / name
    if language == "rust":
        shutil.copy2(ROOT / "target/debug/tiechui-example-plugin", package / "server")
    commit_fixture(repository, f.home)
    f.cli("plugin", "marketplace", "add", str(repository), "--ref", "fixture-v1")
    f.cli("plugin", "add", name + "@tiechui-plugins")
    f.cli("trust", "allow")
    allow_tool(f, "plugin_%s_greet" % name)
    s = Session(f)
    s.wait("Authorize plugin " + name)
    s.send("\r")
    s.wait(name + ": ready")
    s.command("greet the SDK", "SDK_TOOL_DONE")
    s.check("Hello, world!" in s.raw.decode("utf-8", "replace"), "Missing SDK tool result")
    s.command("greet the blocked name", "SDK_DENY_DONE")
    s.check("The example blocks this name" in s.raw.decode("utf-8", "replace"), "Missing SDK interceptor denial")
    s.command("/plugin_" + name + "_panel", "SDK example panel")
    s.wait("Hello from the plugin SDK")
    s.send(ESC)
    s.wait("Panel closed")
    s.exit()


def sdk_replies(language):
    tool = "plugin_sdk-" + language + "_greet"
    return [Reply(tools=[(tool, {})]), Reply("SDK_TOOL_DONE"),
            Reply(tools=[(tool, {"name": "blocked"})]), Reply("SDK_DENY_DONE")]


def diagnostics_disabled(f):
    f.config.write_text(f.config.read_text() + '\n[modules]\nediting=false\npermissions=false\nskills=false\nmcp=false\nsubagents=false\ncontext=false\nbtw=false\ngoal=false\nloop=false\nrewind=false\ncompaction=false\nask=false\nhooks=false\nexec=false\ncontext-files=false\n')
    s = f.start()
    s.command('/status', 'editing: disabled')
    s.wait('User config:')
    s.check(not f.mock.requests, "Diagnostics called the model")
    s.exit()


CASES = {
    "fullscreen": (fullscreen, lambda: [Reply("\n".join("FULL_ROW_%02d" % n for n in range(70)) + "\nFULL_HOLD", hold=True, tail=" FULL_DONE")]),
    "inline": (inline, lambda: [Reply("INLINE_DONE")]),
    "marketplace": (lambda f: t2_plugins(f, marketplace=True), lambda: [Reply(tools=[("plugin_fixture_echo", {})]), Reply("T2_TOOL_DONE"), Reply(tools=[("plugin_fixture_echo", {"action": "deny"})]), Reply("T2_DENY_DONE")]),
    "sdk-typescript": (lambda f: sdk_plugin(f, "typescript"), lambda: sdk_replies("typescript")),
    "sdk-rust": (lambda f: sdk_plugin(f, "rust"), lambda: sdk_replies("rust")),
    "t2-plugins": (t2_plugins, lambda: [Reply(tools=[("plugin_fixture_echo", {})]), Reply("T2_TOOL_DONE"), Reply(tools=[("plugin_fixture_echo", {"action": "deny"})]), Reply("T2_DENY_DONE")]),
    "composer-resources": (composer_resources, lambda: [Reply("RESOURCES_DONE")]),
    "diagnostics-disabled": (diagnostics_disabled, lambda: []),
    "subagents": (subagents, lambda: [Reply(tools=[("subagent", {"tasks": [{"agent": "worker", "task": "edit a file"}, {"agent": "worker", "task": "inspect"}]})]), Reply(tools=[("write", {"path": "child.txt", "content": "child edit"})]), Reply("FINISHED_RESULT"), Reply("EDITED_WAIT", hold=True), Reply("PARENT_DONE")]),
    "range-compaction": (range_compaction, lambda: [Reply("OLD_RANGE_EVIDENCE " * 600 + "REPORT_END"), Reply(tools=[("context_status", {})]), Reply(tools=[("compact_history", lambda request: {"ranges": [{"candidate_id": first_candidate(request), "summary": RANGE_SUMMARY}]})]), Reply(tools=[("context_status", {})]), Reply("RANGE_COMPACTED"), Reply("RANGE_RELOADED")]),
    "compaction": (compaction, lambda: [Reply("FIRST_COMPACTION_TURN"), Reply("SECOND_COMPACTION_TURN", usage=9500), Reply("AUTO_CHECKPOINT", hold=True), Reply("AFTER_AUTO_COMPACTION"), Reply("MANUAL_CHECKPOINT", hold=True), Reply("COMPACTION_RESUMED")]),
    "btw": (btw, lambda: [Reply("MAIN_RUNNING", hold=True, tail=" MAIN_DONE"), Reply("SIDE_PARTIAL", hold=True, tail=" SIDE_DONE"), Reply("NEXT_MAIN_DONE")]),
    "btw_tools": (btw_tools, lambda: [Reply("MAIN_RUNNING", hold=True, tail=" MAIN_DONE"), Reply(tools=[("write", {"path": "side.txt", "content": "SIDE_FILE"}), ("read", {"path": "side.txt"})]), Reply("SIDE_TOOLS_DONE")]),
    "btw_ask": (btw_ask, lambda: [Reply("MAIN_WAITING", hold=True, tools=[("request_user_input", {"questions": [{"id": "path", "header": "Path", "question": "Choose a path", "options": [{"label": "First", "description": "Use the first path"}, {"label": "Second", "description": "Use the second path"}]}]})]), Reply("SIDE_ANSWER"), Reply("MAIN_RESUMED")]),
    "goal": (goal, lambda: [Reply(tools=[("get_goal", {})]), Reply("GOAL_FIRST"), Reply(tools=[("get_goal", {})]), Reply("GOAL_SECOND"), Reply(tools=[("update_goal", {"status": "complete"})]), Reply("GOAL_FINISHED")]),
    "goal-pause": (goal_pause, lambda: [Reply(tools=[("get_goal", {})]), Reply("GOAL_FIRST"), Reply("GOAL_CONTINUING", hold=True)]),
    "scheduled": (scheduled, lambda: [Reply("LOOP_RAN"), Reply(tools=[("cron_create", {"cron": "7 9 * * *", "prompt": "DURABLE_PROMPT", "durable": True})]), Reply("DURABLE_DONE")]),
    "rewind": (rewind, lambda: [Reply(tools=[("write", {"path": "@rewind.txt", "content": "changed\n"})]), Reply("REWIND_EDIT_DONE")]),
    "context": (context, lambda: [Reply("CONTEXT_DONE", usage=1000)]),
    "ask-cancelled": (ask_cancelled, lambda: [Reply(tools=[("request_user_input", {"questions": [
        {"id": "notes", "header": "Details", "question": "What should we consider?"}
    ]})])]),
    "ask-killed": (ask_killed, lambda: [Reply(tools=[("request_user_input", {"questions": [
        {"id": "notes", "header": "Details", "question": "What should we consider?"}
    ]})]), Reply("ASK_CRASH_RECOVERED")]),
    "ask": (ask, lambda: [Reply(tools=[("request_user_input", {"questions": [
        {"id": "plan", "header": "Plan", "question": "Which implementation?", "options": [
            {"label": "Small change (Recommended)", "description": "Keep the existing interface."},
            {"label": "Redesign", "description": "Replace the interface."}]},
        {"id": "notes", "header": "Details", "question": "What else should we consider?"}
    ]})]), Reply("ASK_DONE")]),
    "retry-manual": (retry_manual, lambda: [Reply(status=500), Reply("MANUAL_RECOVERED")]),
    "startup": (startup, lambda: []),
    "consent": (consent, lambda: []),
    "setup": (setup, lambda: [Reply("OK")]),
    "streaming": (streaming, lambda: [Reply("STREAM_PART\n", hold=True, tail="STREAM_DONE")]),
    "tools": (tools, lambda: [Reply(tools=[("read", {"path": "one.txt"}), ("read", {"path": "two.txt"})]),
                               Reply(tools=[("bash", {"command": "printf tool-output"})]), Reply("TOOLS_DONE")]),
    "interrupt": (interrupt, lambda: [Reply("INTERRUPT_PART\n", hold=True)]),
    "retry-interrupt": (retry_interrupt, lambda: [Reply(status=503), Reply("RETRY_RESUMED")]),
    "queue": (queue, lambda: [Reply("QUEUE_HOLD\n", hold=True), Reply("STEER_DONE"), Reply("FOLLOW_ONE"), Reply("QUEUE_LAST")]),
    "paste": (paste, lambda: [Reply("PASTE_DONE")]),
    "cjk": (cjk, lambda: [Reply("中文回答完成")]),
    "resize": (resize, lambda: []),
    "viewport-reflow": (viewport_reflow, lambda: [Reply("STREAM_HEAD " + "中文 word " * 12 + "\n", hold=True, tail="STREAM_END")]),
    "selection-copy": (selection_copy, lambda: [Reply("COPY_ON_RELEASE"), Reply("COPY_ON_RELEASE")]),
    "selection-copy-config": (selection_copy_config, lambda: [Reply("COPY_CONFIG_TEXT"), Reply("COPY_NEW_TEXT")]),
    "theme": (theme, lambda: [Reply("```rs\nlet value = 42;\n```\n\nTHEME_HISTORY")]),
    "resume": (resume, lambda: [Reply("ALPHA_HISTORY"), Reply("BETA_HISTORY"), Reply("RESUMED_DONE")]),
    "model": (model, lambda: [Reply("BETA_DONE"), Reply("ALPHA_DONE")]),
    "approval": (approval, lambda: [Reply(tools=[("bash", {"command": "printf approved > allowed.txt"})]),
                                     Reply(tools=[("bash", {"command": "printf DENIED_SIDE_EFFECT > denied.txt"})]), Reply("APPROVAL_DONE")]),
    "statusline": (statusline, lambda: [Reply("USAGE_DONE", usage=1000)]),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cases", nargs="*", help="case names (default: all)")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--background", choices=["dark", "light"], default="dark")
    args = parser.parse_args()
    if args.list:
        print("\n".join(CASES))
        return 0
    for name in args.cases:
        if name not in CASES:
            parser.error("Unknown case: " + name)
    for name in args.cases or CASES:
        case, replies = CASES[name]
        fixture = Fixture(name, replies(), args.background)
        failed = True
        try:
            case(fixture)
            last = fixture.sessions[-1]
            last.check(not fixture.mock.errors, repr(fixture.mock.errors))
            last.check(len(fixture.mock.requests) == len(fixture.mock.replies), "Script not fully consumed")
            failed = False
            print("PASS", name, flush=True)
        except SkipTest as error:
            failed = False
            print("SKIP", name, str(error), flush=True)
        except Exception as error:
            print("FAIL", name, str(error), flush=True)
            if fixture.sessions and "--- screen ---" not in str(error):
                print("--- screen ---\n" + fixture.sessions[-1].screen.text(), flush=True)
            if isinstance(error, subprocess.CalledProcessError):
                print("Child stderr:", error.stderr.decode("utf-8", "replace"), flush=True)
            return 1
        finally:
            fixture.close(failed)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
