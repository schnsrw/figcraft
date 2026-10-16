use figcraft_cloud_storage::{check_schema_version, migrate_database};
use sqlx::postgres::PgPoolOptions;

const DROP_PROBES: [&str; 2] =
    ["DROP DATABASE IF EXISTS figcraft_upgrade_probe WITH (FORCE)", "DROP DATABASE IF EXISTS figcraft_empty_probe WITH (FORCE)"];
const CREATE_PROBES: [&str; 2] = ["CREATE DATABASE figcraft_upgrade_probe", "CREATE DATABASE figcraft_empty_probe"];

fn database_url(admin_url: &str, database: &str) -> Result<String, Box<dyn std::error::Error>> {
    let (server, _) = admin_url.rsplit_once('/').ok_or("database URL has no path")?;
    Ok(format!("{server}/{database}"))
}

async fn version(url: &str) -> Result<i64, Box<dyn std::error::Error>> {
    let pool = PgPoolOptions::new().max_connections(1).connect(url).await?;
    let version: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success").fetch_one(&pool).await?;
    pool.close().await;
    Ok(version.unwrap_or(0))
}

/// Migrations apply from an empty database and upgrade one at the previous supported schema; the
/// already-applied files must be unchanged, since SQLx verifies their checksums on upgrade.
#[tokio::test]
async fn migrations_apply_from_empty_and_upgrade_the_previous_schema() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(admin_url_file) = std::env::var("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE is unset; PostgreSQL migration upgrade probe skipped");
        return Ok(());
    };
    let admin_url = std::fs::read_to_string(admin_url_file)?.trim_end().to_string();
    let admin = PgPoolOptions::new().max_connections(1).connect(&admin_url).await?;
    for statement in DROP_PROBES.into_iter().chain(CREATE_PROBES) {
        sqlx::query(statement).execute(&admin).await?;
    }

    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files: Vec<_> = std::fs::read_dir(&directory)?.map(|entry| entry.map(|entry| entry.path())).collect::<Result<_, _>>()?;
    files.sort();
    let (latest, previous) = match files.as_slice() {
        [.., previous, latest] => (latest.clone(), previous.clone()),
        _ => return Err("need at least two migrations".into()),
    };
    let version_of = |path: &std::path::Path| -> Result<i64, Box<dyn std::error::Error>> {
        let name = path.file_name().and_then(|name| name.to_str()).ok_or("migration name")?;
        Ok(name.split_once('_').ok_or("migration version")?.0.parse()?)
    };
    let (latest_version, previous_version) = (version_of(&latest)?, version_of(&previous)?);

    // The previous supported schema: every migration but the newest, from an exact copy.
    let older = std::env::temp_dir().join(format!("figcraft-migrations-{}", std::process::id()));
    std::fs::create_dir_all(&older)?;
    for file in files.iter().filter(|file| **file != latest) {
        std::fs::copy(file, older.join(file.file_name().ok_or("migration name")?))?;
    }
    let upgrade_url = database_url(&admin_url, "figcraft_upgrade_probe")?;
    let pool = PgPoolOptions::new().max_connections(1).connect(&upgrade_url).await?;
    sqlx::migrate::Migrator::new(older.as_path()).await?.run(&pool).await?;
    pool.close().await;
    std::fs::remove_dir_all(&older)?;
    assert_eq!(version(&upgrade_url).await?, previous_version);
    assert!(check_schema_version(&upgrade_url, latest_version, latest_version).await.is_err(), "the binary refuses the previous schema");

    // Upgrade with the binary's embedded migrations, then again as a no-op.
    for _ in 0..2 {
        migrate_database(&upgrade_url).await.map_err(|error| format!("upgrade: {error:?}"))?;
        assert_eq!(check_schema_version(&upgrade_url, latest_version, latest_version).await, Ok(latest_version));
    }

    let empty_url = database_url(&admin_url, "figcraft_empty_probe")?;
    migrate_database(&empty_url).await.map_err(|error| format!("from empty: {error:?}"))?;
    assert_eq!(check_schema_version(&empty_url, latest_version, latest_version).await, Ok(latest_version));

    // Both paths end at the same schema.
    let tables = "SELECT string_agg(table_name || '.' || column_name || ':' || data_type, ',' ORDER BY table_name, column_name) \
                  FROM information_schema.columns WHERE table_schema = 'public'";
    let mut shapes = Vec::new();
    for url in [&upgrade_url, &empty_url] {
        let pool = PgPoolOptions::new().max_connections(1).connect(url).await?;
        let shape: Option<String> = sqlx::query_scalar(tables).fetch_one(&pool).await?;
        pool.close().await;
        shapes.push(shape);
    }
    assert_eq!(shapes.first(), shapes.get(1));

    for statement in DROP_PROBES {
        sqlx::query(statement).execute(&admin).await?;
    }
    admin.close().await;
    Ok(())
}
