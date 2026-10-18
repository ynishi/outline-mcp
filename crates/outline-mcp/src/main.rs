//! Thin entry point: dispatches the `migrate` subcommand (see `migrate_db`)
//! or the `import-shelf` subcommand (see `import_shelf`), or else parses the
//! transport flags from argv, opens the Postgres database and hands off to
//! `outline_mcp_rmcp::run` (stdio) or `run_http` (streamable HTTP), which own
//! the MCP server (rmcp transport, tool_router, resources).
//!
//! Usage:
//!
//! ```text
//! outline-mcp migrate [--database-url <url>]          # apply the schema migrations
//! outline-mcp import-shelf --shelf <dir> [...]         # import a pre-Postgres shelf
//! outline-mcp [--database-url <url>]                   # stdio (default)
//! outline-mcp [--database-url <url>] --mcp-http [--bind ADDR]   # streamable HTTP daemon
//! ```
//!
//! The database comes from `--database-url` or `OUTLINE_MCP_DATABASE_URL`;
//! the server refuses to start without one, or on a database `migrate` has
//! not brought up to date. `--bind` defaults to `127.0.0.1:8486`.
//! Non-loopback binds require `OUTLINE_MCP_HTTP_TOKEN` (see
//! `outline_mcp_rmcp::run_http`).

use std::sync::Arc;

use outline_mcp_rmcp::Storage;

mod db;
mod import_shelf;
mod migrate_db;

/// Default bind address for `--mcp-http` (loopback; SSOT-daemon deployments
/// override with `--bind 0.0.0.0:8486` + `OUTLINE_MCP_HTTP_TOKEN`).
const DEFAULT_BIND: &str = "127.0.0.1:8486";

#[derive(Debug, Default, PartialEq)]
struct ServeArgs {
    http: bool,
    bind: Option<String>,
    database_url: Option<String>,
}

fn parse_serve_args(argv: Vec<String>) -> Result<ServeArgs, String> {
    let mut args = ServeArgs::default();
    let mut iter = argv.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--mcp-http" => args.http = true,
            "--bind" => {
                let value = iter.next().ok_or("--bind requires a value (host:port)")?;
                args.bind = Some(value);
            }
            "--database-url" => {
                let value = iter.next().ok_or("--database-url requires a value")?;
                args.database_url = Some(value);
            }
            other if other.starts_with('-') => {
                return Err(format!(
                    "unrecognized flag: {other} (expected --mcp-http / --bind ADDR / --database-url URL)"
                ));
            }
            other => {
                return Err(format!(
                    "unexpected argument: {other}. outline-mcp no longer reads a shelf directory: \
                     books live in Postgres ({}). To bring an existing shelf over, run \
                     `outline-mcp import-shelf --shelf {other}`",
                    db::DATABASE_URL_ENV
                ));
            }
        }
    }
    if args.bind.is_some() && !args.http {
        return Err("--bind only applies with --mcp-http".into());
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    match argv.first().map(String::as_str) {
        Some("migrate") => {
            let exit_code = migrate_db::run(argv.into_iter().skip(1)).await?;
            std::process::exit(exit_code);
        }
        Some("import-shelf") => {
            let exit_code = import_shelf::run(argv.into_iter().skip(1)).await?;
            std::process::exit(exit_code);
        }
        _ => {}
    }

    let args = match parse_serve_args(argv) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(2);
        }
    };

    let store = match db::connect_migrated(args.database_url).await {
        Ok(store) => Arc::new(store),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let storage = Storage::new(store.clone(), store);

    if args.http {
        let bind = args.bind.as_deref().unwrap_or(DEFAULT_BIND);
        outline_mcp_rmcp::run_http(storage, bind).await
    } else {
        outline_mcp_rmcp::run(storage).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn serve_flags_parse() {
        let args = parse_serve_args(argv(&[
            "--mcp-http",
            "--bind",
            "0.0.0.0:1",
            "--database-url",
            "u",
        ]))
        .unwrap();
        assert_eq!(
            args,
            ServeArgs {
                http: true,
                bind: Some("0.0.0.0:1".into()),
                database_url: Some("u".into()),
            }
        );
        assert_eq!(parse_serve_args(vec![]).unwrap(), ServeArgs::default());
    }

    #[test]
    fn a_shelf_path_points_to_import_shelf() {
        let err = parse_serve_args(argv(&["/home/x/books"])).unwrap_err();
        assert!(err.contains("import-shelf --shelf /home/x/books"), "{err}");
        assert!(err.contains(db::DATABASE_URL_ENV));
    }

    #[test]
    fn bind_needs_http_and_unknown_flags_fail() {
        assert!(parse_serve_args(argv(&["--bind", "x"])).is_err());
        assert!(parse_serve_args(argv(&["--nope"])).is_err());
    }
}
