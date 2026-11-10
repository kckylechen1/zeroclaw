#!/usr/bin/env python3
"""Verify and unpack the exact previously generated synthetic legacy inputs."""
from __future__ import annotations

import argparse
import base64
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import stat
import sys
import tarfile

HERE = Path(__file__).resolve().parent
MANIFEST_SHA256 = "040e7b55ef4d03d789eaef1131d451d35b9bc4c904b0bd070f8d4adf84a51649"
ENCODED_SIZE = 194633
ENCODED_SHA256 = "829c59bcaba41d52777f171746ecb46c73ceb8335114543bcfbe0e8e683b0302"
GZIP_SIZE = 145972
GZIP_SHA256 = "28878881f7574aef41e15b0ca91d17fb3076e37b77655b1779ae9b6da2d61299"
TAR_SIZE = 6891520
TAR_SHA256 = "37ccabaf5a824cc96788bb0620c01def29e3fc12e8d1105e91ce469f2109226e"
RECEIPT_SHA256 = "6ad5165a7e3095e95cf4404c88b0ef936b26d2c4dad2648f037018c86a6f69b9"
GENERATOR_SHA256 = "44ae7ee95a611c0e08ccb09d17afad6f1f060e5c98a8af11bab6a1fc74b241e8"
GENERATOR_REVISION = "eba0c48617e8820831bfd22f91d269c6d70f409f"
FIXTURE_NAMES = {f"{role}{suffix}" for role in ("memory", "companion")
                 for suffix in ("-schema-v38.json", "-v38.db", "-v38.db.migration-marker", "-v38.json")}
ARCHIVE_NAMES = {"result.json"} | {"fixtures/" + name for name in FIXTURE_NAMES}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_regular(path):
    require(not path.is_symlink(), f"Input is a symlink: {path.name}")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        before = os.fstat(stream.fileno())
        require(stat.S_ISREG(before.st_mode), "Input is not a regular file")
        data = stream.read()
        after = os.fstat(stream.fileno())
    require((before.st_size, before.st_mtime_ns, before.st_ctime_ns) ==
            (after.st_size, after.st_mtime_ns, after.st_ctime_ns), "Input changed while reading")
    return data


def validated_members(raw, expected):
    """Return verified bytes only; no tar extraction API writes filesystem paths."""
    require(set(expected) == ARCHIVE_NAMES, "Manifest contains missing or extra paths")
    files = {}
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as archive:
        for member in archive:
            name = member.name
            path = PurePosixPath(name)
            require(name in ARCHIVE_NAMES and not path.is_absolute() and
                    path.as_posix() == name and "\\" not in name and
                    all(part not in ("", ".", "..") for part in path.parts),
                    "Unsafe or unexpected archive path")
            require(name not in files, "Duplicate archive member")
            require(member.type == tarfile.REGTYPE and not member.pax_headers and
                    getattr(member, "sparse", None) is None,
                    "Archive member is not an ordinary regular file")
            require(member.mode == 0o600 and member.uid == member.gid == member.mtime == 0 and
                    member.uname == member.gname == "", "Noncanonical archive metadata")
            item = expected[name]
            require(member.size == item["size_bytes"], "Archive member size differs")
            stream = archive.extractfile(member)
            require(stream is not None, "Missing regular member content")
            data = stream.read(item["size_bytes"] + 1)
            require(len(data) == item["size_bytes"] and digest(data) == item["sha256"],
                    "Archive member content differs")
            files[name] = data
    require(set(files) == ARCHIVE_NAMES, "Archive is incomplete")
    return files


def load_payload():
    manifest_data = read_regular(HERE / "fixture-bundle.json")
    require(digest(manifest_data) == MANIFEST_SHA256, "Fixture manifest hash differs")
    manifest = json.loads(manifest_data)
    require(manifest["file_count"] == 9 and manifest["fixture_file_count"] == 8 and
            len(manifest["files"]) == 9, "Manifest count differs")
    expected = {item["archive_path"]: item for item in manifest["files"]}
    require(set(expected) == ARCHIVE_NAMES, "Manifest inventory differs")
    encoded = read_regular(HERE / "legacy-fixtures.tar.gz.b64")
    require(len(encoded) == ENCODED_SIZE and digest(encoded) == ENCODED_SHA256,
            "Encoded bundle hash or size differs")
    require(encoded.endswith(b"\n") and b"\n" not in encoded[:-1], "Noncanonical base64 format")
    compressed = base64.b64decode(encoded[:-1], validate=True)
    require(len(compressed) == GZIP_SIZE and digest(compressed) == GZIP_SHA256,
            "Compressed bundle hash or size differs")
    with gzip.GzipFile(fileobj=io.BytesIO(compressed), mode="rb") as stream:
        raw = stream.read(TAR_SIZE + 1)
    require(len(raw) == TAR_SIZE and digest(raw) == TAR_SHA256,
            "Uncompressed bundle hash or size differs")
    files = validated_members(raw, expected)
    require(digest(files["result.json"]) == RECEIPT_SHA256, "Original generator receipt hash differs")
    receipt = json.loads(files["result.json"])
    require(receipt == manifest["original_generator_receipt"] and
            receipt["result"] == "PASS" and receipt["build_exit_code"] == receipt["generator_exit_code"] == 0 and
            receipt["generator_revision"] == GENERATOR_REVISION and
            receipt["binary_sha256"] == GENERATOR_SHA256, "Original generator provenance differs")
    require(len(receipt["files"]) == 8 and {item["name"] for item in receipt["files"]} == FIXTURE_NAMES,
            "Original receipt inventory differs")
    for item in receipt["files"]:
        data = files["fixtures/" + item["name"]]
        require(len(data) == item["size"] and digest(data) == item["sha256"],
                "Fixture differs from original receipt")
    return files, manifest


def write_new_destination(destination, files):
    require(destination.is_absolute() and destination.name not in ("", ".", ".."),
            "Destination must be an absolute new directory")
    require(destination.parent.resolve(strict=True) == destination.parent,
            "Destination parent contains symlink or traversal")
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    parent_fd = os.open(destination.parent, flags)
    root_fd = fixture_fd = None
    try:
        os.mkdir(destination.name, 0o700, dir_fd=parent_fd)
        root_fd = os.open(destination.name, flags, dir_fd=parent_fd)
        os.mkdir("fixtures", 0o700, dir_fd=root_fd)
        fixture_fd = os.open("fixtures", flags, dir_fd=root_fd)
        for name, data in sorted(files.items()):
            directory_fd = fixture_fd if name.startswith("fixtures/") else root_fd
            leaf = PurePosixPath(name).name
            fd = os.open(leaf, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                         0o600, dir_fd=directory_fd)
            with os.fdopen(fd, "wb") as stream:
                stream.write(data)
                stream.flush()
                os.fchmod(stream.fileno(), 0o600)
            os.utime(leaf, ns=(0, 0), dir_fd=directory_fd, follow_symlinks=False)
            fd = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=directory_fd)
            with os.fdopen(fd, "rb") as stream:
                require(stat.S_ISREG(os.fstat(stream.fileno()).st_mode) and stream.read() == data,
                        "Extracted content differs")
        require(set(os.listdir(root_fd)) == {"result.json", "fixtures"} and
                set(os.listdir(fixture_fd)) == FIXTURE_NAMES, "Extracted inventory differs")
    finally:
        if fixture_fd is not None:
            os.close(fixture_fd)
        if root_fd is not None:
            os.close(root_fd)
        os.close(parent_fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, required=True)
    args = parser.parse_args()
    try:
        files, manifest = load_payload()
        write_new_destination(args.destination, files)
        print(json.dumps({
            "status": "FIXTURE_EXTRACTION_VERIFIED",
            "destination": str(args.destination), "file_count": 9, "fixture_count": 8,
            "manifest_sha256": MANIFEST_SHA256, "encoded_sha256": ENCODED_SHA256,
            "compressed_sha256": GZIP_SHA256, "uncompressed_tar_sha256": TAR_SHA256,
            "original_receipt_sha256": RECEIPT_SHA256,
            "generator_revision": GENERATOR_REVISION, "generator_binary_sha256": GENERATOR_SHA256,
            "generator_executed_here": False, "migration_executed_here": False,
            "original_binary_local_packaging_verification": manifest["original_generator_binary_verification"],
            "files": manifest["files"],
        }, indent=2))
    except (OSError, ValueError, KeyError, TypeError, tarfile.TarError) as error:
        print(json.dumps({"status": "FIXTURE_EXTRACTION_FAILED", "error": str(error),
                          "partial_destination_may_exist": args.destination.exists()}), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
