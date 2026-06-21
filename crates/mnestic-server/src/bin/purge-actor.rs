// SPDX-License-Identifier: AGPL-3.0-only

//! Permanently deletes everything held for one subject (GDPR right to erasure). Build with
//! `--features cli`.
//! Usage: purge-actor <tenant-external-id> <containerTag>
//! Env: DATABASE_URL.
//! This is a hard delete and cannot be undone. It removes the subject's memories, chunks,
//! documents, sources, and profile in one transaction. Take a backup first if you need a
//! record. Export the subject with export-actor beforehand if they also requested their data.

use mnestic_server::parse_container_tag;
use mnestic_store::Store;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let external_id = args
        .next()
        .ok_or("usage: purge-actor <tenant-external-id> <containerTag>")?;
    let container_tag = args
        .next()
        .ok_or("usage: purge-actor <tenant-external-id> <containerTag>")?;
    let dsn = std::env::var("DATABASE_URL")?;

    let pool = PgPoolOptions::new().max_connections(1).connect(&dsn).await?;
    let tenant_id: Uuid =
        sqlx::query_scalar("SELECT id FROM mnestic_tenant WHERE external_id = $1")
            .bind(&external_id)
            .fetch_optional(&pool)
            .await?
            .ok_or_else(|| format!("no tenant with external id {external_id}"))?;

    // Echo the resolved scope so the operator can confirm the right subject before trusting a
    // destructive run. Erasure is by actor across all containers; tags do not narrow it.
    let scope = parse_container_tag(&container_tag);
    eprintln!("resolved containerTag {container_tag} to actor {}", scope.actor_id);
    if !scope.container_tags.is_empty() {
        eprintln!("note: container tags {:?} do not scope erasure; the whole actor is purged", scope.container_tags);
    }

    let counts = Store::new(pool).purge_actor(tenant_id, &scope.actor_id).await?;
    println!(
        "purged actor {} (tenant {external_id}): {} memories, {} chunks, {} documents, {} sources, {} profile",
        scope.actor_id, counts.memories, counts.chunks, counts.documents, counts.sources, counts.profile
    );
    Ok(())
}
