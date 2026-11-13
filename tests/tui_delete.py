"""Run after cargo build --release; deletes only disposable test fixtures."""

import fcntl
import os
from pathlib import Path
import pty
import select
import struct
import tempfile
import termios
import time


class Browser:
    def __init__(self, path):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ["TERM"] = "xterm-256color"
            os.execv("./target/release/whatdu", ["whatdu", str(path)])
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 160, 0, 0))
        self.output = b""
        self.pump(1)

    def pump(self, duration=0.4):
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if select.select([self.fd], [], [], 0.05)[0]:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    break
                self.output += chunk
                if b"\x1b[6n" in chunk:
                    os.write(self.fd, b"\x1b[1;1R")

    def key(self, key):
        os.write(self.fd, key)
        self.pump()

    def close(self):
        self.key(b"\x1b")
        self.key(b"q")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                os.close(self.fd)
                assert os.waitstatus_to_exitcode(status) == 0
                assert b"\x1b[?1049l" in self.output
                return
            self.pump(0.1)
        os.kill(self.pid, 9)
        os.waitpid(self.pid, 0)
        os.close(self.fd)
        raise AssertionError("browser did not exit")


with tempfile.TemporaryDirectory(prefix="whatdu-delete-test-") as directory:
    root = Path(directory)
    nested = root / "a_dir"
    nested.mkdir()
    (nested / "file").write_text("disposable")
    for name in ["b_file", "c_file"]:
        (root / name).write_text("disposable")
    browser = Browser(root)
    try:
        browser.key(b"s")  # Deterministic name ordering.
        browser.key(b"d")
        assert b"again" in browser.output
        browser.key(b"\r")  # No is selected by default.
        assert nested.exists()
        browser.key(b"d")
        browser.key(b"\x1b")  # Escape cancels.
        assert nested.exists()
        browser.key(b"d")
        browser.key(b"\x1b[A")  # No -> Yes.
        browser.key(b"\r")
        assert not nested.exists()
        assert b"progress" in browser.output and b"checked" in browser.output
        assert (root / "b_file").exists()
        browser.key(b"d")
        browser.key(b"\x1b[B")  # No -> Don't ask me again.
        browser.key(b"\r")
        assert not (root / "b_file").exists()
        assert b"WITHOUT" in browser.output and b"ASKING" in browser.output
        browser.key(b"d")  # Deletes c_file without prompting.
        assert not (root / "c_file").exists()
        browser.key(b"d")  # Empty selection is a no-op.
        assert root.exists()
    finally:
        browser.close()

    keep = root / "keep"
    keep.write_text("disposable")
    browser = Browser(root)
    try:
        browser.key(b"d")
        browser.key(b"\r")  # A new session restores default No.
        assert keep.exists()
    finally:
        browser.close()

with tempfile.TemporaryDirectory(prefix="whatdu-cancel-test-") as directory:
    root = Path(directory)
    nested = root / "delete"
    nested.mkdir()
    for index in range(2048):
        (nested / str(index)).write_text("disposable")
    browser = Browser(root)
    try:
        browser.key(b"d")
        browser.key(b"\x1b[A")
        browser.key(b"\r\x1b")  # Confirm, then immediately request cancellation.
        deadline = time.monotonic() + 10
        while b"Stopped" not in browser.output and time.monotonic() < deadline:
            browser.pump(0.1)
        assert b"Stopped" in browser.output
        assert nested.exists()
        assert any(nested.iterdir())
    finally:
        browser.close()

with tempfile.TemporaryDirectory(prefix="whatdu-rescan-test-") as directory:
    root = Path(directory)
    nested = root / "nested"
    nested.mkdir()
    browser = Browser(root)
    try:
        browser.key(b"\r")  # Open the current directory, not a selected child rescan.
        (nested / "added_after_scan").write_text("disposable")
        browser.key(b"r")
        assert b"Rescan" in browser.output
        assert b"added_after_scan" in browser.output
        browser.key(b"\x1b[D")
        (root / "root_added").write_text("disposable")
        browser.key(b"r")
        assert b"root_added" in browser.output
    finally:
        browser.close()

print("TUI passed: deletion, progress, cancellation, current-directory and root rescans")
