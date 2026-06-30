# FerroDB

A small but **real** persistent SQL database engine, written from scratch in Rust —
no SQLite, no ORM, no third-party crates at all. Just bytes, disk pages, and a B+Tree.

It implements the full stack a database needs: a paged storage manager, an on-disk
B+Tree, a hand-written SQL tokenizer and parser, a query executor with **joins and
aggregation**, **ACID-style transactions** with rollback, and an interactive shell.

```
$ ferrodb shop.db
FerroDB 0.1.0  —  a SQL engine from scratch in Rust
database: shop.db
Type SQL terminated by ';'. Meta: .tables  .help  .exit

ferro> SELECT users.name, COUNT(*) AS num_orders, SUM(orders.total) AS revenue
       FROM users JOIN orders ON users.id = orders.user_id
       GROUP BY users.name ORDER BY revenue DESC;
+-------+------------+---------+
| name  | num_orders | revenue |
+-------+------------+---------+
| Ada   | 3          | 140     |
| Grace | 1          | 90      |
| Linus | 1          | 30      |
+-------+------------+---------+
3 row(s)
```

## Highlights

- **Persistent storage** — a single file laid out as 4 KiB pages; data survives restarts.
- **On-disk B+Tree** — one tree per table, with leaf and internal node splitting,
  search routing, and a leaf chain for fast scans.
- **Hand-written SQL frontend** — tokenizer + recursive-descent parser with a Pratt
  expression parser (correct operator precedence).
- **Real query engine** — `WHERE`, `ORDER BY`, `LIMIT`, expressions, `INNER JOIN`,
  aggregates (`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`), and `GROUP BY`.
- **Transactions** — `BEGIN` / `COMMIT` / `ROLLBACK` backed by a pager undo log; a
  rolled-back transaction leaves the database (and disk) exactly as it was.
- **Zero dependencies** — the entire engine is the Rust standard library and nothing else.

## Architecture

FerroDB is built in clean layers, each in its own module:

| Layer | Module | Responsibility |
|-------|--------|----------------|
| Storage manager | `pager.rs` | The only code that touches the file. Fixed-size 4 KiB pages addressed by id, an in-memory page cache, a free-list for recycling pages, a meta/header page (page 0), and a transactional **undo log**. |
| Index / storage | `btree.rs` | A persistent **B+Tree** mapping `u64` keys to variable-length values. Handles leaf and internal node splits, routing, range scans via a leaf chain, and deletes. |
| Values | `value.rs` | SQL values (`NULL`, `INTEGER`, `TEXT`) and a compact, self-describing row encoding stored in B+Tree cells. |
| Catalog | `catalog.rs` | Table schemas, root pages, and rowid counters, persisted in the catalog page. |
| SQL frontend | `sql/` | A hand-written tokenizer (`token.rs`), an AST (`ast.rs`), and a recursive-descent parser with a **Pratt expression parser** (`parser.rs`). |
| Engine | `engine.rs` | Executes parsed statements: transaction control, joins, filtering, grouping/aggregation, ordering, projection, and reads/writes against the B+Trees. |
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

### How a SELECT is executed

```
FROM + JOINs   ->  nested-loop join produces a flat (schema, rows) set
WHERE          ->  rows filtered by evaluating the predicate
GROUP BY / agg ->  rows partitioned into groups; aggregates folded per group
                   (or row-wise projection when there is no aggregation)
ORDER BY       ->  output rows sorted by an output column
LIMIT          ->  output truncated
```

### How transactions work

`BEGIN` snapshots the catalog and switches the pager into transaction mode. From then
on, the first time any page is modified the pager records its **pre-image** in an undo
log, and newly allocated pages are tagged. Nothing is written to disk yet.

- `COMMIT` flushes every dirty page durably and clears the undo log.
- `ROLLBACK` restores every page from its pre-image, drops pages allocated during the
  transaction, rewinds the page count / free-list, and restores the catalog snapshot —
  so the database returns to exactly its pre-transaction state (verified to survive a
  reopen). Outside an explicit transaction, each statement batch auto-commits.

## Supported SQL

```sql
-- DDL
CREATE TABLE t (a INTEGER, b TEXT);
DROP TABLE t;

-- DML
INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y');
INSERT INTO t VALUES (3, 'z');
UPDATE t SET b = 'updated', a = a + 100 WHERE a = 1;
DELETE FROM t WHERE a = 2;

-- Queries
SELECT * FROM t;
SELECT a, b FROM t WHERE a > 1 AND b != 'x' ORDER BY a DESC LIMIT 10;
SELECT a + 1 AS next FROM t;

-- Joins (qualified column names)
SELECT users.name, orders.item
FROM users
JOIN orders ON users.id = orders.user_id;

-- Aggregates and grouping
SELECT COUNT(*), SUM(amount), AVG(amount), MIN(amount), MAX(amount) FROM sales;
SELECT dept, SUM(amount) AS total FROM sales GROUP BY dept ORDER BY total DESC;

-- Transactions
BEGIN;
  INSERT INTO t VALUES (9, 'tmp');
ROLLBACK;          -- the row above never happened
BEGIN;
  UPDATE t SET b = 'kept' WHERE a = 3;
COMMIT;            -- durable
```

- **Types:** `INTEGER` (64-bit signed) and `TEXT`, plus `NULL`.
- **Expressions:** arithmetic (`+ - * /`), comparisons (`= != <> < <= > >=`),
  logical `AND` / `OR` / `NOT`, parentheses, and unary minus, all with correct
  precedence via a Pratt parser.
- **Column references** may be qualified (`table.column`); ambiguous unqualified
  references across a join are rejected.
- **Aggregates:** `COUNT(*)`, `COUNT(expr)`, `SUM`, `AVG`, `MIN`, `MAX`, with or without
  `GROUP BY`.
- **NULL semantics:** comparisons involving `NULL` evaluate to `NULL` (never true),
  matching SQL three-valued logic for filtering. Division by zero yields `NULL`.
  Aggregates skip `NULL` inputs; `SUM`/`AVG`/`MIN`/`MAX` of an empty set return `NULL`.

## Building and running

```bash
cargo build --release          # build
cargo test                     # run the full unit + integration test suite
./target/release/ferrodb db.db # start the shell on a database file
```

You can also pipe SQL straight in:

```bash
printf "CREATE TABLE t (n INTEGER);\nINSERT INTO t VALUES (1),(2),(3);\nSELECT SUM(n) FROM t;\n" \
  | ./target/release/ferrodb demo.db
```

Shell meta-commands: `.tables` (list tables), `.help`, `.exit`.

## Tests

The suite exercises the engine end-to-end and the storage primitives directly:

- **Unit tests** (`src/`): row encode/decode round-trips, B+Tree insert/get/delete,
  sorted scans after thousands of node splits, and reopening a tree from disk.
- **Integration tests** (`tests/sql.rs`): CRUD round-trips, `WHERE`/`ORDER BY`/`LIMIT`,
  `NULL` handling, arithmetic precedence, error reporting, persistence across reopen,
  a 2,000-row B+Tree split stress test, transaction commit/rollback (including
  rollback of `CREATE TABLE` and rollback durability across reopen), aggregates,
  `GROUP BY`, aliases, `INNER JOIN`, and ambiguous-column detection.

```bash
cargo test
```

All tests pass and `cargo clippy --all-targets` is warning-free.

## Deliberate simplifications

This is a from-scratch, teaching-grade engine, not a production database. Notably:
single-threaded with no concurrency control; durability is at the `COMMIT`/flush
boundary (no write-ahead log, so a crash mid-flush is not protected); transaction
isolation is single-connection; B+Tree deletes don't merge/rebalance nodes; the catalog
must fit in one page; `DROP TABLE` doesn't reclaim the table's pages; only `INNER JOIN`
of two-plus tables via nested loops; `AVG` uses integer division; and `HAVING` and
subqueries are not implemented. Each of these is a natural next step rather than a
design flaw.

## License

MIT
