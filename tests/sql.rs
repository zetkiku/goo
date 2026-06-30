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
    db.execute("CREATE TABLE n (x INTEGER, label TEXT);")
        .unwrap();
    db.execute("INSERT INTO n VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e');")
        .unwrap();

    let r = db
        .execute("SELECT x FROM n WHERE x >= 3 AND x < 5;")
        .unwrap();
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
    db.execute("CREATE TABLE p (id INTEGER, qty INTEGER);")
        .unwrap();
    db.execute("INSERT INTO p VALUES (1,10),(2,20),(3,30);")
        .unwrap();

    let r = db
        .execute("UPDATE p SET qty = qty * 2 WHERE id = 2;")
        .unwrap();
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
    db.execute("INSERT INTO s VALUES (5),(3),(9),(1),(7);")
        .unwrap();
    let r = db
        .execute("SELECT v FROM s ORDER BY v ASC LIMIT 3;")
        .unwrap();
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
        db.execute("CREATE TABLE k (id INTEGER, txt TEXT);")
            .unwrap();
        db.execute("INSERT INTO k VALUES (1, 'persisted');")
            .unwrap();
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
    db.execute("CREATE TABLE c (a INTEGER, b INTEGER);")
        .unwrap();
    db.execute("INSERT INTO c VALUES (10, 3);").unwrap();
    let r = db
        .execute("SELECT a FROM c WHERE (a + b) * 2 = 26 AND NOT (a < b);")
        .unwrap();
    assert_eq!(rows(&r).len(), 1);
    // Division by zero yields NULL, so the WHERE is not satisfied.
    let r = db.execute("SELECT a FROM c WHERE a / 0 = 0;").unwrap();
    assert_eq!(rows(&r).len(), 0);
}

#[test]
fn transaction_commit_persists() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE tx (id INTEGER, v TEXT);").unwrap();
    db.execute("BEGIN;").unwrap();
    db.execute("INSERT INTO tx VALUES (1, 'a'), (2, 'b');")
        .unwrap();
    db.execute("COMMIT;").unwrap();
    let r = db.execute("SELECT id FROM tx ORDER BY id;").unwrap();
    assert_eq!(rows(&r).len(), 2);

    // And it survives a reopen.
    drop(db);
    let mut db2 = t.open();
    let r = db2.execute("SELECT id FROM tx;").unwrap();
    assert_eq!(rows(&r).len(), 2);
}

#[test]
fn transaction_rollback_discards_changes() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE tx (id INTEGER, v TEXT);").unwrap();
    db.execute("INSERT INTO tx VALUES (1, 'original');")
        .unwrap();

    db.execute("BEGIN;").unwrap();
    db.execute("INSERT INTO tx VALUES (2, 'temp'), (3, 'temp');")
        .unwrap();
    db.execute("UPDATE tx SET v = 'changed' WHERE id = 1;")
        .unwrap();
    db.execute("DELETE FROM tx WHERE id = 1;").unwrap();
    // Inside the transaction the changes are visible.
    let r = db.execute("SELECT id FROM tx;").unwrap();
    assert_eq!(rows(&r).len(), 2); // ids 2 and 3
    db.execute("ROLLBACK;").unwrap();

    // After rollback we are back to the original single row, unchanged.
    let r = db.execute("SELECT id, v FROM tx ORDER BY id;").unwrap();
    let got = rows(&r);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], Value::Integer(1));
    assert_eq!(got[0][1], Value::Text("original".into()));
}

#[test]
fn rollback_undoes_create_table() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("BEGIN;").unwrap();
    db.execute("CREATE TABLE gone (x INTEGER);").unwrap();
    db.execute("INSERT INTO gone VALUES (1);").unwrap();
    db.execute("ROLLBACK;").unwrap();
    // The table should no longer exist.
    assert!(db.execute("SELECT * FROM gone;").is_err());
    assert!(db.table_names().is_empty());
}

#[test]
fn rollback_survives_reopen() {
    let t = TestDb::new();
    {
        let mut db = t.open();
        db.execute("CREATE TABLE r (id INTEGER);").unwrap();
        db.execute("INSERT INTO r VALUES (1);").unwrap();
        db.execute("BEGIN;").unwrap();
        db.execute("INSERT INTO r VALUES (2),(3),(4);").unwrap();
        db.execute("ROLLBACK;").unwrap();
    }
    let mut db = t.open();
    let r = db.execute("SELECT id FROM r;").unwrap();
    assert_eq!(rows(&r).len(), 1, "rolled-back rows must not be persisted");
}

#[test]
fn transaction_errors() {
    let t = TestDb::new();
    let mut db = t.open();
    assert!(db.execute("COMMIT;").is_err()); // nothing to commit
    assert!(db.execute("ROLLBACK;").is_err()); // nothing to roll back
    db.execute("BEGIN;").unwrap();
    assert!(db.execute("BEGIN;").is_err()); // already in a transaction
    db.execute("ROLLBACK;").unwrap();
}

/// Helper to read a single scalar from the final SELECT's first row/column.
fn scalar(results: &[QueryResult]) -> Value {
    rows(results)[0][0].clone()
}

#[test]
fn aggregates_without_group_by() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE nums (v INTEGER);").unwrap();
    db.execute("INSERT INTO nums VALUES (10),(20),(30),(40);")
        .unwrap();

    assert_eq!(
        scalar(&db.execute("SELECT COUNT(*) FROM nums;").unwrap()),
        Value::Integer(4)
    );
    assert_eq!(
        scalar(&db.execute("SELECT SUM(v) FROM nums;").unwrap()),
        Value::Integer(100)
    );
    assert_eq!(
        scalar(&db.execute("SELECT MIN(v) FROM nums;").unwrap()),
        Value::Integer(10)
    );
    assert_eq!(
        scalar(&db.execute("SELECT MAX(v) FROM nums;").unwrap()),
        Value::Integer(40)
    );
    assert_eq!(
        scalar(&db.execute("SELECT AVG(v) FROM nums;").unwrap()),
        Value::Integer(25)
    );

    // Aggregate of an empty set: COUNT is 0, SUM is NULL.
    db.execute("DELETE FROM nums;").unwrap();
    assert_eq!(
        scalar(&db.execute("SELECT COUNT(*) FROM nums;").unwrap()),
        Value::Integer(0)
    );
    assert_eq!(
        scalar(&db.execute("SELECT SUM(v) FROM nums;").unwrap()),
        Value::Null
    );
}

#[test]
fn group_by_with_aggregate() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE sales (dept TEXT, amount INTEGER);")
        .unwrap();
    db.execute("INSERT INTO sales VALUES ('a', 100), ('b', 200), ('a', 50), ('b', 25), ('a', 10);")
        .unwrap();

    let r = db
        .execute("SELECT dept, SUM(amount) AS total FROM sales GROUP BY dept ORDER BY dept;")
        .unwrap();
    let got = rows(&r);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0][0], Value::Text("a".into()));
    assert_eq!(got[0][1], Value::Integer(160));
    assert_eq!(got[1][0], Value::Text("b".into()));
    assert_eq!(got[1][1], Value::Integer(225));

    // COUNT per group, ordered by the count descending.
    let r = db
        .execute("SELECT dept, COUNT(*) AS n FROM sales GROUP BY dept ORDER BY n DESC;")
        .unwrap();
    let got = rows(&r);
    assert_eq!(got[0][0], Value::Text("a".into()));
    assert_eq!(got[0][1], Value::Integer(3));
}

#[test]
fn aliases_in_output() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE x (a INTEGER);").unwrap();
    db.execute("INSERT INTO x VALUES (5);").unwrap();
    let r = db.execute("SELECT a + 1 AS next FROM x;").unwrap();
    if let QueryResult::Select { columns, rows } = &r[0] {
        assert_eq!(columns, &vec!["next".to_string()]);
        assert_eq!(rows[0][0], Value::Integer(6));
    } else {
        panic!("expected select");
    }
}

#[test]
fn inner_join() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE users (id INTEGER, name TEXT);")
        .unwrap();
    db.execute("CREATE TABLE orders (id INTEGER, user_id INTEGER, item TEXT);")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Linus');")
        .unwrap();
    db.execute("INSERT INTO orders VALUES (10, 1, 'book'), (11, 1, 'pen'), (12, 2, 'laptop');")
        .unwrap();

    let r = db
        .execute(
            "SELECT users.name, orders.item FROM users \
             INNER JOIN orders ON users.id = orders.user_id \
             ORDER BY orders.item;",
        )
        .unwrap();
    let got = rows(&r);
    assert_eq!(got.len(), 3);
    // Sorted by item: book, laptop, pen
    assert_eq!(got[0][0], Value::Text("Ada".into()));
    assert_eq!(got[0][1], Value::Text("book".into()));
    assert_eq!(got[1][0], Value::Text("Linus".into()));
    assert_eq!(got[1][1], Value::Text("laptop".into()));
    assert_eq!(got[2][0], Value::Text("Ada".into()));
    assert_eq!(got[2][1], Value::Text("pen".into()));
}

#[test]
fn join_with_aggregate_and_group_by() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE users (id INTEGER, name TEXT);")
        .unwrap();
    db.execute("CREATE TABLE orders (id INTEGER, user_id INTEGER);")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Linus');")
        .unwrap();
    db.execute("INSERT INTO orders VALUES (10,1),(11,1),(12,2),(13,1);")
        .unwrap();

    let r = db
        .execute(
            "SELECT users.name, COUNT(*) AS orders FROM users \
             JOIN orders ON users.id = orders.user_id \
             GROUP BY users.name ORDER BY orders DESC;",
        )
        .unwrap();
    let got = rows(&r);
    assert_eq!(got[0][0], Value::Text("Ada".into()));
    assert_eq!(got[0][1], Value::Integer(3));
    assert_eq!(got[1][0], Value::Text("Linus".into()));
    assert_eq!(got[1][1], Value::Integer(1));
}

#[test]
fn ambiguous_column_is_an_error() {
    let t = TestDb::new();
    let mut db = t.open();
    db.execute("CREATE TABLE a (id INTEGER);").unwrap();
    db.execute("CREATE TABLE b (id INTEGER);").unwrap();
    db.execute("INSERT INTO a VALUES (1);").unwrap();
    db.execute("INSERT INTO b VALUES (1);").unwrap();
    // `id` is ambiguous across the join; must be qualified.
    let err = db.execute("SELECT id FROM a JOIN b ON a.id = b.id;");
    assert!(err.is_err());
}
