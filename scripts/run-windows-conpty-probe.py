#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later

import errno
import fcntl
import os
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time


if len(sys.argv) < 5:
    raise SystemExit(
        "usage: run-windows-conpty-probe.py TIMEOUT MARKER COMMAND PROGRAM [ARG ...]"
    )

timeout_seconds = int(sys.argv[1])
marker = os.fsencode(sys.argv[2])
command = os.fsencode(sys.argv[3])
argv = sys.argv[4:]
if timeout_seconds <= 0:
    raise SystemExit("error: ConPTY probe timeout must be positive")

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
process = subprocess.Popen(
    argv,
    stdin=slave,
    stdout=slave,
    stderr=slave,
    close_fds=True,
    start_new_session=True,
)
os.close(slave)
deadline = time.monotonic() + timeout_seconds
transcript = bytearray()
command_sent = False
exit_sent = False
timed_out = False

while True:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        timed_out = True
        break
    ready, _, _ = select.select([master], [], [], min(0.1, remaining))
    if ready:
        try:
            data = os.read(master, 32768)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
            data = b""
        if data:
            os.write(sys.stdout.fileno(), data)
            transcript.extend(data)
            if len(transcript) > 1024 * 1024:
                del transcript[: len(transcript) - 1024 * 1024]
            prompt = transcript.find(b"PS C:\\")
            if not command_sent and prompt >= 0 and b">" in transcript[prompt:]:
                os.write(master, command + b"\r")
                command_sent = True
            if command_sent and not exit_sent and marker in transcript:
                os.write(master, b"exit\r")
                exit_sent = True
        elif process.poll() is not None:
            break
    elif process.poll() is not None:
        break

if timed_out:
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
    raise SystemExit("error: timed out waiting for the ConPTY probe")

status = process.wait()
if not command_sent:
    raise SystemExit("error: ConPTY shell never produced a PowerShell prompt")
if marker not in transcript:
    raise SystemExit("error: ConPTY shell exited before returning its marker")
if status < 0 or status > 255:
    raise SystemExit(f"error: ConPTY probe returned unsupported status {status}")
raise SystemExit(status)
