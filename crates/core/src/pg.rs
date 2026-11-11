//! Read-only PostgreSQL access over the native Cloud SQL connector, authenticated
//! with IAM database authentication (the OAuth access token is the password).
//!
//! Read-only guarantees, in layers:
//! - every session starts with `default_transaction_read_only=on`;
//! - user-provided SQL is first *prepared*, which rejects multi-statement input;
//! - it then runs inside `BEGIN READ ONLY` after a dummy query, so it can't
//!   switch the transaction to read-write, and the transaction is always rolled back.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, MutexGuard, OnceCell};
use tokio_postgres::config::SslMode;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

use crate::model::*;
use crate::{connector, gcloud};

const STATEMENT_TIMEOUT_MS: u64 = 120_000;

pub struct Session {
    pub project: String,
    pub instance: String,
    pub database: String,
    client: Mutex<Option<Client>>,
}

type SessionKey = (String, String, String);

fn sessions() -> &'static std::sync::Mutex<HashMap<SessionKey, Arc<OnceCell<Arc<Session>>>>> {
    static SESSIONS: std::sync::OnceLock<std::sync::Mutex<HashMap<SessionKey, Arc<OnceCell<Arc<Session>>>>>> =
        std::sync::OnceLock::new();
    SESSIONS.get_or_init(Default::default)
}

/// A shared session per database; connects on first use. Failed attempts are retried on the next call.
pub async fn session(project: &str, instance: &str, database: &str) -> Result<Arc<Session>> {
    let key = (project.to_string(), instance.to_string(), database.to_string());
    let cell = sessions().lock().unwrap().entry(key).or_default().clone();
    cell.get_or_try_init(|| Session::connect(project, instance, database))
        .await
        .cloned()
}

impl Session {
    pub async fn connect(project: &str, instance: &str, database: &str) -> Result<Arc<Session>> {
        let session = Arc::new(Session {
            project: project.into(),
            instance: instance.into(),
            database: database.into(),
            client: Mutex::new(None),
        });
        drop(session.client().await?);
        Ok(session)
    }

    /// Returns a live client, reconnecting if the previous connection dropped.
    async fn client(&self) -> Result<MutexGuard<'_, Option<Client>>> {
        let mut guard = self.client.lock().await;
        if guard.as_ref().is_none_or(|c| c.is_closed()) {
            *guard = Some(self.open().await?);
        }
        Ok(guard)
    }

    async fn open(&self) -> Result<Client> {
        let account = gcloud::active_account()
            .await?
            .context("not logged in to gcloud")?;
        let token = gcloud::access_token().await?;
        let stream = connector::connect(&self.project, &self.instance).await?;

        let mut config = tokio_postgres::Config::new();
        config
            .user(gcloud::iam_db_user(&account))
            .password(token)
            .dbname(&self.database)
            .application_name("cirrus")
            .options(format!(
                "-c default_transaction_read_only=on -c statement_timeout={STATEMENT_TIMEOUT_MS}"
            ))
            .ssl_mode(SslMode::Disable);

        let (client, connection) = match config.connect_raw(stream, NoTls).await {
            Ok(ok) => ok,
            Err(e) => {
                connector::invalidate(&self.project, &self.instance).await;
                let msg = db_error_message(&e);
                if msg.contains("authentication failed") || msg.contains("does not exist") {
                    bail!(
                        "{msg}\n\nMake sure the instance has the `cloudsql.iam_authentication` flag on and that \
                         {account} is added as a Cloud SQL IAM user with CONNECT on database {:?}.",
                        self.database
                    );
                }
                bail!("{msg}");
            }
        };
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                log::warn!("postgres connection closed: {e}");
            }
        });
        Ok(client)
    }

    /// Runs one user-supplied statement read-only, returning at most `max_rows` rows.
    pub async fn run_readonly(&self, sql: &str, max_rows: usize) -> Result<QueryResult> {
        let sql = sql.trim().trim_end_matches(';').trim();
        if sql.is_empty() {
            bail!("nothing to run");
        }
        let guard = self.client().await?;
        let client = guard.as_ref().unwrap();
        let started = Instant::now();

        let stmt = client.prepare(sql).await.map_err(|e| anyhow::anyhow!(db_error_message(&e)))?;
        if !stmt.params().is_empty() {
            bail!("query parameters ($1, $2, …) are not supported");
        }
        let columns: Vec<ResultColumn> = stmt
            .columns()
            .iter()
            .map(|c| ResultColumn { name: c.name().to_string(), type_name: c.type_().name().to_string() })
            .collect();

        client.batch_execute("BEGIN READ ONLY").await?;
        let outcome = run_in_read_only_tx(client, sql, !columns.is_empty(), max_rows).await;
        let _ = client.batch_execute("ROLLBACK").await;
        let (rows, has_more, message) = outcome?;

        Ok(QueryResult { columns, rows, has_more, elapsed: started.elapsed(), message })
    }

    pub async fn table_page(&self, schema: &str, table: &str, req: &PageRequest) -> Result<QueryResult> {
        let mut sql = format!("SELECT * FROM {}.{}", quote_ident(schema), quote_ident(table));
        if let Some(filter) = req.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            sql.push_str(&format!(" WHERE ({filter})"));
        }
        if let Some((col, dir)) = &req.order_by {
            let dir = if *dir == SortDirection::Asc { "ASC" } else { "DESC" };
            sql.push_str(&format!(" ORDER BY {} {dir} NULLS LAST", quote_ident(col)));
        }
        sql.push_str(&format!(" LIMIT {} OFFSET {}", req.limit + 1, req.offset));
        self.run_readonly(&sql, req.limit).await
    }

    pub async fn table_count(&self, schema: &str, table: &str, filter: Option<&str>) -> Result<i64> {
        let mut sql = format!("SELECT count(*) FROM {}.{}", quote_ident(schema), quote_ident(table));
        if let Some(filter) = filter.map(str::trim).filter(|f| !f.is_empty()) {
            sql.push_str(&format!(" WHERE ({filter})"));
        }
        let res = self.run_readonly(&sql, 1).await?;
        res.rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .context("count returned no rows")?
            .parse()
            .context("count returned a non-number")
    }

    pub async fn schemas(&self) -> Result<Vec<SchemaInfo>> {
        let guard = self.client().await?;
        let rows = guard
            .as_ref()
            .unwrap()
            .query(
                "SELECT nspname::text FROM pg_catalog.pg_namespace
                 WHERE nspname NOT LIKE 'pg\\_temp\\_%' AND nspname NOT LIKE 'pg\\_toast%'
                 ORDER BY nspname",
                &[],
            )
            .await?;
        let mut schemas: Vec<SchemaInfo> = rows
            .iter()
            .map(|r| {
                let name: String = r.get(0);
                let is_system = name == "information_schema" || name.starts_with("pg_");
                SchemaInfo { name, is_system }
            })
            .collect();
        schemas.sort_by_key(|s| s.is_system);
        Ok(schemas)
    }

    pub async fn tables(&self, schema: &str) -> Result<Vec<TableInfo>> {
        let guard = self.client().await?;
        let rows = guard
            .as_ref()
            .unwrap()
            .query(
                "SELECT c.relname::text, c.relkind::text, c.reltuples::float8,
                        pg_catalog.obj_description(c.oid, 'pg_class')
                 FROM pg_catalog.pg_class c
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1 AND c.relkind IN ('r','p','v','m','f') AND NOT c.relispartition
                 ORDER BY c.relname",
                &[&schema],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(TableInfo {
                    schema: schema.to_string(),
                    name: r.get(0),
                    kind: RelKind::from_pg(r.get(1))?,
                    estimated_rows: r.get(2),
                    comment: r.get(3),
                })
            })
            .collect())
    }

    /// Columns for every table in `schema` (or just `table`), keyed by table name.
    async fn columns(&self, schema: &str, table: Option<&str>) -> Result<HashMap<String, Vec<ColumnInfo>>> {
        let guard = self.client().await?;
        let rows = guard
            .as_ref()
            .unwrap()
            .query(
                "SELECT c.relname::text, a.attname::text,
                        pg_catalog.format_type(a.atttypid, a.atttypmod),
                        a.attnotnull, pg_catalog.pg_get_expr(d.adbin, d.adrelid),
                        pg_catalog.col_description(c.oid, a.attnum)
                 FROM pg_catalog.pg_attribute a
                 JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                 LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
                 WHERE n.nspname = $1 AND ($2::text IS NULL OR c.relname = $2)
                   AND c.relkind IN ('r','p','v','m','f') AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY c.relname, a.attnum",
                &[&schema, &table],
            )
            .await?;
        let mut out: HashMap<String, Vec<ColumnInfo>> = HashMap::new();
        for r in rows {
            out.entry(r.get(0)).or_default().push(ColumnInfo {
                name: r.get(1),
                data_type: r.get(2),
                nullable: !r.get::<_, bool>(3),
                default: r.get(4),
                comment: r.get(5),
                is_primary_key: false,
                is_foreign_key: false,
            });
        }
        Ok(out)
    }

    /// Constraints keyed by table name, plus foreign keys as structured data.
    async fn constraints(
        &self,
        schema: &str,
        table: Option<&str>,
    ) -> Result<(HashMap<String, Vec<ConstraintInfo>>, Vec<ForeignKey>)> {
        let guard = self.client().await?;
        let rows = guard
            .as_ref()
            .unwrap()
            .query(
                "SELECT c.relname::text, con.conname::text, con.contype::text,
                        pg_catalog.pg_get_constraintdef(con.oid, true),
                        ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(num, ord)
                              JOIN pg_catalog.pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.num
                              ORDER BY k.ord),
                        fn.nspname::text, fc.relname::text,
                        ARRAY(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY k(num, ord)
                              JOIN pg_catalog.pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.num
                              ORDER BY k.ord)
                 FROM pg_catalog.pg_constraint con
                 JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                 LEFT JOIN pg_catalog.pg_class fc ON fc.oid = con.confrelid
                 LEFT JOIN pg_catalog.pg_namespace fn ON fn.oid = fc.relnamespace
                 WHERE n.nspname = $1 AND ($2::text IS NULL OR c.relname = $2)
                   AND con.contype NOT IN ('n', 't')
                 ORDER BY c.relname, con.contype, con.conname",
                &[&schema, &table],
            )
            .await?;
        let mut by_table: HashMap<String, Vec<ConstraintInfo>> = HashMap::new();
        let mut fks = Vec::new();
        for r in rows {
            let table: String = r.get(0);
            let kind = r.get::<_, String>(2).chars().next().unwrap_or('?');
            let columns: Vec<String> = r.get(4);
            if kind == 'f' {
                fks.push(ForeignKey {
                    name: r.get(1),
                    table: table.clone(),
                    columns: columns.clone(),
                    ref_schema: r.get::<_, Option<String>>(5).unwrap_or_default(),
                    ref_table: r.get::<_, Option<String>>(6).unwrap_or_default(),
                    ref_columns: r.get(7),
                });
            }
            by_table.entry(table).or_default().push(ConstraintInfo {
                name: r.get(1),
                kind,
                definition: r.get(3),
                columns,
            });
        }
        Ok((by_table, fks))
    }

    pub async fn schema_model(&self, schema: &str) -> Result<SchemaModel> {
        let tables = self.tables(schema).await?;
        let mut columns = self.columns(schema, None).await?;
        let (constraints, foreign_keys) = self.constraints(schema, None).await?;
        let tables = tables
            .into_iter()
            .map(|t| {
                let mut cols = columns.remove(&t.name).unwrap_or_default();
                mark_keys(&mut cols, constraints.get(&t.name).map(Vec::as_slice).unwrap_or_default());
                (t, cols)
            })
            .collect();
        Ok(SchemaModel { schema: schema.to_string(), tables, foreign_keys })
    }

    pub async fn table_details(&self, table: &TableInfo) -> Result<TableDetails> {
        let schema = table.schema.as_str();
        let name = table.name.as_str();
        let mut columns = self.columns(schema, Some(name)).await?.remove(name).unwrap_or_default();
        let constraints = self.constraints(schema, Some(name)).await?.0.remove(name).unwrap_or_default();
        mark_keys(&mut columns, &constraints);

        let guard = self.client().await?;
        let client = guard.as_ref().unwrap();
        let indexes = client
            .query(
                "SELECT i.relname::text, pg_catalog.pg_get_indexdef(ix.indexrelid), ix.indisunique, ix.indisprimary
                 FROM pg_catalog.pg_index ix
                 JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid
                 JOIN pg_catalog.pg_class t ON t.oid = ix.indrelid
                 JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace
                 WHERE n.nspname = $1 AND t.relname = $2
                 ORDER BY ix.indisprimary DESC, i.relname",
                &[&schema, &name],
            )
            .await?
            .iter()
            .map(|r| IndexInfo {
                name: r.get(0),
                definition: r.get(1),
                is_unique: r.get(2),
                is_primary: r.get(3),
            })
            .collect::<Vec<_>>();
        let extra = client
            .query_one(
                "SELECT pg_catalog.pg_total_relation_size(c.oid),
                        CASE WHEN c.relkind IN ('v','m') THEN pg_catalog.pg_get_viewdef(c.oid, true) END
                 FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1 AND c.relname = $2",
                &[&schema, &name],
            )
            .await?;
        let total_size_bytes: Option<i64> = extra.get(0);
        let view_def: Option<String> = extra.get(1);
        drop(guard);

        let ddl = build_ddl(table, &columns, &constraints, &indexes, view_def.as_deref());
        Ok(TableDetails { table: table.clone(), columns, constraints, indexes, total_size_bytes, ddl })
    }
}

async fn run_in_read_only_tx(
    client: &Client,
    sql: &str,
    returns_rows: bool,
    max_rows: usize,
) -> Result<(Vec<Vec<Option<String>>>, bool, Option<String>)> {
    // Any query "uses up" the chance to run SET TRANSACTION READ WRITE.
    client.simple_query("SELECT 1").await?;
    let messages = if returns_rows && is_cursorable(sql) {
        client
            .batch_execute(&format!("DECLARE cirrus_cursor NO SCROLL CURSOR FOR {sql}"))
            .await
            .map_err(|e| anyhow::anyhow!(db_error_message(&e)))?;
        client.simple_query(&format!("FETCH FORWARD {} FROM cirrus_cursor", max_rows + 1)).await
    } else {
        client.simple_query(sql).await
    }
    .map_err(|e| anyhow::anyhow!(db_error_message(&e)))?;

    let mut rows = Vec::new();
    let mut message = None;
    for m in messages {
        match m {
            SimpleQueryMessage::Row(row) => {
                rows.push((0..row.len()).map(|i| row.get(i).map(str::to_string)).collect());
            }
            SimpleQueryMessage::CommandComplete(n) if !returns_rows => {
                message = Some(format!("OK ({n} rows affected)"));
            }
            _ => {}
        }
    }
    let has_more = rows.len() > max_rows;
    rows.truncate(max_rows);
    Ok((rows, has_more, message))
}

/// Statements that can be wrapped in `DECLARE … CURSOR FOR` so we can fetch a bounded number of rows.
fn is_cursorable(sql: &str) -> bool {
    let first = strip_leading_comments(sql)
        .trim_start_matches('(')
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(first.as_str(), "SELECT" | "WITH" | "VALUES" | "TABLE")
}

fn strip_leading_comments(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_start();
        if let Some(rest) = sql.strip_prefix("--") {
            sql = rest.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(rest) = sql.strip_prefix("/*") {
            sql = rest.split_once("*/").map_or("", |(_, r)| r);
        } else {
            return sql;
        }
    }
}

fn mark_keys(columns: &mut [ColumnInfo], constraints: &[ConstraintInfo]) {
    let cols_of = |kind: char| -> HashSet<&str> {
        constraints
            .iter()
            .filter(|c| c.kind == kind)
            .flat_map(|c| c.columns.iter().map(String::as_str))
            .collect()
    };
    let pk = cols_of('p');
    let fk = cols_of('f');
    for c in columns {
        c.is_primary_key = pk.contains(c.name.as_str());
        c.is_foreign_key = fk.contains(c.name.as_str());
    }
}

fn build_ddl(
    table: &TableInfo,
    columns: &[ColumnInfo],
    constraints: &[ConstraintInfo],
    indexes: &[IndexInfo],
    view_def: Option<&str>,
) -> String {
    let qualified = format!("{}.{}", quote_ident(&table.schema), quote_ident(&table.name));
    if let Some(def) = view_def {
        let kind = if table.kind == RelKind::MaterializedView { "MATERIALIZED VIEW" } else { "VIEW" };
        return format!("CREATE {kind} {qualified} AS\n{}", def.trim_end());
    }
    let mut lines: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut line = format!("    {} {}", quote_ident(&c.name), c.data_type);
            if !c.nullable {
                line.push_str(" NOT NULL");
            }
            if let Some(d) = &c.default {
                line.push_str(&format!(" DEFAULT {d}"));
            }
            line
        })
        .collect();
    lines.extend(
        constraints
            .iter()
            .map(|c| format!("    CONSTRAINT {} {}", quote_ident(&c.name), c.definition)),
    );
    let kind = if table.kind == RelKind::ForeignTable { "FOREIGN TABLE" } else { "TABLE" };
    let mut ddl = format!("CREATE {kind} {qualified} (\n{}\n);", lines.join(",\n"));
    let constraint_names: HashSet<&str> = constraints.iter().map(|c| c.name.as_str()).collect();
    for idx in indexes.iter().filter(|i| !constraint_names.contains(i.name.as_str())) {
        ddl.push_str(&format!("\n\n{};", idx.definition));
    }
    if let Some(comment) = &table.comment {
        ddl.push_str(&format!("\n\nCOMMENT ON TABLE {qualified} IS {};", quote_literal(comment)));
    }
    ddl
}

pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Postgres errors carry the useful text in `as_db_error()`; the Display impl just says "db error".
pub fn db_error_message(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => {
            let mut msg = format!("{}: {}", db.severity(), db.message());
            if let Some(detail) = db.detail() {
                msg.push_str(&format!("\nDETAIL: {detail}"));
            }
            if let Some(hint) = db.hint() {
                msg.push_str(&format!("\nHINT: {hint}"));
            }
            msg
        }
        None => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursorable_detection() {
        assert!(is_cursorable("select 1"));
        assert!(is_cursorable("  -- c\n/* x */ WITH a AS (select 1) select * from a"));
        assert!(is_cursorable("(select 1)"));
        assert!(!is_cursorable("show search_path"));
        assert!(!is_cursorable("explain select 1"));
    }

    #[test]
    fn idents_are_quoted() {
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }
}
