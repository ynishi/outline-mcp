//! Integration test against a real Postgres. Does not run (returns early and
//! says so on stderr) unless `OUTLINE_MCP_TEST_DATABASE_URL` is set; point it
//! at a throwaway database. The test uses slugs unique to this process and
//! deletes them at the end.

use outline_mcp_core::application::error::AppError;
use outline_mcp_core::application::service::BookService;
use outline_mcp_core::domain::model::book::{AddNodeRequest, TemplateBook, UpdateNodeRequest};
use outline_mcp_core::domain::model::changelog::{ChangeAction, ChangeEntry};
use outline_mcp_core::domain::model::node::{BodyChange, NodeType, Revision};
use outline_mcp_core::domain::model::timestamp::Timestamp;
use outline_mcp_core::domain::store::{BookStore, SnapshotDelete, SnapshotStore, StoreError};
use outline_mcp_core::infra::pg_store::PgBookStore;
use outline_mcp_core::infra::shelf_import::{import_book, read_shelf_book};

fn add_req(
    parent: Option<outline_mcp_core::domain::model::id::NodeId>,
    title: &str,
) -> AddNodeRequest {
    AddNodeRequest {
        parent,
        title: title.to_string(),
        node_type: NodeType::Section,
        body: Some(format!("{title} body")),
        placeholder: None,
        position: usize::MAX,
        properties: [("gate_status".to_string(), "approved".to_string())].into(),
    }
}

fn title_req(title: &str) -> UpdateNodeRequest {
    UpdateNodeRequest {
        title: Some(title.to_string()),
        body: None,
        node_type: None,
        placeholder: None,
        properties: None,
        properties_patch: None,
        status: None,
    }
}

#[tokio::test]
async fn round_trip_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!(
            "OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres integration test did NOT run"
        );
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    store.migrate().await.expect("migrate is idempotent");

    let slug = format!("_test-{}", std::process::id());
    let svc = BookService::new(store.clone(), slug.clone());
    svc.create_book("PG Test", 4).await.expect("create_book");

    // writes: each one stores the tree and its event together
    let (a, w) = svc.add_node(add_req(None, "a")).await.expect("add a");
    assert!(w.is_none());
    let (b, _) = svc.add_node(add_req(None, "b")).await.expect("add b");
    let (a1, _) = svc.add_node(add_req(Some(a), "a1")).await.expect("add a1");
    svc.update_node(b, title_req("b2")).await.expect("update b");
    svc.move_node(b, Some(a), 0).await.expect("move b under a");

    let tree = svc.read_tree().await.expect("read_tree");
    assert_eq!(tree.root_nodes(), &[a]);
    assert_eq!(tree.get_node(a).unwrap().children(), &[b, a1]);
    assert_eq!(tree.get_node(b).unwrap().title(), "b2");
    assert_eq!(
        tree.get_node(a1).unwrap().get_property("gate_status"),
        Some("approved")
    );

    // history is read row by row, per node
    let b_history = svc.history_by_node(b).await.expect("history_by_node");
    let actions: Vec<_> = b_history.iter().map(|e| e.action).collect();
    assert_eq!(
        actions,
        vec![
            ChangeAction::Create,
            ChangeAction::Update,
            ChangeAction::Move
        ]
    );
    assert!(b_history[1].before.as_deref().unwrap().contains("\"b\""));
    assert!(b_history[1].after.as_deref().unwrap().contains("\"b2\""));

    // a batch is one commit with N events
    let v_before = store.load(&slug).await.unwrap().unwrap().version;
    svc.batch_update(vec![(a, title_req("A")), (a1, title_req("A1"))])
        .await
        .expect("batch_update");
    let v_after = store.load(&slug).await.unwrap().unwrap().version;
    assert_eq!(v_after, v_before + 1);

    // a stale writer conflicts and stores nothing
    let stale = store.load(&slug).await.unwrap().unwrap();
    svc.update_node(a, title_req("winner"))
        .await
        .expect("winner");
    let events_before = svc.history().await.unwrap().len();
    let err = store
        .commit(&slug, Some(stale.version), &stale.value, &[])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict { .. }));
    assert_eq!(svc.history().await.unwrap().len(), events_before);
    assert_eq!(
        svc.read_tree().await.unwrap().get_node(a).unwrap().title(),
        "winner"
    );

    // a second create on the same slug conflicts
    let err = store
        .commit(&slug, None, &stale.value, &[])
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Conflict {
            expected: None,
            actual: Some(_)
        }
    ));

    // remove deletes the subtree rows
    svc.remove_node(a).await.expect("remove a");
    assert_eq!(svc.read_tree().await.unwrap().node_count(), 0);

    // missing book
    let missing = BookService::new(store.clone(), format!("{slug}-missing"));
    assert!(matches!(
        missing.read_tree().await.unwrap_err(),
        AppError::BookNotFound
    ));

    assert!(store
        .list_books()
        .await
        .unwrap()
        .iter()
        .any(|s| s.slug == slug));

    // cleanup (nodes and events cascade)
    let pool = sqlx::PgPool::connect(&url).await.expect("cleanup connect");
    sqlx::query("DELETE FROM books WHERE slug = $1")
        .bind(&slug)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
async fn bulk_commit_is_chunked_and_atomic_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!("OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres bulk test did NOT run");
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let slug = format!("_test-bulk-{}", std::process::id());

    // 2,500 nodes with ~1 KB bodies and 5,000 events with ~2 KB each: several
    // chunks by row count and by bytes, in one commit.
    let body = "x".repeat(1000);
    let mut book = TemplateBook::new("Bulk", 4);
    let mut ids = Vec::new();
    for i in 0..2500 {
        let mut req = add_req(None, &format!("n{i}"));
        req.body = Some(body.clone());
        ids.push(book.add_node(req).unwrap());
    }
    let payload = format!(r#"{{"body":"{body}"}}"#);
    let events: Vec<ChangeEntry> = (0..5000)
        .map(|i| {
            ChangeEntry::new(
                ids[i % ids.len()],
                ChangeAction::Update,
                Some(payload.clone()),
                Some(payload.clone()),
                Timestamp::from_millis(1_000 + i as i64),
            )
        })
        .collect();
    let committed = store
        .commit(&slug, None, &book, &events)
        .await
        .expect("bulk commit");
    let loaded = store.load(&slug).await.unwrap().expect("book stored");
    assert_eq!(loaded.value.node_count(), 2500);
    let history = store.history(&slug).await.unwrap();
    assert_eq!(history.len(), 5000);
    assert!(
        history
            .windows(2)
            .all(|w| w[0].timestamp.as_millis() < w[1].timestamp.as_millis()),
        "history keeps the order the events were given across chunks"
    );

    // A failure in the last chunk rolls back every earlier chunk of the commit.
    let mut more = loaded.value.clone();
    for i in 0..1200 {
        more.add_node(add_req(None, &format!("m{i}"))).unwrap();
    }
    let mut bad_events = events[..1200].to_vec();
    bad_events.push(ChangeEntry::new(
        ids[0],
        ChangeAction::Update,
        Some("{ not json".to_string()),
        None,
        Timestamp::from_millis(9_999_999),
    ));
    assert!(store
        .commit(&slug, Some(committed.version), &more, &bad_events)
        .await
        .is_err());
    let after = store.load(&slug).await.unwrap().expect("book still there");
    assert_eq!(after.version, committed.version, "version unchanged");
    assert_eq!(
        after.value.node_count(),
        2500,
        "no node from the failed commit"
    );
    assert_eq!(
        store.history(&slug).await.unwrap().len(),
        5000,
        "no event either"
    );

    assert!(store.delete_book(&slug).await.unwrap());
}

#[tokio::test]
async fn snapshots_restore_and_import_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!(
            "OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres snapshot/import test did NOT run"
        );
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    assert!(store.pending_migrations().await.unwrap().is_empty());

    let slug = format!("_test-snap-{}", std::process::id());
    let svc = BookService::new(store.clone(), slug.clone());
    svc.create_book("Snap Test", 4).await.expect("create_book");
    let (a, _) = svc.add_node(add_req(None, "a")).await.expect("add a");

    // snapshot, change, snapshot again: distinct, ordered, labelled
    let s1 = store
        .create(&slug, &svc.read_tree().await.unwrap(), Some("v1"))
        .await
        .expect("snapshot 1");
    svc.update_node(a, title_req("changed")).await.unwrap();
    let s2 = store
        .create(&slug, &svc.read_tree().await.unwrap(), None)
        .await
        .expect("snapshot 2");
    assert!(s2.timestamp > s1.timestamp);
    assert!(s1.size_bytes > 0);
    let listed = SnapshotStore::list(&store, &slug).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].timestamp, s2.timestamp, "newest first");
    assert_eq!(listed[1].label.as_deref(), Some("v1"));
    assert!(store.tag(&slug, s2.timestamp, "v2").await.unwrap());
    assert!(!store
        .tag(&slug, Timestamp::from_millis(1), "nope")
        .await
        .unwrap());
    assert!(store
        .insert(&slug, s1.timestamp, &TemplateBook::new("x", 4), None)
        .await
        .is_err());

    // restore = one commit of the old tree plus a Restore event per node
    let old = store.get(&slug, s1.timestamp).await.unwrap().expect("s1");
    let events_before = svc.history().await.unwrap().len();
    svc.restore_book(&old).await.expect("restore");
    let tree = svc.read_tree().await.unwrap();
    assert_eq!(tree.get_node(a).unwrap().title(), "a");
    let history = svc.history().await.unwrap();
    assert_eq!(history.len(), events_before + 1);
    assert_eq!(history.last().unwrap().action, ChangeAction::Restore);

    // import of a shelf keeps history and snapshot timestamps, and verifies
    let dir = tempfile::tempdir().unwrap();
    let shelf_slug = format!("{slug}-shelf");
    std::fs::write(
        dir.path().join(format!("{shelf_slug}.json")),
        serde_json::to_string(&tree).unwrap(),
    )
    .unwrap();
    let legacy = vec![ChangeEntry::new(
        a,
        ChangeAction::Create,
        None,
        None,
        Timestamp::from_millis(123),
    )];
    std::fs::write(
        dir.path().join(format!("{shelf_slug}.changelog.json")),
        serde_json::to_string(&legacy).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join(format!("{shelf_slug}.snap.4567.json")),
        serde_json::to_string(&old).unwrap(),
    )
    .unwrap();
    let source = read_shelf_book(dir.path(), &shelf_slug).await.unwrap();
    let report = import_book(&store, &store, source, false, |s| {
        let store = store.clone();
        async move { store.delete_book(&s).await }
    })
    .await
    .expect("import");
    assert!(report.matches(), "{report:?}");
    assert_eq!(
        BookService::new(store.clone(), shelf_slug.clone())
            .history()
            .await
            .unwrap()[0]
            .timestamp,
        Timestamp::from_millis(123)
    );

    // a second import refuses; --replace overwrites and still matches
    let again = read_shelf_book(dir.path(), &shelf_slug).await.unwrap();
    assert!(
        import_book(&store, &store, again, false, |_| async { Ok(true) })
            .await
            .is_err()
    );
    let again = read_shelf_book(dir.path(), &shelf_slug).await.unwrap();
    let report = import_book(&store, &store, again, true, |s| {
        let store = store.clone();
        async move { store.delete_book(&s).await }
    })
    .await
    .expect("replace");
    assert!(report.matches(), "{report:?}");

    // cleanup (nodes, events and snapshots cascade)
    assert!(store.delete_book(&slug).await.unwrap());
    assert!(store.delete_book(&shelf_slug).await.unwrap());
    assert!(SnapshotStore::list(&store, &slug).await.unwrap().is_empty());
}

#[tokio::test]
async fn delete_cascades_to_nodes_events_and_snapshots_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!("OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres delete test did NOT run");
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let pool = sqlx::PgPool::connect(&url).await.expect("count connect");
    let gone = format!("_test-del-gone-{}", std::process::id());
    let kept = format!("_test-del-kept-{}", std::process::id());

    for slug in [&gone, &kept] {
        let svc = BookService::new(store.clone(), slug.clone());
        svc.create_book("Del", 4).await.expect("create_book");
        let (a, _) = svc.add_node(add_req(None, "a")).await.expect("add a");
        svc.add_node(add_req(Some(a), "b")).await.expect("add b");
        let book = svc.read_tree().await.expect("read");
        SnapshotStore::create(&store, slug, &book, Some("s"))
            .await
            .expect("snapshot");
    }

    let rows = |table: &'static str, slug: String| {
        let pool = pool.clone();
        async move {
            let sql = match table {
                "nodes" => "SELECT count(*) FROM nodes WHERE book = $1",
                "events" => "SELECT count(*) FROM events WHERE book = $1",
                "snapshots" => "SELECT count(*) FROM snapshots WHERE book = $1",
                other => panic!("unexpected table {other}"),
            };
            let n: i64 = sqlx::query_scalar(sql)
                .bind(slug)
                .fetch_one(&pool)
                .await
                .expect("count");
            n
        }
    };
    assert_eq!(rows("nodes", gone.clone()).await, 2);
    assert_eq!(rows("events", gone.clone()).await, 2);
    assert_eq!(rows("snapshots", gone.clone()).await, 1);

    // A delete at a version read before a later write deletes nothing.
    let checked = BookStore::load(&store, &gone)
        .await
        .unwrap()
        .unwrap()
        .version;
    BookService::new(store.clone(), gone.clone())
        .add_node(add_req(None, "late"))
        .await
        .expect("late write");
    let err = BookStore::delete(&store, &gone, checked)
        .await
        .expect_err("stale delete");
    assert!(
        matches!(err, StoreError::Conflict { expected: Some(e), actual: Some(a) } if e == checked && a == checked + 1),
        "{err}"
    );
    assert_eq!(rows("nodes", gone.clone()).await, 3);
    assert_eq!(rows("events", gone.clone()).await, 3);
    assert_eq!(rows("snapshots", gone.clone()).await, 1);

    // The port's delete: one statement, everything of that book goes.
    assert!(BookStore::delete(&store, &gone, checked + 1)
        .await
        .expect("delete"));
    assert!(BookStore::load(&store, &gone).await.unwrap().is_none());
    for table in ["nodes", "events", "snapshots"] {
        assert_eq!(rows(table, gone.clone()).await, 0, "{table} rows left");
    }
    assert_eq!(SnapshotStore::delete_all(&store, &gone).await.unwrap(), 0);
    assert!(
        !BookStore::delete(&store, &gone, checked + 1).await.unwrap(),
        "already gone"
    );

    // The slug can be created again and starts empty.
    let again = BookService::new(store.clone(), gone.clone());
    again.create_book("Again", 4).await.expect("re-create");
    assert!(BookStore::history(&store, &gone).await.unwrap().is_empty());
    assert!(SnapshotStore::list(&store, &gone).await.unwrap().is_empty());
    assert_eq!(rows("nodes", gone.clone()).await, 0);
    let v = BookStore::load(&store, &gone)
        .await
        .unwrap()
        .unwrap()
        .version;
    assert!(BookStore::delete(&store, &gone, v).await.unwrap());

    // The other book is untouched.
    assert_eq!(rows("nodes", kept.clone()).await, 2);
    assert_eq!(rows("events", kept.clone()).await, 2);
    assert_eq!(rows("snapshots", kept.clone()).await, 1);
    assert_eq!(SnapshotStore::delete_all(&store, &kept).await.unwrap(), 1);
    assert_eq!(rows("snapshots", kept.clone()).await, 0);
    assert_eq!(
        rows("nodes", kept.clone()).await,
        2,
        "delete_all leaves the book"
    );

    let v = BookStore::load(&store, &kept)
        .await
        .unwrap()
        .unwrap()
        .version;
    assert!(BookStore::delete(&store, &kept, v).await.unwrap());
}

#[tokio::test]
async fn remove_records_every_removed_node_in_one_commit_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!(
            "OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres remove-history test did NOT run"
        );
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let slug = format!("_test-remove-hist-{}", std::process::id());

    let svc = BookService::new(store.clone(), slug.clone());
    svc.create_book("Rm", 4).await.expect("create_book");
    let (root, _) = svc.add_node(add_req(None, "root")).await.unwrap();
    let (child, _) = svc.add_node(add_req(Some(root), "child")).await.unwrap();
    let (grand, _) = svc.add_node(add_req(Some(child), "grand")).await.unwrap();
    let events_before = svc.history().await.unwrap().len();
    let version_before = BookStore::load(&store, &slug)
        .await
        .unwrap()
        .unwrap()
        .version;

    svc.remove_node(root).await.expect("remove");

    let history = svc.history().await.unwrap();
    let tail: Vec<_> = history[events_before..]
        .iter()
        .map(|e| (e.node_id, e.action, e.before.is_some()))
        .collect();
    assert_eq!(
        tail,
        vec![
            (root, ChangeAction::Delete, true),
            (child, ChangeAction::Delete, true),
            (grand, ChangeAction::Delete, true),
        ]
    );
    let version_after = BookStore::load(&store, &slug)
        .await
        .unwrap()
        .unwrap()
        .version;
    assert_eq!(version_after, version_before + 1, "one commit");
    assert_eq!(
        svc.history_by_node(grand)
            .await
            .unwrap()
            .last()
            .unwrap()
            .action,
        ChangeAction::Delete
    );

    assert!(BookStore::delete(&store, &slug, version_after)
        .await
        .unwrap());
}

#[tokio::test]
async fn import_book_commits_tree_and_history_together_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!(
            "OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres import_book test did NOT run"
        );
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let pool = sqlx::PgPool::connect(&url).await.expect("count connect");
    let slug = format!("_test-import-hist-{}", std::process::id());

    let svc = BookService::new(store.clone(), slug.clone());
    svc.create_book("Imp", 4).await.expect("create_book");
    let (a, _) = svc.add_node(add_req(None, "a")).await.expect("add a");
    let (b, _) = svc.add_node(add_req(None, "b")).await.expect("add b");
    let events_before: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE book = $1")
        .bind(&slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    let version_before = BookStore::load(&store, &slug)
        .await
        .unwrap()
        .unwrap()
        .version;

    let mut imported = svc.read_tree().await.unwrap();
    imported.update_node(a, title_req("a2")).unwrap();
    imported.remove_node(b).unwrap();
    let c = imported.add_node(add_req(None, "c")).unwrap();
    assert_eq!(svc.import_book(&imported).await.expect("import"), 3);

    let events_after: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE book = $1")
        .bind(&slug)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(events_after, events_before + 3);
    let version_after = BookStore::load(&store, &slug)
        .await
        .unwrap()
        .unwrap()
        .version;
    assert_eq!(version_after, version_before + 1, "one commit");

    let tail: Vec<_> = svc.history().await.unwrap()[events_before as usize..]
        .iter()
        .map(|e| (e.node_id, e.action))
        .collect();
    assert!(tail.contains(&(a, ChangeAction::Update)));
    assert!(tail.contains(&(b, ChangeAction::Delete)));
    assert!(tail.contains(&(c, ChangeAction::Create)));

    assert!(BookStore::delete(&store, &slug, version_after)
        .await
        .unwrap());
}

#[tokio::test]
async fn export_import_round_trip_keeps_curated_state_against_postgres() {
    use outline_mcp_core::application::eject::{EjectService, EjectTree, ImportOptions};
    use outline_mcp_core::domain::model::changelog::NodeStatus;

    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!(
            "OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres export/import test did NOT run"
        );
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let slug = format!("_test-export-import-{}", std::process::id());
    let copy_slug = format!("{slug}-copy");

    let svc = BookService::new(store.clone(), slug.clone());
    svc.create_book("Curated", 4).await.expect("create_book");
    let (live, _) = svc.add_node(add_req(None, "live")).await.unwrap();
    let (old, _) = svc.add_node(add_req(None, "old")).await.unwrap();
    let mut archive = title_req("old");
    archive.title = None;
    archive.status = Some(NodeStatus::Archived);
    svc.update_node(old, archive).await.unwrap();
    svc.verify_nodes(&[live], Some("alice".into()))
        .await
        .unwrap();
    svc.add_link(live, old, "supersedes").await.unwrap();
    svc.propose_revision(
        live,
        Revision {
            title: Some("live v2".into()),
            body: None,
            properties: None,
            properties_patch: None,
            base: String::new(),
            by: Some("bob".into()),
            note: None,
            at: Timestamp::now(),
        },
        false,
    )
    .await
    .unwrap();
    let before = svc.read_tree().await.unwrap();
    let history_before = svc.history().await.unwrap().len();

    // Re-import the book's own export: nothing changes, nothing is recorded.
    let tree: EjectTree =
        serde_json::from_str(&EjectService::render_json(&before, None).unwrap()).unwrap();
    let report = EjectService::import_tree_with(&tree, ImportOptions::default()).unwrap();
    assert_eq!(svc.import_book(&report.book).await.unwrap(), 0);
    assert_eq!(svc.history().await.unwrap().len(), history_before);
    let after = svc.read_tree().await.unwrap();
    for id in [live, old] {
        assert_eq!(
            serde_json::to_value(before.get_node(id).unwrap()).unwrap(),
            serde_json::to_value(after.get_node(id).unwrap()).unwrap()
        );
    }

    // A copy with new ids in another book keeps the link between its nodes.
    let copy = BookService::new(store.clone(), copy_slug.clone());
    let report = EjectService::import_tree_with(&tree, ImportOptions { new_ids: true }).unwrap();
    copy.import_book(&report.book).await.unwrap();
    let copied = copy.read_tree().await.unwrap();
    let by_title = |t: &str| {
        copied
            .all_nodes_dfs()
            .into_iter()
            .find(|n| n.title() == t)
            .unwrap()
            .id()
    };
    let (cl, co) = (by_title("live"), by_title("old"));
    assert_ne!(cl, live);
    let l = copied.get_node(cl).unwrap();
    assert_eq!(l.links()[0].target(), co);
    assert_eq!(l.verified_by(), Some("alice"));
    assert!(l.proposal().is_some());
    assert_eq!(copied.get_node(co).unwrap().status(), NodeStatus::Archived);

    for s in [&slug, &copy_slug] {
        let v = BookStore::load(&store, s).await.unwrap().unwrap().version;
        assert!(BookStore::delete(&store, s, v).await.unwrap());
    }
}

#[tokio::test]
async fn book_update_records_a_book_level_event_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!(
            "OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres book_update test did NOT run"
        );
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let pool = sqlx::PgPool::connect(&url).await.expect("count connect");
    let slug = format!("_test-book-update-{}", std::process::id());

    let svc = BookService::new(store.clone(), slug.clone());
    svc.create_book("Before", 3).await.expect("create_book");
    let (n, _) = svc.add_node(add_req(None, "n")).await.expect("add n");

    let up = svc
        .update_book(Some("After".into()), Some(5))
        .await
        .expect("update_book");
    assert!(up.changed);

    // Stored with a NULL node id, read back as the book-level id.
    let nulls: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE book = $1 AND node_id IS NULL AND action = 'book_update'",
    )
    .bind(&slug)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(nulls, 1);
    let last = svc.history().await.unwrap().pop().unwrap();
    assert_eq!(last.action, ChangeAction::BookUpdate);
    assert!(last.node_id.is_book_level());
    // A node's own history does not pick the book-level entry up.
    assert!(svc
        .history_by_node(n)
        .await
        .unwrap()
        .iter()
        .all(|e| e.action != ChangeAction::BookUpdate));

    let listed = BookStore::list_books(&store)
        .await
        .unwrap()
        .into_iter()
        .find(|b| b.slug == slug)
        .expect("listed");
    assert_eq!(listed.title, "After");
    let book = svc.read_tree().await.unwrap();
    assert_eq!(book.max_depth(), 5);

    let version = BookStore::load(&store, &slug)
        .await
        .unwrap()
        .unwrap()
        .version;
    assert!(BookStore::delete(&store, &slug, version).await.unwrap());
}

#[tokio::test]
async fn restore_records_the_revert_of_book_fields_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!(
            "OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres restore book-fields test did NOT run"
        );
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let slug = format!("_test-restore-fields-{}", std::process::id());

    let svc = BookService::new(store.clone(), slug.clone());
    svc.create_book("Kept", 3).await.expect("create_book");
    svc.add_node(add_req(None, "n")).await.expect("add n");
    let old = svc.read_tree().await.unwrap();
    svc.update_book(None, Some(5)).await.expect("raise");

    svc.restore_book(&old).await.expect("restore");
    let book_level: Vec<ChangeEntry> = svc
        .history()
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.action == ChangeAction::BookUpdate)
        .collect();
    assert_eq!(book_level.len(), 2, "the raise and the revert");
    let revert = book_level.last().unwrap();
    assert!(revert.node_id.is_book_level());
    assert!(revert
        .before
        .as_deref()
        .unwrap()
        .contains("\"max_depth\":5"));
    assert!(revert.after.as_deref().unwrap().contains("\"max_depth\":3"));
    assert_eq!(svc.read_tree().await.unwrap().max_depth(), 3);

    let version = BookStore::load(&store, &slug)
        .await
        .unwrap()
        .unwrap()
        .version;
    assert!(BookStore::delete(&store, &slug, version).await.unwrap());
}

#[tokio::test]
async fn snapshot_delete_removes_one_row_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!(
            "OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres snapshot delete test did NOT run"
        );
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let slug = format!("_test-snapdel-{}", std::process::id());
    let svc = BookService::new(store.clone(), slug.clone());
    svc.create_book("SnapDel", 4).await.expect("create_book");
    svc.add_node(add_req(None, "a")).await.expect("add a");
    let book = svc.read_tree().await.expect("read");
    let gone = SnapshotStore::create(&store, &slug, &book, None)
        .await
        .expect("snapshot 1");
    let kept = SnapshotStore::create(&store, &slug, &book, Some("keep"))
        .await
        .expect("snapshot 2");
    let version = BookStore::load(&store, &slug)
        .await
        .unwrap()
        .unwrap()
        .version;

    assert_eq!(
        SnapshotStore::delete(&store, &slug, gone.timestamp, false)
            .await
            .expect("delete"),
        SnapshotDelete::Deleted { label: None }
    );
    assert_eq!(
        SnapshotStore::delete(&store, &slug, gone.timestamp, false)
            .await
            .expect("delete again"),
        SnapshotDelete::NotFound
    );
    // The labeled one is refused without allow_labeled, in the same statement.
    assert_eq!(
        SnapshotStore::delete(&store, &slug, kept.timestamp, false)
            .await
            .expect("delete labeled"),
        SnapshotDelete::Labeled("keep".into())
    );
    let left: Vec<_> = SnapshotStore::list(&store, &slug)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.timestamp)
        .collect();
    assert_eq!(left, vec![kept.timestamp]);
    assert!(SnapshotStore::get(&store, &slug, gone.timestamp)
        .await
        .unwrap()
        .is_none());
    // The book is untouched: same version, same node.
    let after = BookStore::load(&store, &slug).await.unwrap().unwrap();
    assert_eq!(after.version, version);
    assert_eq!(after.value.node_count(), 1);

    assert!(BookStore::delete(&store, &slug, version).await.unwrap());
}

#[tokio::test]
async fn snapshot_delete_stays_inside_its_book_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!(
            "OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres snapshot scoping test did NOT run"
        );
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate");
    let pid = std::process::id();
    let (s, t) = (
        format!("_test-snapscope-s-{pid}"),
        format!("_test-snapscope-t-{pid}"),
    );
    for slug in [&s, &t] {
        BookService::new(store.clone(), slug.clone())
            .create_book("Scope", 4)
            .await
            .expect("create_book");
    }
    let book = TemplateBook::new("Scope", 4);
    let ts = Timestamp::from_millis(1_000);
    // Two books with a snapshot at the very same timestamp.
    SnapshotStore::insert(&store, &s, ts, &book, Some("keep"))
        .await
        .expect("insert s");
    SnapshotStore::insert(&store, &t, ts, &book, None)
        .await
        .expect("insert t");

    assert_eq!(
        SnapshotStore::delete(&store, &s, ts, true)
            .await
            .expect("delete s"),
        SnapshotDelete::Deleted {
            label: Some("keep".into())
        }
    );
    assert!(SnapshotStore::get(&store, &s, ts).await.unwrap().is_none());
    assert!(
        SnapshotStore::get(&store, &t, ts).await.unwrap().is_some(),
        "the other book's snapshot at the same timestamp stays"
    );

    for slug in [&s, &t] {
        let v = BookStore::load(&store, slug)
            .await
            .unwrap()
            .unwrap()
            .version;
        assert!(BookStore::delete(&store, slug, v).await.unwrap());
    }
}

#[tokio::test]
async fn archived_status_and_verification_round_trip_against_postgres() {
    use outline_mcp_core::domain::model::changelog::NodeStatus;

    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!(
            "OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres archived/verify test did NOT run"
        );
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate (incl. 0003)");
    let slug = format!("_test-archived-verify-{}", std::process::id());

    let svc = BookService::new(store.clone(), slug.clone());
    svc.create_book("AV", 4).await.expect("create_book");
    let (a, _) = svc.add_node(add_req(None, "a")).await.unwrap();
    let (b, _) = svc.add_node(add_req(None, "b")).await.unwrap();

    let mut archive = title_req("a");
    archive.title = None;
    archive.status = Some(NodeStatus::Archived);
    svc.update_node(a, archive).await.expect("archive");
    let at = svc
        .verify_nodes(&[b], Some("pg-tester".into()))
        .await
        .expect("verify");

    // Read back through a fresh load: both survive the row mapping.
    let book = BookStore::load(&store, &slug).await.unwrap().unwrap().value;
    assert_eq!(book.get_node(a).unwrap().status(), NodeStatus::Archived);
    assert!(book.get_node(a).unwrap().verified_at().is_none());
    let nb = book.get_node(b).unwrap();
    assert_eq!(nb.verified_at(), Some(at));
    assert_eq!(nb.verified_by(), Some("pg-tester"));
    assert_eq!(nb.status(), NodeStatus::Active);
    assert_eq!(
        svc.history_by_node(b).await.unwrap().last().unwrap().action,
        ChangeAction::Update
    );

    let v = BookStore::load(&store, &slug)
        .await
        .unwrap()
        .unwrap()
        .version;
    assert!(BookStore::delete(&store, &slug, v).await.unwrap());
}

#[tokio::test]
async fn links_round_trip_and_follow_removal_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!("OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres links test did NOT run");
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate (incl. 0004)");
    let slug = format!("_test-links-{}", std::process::id());

    let svc = BookService::new(store.clone(), slug.clone());
    svc.create_book("Links", 4).await.expect("create_book");
    let (a, _) = svc.add_node(add_req(None, "a")).await.unwrap();
    let (b, _) = svc.add_node(add_req(None, "b")).await.unwrap();
    let (c, _) = svc.add_node(add_req(None, "c")).await.unwrap();

    // A link-only change must reach the row (NodeRow equality covers links).
    let v0 = BookStore::load(&store, &slug)
        .await
        .unwrap()
        .unwrap()
        .version;
    assert!(svc.add_link(a, b, "supersedes").await.unwrap());
    assert!(svc.add_link(c, b, "see_also").await.unwrap());
    let loaded = BookStore::load(&store, &slug).await.unwrap().unwrap();
    assert_eq!(loaded.version, v0 + 2);
    let book = loaded.value;
    let la = book.get_node(a).unwrap().links();
    assert_eq!(la.len(), 1);
    assert_eq!((la[0].rel(), la[0].target()), ("supersedes", b));
    assert_eq!(book.backlinks(b).len(), 2);

    // Removing the target drops the incoming links from a and c in the same
    // commit, with an update entry for each.
    let base = svc.history().await.unwrap().len();
    svc.remove_node(b).await.expect("remove b");
    let h = svc.history().await.unwrap();
    let new: Vec<_> = h[base..].iter().map(|e| (e.node_id, e.action)).collect();
    assert_eq!(
        new,
        vec![
            (b, ChangeAction::Delete),
            (a, ChangeAction::Update),
            (c, ChangeAction::Update)
        ]
    );
    let book = BookStore::load(&store, &slug).await.unwrap().unwrap().value;
    assert_eq!(book.link_count(), 0);

    let v = BookStore::load(&store, &slug)
        .await
        .unwrap()
        .unwrap()
        .version;
    assert!(BookStore::delete(&store, &slug, v).await.unwrap());
}

#[tokio::test]
async fn proposals_round_trip_and_approve_against_postgres() {
    let Ok(url) = std::env::var("OUTLINE_MCP_TEST_DATABASE_URL") else {
        eprintln!("OUTLINE_MCP_TEST_DATABASE_URL not set; the Postgres proposals test did NOT run");
        return;
    };
    let store = PgBookStore::connect(&url).await.expect("connect");
    store.migrate().await.expect("migrate (incl. 0005)");
    let slug = format!("_test-proposals-{}", std::process::id());

    let svc = BookService::new(store.clone(), slug.clone());
    svc.create_book("Proposals", 4).await.expect("create_book");
    let mut req = add_req(None, "n");
    req.body = Some("v1".to_string());
    let (n, _) = svc.add_node(req).await.unwrap();

    // A proposal-only change must reach the row (NodeRow equality covers it)
    // and come back intact.
    let v0 = BookStore::load(&store, &slug)
        .await
        .unwrap()
        .unwrap()
        .version;
    let revision = Revision {
        title: Some("n2".to_string()),
        body: Some(BodyChange::Set("v2".to_string())),
        properties: None,
        properties_patch: Some([("scope".to_string(), "x".to_string())].into()),
        base: String::new(),
        by: Some("alice".to_string()),
        note: Some("why".to_string()),
        at: Timestamp::now(),
    };
    let stored = svc
        .propose_revision(n, revision, false)
        .await
        .expect("propose");
    let loaded = BookStore::load(&store, &slug).await.unwrap().unwrap();
    assert_eq!(loaded.version, v0 + 1);
    let node = loaded.value.get_node(n).unwrap().clone();
    assert_eq!(node.body(), Some("v1"), "content unchanged");
    assert_eq!(node.proposal(), Some(&stored), "proposal round-trips");

    // Approve: one commit, one update entry, content applied, proposal gone.
    let base = svc.history().await.unwrap().len();
    svc.approve_revision(n, false).await.expect("approve");
    let h = svc.history().await.unwrap();
    assert_eq!(h.len(), base + 1);
    assert_eq!((h[base].node_id, h[base].action), (n, ChangeAction::Update));
    let loaded = BookStore::load(&store, &slug).await.unwrap().unwrap();
    assert_eq!(loaded.version, v0 + 2);
    let node = loaded.value.get_node(n).unwrap();
    assert_eq!((node.title(), node.body()), ("n2", Some("v2")));
    assert_eq!(node.get_property("scope"), Some("x"));
    assert!(node.proposal().is_none());

    assert!(BookStore::delete(&store, &slug, loaded.version)
        .await
        .unwrap());
}
