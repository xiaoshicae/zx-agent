//! What the tests of the stores share: a database for each test, in a schema of its own.

use std::sync::Arc;

use tokio_postgres::{Config, NoTls};
use zx_util::random::random_u64;

use super::Database;

/// The variable that names the Postgres the tests use; without it they are skipped.
const URL_VAR: &str = "ZX_TEST_POSTGRES_URL";

/// The database at `ZX_TEST_POSTGRES_URL`, in a new schema of its own, its tables made;
/// `None`, said on stderr, when the variable is not set.
pub(super) async fn database() -> Option<Arc<Database>> {
    let Some(url) = std::env::var(URL_VAR).ok().filter(|url| !url.is_empty()) else {
        eprintln!("{URL_VAR} is not set: this test of the Postgres stores is skipped");
        return None;
    };
    let mut config: Config = url.parse().expect("ZX_TEST_POSTGRES_URL is a connection string");
    let schema = format!("zx_test_{:016x}", random_u64());
    let (client, connection) = config.connect(NoTls).await.expect("the test database answers");
    tokio::spawn(connection);
    client
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .expect("a schema can be made");
    config.options(format!("-c search_path={schema}"));
    Some(Database::open(config).await.expect("the tables are made"))
}
