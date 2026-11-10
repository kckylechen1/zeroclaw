use super::*;

#[test]
fn profile_role_and_unknown_store_identity_are_never_adopted() {
    for kind in [KernelStoreKind::Memory, KernelStoreKind::Companion] {
        assert_refused(
            kind,
            |conn| {
                conn.execute(
                    "DELETE FROM hard_state WHERE namespace='store_identity' AND key='profile'",
                    [],
                )
                .unwrap();
            },
            "profile stamp is missing",
        );
        for role in [
            "foreign-host",
            "project:zeroclaw-memory",
            "project:zeroclaw-companion",
        ] {
            assert_refused(
                kind,
                |conn| {
                    let value =
                        json!({"value":role,"conferred_by":"fixture","stamped_at":""}).to_string();
                    conn.execute("INSERT INTO hard_state(namespace,key,value_json) VALUES ('store_identity','role',?1)", [value]).unwrap();
                },
                "source role is foreign",
            );
        }
        assert_refused(
            kind,
            |conn| {
                conn.execute("INSERT INTO hard_state(namespace,key,value_json) VALUES ('store_identity','outbox_destination','{}')", []).unwrap();
            },
            "unknown store identity",
        );
        assert_refused(
            kind,
            |conn| {
                conn.execute("UPDATE hard_state SET value_json=json_set(value_json,'$.unexpected_authority','yes') WHERE namespace='store_identity' AND key='profile'", []).unwrap();
            },
            "store identity stamp",
        );
    }
    assert_refused(
        KernelStoreKind::Memory,
        |conn| {
            conn.execute("UPDATE hard_state SET value_json=json_set(value_json,'$.value','portable_kernel') WHERE namespace='store_identity' AND key='profile'", []).unwrap();
        },
        "source profile is incompatible",
    );
}

#[test]
fn schema_must_be_authentic_before_any_source_ddl_can_be_considered() {
    for sql in [
        "CREATE TABLE foreign_product (payload TEXT); INSERT INTO foreign_product VALUES ('preserve me')",
        "CREATE VIEW foreign_view AS SELECT 1",
        "CREATE TRIGGER foreign_trigger AFTER INSERT ON processed_events BEGIN SELECT 1; END",
        "ALTER TABLE access_history ADD COLUMN unaccounted TEXT",
    ] {
        assert_refused(
            KernelStoreKind::Memory,
            |conn| conn.execute_batch(sql).unwrap(),
            "legacy schema object",
        );
    }
    assert_refused(
        KernelStoreKind::Memory,
        |conn| {
            conn.execute(
                "INSERT INTO agent_identities(agent_identity_id) VALUES ('fixture-product-owner')",
                [],
            )
            .unwrap();
        },
        "nonempty product table",
    );
    assert_refused(
        KernelStoreKind::Memory,
        |conn| conn.pragma_update(None, "user_version", 37).unwrap(),
        "schema38",
    );
    assert_refused(
        KernelStoreKind::Memory,
        |conn| {
            let schema = schema::manifest(KernelStoreKind::Memory).unwrap();
            let definition = schema
                .objects
                .iter()
                .find(|object| object.name == "memories_fts")
                .unwrap()
                .sql
                .as_ref()
                .unwrap()
                .replace("'simple'", "'unicode61'");
            conn.execute_batch("DROP TABLE memories_fts").unwrap();
            conn.execute_batch(&definition).unwrap();
        },
        "legacy schema object",
    );
}

#[test]
fn protected_authority_history_is_refused_in_both_roles() {
    for kind in [KernelStoreKind::Memory, KernelStoreKind::Companion] {
        for namespace in [
            "typed_record_kinds",
            "operator_maintenance_receipt",
            "memory_supersession_receipts",
            "agent_capability_audit",
            "soul",
            "user_model",
            "agent_soul",
            "private_dyad",
        ] {
            assert_refused(
                kind,
                |conn| {
                    conn.execute("INSERT INTO hard_state(namespace,key,value_json) VALUES (?1,'fixture-authority','{}')", [namespace]).unwrap();
                },
                "protected kernel, Soul, or User Model authority history",
            );
        }
        assert_refused(
            kind,
            |conn| {
                conn.execute("INSERT INTO memory_edges(source_id,target_id,relation) VALUES ('prior-fixture','new-fixture','supersedes')", []).unwrap();
            },
            "canonical supersession authority",
        );
        assert_refused(
            kind,
            |conn| {
                conn.execute("INSERT INTO tachi_events(id,event_type,payload_json,created_at) VALUES ('supersession-fixture','memory.supersession.receipt.v1','{}','')", []).unwrap();
            },
            "canonical supersession authority",
        );
    }
}

#[test]
fn strict_decoder_refuses_unrepresentable_or_foreign_memory() {
    for (sql, message) in [
        (
            "UPDATE memories SET idless_identity='fixture-idless' WHERE id='legacy-active'",
            "non-NULL idless",
        ),
        (
            "UPDATE memories SET keywords='not-json' WHERE id='legacy-active'",
            "invalid JSON",
        ),
        (
            "UPDATE memories SET keywords='{}' WHERE id='legacy-active'",
            "array of strings",
        ),
        (
            "UPDATE memories SET keywords='[ \"legacy\" ]' WHERE id='legacy-active'",
            "byte-preserving",
        ),
        (
            "UPDATE memories SET archived=2 WHERE id='legacy-active'",
            "exact boolean",
        ),
        (
            "UPDATE memories SET revision='not-an-integer' WHERE id='legacy-active'",
            "not INTEGER",
        ),
        (
            "UPDATE memories SET metadata='[]' WHERE id='legacy-active'",
            "JSON object",
        ),
        (
            "UPDATE memories SET source='external:foreign-host' WHERE id='legacy-active'",
            "not the legacy ZeroClaw",
        ),
        (
            "UPDATE memories SET metadata=json_set(metadata,'$.zeroclaw_agent','foreign-agent') WHERE id='legacy-active'",
            "does not own",
        ),
        (
            "UPDATE memories SET metadata=json_set(metadata,'$.zeroclaw_namespace','soul') WHERE id='legacy-active'",
            "owning migration",
        ),
    ] {
        assert_refused(
            KernelStoreKind::Memory,
            |conn| {
                conn.execute(sql, []).unwrap();
            },
            message,
        );
    }
}

#[test]
fn companion_adoption_rejects_reserved_memory_identities() {
    for sql in [
        "UPDATE memories SET id='anchor:fixture' WHERE id='capture:fixture-pending'",
        "UPDATE memories SET id='wiki-rem:fixture' WHERE id='capture:fixture-pending'",
        "UPDATE memories SET id='wiki-operation-log' WHERE id='capture:fixture-pending'",
        "UPDATE memories SET id='   ' WHERE id='capture:fixture-pending'",
    ] {
        // Remove this fixture object's outbox entry first so orphan-outbox
        // admission does not mask the reserved identity refusal under test.
        assert_refused(
            KernelStoreKind::Companion,
            |conn| {
                conn.execute(
                    "DELETE FROM memory_outbox_events WHERE object_id='capture:fixture-pending'",
                    [],
                )
                .unwrap();
                conn.execute(sql, []).unwrap();
            },
            "reserved kernel memory identity",
        );
    }
    assert_refused(
        KernelStoreKind::Companion,
        |conn| {
            conn.execute(
                "UPDATE memories SET topic='WiKi_LoG' WHERE id='capture:fixture-pending'",
                [],
            )
            .unwrap();
        },
        "reserved kernel memory identity",
    );
}

#[test]
fn outbox_attribution_cannot_upgrade_private_or_foreign_history_to_companion_authority() {
    for (sql, message) in [
        (
            "UPDATE memory_outbox_events SET source_store='foreign' WHERE event_id='capture:fixture-pending'",
            "ownership or authority disagree",
        ),
        (
            "UPDATE memory_outbox_events SET source_partition='private_dyad' WHERE event_id='capture:fixture-pending'",
            "ownership or authority disagree",
        ),
        (
            "UPDATE memory_outbox_events SET source_partition='agent_soul' WHERE event_id='capture:fixture-pending'",
            "ownership or authority disagree",
        ),
        (
            "UPDATE memory_outbox_events SET object_class='user_model_head' WHERE event_id='capture:fixture-pending'",
            "ownership or authority disagree",
        ),
        (
            "UPDATE memory_outbox_events SET authority_class='agent_inferred' WHERE event_id='capture:fixture-pending'",
            "ownership or authority disagree",
        ),
        (
            "UPDATE memory_outbox_events SET source_revision=0 WHERE event_id='capture:fixture-pending'",
            "future source revision",
        ),
        (
            "UPDATE memory_outbox_events SET source_revision=999999 WHERE event_id='capture:fixture-pending'",
            "future source revision",
        ),
        (
            "UPDATE memory_outbox_events SET object_id='unattributable' WHERE event_id='capture:fixture-pending'",
            "no attributable memory",
        ),
        (
            "UPDATE memory_outbox_events SET payload_digest='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' WHERE event_id='capture:fixture-pending'",
            "digest disagrees",
        ),
        (
            "UPDATE memories SET text=json_set(text,'$.partition','private_dyad') WHERE id='capture:fixture-pending'",
            "protected companion payload partition",
        ),
        (
            "UPDATE memories SET text=json_set(text,'$.origin','gateway') WHERE id='capture:fixture-pending'",
            "attribution disagree",
        ),
        (
            "UPDATE memories SET text=json_set(text,'$.session_id','foreign-session') WHERE id='capture:fixture-pending'",
            "attribution disagree",
        ),
    ] {
        assert_refused(
            KernelStoreKind::Companion,
            |conn| {
                conn.execute(sql, []).unwrap();
            },
            message,
        );
    }
}

#[test]
fn unsupported_generic_queues_and_destination_authority_are_refused() {
    assert_refused(
        KernelStoreKind::Memory,
        |conn| {
            let mut events = fixture(KernelStoreKind::Companion)
                .tables
                .remove("memory_outbox_events")
                .unwrap();
            events.rows.truncate(1);
            let index = events
                .columns
                .iter()
                .position(|name| name == "object_id")
                .unwrap();
            events.rows[0][index] = Cell::Text("legacy-active".into());
            insert_rows(conn, "memory_outbox_events", &events);
        },
        "no legacy outbox producer",
    );
    for kind in [KernelStoreKind::Memory, KernelStoreKind::Companion] {
        assert_refused(
            kind,
            |conn| {
                conn.execute("INSERT INTO memory_outbox_destination_apply_receipts VALUES ('foreign-event','foreign-object','foreign-source','user_model',1,'digest','foreign-destination','user_model',1,'digest','applied')",[]).unwrap();
            },
            "destination-apply authority history",
        );
    }
}

#[test]
fn companion_legacy_normalization_is_detected_and_never_published() {
    assert_refused(
        KernelStoreKind::Companion,
        |conn| {
            // The genuine legacy generator advanced this object after ack, so the
            // outbox digest describes an older revision and remains valid history.
            conn.execute("UPDATE memories SET valid_from='',created_at='',updated_at='' WHERE id='capture:fixture-acked'",[]).unwrap();
        },
        "migration readback differs",
    );
}

#[test]
fn reopened_vec_shadows_preserve_the_canonical_legacy_inventory() {
    for kind in [KernelStoreKind::Memory, KernelStoreKind::Companion] {
        let root = tempfile::TempDir::new().unwrap();
        let path = root.path().join("legacy.db");
        restore_fixture(&path, kind);
        let before = source::census(&path).unwrap();
        let expected = schema::manifest(kind).unwrap();
        let conn = open_read_only_snapshot(&path).unwrap();
        let shapes = format::table_shapes(&conn).unwrap();
        for name in [
            "memories_vec_chunks",
            "memories_vec_info",
            "memories_vec_rowids",
        ] {
            assert_eq!(expected.tables[name].kind, "table");
            assert_eq!(shapes[name].kind, "shadow");
        }
        assert_eq!(shapes["memories_vec_vector_chunks00"].kind, "table");
        assert_ne!(shapes, expected.tables);
        let inventory = schema::inspect(&conn, kind).unwrap();
        assert_eq!(inventory.schema.objects, expected.objects);
        assert_eq!(inventory.schema.tables, expected.tables);
        assert_eq!(
            inventory.projection["memories_vec"],
            expected.tables["memories_vec"]
        );
        assert_eq!(
            inventory.schema_sha256,
            snapshot::bytes_digest(&serde_json::to_vec(&expected.objects).unwrap())
        );
        conn.execute_batch("ROLLBACK").unwrap();
        close_connection(conn).unwrap();
        assert_eq!(source::census(&path).unwrap(), before);
    }
}

#[test]
fn vec_shadow_compatibility_rejects_other_kinds_names_and_shape_drift() {
    for kind in [KernelStoreKind::Memory, KernelStoreKind::Companion] {
        let expected = schema::manifest(kind).unwrap().tables;
        let mut shadows = expected.clone();
        for name in [
            "memories_vec_chunks",
            "memories_vec_info",
            "memories_vec_rowids",
        ] {
            shadows.get_mut(name).unwrap().kind = "shadow".into();
        }
        assert!(schema::legacy_table_shapes_match(&expected, &expected));
        assert!(schema::legacy_table_shapes_match(&shadows, &expected));
        for (name, wrong_kind) in [
            ("memories", "shadow"),
            ("memories_vec_vector_chunks00", "shadow"),
            ("memories_vec_info", "virtual"),
            ("memories_vec_info", "view"),
            ("memories_vec", "table"),
        ] {
            let mut changed = shadows.clone();
            changed.get_mut(name).unwrap().kind = wrong_kind.into();
            assert!(
                !schema::legacy_table_shapes_match(&changed, &expected),
                "must reject {name} as {wrong_kind}"
            );
        }
        let mut extra = shadows.clone();
        extra.insert(
            "memories_vec_unknown".into(),
            shadows["memories_vec_info"].clone(),
        );
        assert!(!schema::legacy_table_shapes_match(&extra, &expected));
        let mut missing = shadows.clone();
        missing.remove("memories_vec");
        assert!(!schema::legacy_table_shapes_match(&missing, &expected));
        let changes: &[fn(&mut format::TableShape)] = &[
            |shape| shape.without_rowid = !shape.without_rowid,
            |shape| shape.strict = !shape.strict,
            |shape| shape.columns[0].name.push_str("_changed"),
            |shape| shape.columns[0].declared_type.push_str("_changed"),
            |shape| shape.columns[0].not_null = !shape.columns[0].not_null,
            |shape| shape.columns[0].default = Some("NULL".into()),
            |shape| shape.columns[0].primary_key += 1,
            |shape| shape.columns[0].hidden += 1,
            |shape| {
                shape.columns.pop();
            },
        ];
        for name in [
            "memories_vec",
            "memories_vec_chunks",
            "memories_vec_info",
            "memories_vec_rowids",
        ] {
            for change in changes {
                let mut changed = shadows.clone();
                change(changed.get_mut(name).unwrap());
                assert!(!schema::legacy_table_shapes_match(&changed, &expected));
            }
        }
    }
}

#[test]
fn vec_shadow_compatibility_requires_authentic_parent_and_backing_ddl() {
    for kind in [KernelStoreKind::Memory, KernelStoreKind::Companion] {
        for sql in [
            "CREATE TABLE memories_vec_unknown (payload TEXT)",
            "ALTER TABLE memories_vec_info ADD COLUMN unaccounted TEXT",
            "DROP TABLE memories_vec; CREATE VIRTUAL TABLE memories_vec USING vec0(id TEXT PRIMARY KEY, embedding float[16])",
            "DROP TABLE memories_vec; CREATE TABLE memories_vec(id TEXT PRIMARY KEY, embedding BLOB)",
        ] {
            assert_refused(
                kind,
                |conn| conn.execute_batch(sql).unwrap(),
                "legacy schema object",
            );
        }
    }
}
