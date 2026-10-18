//! Naming and opening the Postgres database, shared by every command.
//!
//! The URL comes from `--database-url` or [`DATABASE_URL_ENV`]. Every failure
//! here ends in a message that says what to set or fix and never repeats the
//! URL (it carries the password).

use outline_mcp_core::infra::pg_store::PgBookStore;

/// Environment variable naming the Postgres database.
pub const DATABASE_URL_ENV: &str = "OUTLINE_MCP_DATABASE_URL";

/// Why the database could not be opened, as a message for the user.
#[derive(Debug)]
pub struct DbError(pub String);

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DbError {}

/// The flag wins over the environment; an empty value counts as unset.
pub fn resolve_url(flag: Option<String>, env: Option<String>) -> Result<String, DbError> {
    flag.into_iter()
        .chain(env)
        .find(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            DbError(format!(
                "no database configured: set {DATABASE_URL_ENV} or pass --database-url <url> \
                 (a Postgres connection URL, e.g. postgres://user:pass@host:5432/db)"
            ))
        })
}

/// Resolve the URL from `flag` / the environment and connect.
pub async fn connect(flag: Option<String>) -> Result<PgBookStore, DbError> {
    let url = resolve_url(flag, std::env::var(DATABASE_URL_ENV).ok())?;
    PgBookStore::connect(&url)
        .await
        .map_err(|e| DbError(e.to_string()))
}

/// [`connect`], then refuse a database whose schema is not migrated.
pub async fn connect_migrated(flag: Option<String>) -> Result<PgBookStore, DbError> {
    let store = connect(flag).await?;
    let pending = store
        .pending_migrations()
        .await
        .map_err(|e| DbError(format!("could not read the schema version: {e}")))?;
    if !pending.is_empty() {
        return Err(DbError(format!(
            "the database schema is not up to date ({} migration(s) pending): \
             run `outline-mcp migrate` first",
            pending.len()
        )));
    }
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_url_names_the_env_var() {
        let err = resolve_url(None, None).unwrap_err();
        assert!(err.0.contains(DATABASE_URL_ENV));
        assert!(err.0.contains("--database-url"));
    }

    #[test]
    fn empty_env_counts_as_unset() {
        assert!(resolve_url(None, Some("  ".into())).is_err());
    }

    #[test]
    fn flag_wins_over_env() {
        let url = resolve_url(
            Some("postgres://flag".into()),
            Some("postgres://env".into()),
        );
        assert_eq!(url.unwrap(), "postgres://flag");
    }

    #[test]
    fn env_is_used_without_flag() {
        let url = resolve_url(None, Some("postgres://env".into()));
        assert_eq!(url.unwrap(), "postgres://env");
    }

    #[tokio::test]
    async fn unreachable_server_is_reported_without_the_url() {
        // Port 1 on loopback: nothing listens, the connect is refused at once.
        let Err(err) = connect(Some("postgres://u:secret-pw@127.0.0.1:1/db".into())).await else {
            panic!("connected to a port nothing listens on");
        };
        assert!(err.0.contains("could not reach"), "{err}");
        assert!(!err.0.contains("secret-pw"), "{err}");
    }

    #[tokio::test]
    async fn invalid_url_is_reported() {
        let Err(err) = connect(Some("not a url".into())).await else {
            panic!("accepted an invalid URL");
        };
        assert!(
            err.0.contains("not a valid Postgres connection URL"),
            "{err}"
        );
    }
}
