use omon_gateway::slack::{SlackPairingOutcome, SlackPairingStore};
use omon_gateway::Database;

async fn store() -> (Database, SlackPairingStore) {
    let database = Database::connect("sqlite::memory:").await.unwrap();
    let store = SlackPairingStore::new(database.pool().clone());
    store.init_cache().await.unwrap();
    (database, store)
}

#[tokio::test]
async fn request_then_approve_pairs_slack_user() {
    let (_db, store) = store().await;
    assert!(!store.is_user_paired_sync("U1"));

    let code = store.request_pairing_code("U1").await.unwrap();
    assert!(code.contains('-'), "formatted code expected, got {code}");

    match store.approve_code(&code).await.unwrap() {
        SlackPairingOutcome::Success { user_id } => assert_eq!(user_id, "U1"),
        other => panic!("expected success, got {other:?}"),
    }
    assert!(store.is_user_paired_sync("U1"));
    assert!(store.get_paired_user_ids_sync().contains(&"U1".to_string()));

    match store.approve_code(&code).await.unwrap() {
        SlackPairingOutcome::InvalidCode => {}
        _ => panic!("consumed code must not approve twice"),
    }
}

#[tokio::test]
async fn unknown_code_is_invalid_and_counts_attempts() {
    let (_db, store) = store().await;
    match store.approve_code("ZZZZ-ZZZZ").await.unwrap() {
        SlackPairingOutcome::InvalidCode => {}
        _ => panic!("unknown code must be invalid"),
    }
}

#[tokio::test]
async fn discord_bound_codes_are_not_slack_approvable() {
    let (_db, store) = store().await;
    let discord_store = omon_gateway::PairingStore::new(_db.pool().clone());
    let code = discord_store.request_pairing_code(42).await.unwrap();
    match store.approve_code(&code).await.unwrap() {
        SlackPairingOutcome::InvalidCode => {}
        SlackPairingOutcome::Success { .. } => {
            panic!("slack approve must reject discord-bound codes")
        }
        _ => panic!("expected invalid code for discord-bound code"),
    }
}

#[tokio::test]
async fn cache_reload_picks_up_persisted_pairings() {
    let (db, store) = store().await;
    let code = store.request_pairing_code("U9").await.unwrap();
    store.approve_code(&code).await.unwrap();

    let fresh = SlackPairingStore::new(db.pool().clone());
    fresh.init_cache().await.unwrap();
    assert!(fresh.is_user_paired_sync("U9"));
    assert!(!fresh.is_user_paired_sync("U8"));
}
