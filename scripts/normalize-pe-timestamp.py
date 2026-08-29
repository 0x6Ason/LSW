#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Validate and normalize reproducibility-sensitive PE header fields."""

import os
import stat
import struct
import sys


DOS_HEADER_SIZE = 64
PE_POINTER_OFFSET = 0x3C
PE_SIGNATURE = b"PE\0\0"
COFF_HEADER_SIZE = 20
TIMESTAMP_OFFSET_IN_COFF_HEADER = 4
OPTIONAL_HEADER_SIZE_OFFSET_IN_COFF_HEADER = 16
CHECKSUM_OFFSET_IN_OPTIONAL_HEADER = 64
MINIMUM_OPTIONAL_HEADER_SIZE = CHECKSUM_OFFSET_IN_OPTIONAL_HEADER + 4
PE32_MAGIC = 0x10B
PE32_PLUS_MAGIC = 0x20B
ZERO_TIMESTAMP = struct.pack("<I", 0)
ZERO_CHECKSUM = struct.pack("<I", 0)


def fail(message: str) -> None:
    raise SystemExit(f"error: {message}")


def inspect_or_normalize(path: str, check_only: bool) -> None:
    access_mode = os.O_RDONLY if check_only else os.O_RDWR
    open_flags = access_mode | getattr(os, "O_BINARY", 0)
    if hasattr(os, "O_NOFOLLOW"):
        open_flags |= os.O_NOFOLLOW

    try:
        descriptor = os.open(path, open_flags)
    except OSError as error:
        fail(f"cannot open PE image {path}: {error}")

    try:
        file_mode = "rb" if check_only else "r+b"
        with os.fdopen(descriptor, file_mode, closefd=True) as image:
            metadata = os.fstat(image.fileno())
            if not stat.S_ISREG(metadata.st_mode):
                fail(f"PE image is not a regular file: {path}")
            if metadata.st_size < DOS_HEADER_SIZE:
                fail(f"PE image is shorter than its DOS header: {path}")

            dos_header = image.read(DOS_HEADER_SIZE)
            if len(dos_header) != DOS_HEADER_SIZE or dos_header[:2] != b"MZ":
                fail(f"PE image has no valid MZ header: {path}")

            pe_offset = struct.unpack_from("<I", dos_header, PE_POINTER_OFFSET)[0]
            complete_header_size = len(PE_SIGNATURE) + COFF_HEADER_SIZE
            if (
                pe_offset < DOS_HEADER_SIZE
                or pe_offset > metadata.st_size - complete_header_size
            ):
                fail(f"PE header offset is outside the image: {path}")

            image.seek(pe_offset)
            pe_header = image.read(complete_header_size)
            if len(pe_header) != complete_header_size or not pe_header.startswith(
                PE_SIGNATURE
            ):
                fail(f"PE image has no valid PE signature: {path}")

            optional_header_size = struct.unpack_from(
                "<H",
                pe_header,
                len(PE_SIGNATURE) + OPTIONAL_HEADER_SIZE_OFFSET_IN_COFF_HEADER,
            )[0]
            optional_header_offset = pe_offset + complete_header_size
            if (
                optional_header_size < MINIMUM_OPTIONAL_HEADER_SIZE
                or optional_header_offset > metadata.st_size - optional_header_size
            ):
                fail(f"PE optional header is outside the image: {path}")
            image.seek(optional_header_offset)
            optional_header_prefix = image.read(MINIMUM_OPTIONAL_HEADER_SIZE)
            if len(optional_header_prefix) != MINIMUM_OPTIONAL_HEADER_SIZE:
                fail(f"PE optional header is truncated: {path}")
            optional_magic = struct.unpack_from("<H", optional_header_prefix, 0)[0]
            if optional_magic not in (PE32_MAGIC, PE32_PLUS_MAGIC):
                fail(f"PE optional header has an unsupported magic: {path}")

            timestamp_offset = (
                pe_offset + len(PE_SIGNATURE) + TIMESTAMP_OFFSET_IN_COFF_HEADER
            )
            image.seek(timestamp_offset)
            timestamp_bytes = image.read(len(ZERO_TIMESTAMP))
            if len(timestamp_bytes) != len(ZERO_TIMESTAMP):
                fail(f"PE timestamp is outside the image: {path}")
            checksum_offset = optional_header_offset + CHECKSUM_OFFSET_IN_OPTIONAL_HEADER
            image.seek(checksum_offset)
            checksum_bytes = image.read(len(ZERO_CHECKSUM))
            if len(checksum_bytes) != len(ZERO_CHECKSUM):
                fail(f"PE checksum is outside the image: {path}")
            if check_only:
                if timestamp_bytes != ZERO_TIMESTAMP:
                    timestamp = struct.unpack("<I", timestamp_bytes)[0]
                    fail(f"PE TimeDateStamp is not zero ({timestamp}): {path}")
                if checksum_bytes != ZERO_CHECKSUM:
                    checksum = struct.unpack("<I", checksum_bytes)[0]
                    fail(f"PE CheckSum is not zero ({checksum}): {path}")
                return

            image.seek(timestamp_offset)
            image.write(ZERO_TIMESTAMP)
            image.seek(checksum_offset)
            image.write(ZERO_CHECKSUM)
            image.flush()

            image.seek(timestamp_offset)
            if image.read(len(ZERO_TIMESTAMP)) != ZERO_TIMESTAMP:
                fail(f"failed to verify the normalized PE timestamp: {path}")
            image.seek(checksum_offset)
            if image.read(len(ZERO_CHECKSUM)) != ZERO_CHECKSUM:
                fail(f"failed to verify the normalized PE checksum: {path}")
    except OSError as error:
        fail(f"cannot normalize PE image {path}: {error}")


def main(arguments) -> None:
    if len(arguments) == 2:
        inspect_or_normalize(arguments[1], check_only=False)
        return
    if len(arguments) == 3 and arguments[1] == "--check":
        inspect_or_normalize(arguments[2], check_only=True)
        return
    fail("usage: normalize-pe-timestamp.py [--check] IMAGE.exe")


if __name__ == "__main__":
    main(sys.argv)
