//! `migrate` CLI subcommand: apply the Postgres schema migrations.
//!
//! `main.rs` dispatches to [`run`] on the `migrate` token, so this module owns
//! argv parsing for everything after it. The server itself does not use
//! Postgres yet; this only prepares a database for the upcoming store.

use outline_mcp_core::infra::pg_store::PgBookStore;

/// Environment variable naming the Postgres database.
pub const DATABASE_URL_ENV: &str = "OUTLINE_MCP_DATABASE_URL";

const HELP_TEXT: &str = "\
outline-mcp migrate [--database-url <url>]

Applies outline-mcp's schema migrations to a Postgres database. Running it
again on an up-to-date database changes nothing.

Options:
  --database-url <url>   Postgres connection URL. Defaults to the
                         OUTLINE_MCP_DATABASE_URL environment variable.
  -h, --help             Show this help text.
";

/// Runs the `migrate` subcommand over `argv` (after the `migrate` token).
///
/// Returns the process exit code: `0` once the migrations are applied, `1`
/// if the arguments are invalid, no database is named, or connecting or
/// migrating fails. Errors name what to fix; none of them panic.
pub async fn run(argv: impl Iterator<Item = String>) -> anyhow::Result<i32> {
    let argv: Vec<String> = argv.collect();
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP_TEXT}");
        return Ok(0);
    }

    let flag = match parse_args(&argv) {
        Ok(flag) => flag,
        Err(message) => {
            eprintln!("error: {message}");
            eprint!("{HELP_TEXT}");
            return Ok(1);
        }
    };
    let url = match resolve_url(flag, std::env::var(DATABASE_URL_ENV).ok()) {
        Ok(url) => url,
        Err(message) => {
            eprintln!("error: {message}");
            return Ok(1);
        }
    };

    let store = match PgBookStore::connect(&url).await {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: could not connect to the database: {e}");
            return Ok(1);
        }
    };
    if let Err(e) = store.migrate().await {
        eprintln!("error: migration failed: {e}");
        return Ok(1);
    }
    println!("Migrations applied.");
    Ok(0)
}

/// The `--database-url` value, if given.
fn parse_args(argv: &[String]) -> Result<Option<String>, String> {
    let mut url: Option<String> = None;
    let mut iter = argv.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--database-url" => {
                let value = iter.next().ok_or("--database-url requires a value")?;
                url = Some(value.clone());
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    Ok(url)
}

/// The flag wins over the environment; an empty value counts as unset.
fn resolve_url(flag: Option<String>, env: Option<String>) -> Result<String, String> {
    flag.into_iter()
        .chain(env)
        .find(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "no database configured: set {DATABASE_URL_ENV} or pass --database-url <url> \
                 (a Postgres connection URL, e.g. postgres://user:pass@host:5432/db)"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_url_names_the_env_var() {
        let err = resolve_url(None, None).unwrap_err();
        assert!(err.contains(DATABASE_URL_ENV));
        assert!(err.contains("--database-url"));
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

    #[test]
    fn parse_args_rejects_unknown_and_missing_value() {
        assert!(parse_args(&["--nope".into()]).is_err());
        assert!(parse_args(&["--database-url".into()]).is_err());
        assert_eq!(
            parse_args(&["--database-url".into(), "postgres://x".into()]).unwrap(),
            Some("postgres://x".into())
        );
    }
}
