#![cfg(feature = "tachi")]

const _: () = assert!(rusqlite::ffi::SQLITE_VERSION_NUMBER >= 3_054_001);
const _: () = assert!(portable_kernel::IS_PORTABLE_BUILD);
const _: () = assert!(!portable_kernel::ADMIN_SURFACE_ENABLED);

#[test]
fn portable_upgrade_preserves_the_consumers_sqlite_security_floor() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    let version: String = connection
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .unwrap();
    let parts: Vec<u32> = version.split('.').map(|part| part.parse().unwrap()).collect();
    assert_eq!(parts.len(), 3, "unexpected SQLite version: {version}");
    assert!(
        (parts[0], parts[1], parts[2]) >= (3, 54, 1),
        "consumer security floor must not regress: {version}"
    );
}
