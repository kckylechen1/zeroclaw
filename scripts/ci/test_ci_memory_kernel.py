import unittest

from qualify_memory_kernel import check_graph, replace_pin

OLD = "c35ace357375f0e3a9db32d7a3bb529507a72291"
NEW = "572db8a4ae5fbdf1fb83d5f3b943c1d1442f9a32"


def graph():
    packages = [
        {"id": name, "name": name, "version": "1.0.0", "source": f"git+https://github.com/kckylechen1/Tachi.git?rev={NEW}#{NEW}"}
        for name in ("memcore", "portable-kernel", "vault-kit")
    ]
    packages.append({"id": "sqlite", "name": "libsqlite3-sys", "version": "0.39.1", "links": "sqlite3", "source": None})
    return {"packages": packages, "resolve": {"nodes": [{"id": package["id"], "features": []} for package in packages]}}


class KernelQualificationTests(unittest.TestCase):
    def test_only_the_verified_git_revision_changes(self):
        source = f'''[dependencies]\nportable_kernel = {{ package = "portable-kernel", version = "1.9.0", git = "https://github.com/kckylechen1/Tachi.git", rev = "{OLD}", optional = true }}\nother = "1"\n'''
        changed, old = replace_pin(source, NEW)
        self.assertEqual(old, OLD)
        self.assertEqual(changed, source.replace(OLD, NEW))
        with self.assertRaises(ValueError):
            replace_pin(source.replace("kckylechen1/Tachi", "other/Kernel"), NEW)
        with self.assertRaises(ValueError):
            replace_pin(source, "main")

    def test_graph_accepts_one_exact_portable_revision(self):
        result = check_graph(graph(), NEW)
        self.assertEqual(len(result["kernel_packages"]), 3)
        self.assertEqual(result["sqlite3"]["version"], "0.39.1")

    def test_mixed_revisions_and_admin_unification_are_failures(self):
        metadata = graph()
        metadata["packages"][0]["source"] = metadata["packages"][0]["source"].replace(NEW, OLD)
        with self.assertRaises(ValueError):
            check_graph(metadata, NEW)
        metadata = graph()
        metadata["resolve"]["nodes"][0]["features"] = ["admin"]
        with self.assertRaises(ValueError):
            check_graph(metadata, NEW)

    def test_absent_kernel_and_competing_sqlite_links_are_failures(self):
        metadata = graph()
        metadata["packages"] = metadata["packages"][1:]
        with self.assertRaises(ValueError):
            check_graph(metadata, NEW)
        metadata = graph()
        metadata["packages"].append({"id": "other-sqlite", "name": "other-sqlite", "version": "1", "links": "sqlite3", "source": None})
        with self.assertRaises(ValueError):
            check_graph(metadata, NEW)


if __name__ == "__main__":
    unittest.main()
