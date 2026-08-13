#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later

"""Standard-library QEMU, QMP, usernet, and swtpm smoke-test driver."""

import argparse
import json
import os
import signal
import socket
import subprocess
import sys
import time


class SmokeError(RuntimeError):
    pass


class QmpClient:
    def __init__(self, host, port, deadline, qemu):
        self.deadline = deadline
        self.socket = self._connect(host, port, qemu)
        self.reader = self.socket.makefile("rb")
        self.next_id = 1
        greeting = self._read_message()
        if "QMP" not in greeting:
            raise SmokeError("QMP did not send a greeting")
        self.command("qmp_capabilities")

    def _connect(self, host, port, qemu):
        last_error = None
        while time.monotonic() < self.deadline:
            if qemu.poll() is not None:
                raise SmokeError(
                    "QEMU exited before QMP became ready with status {}".format(
                        qemu.returncode
                    )
                )
            candidate = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            candidate.settimeout(max(0.1, self.deadline - time.monotonic()))
            try:
                candidate.connect((host, port))
                return candidate
            except (ConnectionRefusedError, socket.timeout) as error:
                last_error = error
                candidate.close()
                time.sleep(0.05)
        raise SmokeError("QMP did not become ready: {}".format(last_error))

    def _read_message(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeError("timed out waiting for a QMP response")
        self.socket.settimeout(remaining)
        line = self.reader.readline()
        if not line:
            raise EOFError("QMP closed the connection")
        try:
            message = json.loads(line.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise SmokeError("QMP returned invalid JSON: {}".format(error)) from error
        if not isinstance(message, dict):
            raise SmokeError("QMP returned a non-object message")
        return message

    def command(self, name, arguments=None):
        command_id = self.next_id
        self.next_id += 1
        request = {"execute": name, "id": command_id}
        if arguments is not None:
            request["arguments"] = arguments
        self.socket.sendall(json.dumps(request).encode("utf-8") + b"\n")
        while True:
            message = self._read_message()
            if message.get("id") != command_id:
                continue
            if "error" in message:
                raise SmokeError(
                    "QMP command {} failed: {}".format(name, message["error"])
                )
            if "return" not in message:
                raise SmokeError("QMP command {} had no result".format(name))
            return message["return"]

    def close(self):
        self.reader.close()
        self.socket.close()


def reserve_loopback_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def assert_status(qmp, expected):
    status = qmp.command("query-status")
    actual = status.get("status") if isinstance(status, dict) else None
    if actual != expected:
        raise SmokeError(
            "expected QEMU status {!r}, got {!r}".format(expected, actual)
        )


def wait_for_tpm_traffic(path, deadline):
    while time.monotonic() < deadline:
        try:
            with open(path, "rb") as log:
                contents = log.read()
            if b"SWTPM_IO_Read" in contents and b"SWTPM_IO_Write" in contents:
                return
        except FileNotFoundError:
            pass
        time.sleep(0.05)
    raise SmokeError("OVMF did not exchange commands with swtpm")


def check_hostfwd(port, deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise SmokeError("timed out before checking host forwarding")
    with socket.create_connection(("127.0.0.1", port), timeout=remaining):
        pass


def assert_port_released(port):
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.settimeout(0.5)
        if probe.connect_ex(("127.0.0.1", port)) == 0:
            raise SmokeError(
                "QEMU did not release loopback hostfwd port {} after quit".format(port)
            )


def terminate_process(process):
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def qemu_command(arguments, qemu_fd, qmp_port, agent_port, published_port):
    command = [arguments.qemu]
    if arguments.qemu_data:
        command.extend(["-L", arguments.qemu_data])
    command.extend(
        [
            "-name",
            "lsw-ci-smoke",
            "-machine",
            "q35,usb=on",
            "-accel",
            "tcg,thread=multi",
            "-cpu",
            "max",
            "-smp",
            "1",
            "-m",
            "256M",
            "-drive",
            "if=pflash,format=raw,readonly=on,file={}".format(arguments.ovmf_code),
            "-drive",
            "if=pflash,format=raw,file={}".format(arguments.ovmf_vars),
            "-drive",
            "file={},if=none,id=smoke-disk,format=qcow2".format(arguments.disk),
            "-device",
            "nvme,drive=smoke-disk,serial=lsw-ci-smoke",
            "-chardev",
            "socket,id=chrtpm,fd={}".format(qemu_fd),
            "-tpmdev",
            "emulator,id=tpm0,chardev=chrtpm",
            "-device",
            "tpm-tis,tpmdev=tpm0",
            "-netdev",
            "user,id=net0,restrict=on,hostfwd=tcp:127.0.0.1:{}-:5040,hostfwd=tcp:127.0.0.1:{}-:8080".format(
                agent_port, published_port
            ),
            "-device",
            "e1000e,netdev=net0{}".format(
                ",romfile={}".format(arguments.nic_rom) if arguments.nic_rom else ""
            ),
            "-qmp",
            "tcp:127.0.0.1:{},server=on,wait=off".format(qmp_port),
            "-nodefaults",
            "-monitor",
            "none",
            "-display",
            "none",
            "-serial",
            "none",
            "-no-reboot",
        ]
    )
    return command


def run_smoke(arguments):
    qmp_port = reserve_loopback_port()
    agent_port = reserve_loopback_port()
    while agent_port == qmp_port:
        agent_port = reserve_loopback_port()
    published_port = reserve_loopback_port()
    while published_port in (qmp_port, agent_port):
        published_port = reserve_loopback_port()

    qemu_end, swtpm_end = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    qemu = None
    swtpm = None
    qmp = None
    try:
        with open(arguments.swtpm_stdout, "wb") as swtpm_output, open(
            arguments.qemu_log, "wb"
        ) as qemu_output:
            swtpm = subprocess.Popen(
                [
                    arguments.swtpm,
                    "socket",
                    "--tpm2",
                    "--tpmstate",
                    "dir={}".format(arguments.swtpm_state),
                    "--ctrl",
                    "type=unixio,clientfd={}".format(swtpm_end.fileno()),
                    "--flags",
                    "not-need-init",
                    "--log",
                    "file={},level=20".format(arguments.swtpm_log),
                    "--terminate",
                ],
                pass_fds=(swtpm_end.fileno(),),
                stdout=swtpm_output,
                stderr=subprocess.STDOUT,
            )
            swtpm_end.close()
            if swtpm.poll() is not None:
                raise SmokeError("swtpm exited before QEMU started")

            qemu = subprocess.Popen(
                qemu_command(
                    arguments,
                    qemu_end.fileno(),
                    qmp_port,
                    agent_port,
                    published_port,
                ),
                pass_fds=(qemu_end.fileno(),),
                stdout=qemu_output,
                stderr=subprocess.STDOUT,
            )
            qemu_end.close()

            deadline = time.monotonic() + arguments.seconds
            qmp = QmpClient("127.0.0.1", qmp_port, deadline, qemu)
            assert_status(qmp, "running")

            tpms = qmp.command("query-tpm")
            if not isinstance(tpms, list) or not any(
                isinstance(tpm, dict)
                and tpm.get("id") == "tpm0"
                and tpm.get("model") == "tpm-tis"
                for tpm in tpms
            ):
                raise SmokeError("QEMU did not expose the expected tpm0/tpm-tis device")
            wait_for_tpm_traffic(arguments.swtpm_log, deadline)

            # There are no guest services in this media-free boot. Successful
            # TCP handshakes still prove QEMU bound both exact loopback-only
            # hostfwd endpoints and accepted them through the usernet backend.
            check_hostfwd(agent_port, deadline)
            check_hostfwd(published_port, deadline)

            qmp.command("stop")
            assert_status(qmp, "paused")
            qmp.command("cont")
            assert_status(qmp, "running")
            try:
                qmp.command("quit")
            except (EOFError, ConnectionResetError, BrokenPipeError):
                # QEMU may close QMP while completing the quit command.
                pass
            qmp.close()
            qmp = None

            remaining = max(0.1, deadline - time.monotonic())
            try:
                qemu_status = qemu.wait(timeout=remaining)
            except subprocess.TimeoutExpired as error:
                raise SmokeError("QEMU did not exit after QMP quit") from error
            if qemu_status != 0:
                raise SmokeError(
                    "QEMU exited with status {} after QMP quit".format(qemu_status)
                )
            assert_port_released(agent_port)
            assert_port_released(published_port)

            try:
                swtpm_status = swtpm.wait(timeout=5)
            except subprocess.TimeoutExpired as error:
                raise SmokeError("swtpm did not exit after QEMU closed its TPM channel") from error
            if swtpm_status != 0:
                raise SmokeError("swtpm exited with status {}".format(swtpm_status))
    finally:
        if qmp is not None:
            qmp.close()
        qemu_end.close()
        swtpm_end.close()
        terminate_process(qemu)
        terminate_process(swtpm)

    print(
        "QEMU TCG/OVMF TPM traffic, QMP stop/cont/quit, and loopback hostfwd ports {}/{} passed and were released.".format(
            agent_port, published_port
        )
    )


def parser():
    result = argparse.ArgumentParser()
    result.add_argument("--qemu", required=True)
    result.add_argument("--swtpm", required=True)
    result.add_argument("--qemu-data", default="")
    result.add_argument("--nic-rom", default="")
    result.add_argument("--ovmf-code", required=True)
    result.add_argument("--ovmf-vars", required=True)
    result.add_argument("--disk", required=True)
    result.add_argument("--swtpm-state", required=True)
    result.add_argument("--swtpm-log", required=True)
    result.add_argument("--swtpm-stdout", required=True)
    result.add_argument("--qemu-log", required=True)
    result.add_argument("--seconds", type=float, default=40.0)
    return result


def interrupted(signum, _frame):
    raise SmokeError("received signal {}".format(signum))


def main():
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGHUP, interrupted)
    arguments = parser().parse_args()
    try:
        run_smoke(arguments)
    except (OSError, SmokeError, EOFError, ValueError, subprocess.SubprocessError) as error:
        print("qemu smoke helper: {}".format(error), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("qemu smoke helper: interrupted", file=sys.stderr)
        return 130
    return 0


if __name__ == "__main__":
    sys.exit(main())
