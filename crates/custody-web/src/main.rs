use anyhow::{Context, Result};
use custody_store::{PgPool, MIGRATIONS};
use std::env;

#[tokio::main]
async fn main() -> Result<()> {
    let db = PgPool::connect(
        &env::var("DATABASE_URL").context("set DATABASE_URL to a dedicated demo database")?,
    )
    .await?;
    MIGRATIONS.run(&db).await?;
    match env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => (),
        [v] if v == "--migrate" => return Ok(()),
        _ => anyhow::bail!("usage: custody-web [--migrate]"),
    }
    let port = env::var("CUSTODY_WEB_PORT")
        .unwrap_or_else(|_| "4310".into())
        .parse::<u16>()?;
    let host = if env::var("CUSTODY_CONTAINER_LISTEN").as_deref() == Ok("1") {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    };
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    println!("Custody Desk listening on {}", listener.local_addr()?);
    axum::serve(listener, custody_web::app(db)).await?;
    Ok(())
}
