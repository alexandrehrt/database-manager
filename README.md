# database-manager

A desktop database client written in Rust, modelled on DataGrip's feature set. It supports PostgreSQL, SQLite and Oracle and uses an [egui](https://github.com/emilk/egui) interface. It also has DBeaver-style foreign-key navigation in the results grid.

Work was tracked in the [v1](https://github.com/alexandrehrt/database-manager/issues/8) and [v2](https://github.com/alexandrehrt/database-manager/issues/24) epics. [docs/parity.md](docs/parity.md) lists which DataGrip features exist and which don't.

## Build and run

You need Rust 1.95. `rust-toolchain.toml` pins it, so rustup installs it automatically.

```sh
cargo run --release -p database-manager
```

## Install on macOS

```sh
scripts/bundle-macos.sh ~/Downloads
```

This builds `Database Manager.app`. Drag it into **Applications** and open it from there or from Spotlight. The app is
signed ad hoc, which is enough on the Mac that built it; sharing it with other Macs needs an Apple Developer ID signature
and notarization (#56).

CI also builds the program for Windows (`database-manager.exe`), macOS and Linux on every push to `main`; download it
from the run's **Artifacts** on the Actions tab.

## Try it with the sample database

`fixtures/seed.sql` loads into both engines. It has customers, orders (some with a NULL customer), order items, products, and shipments that reference order items through a composite foreign key.

```sh
# SQLite
sqlite3 seed.db < fixtures/seed.sql

# PostgreSQL in Docker
docker run -d --name dbm-pg -e POSTGRES_PASSWORD=pw -p 54329:5432 postgres:16
docker exec -i dbm-pg psql -U postgres < fixtures/seed.sql
```

Then use **File → New data source…** to add them. For the SQLite database, pick `seed.db`. For Postgres, use host `localhost`, port `54329`, user `postgres` and password `pw`.

## Oracle

Oracle connections need **Oracle Instant Client** (the free Basic package) from
[oracle.com](https://www.oracle.com/database/technologies/instant-client/downloads.html). Either put it on the system
library path, or set its folder in the connection's **Instant Client** field. It is loaded once per run, so restart
the app after changing that folder.

To try it locally, run Oracle Free in Docker and load the Oracle version of the sample data:

```sh
docker run -d --name dbm-oracle -p 1521:1521 -e ORACLE_PASSWORD=pw -e APP_USER=dbm -e APP_USER_PASSWORD=pw gvenzl/oracle-free:23-slim
docker exec -i dbm-oracle sqlplus -s dbm/pw@FREEPDB1 < fixtures/seed-oracle.sql
```

Then add a connection with host `localhost`, port `1521`, service name `FREEPDB1`, user `dbm`, password `pw`.

Oracle doesn't report which table a result column comes from, so FK links and grid editing work for single-table
queries (`SELECT * | columns FROM table [WHERE …] [ORDER BY …]`), which covers table tabs and FK navigation. In a
console, PL/SQL blocks end with a line holding only `/`, as in SQL*Plus.

## Using it

The window follows a fixed layout. The top bar holds the data source picker, the schema breadcrumb, a **Content / Structure / SQL** switch for table tabs, **Go to table** (Cmd+K) and the **Auto-commit** menu. The sidebar lists the current source's tables and views, then your connections. Tabs run across the top of the main area.


- **Explorer:** expand a data source to connect. Schemas, tables, columns, keys and indexes load as you expand them. Columns show PK/FK badges, and hovering an FK shows its target.
- **Consoles:** double-click a data source, or right-click it → New console. **Cmd+Enter** runs the statement under the cursor, or the selection. **Cmd+Shift+Enter** runs the whole script. Each statement gets its own results tab, and execution stops at the first error.
- **Table data:** click a table in the sidebar, or press **Cmd+K** and type part of its name. Filters and the sort order appear as chips above the data: **+ Filter** adds a condition, and clicking a column header sorts on the server. **Structure** shows the table's columns, indexes and keys, and **SQL** shows its DDL. Right-click a table → **Show DDL** for its definition.
- **Explorer filter:** type in the box at the top of the explorer to show only matching tables.
- **Autocomplete:** suggestions appear as you type, or with **Ctrl+Space**. After `alias.` you get that table's columns.
- **Transactions:** each console has its own connection. Switch `Tx:` to **Manual** to keep a transaction open until you press **Commit** or **Rollback**. A yellow badge shows when one is open.
- **Results:**
  - Click a column header to sort.
  - Right-click a cell to copy its value.
  - **Load more** fetches the next 500 rows.
  - Results can be exported to CSV or JSON.
- **Row panel:** selecting a row opens it on the right, with a field per column and a **Referenced by** list.
- **Editing data:** when a result comes from one table and includes its primary key, double-click a cell or use the row panel to edit it. Right-click a cell for NULL, revert or delete. **+ Row** adds a row. Changes are tinted until you press **Save** (Cmd+S) in the bottom bar, which applies them all or none. **View SQL** shows the statements first.
- **Copying rows:** click, Shift+click or Cmd+click to select rows. Cmd+C copies them as TSV for spreadsheets. Right-click → Copy rows as CSV, JSON, Markdown or INSERT statements.
- **Foreign-key navigation:** a value in a foreign-key column is a link. Clicking it opens the referenced row in a new results tab. Back/Forward and the breadcrumb take you along the path. Navigation works for any query whose columns come straight from a table, not just for "open table". Right-click a row → **Referencing rows** goes the other way, to the rows that point at it.
- **History:** the console's History menu lists earlier statements for that data source.

| Shortcut | Action |
| --- | --- |
| Cmd+Enter | Run statement at cursor / selection |
| Cmd+Shift+Enter | Run all |
| Cmd+N | New data source |
| Cmd+T | New console |
| Cmd+W | Close tab |
| Cmd+K (or Cmd+O) | Go to table |
| Cmd+S | Save pending grid edits |
| Ctrl+Space | Show completions |
| Cmd+A / Cmd+C (grid) | Select all rows / copy selected rows |

## Where things are stored

- Data sources: `~/Library/Application Support/database-manager/connections.toml`. On Linux it's `~/.config/database-manager/`.
- Query history: `history.json` in the same folder.
- Passwords: the macOS Keychain, or the platform secret store elsewhere, under the service `database-manager`. If you untick "Save password", the password is kept only until you quit.

## Layout

| Crate | Purpose |
| --- | --- |
| `crates/core` (`dbm-core`) | Shared model and the async `Connection` trait. Also the engine-independent logic: statement splitting, statement at cursor, FK navigation, export, DDL. |
| `crates/driver-sqlite` | SQLite driver (rusqlite, bundled) |
| `crates/driver-postgres` | PostgreSQL driver (tokio-postgres, native-tls) |
| `crates/app` | The egui application |

There is no automated test suite yet. Each change was checked by building it, running clippy and doing a walkthrough against the sample databases. The pull requests record those walkthroughs.

## Fonts and icons

The interface bundles [Inter](https://rsms.me/inter/) and [JetBrains Mono](https://www.jetbrains.com/lp/mono/), both under the SIL Open Font License (licences in `crates/app/assets/fonts`). Both are subset to drop their private-use glyphs, which would otherwise hide the [Phosphor](https://phosphoricons.com/) icons that share that Unicode range.
