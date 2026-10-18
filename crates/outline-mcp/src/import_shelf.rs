//! `import-shelf` CLI subcommand: bring a pre-Postgres shelf directory into
//! the database (see `outline_mcp_core::infra::shelf_import`).
//!
//! Each book is imported on its own, then read back and compared with the
//! shelf; the command prints one verification line per book and exits
//! non-zero if any book failed or differs.

use std::path::PathBuf;

use outline_mcp_core::infra::pg_store::PgBookStore;
use outline_mcp_core::infra::shelf_import::{
    import_book, list_shelf_slugs, read_shelf_book, ImportError, ImportReport,
};

use crate::db;

const HELP_TEXT: &str = "\
outline-mcp import-shelf --shelf <dir> [--slug <slug>] [--replace] [--database-url <url>]

Imports the books of a shelf directory (the pre-Postgres storage: <slug>.json,
<slug>.changelog.json, <slug>.events.db, <slug>.snap.<millis>.json) into the
database, keeping history and snapshot timestamps, and verifies each book
against its source.

Options:
  --shelf <dir>          The shelf directory to read. Required.
  --slug <slug>          Import only this book.
  --replace              Overwrite a book that already exists in the database
                         (its history and snapshots are replaced too).
                         Without it, an existing book is skipped as an error.
  --database-url <url>   Postgres connection URL. Defaults to the
                         OUTLINE_MCP_DATABASE_URL environment variable.
  -h, --help             Show this help text.

Run `outline-mcp migrate` on the database first. Opening <slug>.events.db may
create SQLite -wal / -shm files next to it; nothing else in the shelf is
written. Import from a copy, or while no server uses the shelf.
";

#[derive(Debug, Default, PartialEq)]
struct Args {
    shelf: Option<PathBuf>,
    slug: Option<String>,
    replace: bool,
    database_url: Option<String>,
}

/// Runs the `import-shelf` subcommand over `argv` (after its token).
///
/// Exit code: `0` when every book imported and matched its source, `1`
/// otherwise (bad arguments, no database, a book that failed, or a mismatch).
pub async fn run(argv: impl Iterator<Item = String>) -> anyhow::Result<i32> {
    let argv: Vec<String> = argv.collect();
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP_TEXT}");
        return Ok(0);
    }
    let args = match parse_args(&argv) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("error: {message}");
            eprint!("{HELP_TEXT}");
            return Ok(1);
        }
    };
    let Some(shelf) = args.shelf.clone() else {
        eprintln!("error: --shelf <dir> is required");
        eprint!("{HELP_TEXT}");
        return Ok(1);
    };

    let slugs = match &args.slug {
        Some(slug) => vec![slug.clone()],
        None => match list_shelf_slugs(&shelf) {
            Ok(slugs) => slugs,
            Err(e) => {
                eprintln!("error: cannot read shelf {}: {e}", shelf.display());
                return Ok(1);
            }
        },
    };
    if slugs.is_empty() {
        eprintln!("error: no books (<slug>.json) found in {}", shelf.display());
        return Ok(1);
    }

    let store = match db::connect_migrated(args.database_url.clone()).await {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: {e}");
            return Ok(1);
        }
    };

    println!("slug\tresult\tnodes(src/db)\thash\thistory(src/db)\tsnapshots(src/db)");
    let mut failed = 0usize;
    for slug in &slugs {
        match import_one(&store, &shelf, slug, args.replace).await {
            Ok(report) => {
                let ok = report.matches();
                if !ok {
                    failed += 1;
                }
                println!("{}", report_line(&report));
                for (path, reason) in &report.unreadable_snapshots {
                    println!("  unreadable snapshot {}: {reason}", path.display());
                }
            }
            Err(message) => {
                failed += 1;
                println!("{slug}\tFAILED\t{message}");
            }
        }
    }
    println!(
        "{} book(s): {} matched, {} failed or differ",
        slugs.len(),
        slugs.len() - failed,
        failed
    );
    Ok(if failed == 0 { 0 } else { 1 })
}

async fn import_one(
    store: &PgBookStore,
    shelf: &std::path::Path,
    slug: &str,
    replace: bool,
) -> Result<ImportReport, String> {
    let source = read_shelf_book(shelf, slug)
        .await
        .map_err(|e| format!("cannot read from the shelf: {e}"))?;
    import_book(store, store, source, replace, |slug| async move {
        store.delete_book(&slug).await
    })
    .await
    .map_err(|e| match e {
        ImportError::AlreadyExists(_) => e.to_string(),
        ImportError::Store(e) => format!("database error: {e}"),
    })
}

fn report_line(r: &ImportReport) -> String {
    let hash = if r.source.hash == r.imported.hash {
        "same"
    } else {
        "DIFFERS"
    };
    format!(
        "{}\t{}\t{}/{}\t{}\t{}/{}\t{}/{}",
        r.slug,
        if r.matches() { "ok" } else { "MISMATCH" },
        r.source.nodes,
        r.imported.nodes,
        hash,
        r.source.history,
        r.imported.history,
        r.source.snapshots,
        r.imported.snapshots,
    )
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args::default();
    let mut iter = argv.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--shelf" => {
                let v = iter.next().ok_or("--shelf requires a value")?;
                args.shelf = Some(PathBuf::from(v));
            }
            "--slug" => {
                let v = iter.next().ok_or("--slug requires a value")?;
                args.slug = Some(v.clone());
            }
            "--database-url" => {
                let v = iter.next().ok_or("--database-url requires a value")?;
                args.database_url = Some(v.clone());
            }
            "--replace" => args.replace = true,
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_flag() {
        let argv: Vec<String> = [
            "--shelf",
            "/s",
            "--slug",
            "b",
            "--replace",
            "--database-url",
            "postgres://x",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            parse_args(&argv).unwrap(),
            Args {
                shelf: Some(PathBuf::from("/s")),
                slug: Some("b".into()),
                replace: true,
                database_url: Some("postgres://x".into()),
            }
        );
    }

    #[test]
    fn rejects_unknown_and_valueless_flags() {
        assert!(parse_args(&["--nope".into()]).is_err());
        assert!(parse_args(&["--shelf".into()]).is_err());
    }
}
