//! Integration test against a real Postgres. Does not run (returns early and
//! says so on stderr) unless `OUTLINE_MCP_TEST_DATABASE_URL` is set; point it
//! at a throwaway database. The test uses slugs unique to this process and
//! deletes them at the end.

use outline_mcp_core::application::error::AppError;
use outline_mcp_core::application::service::BookService;
use outline_mcp_core::domain::model::book::{AddNodeRequest, TemplateBook, UpdateNodeRequest};
use outline_mcp_core::domain::model::changelog::{ChangeAction, ChangeEntry};
use outline_mcp_core::domain::model::node::NodeType;
use outline_mcp_core::domain::model::timestamp::Timestamp;
use outline_mcp_core::domain::store::{BookStore, SnapshotStore, StoreError};
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
