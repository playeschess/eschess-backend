//! PostgreSQL is the system of record. Everything that touches it goes through this crate.

pub mod ledger;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

pub use sqlx::migrate::MigrateError;

/// Every migration, compiled into the binary. They are forward-only: a change is a new file, never an edit.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await
}

/// Applies any migrations that have not run yet. Safe to run repeatedly.
pub async fn migrate(pool: &PgPool) -> Result<(), MigrateError> {
    MIGRATOR.run(pool).await
}
