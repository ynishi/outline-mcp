//! `migrate-snapshots` CLI subcommand.
//!
//! # Architecture
//!
//! Imports each book's legacy on-disk `{slug}.snap.{millis}.json` snapshot
//! dumps (predating `outline-mcp-core`'s ai-store-backed
//! `crate::infra::snapshot::SnapshotService`) into that book's ai-store
//! SQLite event log, via
//! `outline_mcp_core::infra::snapshot_migrator::migrate_slug`. See that
//! function's doc comment for the timestamp/idempotency contract this
//! subcommand relies on.
//!
//! `main.rs` dispatches to [`run`] before falling back to its normal
//! "start the MCP server" behavior, so this module owns argv parsing for
//! everything after the `migrate-snapshots` token.

use std::path::{Path, PathBuf};

use outline_mcp_core::infra::snapshot_migrator::{migrate_slug, MigrationReport};

const HELP_TEXT: &str = "\
outline-mcp migrate-snapshots --shelf <path> [--slug <slug>]

Imports legacy on-disk snapshot dumps (`{slug}.snap.{millis}.json`) into
their book's ai-store-backed snapshot stream, so they become restorable via
the `snapshot_restore` MCP tool.

Options:
  --shelf <path>   Shelf directory (the directory containing one `.json`
                    file per book). Required.
  --slug <slug>    Migrate only this book. If omitted, every book slug found
                    directly under --shelf is migrated.
  -h, --help       Show this help text.
";

/// Parsed `migrate-snapshots` subcommand arguments.
#[derive(Debug)]
struct Args {
    shelf: PathBuf,
    slug: Option<String>,
}

/// Runs the `migrate-snapshots` subcommand over `argv` (the remaining argv
/// after the `migrate-snapshots` token has already been consumed by the
/// caller), printing a per-slug [`MigrationReport`] to stdout.
///
/// Returns the process exit code the caller should pass to
/// `std::process::exit`: `0` if every slug's migration completed with zero
/// failed files, `1` if any slug reported at least one failure, the
/// arguments themselves were invalid, or a slug's migration errored out
/// entirely (e.g. its SQLite backend could not be opened).
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

    let slugs = match &args.slug {
        Some(slug) => vec![slug.clone()],
        None => list_book_slugs(&args.shelf)?,
    };

    if slugs.is_empty() {
        println!("No books found under {}", args.shelf.display());
        return Ok(0);
    }

    let mut any_failed = false;
    for slug in &slugs {
        println!("== {slug} ==");
        let outcome: Result<MigrationReport, _> = migrate_slug(&args.shelf, slug).await;
        match outcome {
            Ok(report) => {
                print!("{report}");
                if !report.failed.is_empty() {
                    any_failed = true;
                }
            }
            Err(e) => {
                eprintln!("  error: {e}");
                any_failed = true;
            }
        }
    }

    Ok(if any_failed { 1 } else { 0 })
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut shelf: Option<PathBuf> = None;
    let mut slug: Option<String> = None;

    let mut iter = argv.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--shelf" => {
                let value = iter.next().ok_or("--shelf requires a value")?;
                shelf = Some(PathBuf::from(value));
            }
            "--slug" => {
                let value = iter.next().ok_or("--slug requires a value")?;
                slug = Some(value.clone());
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let shelf = shelf.ok_or("--shelf is required")?;
    Ok(Args { shelf, slug })
}

/// Enumerates book slugs directly under `shelf` (one `.json` file per book,
/// sibling to that book's `.snap.*` / `.events.db` files).
///
/// Duplicated from `outline_mcp_rmcp::OutlineMcpServer`'s equivalent
/// (private) bookkeeping rather than reused from it: that crate's slug
/// enumeration is `pub(crate)` server-internal state, not a public API this
/// binary should reach into for an unrelated CLI subcommand.
fn list_book_slugs(shelf: &Path) -> anyhow::Result<Vec<String>> {
    if !shelf.exists() {
        return Ok(Vec::new());
    }
    let mut slugs: Vec<String> = std::fs::read_dir(shelf)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let ext_ok = path.extension().and_then(|x| x.to_str()) == Some("json");
            let stem_ok = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| !s.contains('.'))
                .unwrap_or(false);
            if ext_ok && stem_ok {
                path.file_stem().and_then(|s| s.to_str()).map(String::from)
            } else {
                None
            }
        })
        .collect();
    slugs.sort();
    Ok(slugs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_args_requires_shelf() {
        let err = parse_args(&[]).expect_err("shelf is required");
        assert!(err.contains("--shelf"));
    }

    #[test]
    fn test_parse_args_shelf_only() {
        let argv = vec!["--shelf".to_string(), "/tmp/shelf".to_string()];
        let args = parse_args(&argv).expect("parse");
        assert_eq!(args.shelf, PathBuf::from("/tmp/shelf"));
        assert!(args.slug.is_none());
    }

    #[test]
    fn test_parse_args_shelf_and_slug() {
        let argv = vec![
            "--shelf".to_string(),
            "/tmp/shelf".to_string(),
            "--slug".to_string(),
            "my-book".to_string(),
        ];
        let args = parse_args(&argv).expect("parse");
        assert_eq!(args.shelf, PathBuf::from("/tmp/shelf"));
        assert_eq!(args.slug.as_deref(), Some("my-book"));
    }

    #[test]
    fn test_parse_args_unrecognized_flag() {
        let argv = vec!["--bogus".to_string()];
        let err = parse_args(&argv).expect_err("unrecognized flag");
        assert!(err.contains("--bogus"));
    }

    #[test]
    fn test_parse_args_missing_value() {
        let argv = vec!["--shelf".to_string()];
        let err = parse_args(&argv).expect_err("missing value");
        assert!(err.contains("--shelf"));
    }

    #[test]
    fn test_list_book_slugs_missing_dir_is_empty() {
        let slugs = list_book_slugs(Path::new("/nonexistent/path/for/outline-mcp-test")).unwrap();
        assert!(slugs.is_empty());
    }

    #[test]
    fn test_list_book_slugs_filters_and_sorts() {
        let dir = std::env::temp_dir().join("outline-mcp-cli-test-list-slugs");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join("zebra.json"), "{}").expect("write zebra");
        std::fs::write(dir.join("apple.json"), "{}").expect("write apple");
        // Non-book files should be excluded: sidecar / changelog / events db.
        std::fs::write(dir.join("apple.snap.1.json"), "{}").expect("write snap");
        std::fs::write(dir.join("apple.changelog.json"), "[]").expect("write changelog");
        std::fs::write(dir.join("apple.events.db"), "").expect("write events db");

        let slugs = list_book_slugs(&dir).expect("list slugs");
        assert_eq!(slugs, vec!["apple".to_string(), "zebra".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_migration_report_help_text_mentions_usage() {
        assert!(HELP_TEXT.contains("--shelf"));
        assert!(HELP_TEXT.contains("--slug"));
    }
}
