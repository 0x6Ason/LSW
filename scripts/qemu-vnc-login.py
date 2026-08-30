#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later

"""Enter one ephemeral Windows test password through QEMU's private RFB socket."""

import argparse
import socket
import struct
import sys
import time


RFB_NONE_SECURITY = 1
XK_CONTROL_L = 0xFFE3
XK_ALT_L = 0xFFE9
XK_DELETE = 0xFFFF
XK_RETURN = 0xFF0D
XK_SPACE = 0x0020


def receive_exact(connection, length):
    payload = bytearray()
    while len(payload) < length:
        chunk = connection.recv(length - len(payload))
        if not chunk:
            raise RuntimeError("the QEMU RFB socket closed during its handshake")
        payload.extend(chunk)
    return bytes(payload)


def receive_reason(connection):
    length = struct.unpack("!I", receive_exact(connection, 4))[0]
    if length > 16 * 1024:
        raise RuntimeError("the QEMU RFB failure reason exceeded its bound")
    return receive_exact(connection, length).decode("utf-8", "replace")


def negotiate(connection):
    version = receive_exact(connection, 12)
    if version not in (b"RFB 003.003\n", b"RFB 003.007\n", b"RFB 003.008\n"):
        raise RuntimeError("QEMU offered an unsupported RFB protocol version")
    connection.sendall(version)

    if version == b"RFB 003.003\n":
        security = struct.unpack("!I", receive_exact(connection, 4))[0]
        if security == 0:
            raise RuntimeError("QEMU rejected RFB negotiation: " + receive_reason(connection))
        if security != RFB_NONE_SECURITY:
            raise RuntimeError("the private QEMU RFB socket unexpectedly requires authentication")
    else:
        count = receive_exact(connection, 1)[0]
        if count == 0:
            raise RuntimeError("QEMU rejected RFB negotiation: " + receive_reason(connection))
        security_types = receive_exact(connection, count)
        if RFB_NONE_SECURITY not in security_types:
            raise RuntimeError("the private QEMU RFB socket did not offer no-auth security")
        connection.sendall(bytes((RFB_NONE_SECURITY,)))
        result = struct.unpack("!I", receive_exact(connection, 4))[0]
        if result != 0:
            reason = receive_reason(connection) if version == b"RFB 003.008\n" else "unknown"
            raise RuntimeError("QEMU rejected RFB security negotiation: " + reason)

    connection.sendall(b"\x01")
    server_init = receive_exact(connection, 24)
    width, height = struct.unpack("!HH", server_init[:4])
    name_length = struct.unpack("!I", server_init[20:24])[0]
    if width == 0 or height == 0 or name_length > 16 * 1024:
        raise RuntimeError("QEMU returned invalid RFB server dimensions or name length")
    receive_exact(connection, name_length)


def send_key(connection, keysym, pressed):
    connection.sendall(struct.pack("!BBHI", 4, int(pressed), 0, keysym))


def tap(connection, keysym):
    send_key(connection, keysym, True)
    send_key(connection, keysym, False)


def secure_attention(connection):
    for keysym in (XK_CONTROL_L, XK_ALT_L, XK_DELETE):
        send_key(connection, keysym, True)
    for keysym in (XK_DELETE, XK_ALT_L, XK_CONTROL_L):
        send_key(connection, keysym, False)


def enter_password(connection, password, use_secure_attention):
    if use_secure_attention:
        secure_attention(connection)
        time.sleep(1.0)
    else:
        tap(connection, XK_SPACE)
        time.sleep(0.75)
    for character in password:
        tap(connection, ord(character))
        time.sleep(0.02)
    tap(connection, XK_RETURN)


def parse_arguments():
    parser = argparse.ArgumentParser()
    parser.add_argument("socket", help="private QEMU RFB Unix socket")
    parser.add_argument(
        "--secure-attention",
        action="store_true",
        help="send Ctrl+Alt+Delete before entering the password",
    )
    return parser.parse_args()


def main():
    arguments = parse_arguments()
    password = sys.stdin.readline().rstrip("\r\n")
    if not password or len(password) > 127 or not password.isascii():
        raise SystemExit("error: the GUI login password must be 1-127 ASCII characters")
    if sys.stdin.read(1):
        raise SystemExit("error: password input contained unexpected trailing data")

    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(5.0)
        connection.connect(arguments.socket)
        negotiate(connection)
        enter_password(connection, password, arguments.secure_attention)


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError) as error:
        raise SystemExit("error: " + str(error)) from error
