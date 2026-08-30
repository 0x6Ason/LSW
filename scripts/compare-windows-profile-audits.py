#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later

import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import re
import statistics
import sys
import tempfile


BOOT_FILES = ("boot-1.json", "boot-2.json", "boot-3.json")
MAX_AUDIT_BYTES = 8 * 1024 * 1024
MIN_PROCESS_DELTA = 10
MIN_COMMITTED_DELTA = 256 * 1024 * 1024
MIN_SYSTEM_VOLUME_DELTA = 3 * 1024 * 1024 * 1024
NUMERIC_FIELDS = (
    "process_count",
    "committed_bytes",
    "working_set_bytes",
    "system_volume_used_bytes",
    "total_physical_bytes",
)


def read_audits(directory: Path, expected_profile: str, expected_revision: str) -> list[dict]:
    if directory.is_symlink() or not directory.is_dir():
        raise ValueError(f"audit directory is not a real directory: {directory}")
    audits = []
    for filename in BOOT_FILES:
        path = directory / filename
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"audit file is missing or unsafe: {path}")
        if path.stat().st_size > MAX_AUDIT_BYTES:
            raise ValueError(f"audit file exceeds {MAX_AUDIT_BYTES} bytes: {path}")
        try:
            audit = json.loads(path.read_text(encoding="utf-8-sig"))
        except (OSError, UnicodeError, json.JSONDecodeError) as error:
            raise ValueError(f"invalid audit JSON {path}: {error}") from error
        if audit.get("schema_version") != 1 or audit.get("outcome") != "passed":
            raise ValueError(f"audit did not pass with schema version 1: {path}")
        if audit.get("profile") != expected_profile or audit.get("revision") != expected_revision:
            raise ValueError(f"audit identity does not match {expected_revision}: {path}")
        if not re.fullmatch(r"[0-9]+", str(audit.get("windows_build", ""))):
            raise ValueError(f"audit has no numeric Windows build: {path}")
        if not isinstance(audit.get("edition"), str) or not audit["edition"]:
            raise ValueError(f"audit has no Windows edition: {path}")
        if not isinstance(audit.get("last_boot_utc"), str) or not audit["last_boot_utc"]:
            raise ValueError(f"audit has no boot identity: {path}")
        try:
            boot_time = datetime.fromisoformat(audit["last_boot_utc"].replace("Z", "+00:00"))
        except ValueError as error:
            raise ValueError(f"audit has an invalid boot identity: {path}") from error
        boot_offset = boot_time.utcoffset()
        if boot_offset is None or boot_offset.total_seconds() != 0:
            raise ValueError(f"audit boot identity is not UTC: {path}")
        if not re.fullmatch(r"[0-9a-f]{64}", str(audit.get("report_sha256", ""))):
            raise ValueError(f"audit has no exact report hash: {path}")
        for field in NUMERIC_FIELDS:
            value = audit.get(field)
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                raise ValueError(f"audit field {field} is invalid: {path}")
        if audit["total_physical_bytes"] == 0:
            raise ValueError(f"audit reports no physical memory: {path}")
        audits.append(audit)
    if len({audit["last_boot_utc"] for audit in audits}) != len(BOOT_FILES):
        raise ValueError(f"{expected_profile} samples do not represent three distinct cold boots")
    if len({audit["report_sha256"] for audit in audits}) != 1:
        raise ValueError(f"{expected_profile} samples do not share one immutable profile report")
    return audits


def median(audits: list[dict], field: str) -> int:
    return int(statistics.median(audit[field] for audit in audits))


def compare(
    slim: list[dict],
    vanilla: list[dict],
    candidate_sha: str,
    iso_sha256: str,
    slim_host_bytes: int,
    vanilla_host_bytes: int,
) -> dict:
    identities = {
        (audit["windows_build"], audit["edition"], audit["total_physical_bytes"])
        for audit in slim + vanilla
    }
    if len(identities) != 1:
        raise ValueError("profile samples do not share one Windows build, edition, and memory size")
    if not re.fullmatch(r"[0-9a-f]{40}", candidate_sha):
        raise ValueError("candidate SHA must be exactly 40 lowercase hexadecimal characters")
    if not re.fullmatch(r"[0-9a-f]{64}", iso_sha256):
        raise ValueError("ISO SHA-256 must be exactly 64 lowercase hexadecimal characters")
    if slim_host_bytes < 0 or vanilla_host_bytes < 0:
        raise ValueError("host allocation measurements must be non-negative")
    if {audit["report_sha256"] for audit in slim} == {
        audit["report_sha256"] for audit in vanilla
    }:
        raise ValueError("vanilla and slim samples unexpectedly share one profile report")

    fields = ("process_count", "committed_bytes", "working_set_bytes", "system_volume_used_bytes")
    slim_medians = {field: median(slim, field) for field in fields}
    vanilla_medians = {field: median(vanilla, field) for field in fields}
    deltas = {
        field: vanilla_medians[field] - slim_medians[field]
        for field in fields
    }
    failures = []
    if deltas["process_count"] < MIN_PROCESS_DELTA:
        failures.append(
            f"process delta {deltas['process_count']} is below {MIN_PROCESS_DELTA}"
        )
    if deltas["committed_bytes"] < MIN_COMMITTED_DELTA:
        failures.append(
            f"committed-memory delta {deltas['committed_bytes']} is below {MIN_COMMITTED_DELTA}"
        )
    if deltas["system_volume_used_bytes"] < MIN_SYSTEM_VOLUME_DELTA:
        failures.append(
            f"system-volume delta {deltas['system_volume_used_bytes']} is below {MIN_SYSTEM_VOLUME_DELTA}"
        )
    if failures:
        raise ValueError("; ".join(failures))

    build, edition, total_physical_bytes = identities.pop()
    return {
        "schema_version": 1,
        "outcome": "passed",
        "candidate_sha": candidate_sha,
        "iso_sha256": iso_sha256,
        "windows_build": build,
        "edition": edition,
        "total_physical_bytes": total_physical_bytes,
        "samples_per_profile": len(BOOT_FILES),
        "thresholds": {
            "minimum_process_delta": MIN_PROCESS_DELTA,
            "minimum_committed_bytes_delta": MIN_COMMITTED_DELTA,
            "minimum_system_volume_used_bytes_delta": MIN_SYSTEM_VOLUME_DELTA,
        },
        "vanilla_medians": vanilla_medians,
        "slim_medians": slim_medians,
        "vanilla_minus_slim": deltas,
        "host_allocated_bytes_after_trim": {
            "vanilla": vanilla_host_bytes,
            "slim": slim_host_bytes,
            "vanilla_minus_slim": vanilla_host_bytes - slim_host_bytes,
        },
        "profile_report_sha256": {
            "vanilla": vanilla[0]["report_sha256"],
            "slim": slim[0]["report_sha256"],
        },
        "boots": {
            "vanilla": [audit["last_boot_utc"] for audit in vanilla],
            "slim": [audit["last_boot_utc"] for audit in slim],
        },
    }


def write_result(path: Path, result: dict) -> None:
    parent = path.parent
    if parent.is_symlink() or not parent.is_dir() or path.exists() or path.is_symlink():
        raise ValueError("comparison output must be new beneath a real directory")
    temporary = parent / f"{path.name}.tmp.{os.getpid()}"
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as output:
            json.dump(result, output, indent=2, sort_keys=True)
            output.write("\n")
        os.replace(temporary, path)
    except BaseException:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass
        raise


def self_test() -> None:
    def samples(profile: str, process: int, committed: int, used: int) -> list[dict]:
        revision = f"{profile}-v2"
        return [
            {
                "schema_version": 1,
                "outcome": "passed",
                "profile": profile,
                "revision": revision,
                "windows_build": "26200",
                "edition": "Professional",
                "last_boot_utc": f"2026-01-0{index}T00:00:00Z",
                "report_sha256": ("a" if profile == "slim" else "b") * 64,
                "process_count": process + index % 2,
                "committed_bytes": committed + index,
                "working_set_bytes": committed // 2 + index,
                "system_volume_used_bytes": used + index,
                "total_physical_bytes": 4 * 1024**3,
            }
            for index in range(1, 4)
        ]

    slim = samples("slim", 60, 1024**3, 15 * 1024**3)
    vanilla = samples("vanilla", 75, 2 * 1024**3, 20 * 1024**3)
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        slim_dir = root / "slim"
        vanilla_dir = root / "vanilla"
        slim_dir.mkdir()
        vanilla_dir.mkdir()
        for filename, slim_sample, vanilla_sample in zip(BOOT_FILES, slim, vanilla):
            (slim_dir / filename).write_text(json.dumps(slim_sample), encoding="utf-8")
            (vanilla_dir / filename).write_text(
                json.dumps(vanilla_sample), encoding="utf-8"
            )
        loaded_slim = read_audits(slim_dir, "slim", "slim-v2")
        loaded_vanilla = read_audits(vanilla_dir, "vanilla", "vanilla-v2")
        result = compare(
            loaded_slim,
            loaded_vanilla,
            "d" * 40,
            "c" * 64,
            8 * 1024**3,
            12 * 1024**3,
        )
        output = root / "comparison.json"
        write_result(output, result)
        assert json.loads(output.read_text(encoding="utf-8"))["outcome"] == "passed"
        try:
            write_result(output, result)
        except ValueError:
            pass
        else:
            raise AssertionError("comparison output overwrite did not fail closed")
    insufficient_slim = samples("slim", 70, 2 * 1024**3, 20 * 1024**3)
    try:
        compare(insufficient_slim, vanilla, "d" * 40, "c" * 64, 1, 1)
    except ValueError:
        pass
    else:
        raise AssertionError("threshold failure did not fail closed")


def main() -> None:
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        print("Windows profile comparison self-test passed.")
        return
    parser = argparse.ArgumentParser()
    parser.add_argument("--slim-dir", type=Path, required=True)
    parser.add_argument("--vanilla-dir", type=Path, required=True)
    parser.add_argument("--candidate-sha", required=True)
    parser.add_argument("--iso-sha256", required=True)
    parser.add_argument("--slim-host-bytes", type=int, required=True)
    parser.add_argument("--vanilla-host-bytes", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    slim = read_audits(arguments.slim_dir, "slim", "slim-v2")
    vanilla = read_audits(arguments.vanilla_dir, "vanilla", "vanilla-v2")
    result = compare(
        slim,
        vanilla,
        arguments.candidate_sha,
        arguments.iso_sha256,
        arguments.slim_host_bytes,
        arguments.vanilla_host_bytes,
    )
    write_result(arguments.output, result)
    print(
        "Windows profile comparison passed: "
        f"{result['vanilla_minus_slim']['process_count']} fewer processes, "
        f"{result['vanilla_minus_slim']['committed_bytes']} fewer committed bytes, "
        f"{result['vanilla_minus_slim']['system_volume_used_bytes']} fewer used bytes."
    )


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError) as error:
        raise SystemExit(f"error: {error}") from error
