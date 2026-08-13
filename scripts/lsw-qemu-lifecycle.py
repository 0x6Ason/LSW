#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later

"""Small standard-library probes for the LSW/QEMU lifecycle gate."""

import argparse
import json
import os
import socket
import sys
import time


class ProbeError(RuntimeError):
    pass


def allocate_port(_arguments):
    while True:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        # LSW reserves this range for deterministic guest-agent endpoints.
        if not 42000 <= port <= 43999:
            print(port)
            return


def probe_unix(arguments):
    probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        probe.bind(arguments.path)
    finally:
        probe.close()
        try:
            os.unlink(arguments.path)
        except FileNotFoundError:
            pass


def ports(arguments):
    deadline = time.monotonic() + arguments.seconds
    pending = set(arguments.port)
    last_results = {}
    while pending and time.monotonic() < deadline:
        for port in tuple(pending):
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
                probe.settimeout(0.5)
                result = probe.connect_ex(("127.0.0.1", port))
            last_results[port] = result
            if (arguments.state == "bound" and result == 0) or (
                arguments.state == "released" and result != 0
            ):
                pending.remove(port)
        if pending:
            time.sleep(0.05)
    if pending:
        details = ", ".join(
            "{} (connect_ex={})".format(port, last_results.get(port, "not run"))
            for port in sorted(pending)
        )
        raise ProbeError(
            "ports did not become {} before the deadline: {}".format(
                arguments.state, details
            )
        )


class QmpClient:
    def __init__(self, path, seconds):
        self.deadline = time.monotonic() + seconds
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(seconds)
        self.socket.connect(path)
        self.reader = self.socket.makefile("rb")
        self.next_id = 1
        greeting = self._read()
        if "QMP" not in greeting:
            raise ProbeError("QMP did not send a greeting")
        self.command("qmp_capabilities")

    def _read(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise ProbeError("timed out waiting for QMP")
        self.socket.settimeout(remaining)
        line = self.reader.readline()
        if not line:
            raise ProbeError("QMP closed the connection")
        try:
            message = json.loads(line.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ProbeError("QMP returned invalid JSON: {}".format(error)) from error
        if not isinstance(message, dict):
            raise ProbeError("QMP returned a non-object message")
        return message

    def command(self, name):
        command_id = self.next_id
        self.next_id += 1
        request = {"execute": name, "id": command_id}
        self.socket.sendall(json.dumps(request).encode("utf-8") + b"\n")
        while True:
            message = self._read()
            if message.get("id") != command_id:
                continue
            if "error" in message:
                raise ProbeError(
                    "QMP command {} failed: {}".format(name, message["error"])
                )
            if "return" not in message:
                raise ProbeError("QMP command {} had no result".format(name))
            return message["return"]

    def close(self):
        self.reader.close()
        self.socket.close()


def qmp_tpm(arguments):
    client = QmpClient(arguments.path, arguments.seconds)
    try:
        status = client.command("query-status")
        if not isinstance(status, dict) or status.get("status") != "running":
            raise ProbeError("QMP did not report a running VM: {!r}".format(status))
        tpms = client.command("query-tpm")
        if not isinstance(tpms, list) or not any(
            isinstance(tpm, dict)
            and tpm.get("id") == "tpm0"
            and tpm.get("model") == "tpm-tis"
            for tpm in tpms
        ):
            raise ProbeError("QEMU did not expose the planned tpm0/tpm-tis device")
    finally:
        client.close()


def parser():
    result = argparse.ArgumentParser()
    commands = result.add_subparsers(dest="command", required=True)

    allocate = commands.add_parser("allocate-port")
    allocate.set_defaults(function=allocate_port)

    unix = commands.add_parser("probe-unix")
    unix.add_argument("path")
    unix.set_defaults(function=probe_unix)

    port_probe = commands.add_parser("ports")
    port_probe.add_argument("state", choices=("bound", "released"))
    port_probe.add_argument("port", type=int, nargs="+")
    port_probe.add_argument("--seconds", type=float, default=5.0)
    port_probe.set_defaults(function=ports)

    tpm = commands.add_parser("qmp-tpm")
    tpm.add_argument("path")
    tpm.add_argument("--seconds", type=float, default=5.0)
    tpm.set_defaults(function=qmp_tpm)
    return result


def main():
    arguments = parser().parse_args()
    try:
        arguments.function(arguments)
    except (OSError, ProbeError, ValueError) as error:
        print("LSW QEMU lifecycle probe: {}".format(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
