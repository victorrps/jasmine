use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::time::Duration;

/// Initialize the database connection pool and run migrations.
pub async fn init_db(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .idle_timeout(Duration::from_secs(30))
        .connect(database_url)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    tracing::info!("Database initialized and migrations applied");
    Ok(pool)
}
