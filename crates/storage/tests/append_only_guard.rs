//! Needs a real Postgres (`make dev`, then `make test-integration`).
//!
//! `#[sqlx::test]` gives each test its own freshly created database, with every migration applied.

use sqlx::PgPool;

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_append_only_table_accepts_inserts_and_refuses_everything_else(pool: PgPool) {
    sqlx::query("CREATE TABLE notes (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("SELECT eschess_make_append_only('notes')")
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("INSERT INTO notes (body) VALUES ('first'), ('second')")
        .execute(&pool)
        .await
        .unwrap();

    for (what, sql) in [
        ("update", "UPDATE notes SET body = 'changed'"),
        ("delete", "DELETE FROM notes"),
        ("truncate", "TRUNCATE notes"),
    ] {
        let err = sqlx::query(sql).execute(&pool).await.expect_err(what);
        assert!(err.to_string().contains("append-only"), "{what}: {err}");
    }

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM notes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 2, "nothing was lost or changed");
    let first: String = sqlx::query_scalar("SELECT body FROM notes ORDER BY id LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(first, "first");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn other_tables_are_not_affected(pool: PgPool) {
    sqlx::query("CREATE TABLE guarded (id INT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE ordinary (id INT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("SELECT eschess_make_append_only('guarded')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO ordinary VALUES (1)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE ordinary SET id = 2")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM ordinary")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn migrations_can_be_run_twice_and_the_runner_reports_them() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let admin = eschess_storage::connect(&url).await.unwrap();
    let name = format!("eschess_migrate_check_{}", std::process::id());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();

    let target = url
        .rsplit_once('/')
        .map(|(base, _)| format!("{base}/{name}"))
        .unwrap();
    let pool = eschess_storage::connect(&target).await.unwrap();
    eschess_storage::migrate(&pool).await.unwrap();
    eschess_storage::migrate(&pool)
        .await
        .expect("a second run changes nothing and does not fail");
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applied as usize, eschess_storage::MIGRATOR.migrations.len());

    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
}
