//! `migrate` CLI subcommand: apply the Postgres schema migrations.
//!
//! `main.rs` dispatches to [`run`] on the `migrate` token, so this module owns
//! argv parsing for everything after it. `serve` / `stdio` refuse to start on
//! a database this has not brought up to date.

use crate::db;

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
    let store = match db::connect(flag).await {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: {e}");
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

#[cfg(test)]
mod tests {
    use super::*;

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
