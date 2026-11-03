use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL is not set (see .env.example)");
        return ExitCode::FAILURE;
    };
    let pool = match eschess_storage::connect(&url).await {
        Ok(pool) => pool,
        Err(e) => {
            eprintln!("cannot connect to the database: {e}");
            return ExitCode::FAILURE;
        }
    };
    match eschess_storage::migrate(&pool).await {
        Ok(()) => {
            println!("migrations are up to date");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("migration failed: {e}");
            ExitCode::FAILURE
        }
    }
}
