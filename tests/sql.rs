//! End-to-end integration tests driving FerroDB through its public SQL API.

use ferrodb::engine::{Database, QueryResult};
use ferrodb::value::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A database backed by a unique temp file that is deleted on drop.
struct TestDb {
    path: PathBuf,
}

impl TestDb {
    fn new() -> TestDb {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("ferrodb-test-{}-{}.db", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        TestDb { path }
    }

    fn open(&self) -> Database {
        Database::open(&self.path).expect("open database")
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Convenience: run SQL and return the rows of the final SELECT.
fn rows(results: &[QueryResult]) -> &Vec<Vec<Value>> {
    match results.last().expect("at least one result") {
        QueryResult::Select { rows, .. } => rows,
        other => panic!("expected SELECT result, got {other:?}"),
    }
}

#[test]
fn create_insert_select_roundtrip() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE users (id INTEGER, name TEXT, age INTEGER);")
        .unwrap();
    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30), (2, 'Bob', 25);")
        .unwrap();
    let r = db.execute("SELECT * FROM users ORDER BY id;").unwrap();
    let rows = rows(&r);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Text("Alice".into()));
    assert_eq!(rows[1][2], Value::Integer(25));
}

#[test]
fn where_filters_and_operators() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE n (x INTEGER, label TEXT);").unwrap();
    db.execute("INSERT INTO n VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e');")
        .unwrap();

    let r = db.execute("SELECT x FROM n WHERE x >= 3 AND x < 5;").unwrap();
    let got: Vec<i64> = rows(&r)
        .iter()
        .map(|row| match &row[0] {
            Value::Integer(n) => *n,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![3, 4]);

    let r = db
        .execute("SELECT x FROM n WHERE x = 1 OR x = 5 ORDER BY x DESC;")
        .unwrap();
    let got: Vec<i64> = rows(&r)
        .iter()
        .map(|row| match &row[0] {
            Value::Integer(n) => *n,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![5, 1]);
}

#[test]
fn update_and_delete() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE p (id INTEGER, qty INTEGER);").unwrap();
    db.execute("INSERT INTO p VALUES (1,10),(2,20),(3,30);").unwrap();

    let r = db.execute("UPDATE p SET qty = qty * 2 WHERE id = 2;").unwrap();
    assert_eq!(r[0], QueryResult::Updated(1));

    let r = db.execute("SELECT qty FROM p WHERE id = 2;").unwrap();
    assert_eq!(rows(&r)[0][0], Value::Integer(40));

    let r = db.execute("DELETE FROM p WHERE qty < 30;").unwrap();
    assert_eq!(r[0], QueryResult::Deleted(1)); // only id=1 (qty 10) remains < 30

    let r = db.execute("SELECT id FROM p ORDER BY id;").unwrap();
    let got: Vec<i64> = rows(&r)
        .iter()
        .map(|row| match &row[0] {
            Value::Integer(n) => *n,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![2, 3]);
}

#[test]
fn limit_and_order_by() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE s (v INTEGER);").unwrap();
    db.execute("INSERT INTO s VALUES (5),(3),(9),(1),(7);").unwrap();
    let r = db.execute("SELECT v FROM s ORDER BY v ASC LIMIT 3;").unwrap();
    let got: Vec<i64> = rows(&r)
        .iter()
        .map(|row| match &row[0] {
            Value::Integer(n) => *n,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![1, 3, 5]);
}

#[test]
fn null_handling() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE m (a INTEGER, b TEXT);").unwrap();
    db.execute("INSERT INTO m (a) VALUES (1);").unwrap(); // b is NULL
    let r = db.execute("SELECT a, b FROM m;").unwrap();
    assert_eq!(rows(&r)[0][1], Value::Null);
    // Comparison with NULL is never true.
    let r = db.execute("SELECT a FROM m WHERE b = 'x';").unwrap();
    assert_eq!(rows(&r).len(), 0);
}

#[test]
fn persistence_across_reopen() {
    let t = TestDb::new();
    {
        let mut db = t.open();
        db.execute("CREATE TABLE k (id INTEGER, txt TEXT);").unwrap();
        db.execute("INSERT INTO k VALUES (1, 'persisted');").unwrap();
    } // db dropped, file flushed
    {
        let mut db = t.open();
        let r = db.execute("SELECT txt FROM k WHERE id = 1;").unwrap();
        assert_eq!(rows(&r)[0][0], Value::Text("persisted".into()));
    }
}

#[test]
fn btree_split_many_rows() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE big (k INTEGER, v TEXT);").unwrap();
    // Enough rows to force many leaf and internal node splits.
    for i in 0..2000 {
        db.execute(&format!(
            "INSERT INTO big (k, v) VALUES ({i}, 'value-number-{i}');"
        ))
        .unwrap();
    }
    // Spot-check boundary keys.
    for &i in &[0i64, 1, 999, 1000, 1999] {
        let r = db
            .execute(&format!("SELECT v FROM big WHERE k = {i};"))
            .unwrap();
        assert_eq!(
            rows(&r)[0][0],
            Value::Text(format!("value-number-{i}")),
            "lookup failed for key {i}"
        );
    }
    // Full ordered scan should return all rows in ascending key order.
    let r = db.execute("SELECT k FROM big ORDER BY k ASC;").unwrap();
    let all = rows(&r);
    assert_eq!(all.len(), 2000);
    assert_eq!(all[0][0], Value::Integer(0));
    assert_eq!(all[1999][0], Value::Integer(1999));
}

#[test]
fn errors_are_reported() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE e (a INTEGER);").unwrap();
    assert!(db.execute("CREATE TABLE e (a INTEGER);").is_err()); // duplicate
    assert!(db.execute("SELECT * FROM missing;").is_err()); // unknown table
    assert!(db.execute("INSERT INTO e VALUES ('not-an-int');").is_err()); // type error
    assert!(db.execute("SELECT bogus FROM e;").is_err()); // unknown column
    assert!(db.execute("SELECT * FROM;").is_err()); // parse error
}

#[test]
fn arithmetic_and_expressions() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE c (a INTEGER, b INTEGER);").unwrap();
    db.execute("INSERT INTO c VALUES (10, 3);").unwrap();
    let r = db
        .execute("SELECT a FROM c WHERE (a + b) * 2 = 26 AND NOT (a < b);")
        .unwrap();
    assert_eq!(rows(&r).len(), 1);
    // Division by zero yields NULL, so the WHERE is not satisfied.
    let r = db.execute("SELECT a FROM c WHERE a / 0 = 0;").unwrap();
    assert_eq!(rows(&r).len(), 0);
}
