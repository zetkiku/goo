# FerroDB

A small but **real** persistent SQL database engine, written from scratch in Rust —
no SQLite, no ORM, no serialization crates for storage. Just bytes, pages, and a
B+Tree on disk.

It implements the full stack a database needs: a paged storage manager, an on-disk
B+Tree, a hand-written SQL tokenizer and parser, a query executor, and an
interactive shell.

```
$ ferrodb mydata.db
FerroDB 0.1.0  —  a SQL engine from scratch in Rust
database: mydata.db
Type SQL terminated by ';'. Meta: .tables  .help  .exit

ferro> CREATE TABLE users (id INTEGER, name TEXT, age INTEGER);
Table 'users' created.
ferro> INSERT INTO users VALUES (1, 'Alice', 30), (2, 'Bob', 25);
2 row(s) inserted.
ferro> SELECT name, age FROM users WHERE age > 26 ORDER BY age DESC;
+-------+-----+
| name  | age |
+-------+-----+
| Alice | 30  |
+-------+-----+
1 row(s)
```

## Architecture

FerroDB is built in clean layers, each in its own module:

| Layer | Module | Responsibility |
|-------|--------|----------------|
| Storage manager | `pager.rs` | The only code that touches the file. Fixed-size 4 KiB pages addressed by id, an in-memory page cache, a free-list for recycling pages, and a meta/header page (page 0). |
| Index / storage | `btree.rs` | A persistent **B+Tree** mapping `u64` keys to variable-length values. Handles node splits (leaf and internal), routing, range scans via a leaf chain, and deletes. |
| Values | `value.rs` | SQL values (`NULL`, `INTEGER`, `TEXT`) and a compact, self-describing row encoding stored in B+Tree cells. |
| Catalog | `catalog.rs` | Table schemas, root pages, and rowid counters, persisted in the catalog page. |
| SQL frontend | `sql/` | A hand-written tokenizer (`token.rs`), an AST (`ast.rs`), and a recursive-descent parser with a **Pratt expression parser** (`parser.rs`). |
| Engine | `engine.rs` | Executes parsed statements: expression evaluation, filtering, ordering, projection, and reads/writes against the B+Trees. |
| Shell | `main.rs` | A REPL that reads SQL from stdin and renders results as aligned ASCII tables. |

### How data is laid out on disk

```
file = [ page 0 | page 1 | page 2 | ... ]   each page = 4096 bytes

page 0  -> meta:    magic "FRDB", version, page count, free-list head, catalog page id
page 1  -> catalog: serialized table schemas (name, columns, root page, next rowid)
page 2+ -> B+Tree nodes (one tree per table) and recycled free pages
```

Each table is one B+Tree keyed by an auto-incrementing `rowid`. Leaf nodes store the
encoded rows and are chained left-to-right so a full table scan is a single linked-list
walk. Internal nodes only route searches.

## Supported SQL

```sql
CREATE TABLE t (a INTEGER, b TEXT);
DROP TABLE t;

INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y');
INSERT INTO t VALUES (3, 'z');

SELECT * FROM t;
SELECT a, b FROM t WHERE a > 1 AND b != 'x' ORDER BY a DESC LIMIT 10;

UPDATE t SET b = 'updated', a = a + 100 WHERE a = 1;

DELETE FROM t WHERE a = 2;
```

- **Types:** `INTEGER` (64-bit) and `TEXT`, plus `NULL`.
- **Expressions:** arithmetic (`+ - * /`), comparisons (`= != < <= > >=`),
  logical `AND` / `OR` / `NOT`, parentheses, and unary minus, all with correct
  precedence via a Pratt parser.
- **NULL semantics:** comparisons involving `NULL` evaluate to `NULL` (never true),
  matching SQL three-valued-logic behavior for filtering. Division by zero yields `NULL`.

## Building and running

```bash
cargo build --release          # build
cargo test                     # run the unit + integration test suite
./target/release/ferrodb db.db # start the shell on a database file
```

You can also pipe SQL straight in:

```bash
printf "CREATE TABLE t (n INTEGER);\nINSERT INTO t VALUES (1),(2),(3);\nSELECT * FROM t;\n" \
  | ./target/release/ferrodb demo.db
```

## Tests

The suite exercises the engine end-to-end and the storage primitives directly:

- **Unit tests** (`src/`): row encode/decode round-trips, B+Tree insert/get/delete,
  sorted scans after thousands of node splits, and reopening a tree from disk.
- **Integration tests** (`tests/sql.rs`): CRUD round-trips, `WHERE`/`ORDER BY`/`LIMIT`,
  `NULL` handling, arithmetic precedence, error reporting, persistence across reopen,
  and a 2,000-row B+Tree split stress test.

```bash
cargo test
```

## Deliberate simplifications

This is a from-scratch teaching-grade engine, not a production database. Notably:
single-threaded with no concurrency control, no transactions/WAL (durability is at the
`flush` boundary), deletes don't merge/rebalance B+Tree nodes, the catalog must fit in
one page, and `DROP TABLE` doesn't reclaim the table's pages. Each of these is a natural
next step rather than a design flaw.

## License

MIT
