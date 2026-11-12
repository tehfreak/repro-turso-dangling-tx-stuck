use std::error::Error;
use std::path::Path;

use turso::transaction::DropBehavior;
use turso::{Builder, Connection, Database};

const ROUNDS: usize = 3;
const CONFLICT: &str = "INSERT OR ROLLBACK INTO t (id, v) VALUES (1, 'conflict')";

#[derive(Clone, Copy)]
enum Case {
    Rollback,
    OrRollback,
    DiskFull,
    RollbackCall,
    FinishCall,
    CommitOnDrop,
    Cleared,
    Plain,
}

impl Case {
    fn label(self) -> &'static str {
        match self {
            Case::Rollback => "rollback",
            Case::OrRollback => "or-rollback",
            Case::DiskFull => "disk-full",
            Case::RollbackCall => "rollback()",
            Case::FinishCall => "finish()",
            Case::CommitOnDrop => "commit-on-drop",
            Case::Cleared => "cleared",
            Case::Plain => "plain",
        }
    }
}

fn show<T>(result: turso::Result<T>) -> String {
    match result {
        Ok(_) => "ok".to_string(),
        Err(e) => e.to_string(),
    }
}

async fn scalar(conn: &Connection, sql: &str) -> turso::Result<i64> {
    let mut rows = conn.query(sql, ()).await?;
    let n = rows.next().await?.unwrap().get::<i64>(0)?;
    while rows.next().await?.is_some() {}
    Ok(n)
}

async fn open(dir: &Path, n: usize) -> turso::Result<Database> {
    let path = dir.join(format!("case-{n}.db"));
    let db = Builder::new_local(path.to_str().unwrap()).build().await?;
    db.connect()?
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", ())
        .await?;
    Ok(db)
}

async fn end(db: &Database, conn: &mut Connection, case: Case) -> turso::Result<String> {
    if let Case::DiskFull = case {
        let pages = scalar(conn, "PRAGMA page_count").await?;
        scalar(conn, &format!("PRAGMA max_page_count = {pages}")).await?;
    }
    let mut tx = conn.transaction().await?;
    tx.execute("INSERT INTO t (id, v) VALUES (1, 'first')", ())
        .await?;
    let last = match case {
        Case::Rollback => show(tx.execute("ROLLBACK", ()).await),
        Case::DiskFull => show(
            tx.execute("INSERT INTO t (v) VALUES (zeroblob(1048576))", ())
                .await,
        ),
        Case::Plain => "ok".to_string(),
        Case::RollbackCall => {
            tx.execute(CONFLICT, ()).await.unwrap_err();
            return Ok(show(tx.rollback().await));
        }
        Case::FinishCall => {
            tx.execute(CONFLICT, ()).await.unwrap_err();
            return Ok(show(tx.finish().await));
        }
        Case::CommitOnDrop => {
            tx.set_drop_behavior(DropBehavior::Commit);
            show(tx.execute(CONFLICT, ()).await)
        }
        Case::Cleared => {
            let last = show(tx.execute(CONFLICT, ()).await);
            if tx.is_autocommit()? {
                tx.set_drop_behavior(DropBehavior::Ignore);
            }
            last
        }
        Case::OrRollback => show(tx.execute(CONFLICT, ()).await),
    };
    drop(tx);
    if let Case::DiskFull = case {
        scalar(&db.connect()?, "PRAGMA max_page_count = 1073741823").await?;
    }
    Ok(last)
}

async fn calls(conn: &mut Connection) -> (usize, Option<String>) {
    let mut ok = 0;
    let mut first = None;
    for _ in 0..ROUNDS {
        let results = [
            scalar(conn, "SELECT 1").await.map(|_| ()),
            conn.execute("INSERT INTO t (v) VALUES ('execute')", ())
                .await
                .map(|_| ()),
            conn.execute_batch("INSERT INTO t (v) VALUES ('execute_batch')")
                .await,
            match conn.transaction().await {
                Ok(tx) => tx.commit().await,
                Err(e) => Err(e),
            },
        ];
        for result in results {
            match result {
                Ok(()) => ok += 1,
                Err(e) => {
                    first.get_or_insert(e.to_string());
                }
            }
        }
    }
    (ok, first)
}

fn verdict(works: bool) -> &'static str {
    if works { "WORKS" } else { "BROKEN" }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::temp_dir().join(format!("repro-turso-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let total = ROUNDS * 4;
    let cases = [
        Case::Rollback,
        Case::OrRollback,
        Case::DiskFull,
        Case::RollbackCall,
        Case::FinishCall,
        Case::CommitOnDrop,
        Case::Cleared,
        Case::Plain,
    ];
    let mut ended = Vec::new();
    let mut results = Vec::new();
    for (n, case) in cases.into_iter().enumerate() {
        let db = open(&dir, n).await?;
        let mut conn = db.connect()?;
        let last = end(&db, &mut conn, case).await?;
        ended.push((case, last, conn.is_autocommit()?));
        results.push((case, calls(&mut conn).await));
    }

    println!("is_autocommit() once the Transaction is gone, and its last call");
    for (case, last, autocommit) in &ended {
        println!("  {:<16}{autocommit:<7}{last}", case.label());
    }

    println!("calls that worked out of {total}, and the first error");
    for (case, (ok, first)) in &results {
        let ok = format!("{ok} / {total}");
        let first = first.as_deref().unwrap_or("");
        let line = format!("  {:<16}{ok:<9}{first}", case.label());
        println!("{}", line.trim_end());
    }

    println!();
    for (case, (ok, _)) in &results {
        println!("VERDICT {}: {}", case.label(), verdict(*ok == total));
    }

    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
