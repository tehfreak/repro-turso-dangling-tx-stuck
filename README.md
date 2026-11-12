# repro-turso-dangling-tx-stuck

Repro for a transaction bug in the `turso` Rust crate. When a `Transaction` is dropped without
`commit` or `rollback`, the crate stores a ROLLBACK and runs it on the next call on the connection.
If the engine already ended the transaction, that ROLLBACK fails and is never cleared, so every
later call on the connection fails with it.

## Status

Reported upstream as https://github.com/tursodatabase/turso/issues/11759, unassigned. No issue or
PR covered it when it was filed. The closest ones are about other things:
https://github.com/tursodatabase/turso/issues/11746 (prepared statements skip the same stored
ROLLBACK) and https://github.com/tursodatabase/turso/issues/11217 (the Go driver keeps writing
after an engine rollback).

Checked with turso 0.8.3, the latest release, and 0.9.0-pre.5, the latest pre-release. main at
`0bec181e` has the same bindings code.

## What happens

```rust
let mut conn = db.connect()?;
{
    let tx = conn.transaction().await?;
    tx.execute("ROLLBACK", ()).await?;
}
conn.query("SELECT 1", ()).await?;
```

The query fails with `cannot rollback - no transaction is active`. So does every later `query`,
`execute`, `execute_batch` and `transaction` on that connection.

The ROLLBACK statement is only the shortest way in. In practice the engine ends the transaction
itself, on an `INSERT OR ROLLBACK` conflict or when the database is full, and the `?` that returns
the error drops the Transaction. A server that keeps one long-lived connection fails every request
after that.

`src/main.rs` runs eight cases, each on a new database file:

```
rollback        tx.execute("ROLLBACK"), then the Transaction is dropped
or-rollback     INSERT OR ROLLBACK hits a UNIQUE conflict, then the Transaction is dropped
disk-full       PRAGMA max_page_count is set to the current size, then a big INSERT fails
rollback()      the or-rollback conflict, then tx.rollback()
finish()        the or-rollback conflict, then tx.finish()
commit-on-drop  the or-rollback conflict with DropBehavior::Commit
cleared         the or-rollback conflict, then DropBehavior::Ignore if is_autocommit()
plain           an INSERT, then the Transaction is dropped while it is still open
```

After each case it makes 12 calls on the same connection: `query`, `execute`, `execute_batch` and
`transaction`, three rounds.

Output on 0.8.3. 0.9.0-pre.5 prints the same without the `Transaction error: ` prefix.

```
is_autocommit() once the Transaction is gone, and its last call
  rollback        true   ok
  or-rollback     true   UNIQUE constraint failed: t.id
  disk-full       true   database or disk is full
  rollback()      true   Transaction error: cannot rollback - no transaction is active
  finish()        true   ok
  commit-on-drop  true   UNIQUE constraint failed: t.id
  cleared         true   UNIQUE constraint failed: t.id
  plain           false  ok
calls that worked out of 12, and the first error
  rollback        0 / 12   Transaction error: cannot rollback - no transaction is active
  or-rollback     0 / 12   Transaction error: cannot rollback - no transaction is active
  disk-full       0 / 12   Transaction error: cannot rollback - no transaction is active
  rollback()      0 / 12   Transaction error: cannot rollback - no transaction is active
  finish()        0 / 12   Transaction error: cannot rollback - no transaction is active
  commit-on-drop  0 / 12   Transaction error: cannot commit - no transaction is active
  cleared         12 / 12
  plain           12 / 12

VERDICT rollback: BROKEN
VERDICT or-rollback: BROKEN
VERDICT disk-full: BROKEN
VERDICT rollback(): BROKEN
VERDICT finish(): BROKEN
VERDICT commit-on-drop: BROKEN
VERDICT cleared: WORKS
VERDICT plain: WORKS
```

`is_autocommit()` is true in every broken case, so there is no transaction left to roll back.
`finish()` returns Ok and still leaves the connection broken. `cleared` is the control: the same
conflict, with the stored step cleared by hand. `plain` shows the stored ROLLBACK working when
there is a transaction to end.

## Run it

```
cargo run
```

For the pre-release:

```
sed -i 's/"=0.8.3"/"=0.9.0-pre.5"/' Cargo.toml
cargo run
```

## Why it's a bug

The docs say the rollback happens on the next access to the connection:

```
bindings/rust/src/transaction.rs:246
    /// The transaction defaults to rolling back on the next access to the connection
    /// if it is not finished when the transaction is dropped.
```

`Drop` can't run SQL, so it stores the `DropBehavior` in `Connection::dangling_tx`, and the next
call runs it. The flag is cleared only after that statement succeeds. Lines are from main, under
`bindings/rust/src/`:

```
connection.rs:104-107    maybe_handle_dangling_tx runs ROLLBACK with ?, then store(Ignore)
connection.rs:414-416    maybe_handle_dangling_tx_without_operation_guard, the same
connection.rs:125, 132   Connection::query, execute run it
connection.rs:148, 266   execute_batch, batch and transactional_batch run it
transaction.rs:289       Connection::transaction runs it
transaction.rs:184-185   rollback clears in_progress only after its ROLLBACK succeeds
transaction.rs:201-203   finish returns Ok in autocommit and leaves in_progress set
transaction.rs:231-234   Drop stores the DropBehavior while in_progress is set
```

When the engine already ended the transaction, the stored ROLLBACK fails, the `?` returns before
the store, and the next call tries again. `finish` checks `is_autocommit()` first. The stored step
doesn't. `dangling_tx` is `pub(crate)`, so once the Transaction is gone the caller can't clear it.

The stored step came in with https://github.com/tursodatabase/turso/pull/3750.

## Suggested fix

One commit with tests, on top of main:
https://github.com/tursodatabase/turso/compare/main...tehfreak:turso:dropped-tx-engine-rollback

In both handlers, clear the flag without running the step when the connection is already in
autocommit:

```rust
DropBehavior::Rollback | DropBehavior::Commit if self.is_autocommit()? => {
    self.dangling_tx.store(DropBehavior::Ignore, Ordering::SeqCst);
}
```

Clearing the flag before the step every time would be wrong. A ROLLBACK can also fail while the
transaction is still open, with `cannot rollback transaction - SQL statements in progress` while a
`RETURNING` statement has unread rows. Then the flag has to stay, so the next call rolls back. One
of the tests covers that. With the fix every line here prints WORKS.

The fix for https://github.com/tursodatabase/turso/issues/11746 changes the same two functions, and
the two merge with one small conflict. That fix alone makes prepared statements fail the same way,
since `Statement::query` and `Statement::execute` then run the stored step too.

Until then, check `is_autocommit()` before a Transaction goes away and call
`set_drop_behavior(DropBehavior::Ignore)` when it is true. That is what `cleared` does.

## CI

`.github/workflows/reproduce.yml` runs the repro on 0.8.3 and on 0.9.0-pre.5 and compares the
VERDICT lines with the list above. The job stays green while the bug is there and goes red when
any line changes, a fix included.
