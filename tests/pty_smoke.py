#!/usr/bin/env python3
"""Exercise the real executable in a PTY, without accounts or network calls.

Usage: python3 tests/pty_smoke.py [target/debug/intercom-tui]
Python is only a development-test dependency, not an application dependency.
"""
import errno
import fcntl
import os
import pty
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time


def main():
    binary = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/debug/intercom-tui")
    temporary_parent = "/tmp/opencode" if os.path.isdir("/tmp/opencode") else None
    with tempfile.TemporaryDirectory(prefix="intercom-pty-", dir=temporary_parent) as root:
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        before = termios.tcgetattr(slave)
        env = dict(os.environ, TERM="xterm-256color")
        env.pop("INTERCOM_ACCESS_TOKEN", None)
        process = subprocess.Popen(
            [binary, "--demo", "--data-dir", root], stdin=slave, stdout=slave,
            stderr=slave, env=env,
        )
        output = bytearray()

        def read_for(seconds):
            until = time.monotonic() + seconds
            while time.monotonic() < until:
                if select.select([master], [], [], 0.02)[0]:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            break
                        raise
                    if not chunk:
                        break
                    output.extend(chunk)
                    if b"\x1b[?u" in chunk:
                        os.write(master, b"\x1b[?0u")
                    if b"\x1b[c" in chunk:
                        os.write(master, b"\x1b[?1;2c")

        def key(value):
            os.write(master, value)
            read_for(0.15)

        try:
            read_for(1)
            assert b"INTERCOM" in output, output[-3000:]
            key(b"/billing")
            read_for(0.3)
            key(b"\x1b[B")  # search results
            key(b"\x1b")
            key(b"\r")     # composer
            key(b"\x1b[200~Smoke test\nsecond line\x1b[201~")
            key(b"\x0e")  # private note
            key(b"\x13")  # schedule dialog
            key(b"tomorrow 9am")
            key(b"\r")     # date confirmation
            key(b"\x1b")  # back to date
            key(b"\x1b")  # back to composer
            key(b"\x1b")  # save draft
            key(b"s")
            key(b"\x1bOP")  # F1: setup help from Settings
            key(b"\x1b")  # back to Settings
            key(b"\x1b")
            key(b"?")
            key(b"\x1b")
            key(b"q")
            read_for(0.2)
            process.wait(timeout=5)
            assert process.returncode == 0, output[-5000:]
            after = termios.tcgetattr(slave)
            assert before == after, "Terminal attributes were not restored"
            assert b"Send later" in output, "Schedule dialog did not render"
            assert b"Settings" in output and b"Help" in output
            assert b"SETUP" in output and b"Developer Hub" in output
            assert b"panic" not in output.lower(), output[-5000:]
            assert not os.path.exists(os.path.join(root, "config", "token"))
            print("PTY smoke test passed: search, compose, note, scheduling, draft, settings, help, quit; terminal restored")
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
            os.close(master)
            os.close(slave)


if __name__ == "__main__":
    main()
