#!/usr/bin/env python3
"""Exercise the real offline migration CLI against unchanged old-kernel fixtures.

No builds, SQL writes, schema fabrication, or virtual-table extension loading.
The caller builds the original fixture generator and the candidate CLI first.
Python reads ordinary tables with immutable read-only SQLite connections. Logical
vector bytes are checked against the old generator's JSON and fixed fixture
payload through the CLI's source/target digest receipts; Python does not query
vec0. The CLI itself performs native source/target vector readback and exact-role
kernel reopens. External Python reopens do not establish kernel admission.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import stat
import struct
import subprocess
import sys
import time
import traceback

LEGACY_SHA = "eba0c48617e8820831bfd22f91d269c6d70f409f"
BASE_SHA = "10cdae7eec403a5bbbd68cc3066a5386b41033e3"
SCHEMA_HASHES = {
    "memory": "be0ab6750fdbc6806b90b08ec363abd5ab75b062c93cc55cee3f634c6003f32a",
    "companion": "21b7b8f272646df8a617bfeeda0de8d920217586056007ba32a1b4a1d89b1dc8",
}
VECTOR_HASHES = {
    "memory": "61beb1b19cbfbe5e4883d15095d9e45b2223edd8fb76a8cce366951b2c074b5b",
    "companion": "82b5c403fa0de7fe3be021c0eacb678004fd214cd5580ed74f930d0813c20dc6",
}
SUFFIXES = ("", "-wal", "-shm", "-journal", ".migration-marker", ".identity.json")
FIXTURE_PREFIX = "crates/zeroclaw-memory/tests/fixtures/legacy-kernel/"
FROZEN_INPUTS = [FIXTURE_PREFIX + name for name in (
    "Cargo.toml", "Cargo.lock", "generate.rs", "data.rs", "README.md",
    "memory-v38.json", "companion-v38.json",
)] + ["crates/zeroclaw-memory/src/portable_migration/" + name for name in (
    "format.rs", "memory-schema-v38.json", "companion-schema-v38.json",
)]


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()


def json_file(path):
    return json.loads(path.read_text())


def write_json(path, data):
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")


def census(path):
    result = []
    for suffix in SUFFIXES:
        item = Path(str(path) + suffix)
        try:
            before = item.lstat()
        except FileNotFoundError:
            require(suffix != "", "fixture source database is missing")
            result.append(dict(suffix=suffix, present=False, physical_identity=None,
                               size=None, modified=None, sha256=None))
            continue
        require(stat.S_ISREG(before.st_mode), f"nonregular source file: {item}")
        digest = sha256(item)
        after = item.lstat()
        attributes = ("st_dev", "st_ino", "st_size", "st_mtime_ns")
        require(all(getattr(before, key) == getattr(after, key) for key in attributes),
                f"source changed during census: {item}")
        seconds, nanos = divmod(before.st_mtime_ns, 1_000_000_000)
        result.append(dict(suffix=suffix, present=True,
                           physical_identity=f"unix:{before.st_dev}:{before.st_ino}",
                           size=before.st_size, modified=f"{seconds}.{nanos:09d}", sha256=digest))
    return result


def readonly(path):
    # The original generator and successful CLI checkpoint their databases.
    # Immutable reads avoid SHM creation or read-mark changes on source files.
    wal = Path(str(path) + "-wal")
    require(not wal.exists() or wal.stat().st_size == 0,
            "external immutable oracle requires a checkpointed database")
    conn = sqlite3.connect(path.as_uri() + "?mode=ro&immutable=1", uri=True)
    conn.execute("PRAGMA query_only=ON")
    return conn


def quoted(name):
    return '"' + name.replace('"', '""') + '"'


def component(data):
    return struct.pack("<Q", len(data)) + data


def cell(value):
    if value is None:
        return b"\x00"
    if isinstance(value, int):
        return b"\x01" + struct.pack("<q", value)
    if isinstance(value, float):
        return b"\x02" + struct.pack("<d", value)
    if isinstance(value, str):
        return b"\x03" + component(value.encode("utf-8"))
    if isinstance(value, bytes):
        return b"\x04" + component(value)
    raise TypeError(f"unsupported SQLite cell type: {type(value).__name__}")


def table_digest(name, columns, rows):
    hashes = sorted(hashlib.sha256(b"".join(cell(value) for value in row)).digest()
                    for row in rows)
    payload = b"zeroclaw-kernel-migration-sqlite-values-v1\0"
    payload += b"".join(component(column.encode("utf-8")) for column in columns)
    payload += struct.pack("<Q", len(hashes)) + b"".join(hashes)
    return dict(table=name, columns=columns, rows=len(hashes),
                sha256=hashlib.sha256(payload).hexdigest())


def projection_name(name):
    return any(name == prefix or name.startswith(prefix + "_")
               for prefix in ("memories_fts", "memories_symbolic_fts", "memories_vec"))


def ordinary_projection(source_schema, portable_schema):
    result = {}
    for name, shape in portable_schema["tables"].items():
        if projection_name(name) or name == "recall_cache":
            continue
        require(source_schema["tables"].get(name) == shape, f"source shape mismatch: {name}")
        columns = []
        if shape["kind"] == "table" and not shape["without_rowid"] and name not in ("memories", "hard_state"):
            columns.append("rowid")
        columns += [column["name"] for column in shape["columns"] if column["hidden"] == 0]
        result[name] = columns
    require(len(result) == 24, "unexpected independent ordinary projection inventory")
    return result


def ordinary_digests(conn, projection, receipt_key=None):
    result = {}
    for name, columns in sorted(projection.items()):
        sql = f"SELECT {','.join(map(quoted, columns))} FROM {quoted(name)}"
        parameters = ()
        if name == "hard_state":
            sql += " WHERE namespace != 'migrations' AND NOT (namespace='store_identity' AND key IN ('profile','role'))"
            if receipt_key is not None:
                sql += " AND NOT (namespace=? AND key=?)"
                parameters = ("zeroclaw_kernel_migration", receipt_key)
        result[name] = table_digest(name, columns, conn.execute(sql, parameters).fetchall())
    return result


def vector_oracle(role, fixture):
    data = fixture["tables"]["memories_vec"]
    require(data["columns"] == ["id", "embedding"], "unexpected old-generator vector columns")
    expected_rows = []
    if role == "memory":
        vector = b"".join(struct.pack("<f", index / 1024.0) for index in range(1024))
        expected_rows = [["legacy-active", vector]]
        require(hashlib.sha256(vector).hexdigest() ==
                "60d87a7d659a21437b3df36ed46b125464a8494c2a51ddbd21ce1b9ee054ba2a",
                "fixed logical vector payload differs")
    encoded = [[{"type": "text", "value": row[0]},
                {"type": "blob", "value": list(row[1])}] for row in expected_rows]
    require(data["rows"] == encoded, "old-generator logical vector rows differ from fixed fixture")
    digest = table_digest("memories_vec", data["columns"], expected_rows)
    require(digest["sha256"] == VECTOR_HASHES[role], "independent vector digest differs")
    return digest


def check_target(conn, role, receipt, projection, expected):
    require(conn.execute("PRAGMA user_version").fetchone()[0] == 41, "target is not schema41")
    require(conn.execute("SELECT count(*) FROM memories").fetchone()[0] == 4,
            "target memory row count differs")
    require(conn.execute("SELECT count(*) FROM memory_outbox_events").fetchone()[0] ==
            (5 if role == "companion" else 0), "target outbox row count differs")
    require(conn.execute("SELECT count(*) FROM recall_cache").fetchone()[0] == 0,
            "target recall cache was not invalidated")
    for key, value in (("profile", "portable_kernel"), ("role", "zeroclaw-" + role)):
        record = conn.execute("SELECT value_json FROM hard_state WHERE namespace='store_identity' AND key=?", (key,)).fetchone()
        require(record is not None and json.loads(record[0])["value"] == value,
                f"wrong target {key}")
    stored = conn.execute("SELECT value_json,version FROM hard_state WHERE namespace=? AND key=?",
                          (receipt["receipt_namespace"], receipt["receipt_key"])).fetchone()
    require(stored is not None and stored[1] == 1 and json.loads(stored[0]) == receipt,
            "persisted receipt differs from actual CLI stdout")
    observed = ordinary_digests(conn, projection, receipt["receipt_key"])
    require(observed == expected, "independent target ordinary projections differ from source")
    return observed


def run_role(args, role, output, binary, portable_schema):
    source = (args.fixtures / f"{role}-v38.db").resolve()
    before = census(source)
    source_schema = json_file(args.fixtures / f"{role}-schema-v38.json")
    fixture = json_file(args.fixtures / f"{role}-v38.json")
    for artifact in (source_schema, fixture):
        require(artifact["source_revision"] == LEGACY_SHA and artifact["user_version"] == 38,
                "fixture provenance/version mismatch")
    require(sha256(args.fixtures / f"{role}-schema-v38.json") == SCHEMA_HASHES[role],
            "generated schema differs from frozen authentic legacy schema")
    projection = ordinary_projection(source_schema, portable_schema)
    conn = readonly(source)
    try:
        require(conn.execute("PRAGMA user_version").fetchone()[0] == 38, "source is not schema38")
        require(conn.execute("SELECT count(*) FROM memories").fetchone()[0] == 4,
                "source memory row count differs")
        actual_objects = [dict(zip(("object_type", "name", "table", "sql"), row)) for row in
                          conn.execute("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")]
        require(actual_objects == source_schema["objects"], "source DDL differs from generated schema")
        expected = ordinary_digests(conn, projection)
    finally:
        conn.close()
    require(census(source) == before, "source changed during independent read-only census")
    vector = vector_oracle(role, fixture)
    run_dir = output / role
    run_dir.mkdir(mode=0o700)
    destination = run_dir / "migrated.db"
    config_dir = run_dir / "not-configured"
    command = [str(binary), "--config-dir", str(config_dir), "memory", "migrate-kernel",
               "--source", str(source), "--destination", str(destination), "--store", role,
               "--approved-by", "sqlite-backport-cli-e2e"]
    environment = os.environ.copy()
    environment.update(ZEROCLAW_CONFIG_DIR=str(config_dir), LC_ALL="C")
    started = time.monotonic()
    completed = subprocess.run(command, env=environment, cwd=run_dir, capture_output=True, timeout=180)
    (run_dir / "stdout.json").write_bytes(completed.stdout)
    (run_dir / "stderr.log").write_bytes(completed.stderr)
    write_json(run_dir / "invocation.json", dict(command=command, exit_code=completed.returncode,
               elapsed_seconds=time.monotonic() - started))
    require(completed.returncode == 0, f"{role} actual CLI failed; inspect stderr.log")
    receipt = json.loads(completed.stdout)
    fields = dict(format="zeroclaw.kernel.migration/v1", source_revision=LEGACY_SHA,
                  source_schema=38, target_schema=41, target_profile="portable_kernel",
                  source_profile="tachi_full" if role == "memory" else "portable_kernel",
                  role="zeroclaw-" + role, approved_by="sqlite-backport-cli-e2e",
                  receipt_namespace="zeroclaw_kernel_migration", source=str(source),
                  destination=str(destination), requires_offline_quiescent_source=True,
                  recall_cache_rows_invalidated=1)
    require(all(receipt.get(key) == value for key, value in fields.items()), "CLI receipt identity mismatch")
    require(receipt["source_row_counts"]["memories"] == 4, "receipt memory count mismatch")
    require(receipt["source_row_counts"]["memory_outbox_events"] ==
            (5 if role == "companion" else 0), "receipt outbox count mismatch")
    verification = receipt["verification"]
    require(verification["source_files"] == before, "CLI source-file receipt differs from independent census")
    encoded_objects = json.dumps(actual_objects, ensure_ascii=False, separators=(",", ":")).encode()
    require(verification["source_schema_sha256"] == hashlib.sha256(encoded_objects).hexdigest(),
            "CLI source schema digest differs from independent DDL census")
    expected_all = dict(expected, memories_vec=vector)
    for side in ("source_tables", "target_tables"):
        records = verification[side]
        require(len(records) == 25 and len({record["table"] for record in records}) == 25,
                "CLI verification table set is incomplete or duplicated")
        require({record["table"]: record for record in records} == expected_all,
                f"CLI {side} differs from independent ordinary/vector oracles")
    require(not config_dir.exists(), "offline migration initialized agent configuration")
    require(not any(path.name.startswith(".zeroclaw-kernel-migration-") for path in run_dir.iterdir()),
            "temporary migration staging remains")
    for path in (destination, Path(str(destination) + ".migration-marker")):
        require(stat.S_IMODE(path.stat().st_mode) == 0o600, f"published file is not private: {path}")
    reopen_results = []
    for phase in range(2):
        conn = readonly(destination)
        try:
            observed = check_target(conn, role, receipt, projection, expected)
        finally:
            conn.close()
        require(census(source) == before, "original source or sidecar changed after CLI/reopen")
        reopen_results.append(dict(phase=phase + 1, ordinary_tables=len(observed), result="PASS"))
    summary = dict(result="PASS", exit_code=completed.returncode, source=str(source),
                   destination=str(destination), source_census=before,
                   source_schema=38, target_schema=41, role=receipt["role"], memory_rows=4,
                   outbox_rows=5 if role == "companion" else 0,
                   ordinary_projection_tables=24, logical_vector_receipt_oracles=1,
                   external_readonly_reopens=reopen_results,
                   vector_oracle=vector, source_unchanged=True, stored_receipt_matches_stdout=True)
    write_json(run_dir / "result.json", summary)
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--binary-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.repo = args.repo.resolve()
    args.fixtures = args.fixtures.resolve()
    args.binary = args.binary.resolve()
    output = args.output.resolve()
    output.mkdir(mode=0o700, parents=False, exist_ok=False)
    result = dict(result="FAIL", coverage="Two real CLI migrations; independent ordinary readback; receipt-based logical vector oracle",
                  python_sqlite_version=sqlite3.sqlite_version,
                  python_sqlite_is_not_the_production_bundled_sqlite=True,
                  source_sha=args.source_sha, binary_sha256=args.binary_sha256,
                  harness_sha256=sha256(Path(__file__).resolve()), roles={})
    try:
        require(sys.platform != "win32", "Unix-only migration harness")
        require(git(args.repo, "rev-parse", "HEAD") == args.source_sha, "candidate HEAD mismatch")
        result["source_tree"] = git(args.repo, "rev-parse", "HEAD^{tree}")
        require(not git(args.repo, "status", "--porcelain", "--untracked-files=no"), "candidate tracked files are dirty")
        require(not git(args.repo, "diff", "--name-only", BASE_SHA, args.source_sha, "--", *FROZEN_INPUTS),
                "original generator, raw format, or frozen fixture inputs changed")
        require(sha256(args.binary) == args.binary_sha256, "candidate binary hash mismatch")
        binary = output / "zeroclaw"
        shutil.copy2(args.binary, binary)
        require(sha256(binary) == args.binary_sha256, "copied CLI hash mismatch")
        result["binary_source_binding"] = "Caller supplies the build's source SHA and binary SHA; harness verifies both independently but does not compile or prove their relation. Preserve the build receipt with this result."
        result["fixture_files"] = {path.name: sha256(path) for path in sorted(args.fixtures.iterdir()) if path.is_file()}
        portable_schema = json_file(args.fixtures / "companion-schema-v38.json")
        for role in ("memory", "companion"):
            result["roles"][role] = run_role(args, role, output, binary, portable_schema)
        require(git(args.repo, "rev-parse", "HEAD") == args.source_sha and
                not git(args.repo, "status", "--porcelain", "--untracked-files=no"),
                "candidate changed during validation")
        require(sha256(binary) == args.binary_sha256, "CLI binary changed during validation")
        result["result"] = "PASS"
    except Exception as error:
        result["error"] = f"{type(error).__name__}: {error}"
        result["traceback"] = traceback.format_exc()
    write_json(output / "result.json", result)
    print(json.dumps(dict(result=result["result"], output=str(output), error=result.get("error"))))
    return 0 if result["result"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
