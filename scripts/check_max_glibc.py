#!/usr/bin/env python3
"""Fail if an ELF binary requires a GLIBC newer than 2.28 (Debian 10 / RHEL 8)."""

from __future__ import print_function

import re
import subprocess
import sys

# Linux x86_64 release binaries must run on glibc 2.28 (Debian 10 / RHEL 8).
MAX_GLIBC = (2, 28, 0)
GLIBC_RE = re.compile(r"GLIBC_(\d+)\.(\d+)(?:\.(\d+))?")


def parse_version_tuple(match):
    major = int(match.group(1))
    minor = int(match.group(2))
    patch = int(match.group(3) or 0)
    return (major, minor, patch)


def glibc_versions_from_text(text):
    return [parse_version_tuple(match) for match in GLIBC_RE.finditer(text)]


def max_glibc(versions):
    if not versions:
        return None
    return max(versions)


def format_glibc(version):
    major, minor, patch = version
    if patch:
        return "GLIBC_{}.{}.{}".format(major, minor, patch)
    return "GLIBC_{}.{}".format(major, minor)


def allowed(version, maximum=MAX_GLIBC):
    return version <= maximum


def elf_symbol_text(path):
    errors = []
    for cmd in (["readelf", "-V", path], ["objdump", "-T", path]):
        try:
            proc = subprocess.run(
                cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True
            )
        except OSError as exc:
            errors.append("{}: {}".format(cmd[0], exc))
            continue
        if proc.returncode == 0 and proc.stdout.strip():
            return proc.stdout
        errors.append(
            "{} exited {}: {}".format(cmd[0], proc.returncode, proc.stderr.strip() or "no output")
        )
    raise SystemExit(
        "need readelf or objdump to inspect {}: {}".format(path, "; ".join(errors))
    )


def check_binary(path):
    versions = glibc_versions_from_text(elf_symbol_text(path))
    highest = max_glibc(versions)
    if highest is None:
        raise SystemExit("{}: no GLIBC versioned symbols found".format(path))
    print("{}: max {}".format(path, format_glibc(highest)))
    if not allowed(highest):
        raise SystemExit(
            "{} requires {} (policy: <= {})".format(
                path, format_glibc(highest), format_glibc(MAX_GLIBC)
            )
        )


def self_test():
    ok_text = """
0000000000000000 DF *UND*  GLIBC_2.2.5 __libc_start_main
0000000000000000 DF *UND*  GLIBC_2.14 memcpy
0000000000000000 DF *UND*  GLIBC_2.27 copy_file_range
0000000000000000 DF *UND*  GLIBC_2.28 fcntl64
"""
    ok_versions = glibc_versions_from_text(ok_text)
    assert max_glibc(ok_versions) == (2, 28, 0), max_glibc(ok_versions)
    assert allowed((2, 28, 0))
    assert allowed((2, 27, 0))
    assert allowed((2, 2, 5))
    assert allowed((2, 14, 0))

    too_new = glibc_versions_from_text("GLIBC_2.2.5\nGLIBC_2.31 pthread_create\n")
    assert max_glibc(too_new) == (2, 31, 0), max_glibc(too_new)
    assert not allowed((2, 29, 0))
    assert not allowed((2, 31, 0))

    three_part = glibc_versions_from_text("Name: GLIBC_2.2.5  Flags: none")
    assert max_glibc(three_part) == (2, 2, 5)
    assert allowed((2, 2, 5))

    empty = glibc_versions_from_text("no versioned symbols")
    assert max_glibc(empty) is None
    print("self-test ok")


def main(argv):
    if len(argv) == 2 and argv[1] == "--self-test":
        self_test()
        return 0
    if len(argv) != 2:
        print("usage: {} <elf> | --self-test".format(argv[0]), file=sys.stderr)
        return 2
    check_binary(argv[1])
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
