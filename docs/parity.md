# DataGrip parity checklist (v1)

What v1 covers compared with DataGrip, limited to PostgreSQL and SQLite. Status:

- **Done:** works, and was checked by hand against `fixtures/seed.sql`.
- **Partial:** works, with the limitation noted.
- **Not in v1:** missing.

## Data sources

| Feature | Status | Notes |
| --- | --- | --- |
| Add / edit / delete data sources | Done | |
| Test connection | Done | |
| PostgreSQL connection | Done | host, port, database, user, password |
| PostgreSQL SSL | Partial | `disable` / `prefer` / `require` follow libpq: traffic is encrypted, but the certificate isn't verified. No `verify-ca` / `verify-full` or client certificates. |
| SQLite file connection | Done | Creates the file if it doesn't exist. Foreign keys are enforced. |
| Password storage | Done | Keychain, or session-only |
| SSH tunnel, proxy, connection URL editing | Not in v1 | |
| Other engines (MySQL, SQL Server, Oracle, …) | Not in v1 | Each would be another `Connection` implementation. |

## Database explorer

| Feature | Status | Notes |
| --- | --- | --- |
| Schemas → tables / views → columns | Done | Loads lazily, as nodes are expanded. Postgres materialized views are listed under views. |
| Column types, nullability, PK/FK markers | Done | Hovering an FK shows its target. |
| Indexes, foreign keys | Done | |
| Refresh, connect, disconnect | Done | |
| Sequences, functions, triggers, roles | Not in v1 | SQLite triggers do appear in Show DDL. |
| Search / filter in the tree | Not in v1 | |

## Query console

| Feature | Status | Notes |
| --- | --- | --- |
| Run statement at cursor (Cmd+Enter) | Done | |
| Run selection | Done | |
| Run script (Cmd+Shift+Enter) | Done | The splitter understands quotes, comments, `$tag$` bodies, `BEGIN ATOMIC` bodies and SQLite trigger bodies. |
| Stop on first error | Done | DataGrip lets you choose; here it always stops. |
| Cancel running statement | Done | Uses the Postgres cancel request and `sqlite3_interrupt`. |
| Syntax highlighting | Done | Keywords, strings, numbers, comments |
| Transactions | Partial | Autocommit, with manual BEGIN/COMMIT/ROLLBACK. There's no transaction-mode switch. On Postgres, reads inside an open transaction fetch every row instead of a 500-row page. |
| Query history | Done | Per data source, saved between runs |
| Code completion, inspections, formatting | Not in v1 | |
| Parameters / user variables | Not in v1 | |
| EXPLAIN plan view | Not in v1 | EXPLAIN output appears as a normal result. |

## Results

| Feature | Status | Notes |
| --- | --- | --- |
| Grid with virtual scrolling | Done | |
| Paging: 500 rows plus Load more | Partial | Load more re-runs the query with a larger limit, and only for read-only statements. Writing statements (`… RETURNING`) are always read in full. |
| NULL display, client-side sort | Done | |
| Multiple result tabs per run | Done | |
| Rows affected, timing, error message / detail / SQLSTATE | Done | |
| Copy cell value | Done | |
| Export CSV / JSON | Done | Exports the rows currently fetched. |
| Postgres value decoding | Partial | Numeric, money, temporal values (including infinity), interval, inet/cidr, json(b), uuid, bytea, point, arrays, enums, domains, citext. Other types (ranges, geometric types other than point, tsvector, …) appear as `<type> \x…`. |
| Edit cells / insert / delete rows in the grid | Not in v1 | |
| Filter / ORDER BY bar on table data | Not in v1 | |

## Navigation

| Feature | Status | Notes |
| --- | --- | --- |
| Foreign-key navigation from a cell (DBeaver-style) | Done | Works for any query whose result columns come straight from a table. Composite keys are supported. There's no link if a key column is missing from the result or NULL. |
| Back / Forward, breadcrumb | Done | |
| Navigate to referencing rows (reverse FK) | Not in v1 | |

## Other

| Feature | Status | Notes |
| --- | --- | --- |
| Show DDL | Done | SQLite shows the original DDL. Postgres DDL is rebuilt from the catalog, and identity/generated columns, CHECK constraints and FK actions are left out. |
| Keyboard shortcuts | Partial | Cmd+Enter, Cmd+Shift+Enter, Cmd+N, Cmd+T, Cmd+W |
| ER diagrams, schema compare, data import, user management | Not in v1 | |
