# DataGrip parity checklist

What the app covers compared with DataGrip, as of v2, limited to PostgreSQL and SQLite. Status:

- **Done:** works, and was checked by hand against `fixtures/seed.sql`.
- **Partial:** works, with the limitation noted.
- **Not yet:** missing.

## Data sources

| Feature | Status | Notes |
| --- | --- | --- |
| Add / edit / delete data sources | Done | |
| Test connection | Done | |
| PostgreSQL connection | Done | host, port, database, user, password |
| PostgreSQL SSL | Partial | `disable` / `prefer` / `require` follow libpq: traffic is encrypted, but the certificate isn't verified. No `verify-ca` / `verify-full` or client certificates. |
| SQLite file connection | Done | Creates the file if it doesn't exist. Foreign keys are enforced. |
| Password storage | Done | Keychain, or session-only |
| SSH tunnel, proxy, connection URL editing | Not yet | |
| Other engines (MySQL, SQL Server, Oracle, …) | Not yet | Each would be another `Connection` implementation. |

## Database explorer

| Feature | Status | Notes |
| --- | --- | --- |
| Schemas → tables / views → columns | Done | Loads lazily, as nodes are expanded. Postgres materialized views are listed under views. |
| Column types, nullability, PK/FK markers | Done | Hovering an FK shows its target. |
| Indexes, foreign keys | Done | |
| Refresh, connect, disconnect | Done | |
| Sequences, functions, triggers, roles | Not yet | SQLite triggers do appear in Show DDL. |
| Search / filter in the tree | Done | Filter box narrows connected sources to matching tables and loads unloaded schemas; Cmd+O "Go to table" fuzzy picker opens a table's data. |

## Query console

| Feature | Status | Notes |
| --- | --- | --- |
| Run statement at cursor (Cmd+Enter) | Done | |
| Run selection | Done | |
| Run script (Cmd+Shift+Enter) | Done | The splitter understands quotes, comments, `$tag$` bodies, `BEGIN ATOMIC` bodies and SQLite trigger bodies. |
| Stop on first error | Done | DataGrip lets you choose; here it always stops. |
| Cancel running statement | Done | Uses the Postgres cancel request and `sqlite3_interrupt`. |
| Syntax highlighting | Done | Keywords, strings, numbers, comments |
| Transactions | Done | Per-console Auto / Manual mode, Commit / Rollback buttons, an open-transaction badge, and a separate connection per console. Closing a console, disconnecting or quitting with an open transaction asks first. |
| Query history | Done | Per data source, saved between runs |
| Code completion | Done | Keywords, schemas, tables and columns; resolves aliases from FROM/JOIN. |
| Inspections, formatting | Not yet | |
| Parameters / user variables | Not yet | |
| EXPLAIN plan view | Not yet | EXPLAIN output appears as a normal result. |

## Results

| Feature | Status | Notes |
| --- | --- | --- |
| Grid with virtual scrolling | Done | |
| Paging: 500 rows plus Load more | Partial | Load more re-runs the query with a larger limit, and only for read-only statements. Writing statements (`… RETURNING`) are always read in full. |
| NULL display, client-side sort | Done | |
| Multiple result tabs per run | Done | |
| Rows affected, timing, error message / detail / SQLSTATE | Done | |
| Copy cell value | Done | |
| Row selection, copy rows as TSV / CSV / JSON / Markdown / INSERT | Done | Cmd+C copies the selection as TSV. INSERTs use dialect-correct literals and run in either engine. |
| Export CSV / JSON | Done | Exports the rows currently fetched. |
| Postgres value decoding | Partial | Numeric, money, temporal values (including infinity), interval, inet/cidr, json(b), uuid, bytea, point, arrays, enums, domains, citext. Other types (ranges, geometric types other than point, tsvector, …) appear as `<type> \x…`. |
| Edit cells / insert / delete rows in the grid | Done | For results whose columns come from one table with its primary key present. Changes are submitted as one batch, which runs in its own transaction or joins the console's open one. SQLite tables without a primary key are read-only. |
| Filter / ORDER BY bar on table data | Done | Header clicks on table data also sort server-side. The last filter for each table is remembered for the session. |

## Navigation

| Feature | Status | Notes |
| --- | --- | --- |
| Foreign-key navigation from a cell (DBeaver-style) | Done | Works for any query whose result columns come straight from a table. Composite keys are supported. There's no link if a key column is missing from the result or NULL. |
| Back / Forward, breadcrumb | Done | |
| Navigate to referencing rows (reverse FK) | Done | Right-click a row → Referencing rows. |

## Other

| Feature | Status | Notes |
| --- | --- | --- |
| Show DDL | Done | SQLite shows the original DDL. Postgres DDL is rebuilt from the catalog, and identity/generated columns, CHECK constraints and FK actions are left out. |
| Keyboard shortcuts | Partial | Cmd+Enter, Cmd+Shift+Enter, Cmd+N, Cmd+T, Cmd+W, Cmd+O, Ctrl+Space, Cmd+A / Cmd+C in the grid |
| ER diagrams, schema compare, data import, user management | Not yet | |
