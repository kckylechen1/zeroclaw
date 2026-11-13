#!/usr/bin/env python3
"""Read-only inventory of an explicitly supplied, offline native-memory copy.

This is not an importer or a cutover approval. Stop writers and checkpoint a
private copy first. Files with SQLite sidecars or symlinked paths are refused.
The JSON contains aggregate observations and fingerprints, never row bodies,
keys, aliases or a reusable deletion list. No destination store is created.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sqlite3
import stat
import time

KNOWN_COLUMNS = set("id key content category embedding created_at updated_at session_id namespace importance superseded_by agent_id kind pinned tenant_id".split())
REQUIRED_COLUMNS = set("id key content category embedding created_at updated_at agent_id".split())


def fingerprint(path: Path) -> tuple[tuple[int, ...], str]:
    details = path.lstat()
    if not stat.S_ISREG(details.st_mode) or path.resolve(strict=True) != path.absolute():
        raise ValueError("source must be a regular offline copy without symlink components")
    if any(Path(str(path) + suffix).exists() or Path(str(path) + suffix).is_symlink()
           for suffix in ("-wal", "-shm", "-journal")):
        raise ValueError("source has SQLite sidecars; checkpoint an offline copy first")
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    identity = (details.st_dev, details.st_ino, details.st_size, details.st_mtime_ns, details.st_ctime_ns)
    return identity, digest.hexdigest()


def inspect_copy(path: Path, max_seconds: float = 30.0) -> dict:
    if not 0 < max_seconds <= 300:
        raise ValueError("inspection budget must be within 0..300 seconds")
    path = path.absolute()
    before = fingerprint(path)
    deadline = time.monotonic() + max_seconds
    connection = sqlite3.connect(path.as_uri() + "?mode=ro&immutable=1", uri=True)
    try:
        connection.set_progress_handler(lambda: int(time.monotonic() >= deadline), 1000)
        connection.execute("PRAGMA query_only=ON")
        connection.execute("PRAGMA trusted_schema=OFF")
        if connection.execute("PRAGMA quick_check(1)").fetchone() != ("ok",):
            raise ValueError("source integrity check failed")
        table_kinds = {row[1]: row[2] for row in connection.execute("PRAGMA table_list")}
        if any(table_kinds.get(table) != "table" for table in ("memories", "agents")):
            raise ValueError("expected native ordinary memories and agents tables")
        columns = {row[1] for row in connection.execute("PRAGMA table_info(memories)")}
        agents_columns = {row[1] for row in connection.execute("PRAGMA table_info(agents)")}
        if not REQUIRED_COLUMNS <= columns or not {"id", "alias"} <= agents_columns:
            raise ValueError("native agent-attributed schema is not recognized")

        def count(predicate: str = "1") -> int:
            return connection.execute("SELECT COUNT(*) FROM memories WHERE " + predicate).fetchone()[0]

        report = {
            "format": "zeroclaw.memory-cutover-inventory.v1",
            "source_sha256": before[1],
            "source_bytes": before[0][2],
            "read_only": True,
            "cutover_allowed": False,
            "rows": count(),
            "agents": connection.execute("SELECT COUNT(*) FROM agents").fetchone()[0],
            "unmapped_agent_rows": connection.execute(
                "SELECT COUNT(*) FROM memories m LEFT JOIN agents a ON a.id=m.agent_id WHERE a.id IS NULL"
            ).fetchone()[0],
            "ambiguous_agent_ids": connection.execute(
                "SELECT COUNT(*) FROM (SELECT id FROM agents GROUP BY id HAVING COUNT(*) != 1)"
            ).fetchone()[0],
            "ambiguous_agent_aliases": connection.execute(
                "SELECT COUNT(*) FROM (SELECT alias FROM agents GROUP BY alias HAVING COUNT(*) != 1 OR alias IS NULL OR alias = '')"
            ).fetchone()[0],
            "unknown_memory_columns": len(columns - KNOWN_COLUMNS),
            "vectors": count("embedding IS NOT NULL"),
            "malformed_vector_blobs": count("embedding IS NOT NULL AND (typeof(embedding) != 'blob' OR length(embedding) = 0 OR length(embedding) % 4 != 0)"),
            "vector_dimensions": [
                {"dimensions": row[0], "rows": row[1]}
                for row in connection.execute(
                    "SELECT length(embedding)/4, COUNT(*) FROM memories "
                    "WHERE typeof(embedding)='blob' AND length(embedding)>0 AND length(embedding)%4=0 "
                    "GROUP BY length(embedding) ORDER BY length(embedding)"
                )
            ],
            "protected_soul_rows": count("substr(key,1,6)='soul::'" + (" OR namespace='soul'" if "namespace" in columns else "")),
            "requires": ["verified_importer_and_full_row_roundtrip", "rollback_rehearsal", "explicit_owner_cutover"],
        }
        for field in ("session_id", "tenant_id", "kind", "superseded_by"):
            report[field + "_rows"] = count(field + " IS NOT NULL") if field in columns else None
        report["pinned_rows"] = count("pinned != 0") if "pinned" in columns else None
        schema = list(connection.execute(
            "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name,tbl_name"
        ))
        report["schema_sha256"] = hashlib.sha256(json.dumps(schema, ensure_ascii=False).encode()).hexdigest()
        report["requires"].append("uuid_to_alias_identity_mapping")
        if report["vectors"]:
            report["requires"].append("embedding_identity_and_dimension_admission")
        if report.get("superseded_by_rows"):
            report["requires"].append("canonical_supersession_history_preservation")
        report["blocked_findings"] = [name for name in (
            "unmapped_agent_rows", "ambiguous_agent_ids", "ambiguous_agent_aliases",
            "unknown_memory_columns", "malformed_vector_blobs", "protected_soul_rows",
        ) if report[name]]
    finally:
        connection.close()
    if fingerprint(path) != before:
        raise ValueError("source changed during inspection; discard this inventory")
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline-copy", type=Path, required=True)
    parser.add_argument("--max-seconds", type=float, default=30.0)
    args = parser.parse_args()
    try:
        report = inspect_copy(args.offline_copy, args.max_seconds)
    except (OSError, ValueError, sqlite3.Error):
        # SQLite diagnostics can interpolate data/schema text. Keep refusals
        # content-free; no success report is emitted after a read error.
        print(json.dumps({"status": "REFUSED", "cutover_allowed": False}))
        return 1
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
