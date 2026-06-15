// SPDX-License-Identifier: Apache-2.0

//! Exports everything held for one subject as JSON (GDPR right to access). Build with
//! `--features cli`.
//! Usage: export-actor <tenant-external-id> <containerTag>
//! Env: DATABASE_URL.
//! The containerTag is resolved to an actor the same way the server resolves it, so the
//! export covers exactly what recall would surface for that subject. Prints JSON to stdout.

use mnestic_server::parse_container_tag;
use mnestic_store::Store;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let external_id = args
        .next()
        .ok_or("usage: export-actor <tenant-external-id> <containerTag>")?;
    let container_tag = args
        .next()
        .ok_or("usage: export-actor <tenant-external-id> <containerTag>")?;
    let dsn = std::env::var("DATABASE_URL")?;

    let pool = PgPoolOptions::new().max_connections(1).connect(&dsn).await?;
    let tenant_id: Uuid =
        sqlx::query_scalar("SELECT id FROM mnestic_tenant WHERE external_id = $1")
            .bind(&external_id)
            .fetch_optional(&pool)
            .await?
            .ok_or_else(|| format!("no tenant with external id {external_id}"))?;

    // The resolution note goes to stderr so stdout stays clean JSON for piping.
    let scope = parse_container_tag(&container_tag);
    eprintln!("resolved containerTag {container_tag} to actor {}", scope.actor_id);
    if !scope.container_tags.is_empty() {
        eprintln!("note: container tags {:?} do not scope the export; the whole actor is exported", scope.container_tags);
    }

    let json = Store::new(pool).export_actor(tenant_id, &scope.actor_id).await?;
    println!("{json}");
    Ok(())
}
