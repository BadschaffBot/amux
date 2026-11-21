// Own test binary = own process, so no Connection has been opened yet.
#[test]
fn memstatus_off_succeeds_before_first_open() {
    assert_eq!(amux_server::disable_sqlite_memstatus(), 0);
    let c = rusqlite::Connection::open_in_memory().unwrap();
    c.execute_batch("SELECT 1").unwrap();
}
