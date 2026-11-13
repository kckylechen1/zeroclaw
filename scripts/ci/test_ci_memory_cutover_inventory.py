import importlib.util
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "dev" / "memory_cutover_inventory.py"
SPEC = importlib.util.spec_from_file_location("memory_cutover_inventory", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class CutoverInventoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "offline.db"
        with sqlite3.connect(self.path) as conn:
            conn.executescript("""
                CREATE TABLE agents(id TEXT PRIMARY KEY, alias TEXT);
                CREATE TABLE memories(id TEXT PRIMARY KEY, key TEXT, content TEXT,
                    category TEXT, embedding BLOB, created_at TEXT, updated_at TEXT,
                    agent_id TEXT, session_id TEXT, namespace TEXT, kind TEXT,
                    tenant_id TEXT, pinned INTEGER, superseded_by TEXT);
                INSERT INTO agents VALUES ('uuid-1', 'PRIVATE_ALIAS');
                INSERT INTO memories VALUES ('one','PRIVATE_KEY','PRIVATE_BODY',
                    'core',NULL,'2026','2026','uuid-1',NULL,'default',NULL,NULL,0,NULL);
            """)

    def test_readonly_report_never_contains_rows_or_grants_cutover(self):
        before = self.path.read_bytes()
        result = MODULE.inspect_copy(self.path)
        self.assertEqual(result["rows"], 1)
        self.assertEqual(result["agents"], 1)
        self.assertEqual(result["blocked_findings"], [])
        self.assertFalse(result["cutover_allowed"])
        self.assertEqual(self.path.read_bytes(), before)
        self.assertEqual(sorted(p.name for p in self.path.parent.iterdir()), ["offline.db"])
        self.assertNotIn("PRIVATE", json.dumps(result))
        self.assertIn("uuid_to_alias_identity_mapping", result["requires"])

    def test_unmapped_identity_vectors_and_protected_rows_block(self):
        with sqlite3.connect(self.path) as conn:
            conn.execute("UPDATE memories SET agent_id='missing',key='soul::private',embedding=?", (b"bad",))
        result = MODULE.inspect_copy(self.path)
        self.assertEqual(set(result["blocked_findings"]), {
            "unmapped_agent_rows", "malformed_vector_blobs", "protected_soul_rows"})

    def test_vector_dimensions_and_history_are_not_silently_discarded(self):
        with sqlite3.connect(self.path) as conn:
            conn.execute("UPDATE memories SET embedding=?,superseded_by='later',pinned=1,session_id='private-session'", (bytes(4096),))
        result = MODULE.inspect_copy(self.path)
        self.assertEqual(result["vector_dimensions"], [{"dimensions": 1024, "rows": 1}])
        self.assertEqual(result["superseded_by_rows"], 1)
        self.assertEqual(result["pinned_rows"], 1)
        self.assertIn("canonical_supersession_history_preservation", result["requires"])
        self.assertNotIn("private-session", json.dumps(result))

    def test_unknown_columns_are_blockers(self):
        with sqlite3.connect(self.path) as conn:
            conn.execute("ALTER TABLE memories ADD COLUMN future_authority TEXT")
        self.assertIn("unknown_memory_columns", MODULE.inspect_copy(self.path)["blocked_findings"])

    def test_sidecars_and_symlinks_refuse_without_mutation(self):
        before = self.path.read_bytes()
        for suffix in ("-wal", "-shm", "-journal"):
            sidecar = Path(str(self.path) + suffix)
            sidecar.write_bytes(b"fixture")
            with self.assertRaises(ValueError):
                MODULE.inspect_copy(self.path)
            self.assertEqual(sidecar.read_bytes(), b"fixture")
            sidecar.unlink()
        link = self.path.parent / "link.db"
        link.symlink_to(self.path)
        with self.assertRaises(ValueError):
            MODULE.inspect_copy(link)
        self.assertEqual(self.path.read_bytes(), before)

    def test_cli_refuses_missing_schema_without_private_error_text(self):
        empty = self.path.parent / "empty.db"
        sqlite3.connect(empty).close()
        result = subprocess.run([sys.executable, str(SCRIPT), "--offline-copy", str(empty)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(result.stdout), {"status": "REFUSED", "cutover_allowed": False})
        self.assertEqual(result.stderr, "")


if __name__ == "__main__":
    unittest.main()
