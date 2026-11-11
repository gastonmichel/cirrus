<img src="assets/icon.svg" width="64" height="64" alt="">

# Cirrus

A read-only PostgreSQL viewer for Google Cloud SQL, built with [GPUI](https://gpui.rs) and GPUI Kit. Think DBeaver, but you sign in with Google and it connects through a native Rust Cloud SQL connector — no passwords, no `cloud-sql-proxy` process.

Created in [T3 Code](https://t3.codes).

## Features

- **Sign in with Google** — reuses your `gcloud` CLI credentials (`gcloud auth login` from the app if needed).
- **Navigator** — all accessible projects → Cloud SQL instances → databases → schemas → tables/views, loaded lazily, with a filter box.
- **Table view** — virtualized data grid with infinite scroll, server-side sort (click headers), a `WHERE` filter bar, row count, plus Columns / Constraints / Indexes / DDL tabs.
- **ER diagram** — per schema: auto-layout, foreign-key edges, drag tables, pan, ⌘-scroll / pinch zoom, fit, detail panel; double-click opens the table.
- **SQL console** — read-only, ⌘↵ runs the selection (or everything), results capped at 1,000 rows.
- **Copy / export** — ⌘C copies the selected cell/row/column, "Copy" puts all loaded rows on the clipboard as TSV, "Export CSV" saves them to a file.

## Requirements

- macOS, Rust (stable)
- Google Cloud SDK (`gcloud`) installed and logged in
- PostgreSQL Cloud SQL instances with **IAM database authentication** enabled (`cloudsql.iam_authentication=on`), and your Google account added as an IAM database user with `CONNECT` on the databases you want to browse
- The instance must have a public IP reachable from your machine (private IP works only from inside the VPC)

## Install

```sh
cargo xtask install   # builds "Cirrus.app", copies it to /Applications and opens it
cargo xtask bundle    # only build target/release/Cirrus.app
```

In T3 Code, use the **Install Cirrus** script from `t3.json`.

## Run (dev)

```sh
cargo run --release -p cirrus
```

If the build fails with "You have not agreed to the Xcode license agreements", either run `sudo xcodebuild -license accept` once, or build with the Command Line Tools: `DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo run ...`. Full Xcode is not required — GPUI Kit compiles Metal shaders at runtime.

Smoke-test the backend without the UI:

```sh
cargo run -p cirrus-core --example probe                              # list projects + instances
cargo run -p cirrus-core --example probe -- PROJECT INSTANCE DATABASE # connect + introspect
```

## How it works

```
crates/core   (cirrus-core, no UI)
  gcloud.rs     tokens from `gcloud auth print-access-token`
  admin.rs      Resource Manager + SQL Admin REST APIs
  connector.rs  native Cloud SQL connector: RSA key → generateEphemeralCert (IAM-bound)
                → mutual TLS to instance:3307, verified against the instance CA
  pg.rs         tokio-postgres over that stream, IAM auth (token as password),
                read-only enforcement, introspection, paging
crates/app    (GPUI UI)
  workspace.rs  sign-in gate, title bar, sidebar + tabs
  navigator.rs  lazy tree (tree.rs = pure model)
  table_view.rs data/structure/DDL tab     grid.rs     shared result grid
  diagram.rs    ER diagram (er_layout.rs = layout)  console.rs  SQL console
```

### Read-only guarantees

1. Sessions start with `default_transaction_read_only=on` and a 2-minute `statement_timeout`.
2. User SQL (console and filters) is first *prepared*, which rejects multiple statements.
3. It then runs inside `BEGIN READ ONLY` after a dummy query (so `SET TRANSACTION READ WRITE` can't flip it), and the transaction is always rolled back.

For hard guarantees, also grant your IAM database user only read privileges.

## License

Copyright (C) 2026 Gastón Michel

Cirrus is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. It is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See [LICENSE](LICENSE) for the full text.

Cirrus is an independent project and is not affiliated with or endorsed by Google. "Google Cloud" and "Cloud SQL" are trademarks of Google LLC.
