//! FerroDB interactive shell.
//!
//! Usage:
//!   ferrodb [DB_PATH]      # defaults to ./ferro.db
//!
//! Reads SQL from stdin. Statements are accumulated until a `;` that lies
//! outside a string literal, then executed. Lines beginning with `.` are
//! meta-commands:
//!   .tables    list tables
//!   .help      show help
//!   .exit      quit

use std::io::{self, BufRead, Write};

use ferrodb::engine::{Database, QueryResult};
use ferrodb::value::Value;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ferro.db".to_string());

    let mut db = match Database::open(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("failed to open database '{path}': {e}");
            std::process::exit(1);
        }
    };

    let stdin = io::stdin();
    let interactive = atty();

    if interactive {
        println!("FerroDB 0.1.0  —  a SQL engine from scratch in Rust");
        println!("database: {path}");
        println!("Type SQL terminated by ';'. Meta: .tables  .help  .exit\n");
    }

    let mut buffer = String::new();
    print_prompt(interactive, buffer.is_empty());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();

        // Meta-commands only when not in the middle of a statement.
        if buffer.trim().is_empty() && trimmed.starts_with('.') {
            match trimmed {
                ".exit" | ".quit" => break,
                ".help" => print_help(),
                ".tables" => {
                    let names = db.table_names();
                    if names.is_empty() {
                        println!("(no tables)");
                    } else {
                        println!("{}", names.join("\n"));
                    }
                }
                other => println!("unknown command: {other}"),
            }
            print_prompt(interactive, true);
            continue;
        }

        buffer.push_str(&line);
        buffer.push('\n');

        // Execute up to the last statement terminator that lies *outside* a
        // string literal, keeping any trailing partial statement buffered.
        if let Some(idx) = last_terminator(&buffer) {
            let complete = buffer[..idx].to_string();
            let remainder = buffer[idx..].to_string();
            run(&mut db, &complete);
            buffer = remainder;
        }
        print_prompt(interactive, buffer.trim().is_empty());
    }

    // Execute any trailing statement without a final semicolon.
    if !buffer.trim().is_empty() {
        run(&mut db, &buffer);
    }
}

/// Find the byte index just past the last `;` that is not inside a string
/// literal. Returns `None` when the buffer holds no complete statement yet
/// (e.g. a semicolon that only appears inside an unterminated string).
///
/// Mirrors the lexer's string rules: single quotes delimit strings and a
/// doubled `''` is an escaped quote that stays inside the string.
fn last_terminator(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut in_string = false;
    let mut last = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if in_string => {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2; // escaped quote -> remain in string
                    continue;
                }
                in_string = false;
            }
            b'\'' => in_string = true,
            b';' if !in_string => last = Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    last
}

fn run(db: &mut Database, sql: &str) {
    match db.execute(sql) {
        Ok(results) => {
            for r in results {
                print_result(&r);
            }
        }
        Err(e) => eprintln!("Error: {e}"),
    }
}

fn print_result(r: &QueryResult) {
    match r {
        QueryResult::Created(name) => println!("Table '{name}' created."),
        QueryResult::Dropped(name) => println!("Table '{name}' dropped."),
        QueryResult::Inserted(n) => println!("{n} row(s) inserted."),
        QueryResult::Updated(n) => println!("{n} row(s) updated."),
        QueryResult::Deleted(n) => println!("{n} row(s) deleted."),
        QueryResult::Begin => println!("BEGIN"),
        QueryResult::Commit => println!("COMMIT"),
        QueryResult::Rollback => println!("ROLLBACK"),
        QueryResult::Select { columns, rows } => print_table(columns, rows),
    }
}

/// Render a result set as an aligned ASCII table.
fn print_table(columns: &[String], rows: &[Vec<Value>]) {
    let ncols = columns.len();
    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| row.iter().map(|v| v.display()).collect())
        .collect();
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            if i < ncols {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    let sep = || {
        let mut s = String::from("+");
        for w in &widths {
            s.push_str(&"-".repeat(w + 2));
            s.push('+');
        }
        s
    };

    println!("{}", sep());
    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!(" {:<width$} ", c, width = widths[i]))
        .collect();
    println!("|{}|", header.join("|"));
    println!("{}", sep());
    for row in &cells {
        let line: Vec<String> = (0..ncols)
            .map(|i| {
                let val = row.get(i).map(|s| s.as_str()).unwrap_or("");
                format!(" {:<width$} ", val, width = widths[i])
            })
            .collect();
        println!("|{}|", line.join("|"));
    }
    println!("{}", sep());
    println!("{} row(s)", rows.len());
}

fn print_help() {
    println!("Supported SQL:");
    println!("  CREATE TABLE t (a INTEGER, b TEXT);");
    println!("  DROP TABLE t;");
    println!("  INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y');");
    println!("  SELECT * FROM t WHERE a > 1 ORDER BY a DESC LIMIT 10;");
    println!("  UPDATE t SET b = 'z' WHERE a = 1;");
    println!("  DELETE FROM t WHERE a = 2;");
    println!("Meta-commands: .tables  .help  .exit");
}

fn print_prompt(interactive: bool, fresh: bool) {
    if interactive {
        print!("{}", if fresh { "ferro> " } else { "  ...> " });
        let _ = io::stdout().flush();
    }
}

/// Best-effort TTY detection without external crates.
fn atty() -> bool {
    libc_isatty(0)
}

#[cfg(unix)]
fn libc_isatty(fd: i32) -> bool {
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(fd) == 1 }
}

#[cfg(not(unix))]
fn libc_isatty(_fd: i32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::last_terminator;

    #[test]
    fn terminator_outside_string() {
        assert_eq!(last_terminator("SELECT 1;"), Some(9));
        assert_eq!(last_terminator("SELECT 1"), None);
    }

    #[test]
    fn ignores_semicolon_inside_string() {
        // The ';' is inside a string literal, so there is no real terminator.
        assert_eq!(last_terminator("INSERT INTO t VALUES ('a;b'"), None);
        // ...until the statement is actually terminated.
        let s = "INSERT INTO t VALUES ('a;b');";
        assert_eq!(last_terminator(s), Some(s.len()));
    }

    #[test]
    fn handles_escaped_quotes() {
        // 'it''s' is one string containing a quote; the ';' after closes it.
        let s = "INSERT INTO t VALUES ('it''s; fine');";
        assert_eq!(last_terminator(s), Some(s.len()));
    }

    #[test]
    fn returns_last_of_multiple() {
        assert_eq!(last_terminator("A; B; C"), Some(5));
    }
}
