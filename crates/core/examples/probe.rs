//! Smoke test for the core crate against real GCP (read-only).
//!   cargo run -p cirrus-core --example probe                      # list projects + Postgres instances
//!   cargo run -p cirrus-core --example probe -- PROJECT INSTANCE DB  # connect and introspect
use cirrus_core::{admin, gcloud, pg::Session, runtime};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    runtime::runtime().block_on(async move {
        println!("account: {:?}", gcloud::active_account().await?);
        if let [project, instance, db] = args.as_slice() {
            println!("databases: {:?}", admin::list_databases(project, instance).await?);
            let s = Session::connect(project, instance, db).await?;
            let schemas = s.schemas().await?;
            println!("schemas: {:?}", schemas.iter().map(|s| &s.name).collect::<Vec<_>>());
            let first = schemas.iter().find(|s| !s.is_system).map(|s| s.name.clone()).unwrap_or("public".into());
            let model = s.schema_model(&first).await?;
            println!("{first}: {} tables, {} fks", model.tables.len(), model.foreign_keys.len());
            if let Some((t, _)) = model.tables.first() {
                let d = s.table_details(t).await?;
                println!("{}\n", d.ddl);
                let page = s.table_page(&t.schema, &t.name, &cirrus_core::PageRequest { limit: 3, ..Default::default() }).await?;
                println!("cols {:?} rows {} more {}", page.columns.iter().map(|c| format!("{}:{}", c.name, c.type_name)).collect::<Vec<_>>(), page.rows.len(), page.has_more);
            }
            for sql in ["select now(), current_user", "show transaction_read_only", "create table cirrus_should_fail(x int)", "select 1; select 2"] {
                match s.run_readonly(sql, 10).await {
                    Ok(r) => println!("{sql:?} -> {:?} {:?}", r.rows, r.message),
                    Err(e) => println!("{sql:?} -> ERR {e}"),
                }
            }
            return Ok(());
        }
        let projects = admin::list_projects().await?;
        println!("{} projects", projects.len());
        for p in projects.iter().take(40) {
            match admin::list_instances(&p.project_id).await {
                Ok(list) => for i in list {
                    println!("  {} / {} {:?} {:?}", p.project_id, i.name, i.database_version, i.state);
                },
                Err(e) => println!("  {}: {}", p.project_id, e.to_string().lines().next().unwrap_or("")),
            }
        }
        Ok(())
    })
}
