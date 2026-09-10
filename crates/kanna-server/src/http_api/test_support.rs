use super::router;
use super::state::{
    AppState, TestMergeAgentRunner, TestRevisionRequester, TestStageAdvancer, TestStageCompleter,
    TestStageRerunner, TestTaskCloser, TestTaskCreator, TestTaskInputSender,
};
use crate::config::Config;
use crate::db::Db;
use axum::Router;
use std::sync::Arc;

/// A revision route returns before its detached worker releases ownership.
/// Tests that subsequently deliver a provider notice must wait for that
/// release, not race the notice against the preceding stage transition.
pub(crate) async fn wait_for_task_mutation_to_finish(state: &AppState, task_id: &str) {
    let _mutation = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        state.begin_requested_task_mutation(task_id),
    )
    .await
    .expect("the detached task mutation did not finish");
}

fn seed_test_mutation_tasks(config: &Config, task_ids: &[&str]) {
    let db = Db::open_for_tests(&config.db_path).expect("open test db");
    db.insert_test_repo("repo-test-mutation", "Mutation Test Repo")
        .expect("insert mutation test repo");
    for task_id in task_ids {
        db.insert_test_pipeline_item(
            task_id,
            "repo-test-mutation",
            "mutation test task",
            Some("Mutation Test Task"),
            "in progress",
            "2026-07-26 00:00:00",
        )
        .expect("insert mutation test task");
    }
}

pub(crate) fn test_router(desktop_id: &str, desktop_name: &str) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings", "json"),
    };
    let _ = Db::open_for_tests(&config.db_path).expect("open test db");
    router(Arc::new(AppState::new(config)))
}

pub(super) fn test_router_with_repo_checkout_root(
    desktop_id: &str,
    desktop_name: &str,
    repo_checkout_root: std::path::PathBuf,
) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings", "json"),
    };
    let _ = Db::open_for_tests(&config.db_path).expect("open test db");
    let mut state = AppState::new(config);
    state.repo_checkout_root = repo_checkout_root;
    router(Arc::new(state))
}

pub(super) fn test_router_with_seed(
    desktop_id: &str,
    desktop_name: &str,
    seed: impl FnOnce(&Db),
) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings", "json"),
    };
    let db = Db::open_for_tests(&config.db_path).expect("open test db");
    seed(&db);
    router(Arc::new(AppState::new(config)))
}

pub(crate) fn test_state_with_seed(
    desktop_id: &str,
    desktop_name: &str,
    seed: impl FnOnce(&Db),
) -> Arc<AppState> {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-invoke-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings-invoke", "json"),
    };
    let db = Db::open_for_tests(&config.db_path).expect("open test db");
    seed(&db);
    Arc::new(AppState::new(config))
}

/// A state whose daemon socket is test-local, for tests that script a fake
/// daemon and need the code under test to dial *that* one.
pub(crate) fn test_state_with_daemon_dir(
    desktop_id: &str,
    desktop_name: &str,
    daemon_dir: &str,
    seed: impl FnOnce(&Db),
) -> Arc<AppState> {
    test_state_with_daemon_dir_and_debounce(desktop_id, desktop_name, daemon_dir, 300, seed)
}

pub(crate) fn test_state_with_daemon_dir_and_debounce(
    desktop_id: &str,
    desktop_name: &str,
    daemon_dir: &str,
    activity_event_debounce_seconds: u64,
    seed: impl FnOnce(&Db),
) -> Arc<AppState> {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: daemon_dir.to_string(),
        db_path: Db::test_db_path(&format!("daemon-dir-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings-daemon", "json"),
    };
    let db = Db::open_for_tests(&config.db_path).expect("open test db");
    seed(&db);
    Arc::new(AppState::new(config))
}

pub(super) fn test_state_with_task_input_sender(
    desktop_id: &str,
    desktop_name: &str,
    task_input_sender: TestTaskInputSender,
) -> Arc<AppState> {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-invoke-input-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file(
            "kanna-pairings-invoke-input",
            "json",
        ),
    };
    let _ = Db::open_for_tests(&config.db_path).expect("open test db");
    Arc::new(AppState::with_task_input_sender(config, task_input_sender))
}

pub(super) fn test_state_with_seed_and_task_input_sender(
    desktop_id: &str,
    desktop_name: &str,
    seed: impl FnOnce(&Db),
    task_input_sender: TestTaskInputSender,
) -> Arc<AppState> {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-invoke-input-seeded-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file(
            "kanna-pairings-invoke-input-seeded",
            "json",
        ),
    };
    let db = Db::open_for_tests(&config.db_path).expect("open test db");
    seed(&db);
    Arc::new(AppState::with_task_input_sender(config, task_input_sender))
}

pub(super) fn test_router_with_task_creator(
    desktop_id: &str,
    desktop_name: &str,
    task_creator: TestTaskCreator,
) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings", "json"),
    };
    let _ = Db::open_for_tests(&config.db_path).expect("open test db");
    router(Arc::new(AppState::with_task_creator(config, task_creator)))
}

pub(super) fn test_router_with_seed_and_task_creator(
    desktop_id: &str,
    desktop_name: &str,
    seed: impl FnOnce(&Db),
    task_creator: TestTaskCreator,
) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings", "json"),
    };
    let db = Db::open_for_tests(&config.db_path).expect("open test db");
    seed(&db);
    router(Arc::new(AppState::with_task_creator(config, task_creator)))
}

pub(super) fn test_router_with_merge_agent_runner(
    desktop_id: &str,
    desktop_name: &str,
    merge_agent_runner: TestMergeAgentRunner,
) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings", "json"),
    };
    let _ = Db::open_for_tests(&config.db_path).expect("open test db");
    router(Arc::new(AppState::with_merge_agent_runner(
        config,
        merge_agent_runner,
    )))
}

pub(super) fn test_router_with_task_input_sender(
    desktop_id: &str,
    desktop_name: &str,
    task_input_sender: TestTaskInputSender,
) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings", "json"),
    };
    let _ = Db::open_for_tests(&config.db_path).expect("open test db");
    router(Arc::new(AppState::with_task_input_sender(
        config,
        task_input_sender,
    )))
}

pub(super) fn test_router_with_task_closer(
    desktop_id: &str,
    desktop_name: &str,
    task_closer: TestTaskCloser,
) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings", "json"),
    };
    seed_test_mutation_tasks(&config, &["task-1", "a1b2c3d4"]);
    router(Arc::new(AppState::with_task_closer(config, task_closer)))
}

pub(super) fn test_router_with_stage_advancer(
    desktop_id: &str,
    desktop_name: &str,
    stage_advancer: TestStageAdvancer,
) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings", "json"),
    };
    seed_test_mutation_tasks(&config, &["task-1"]);
    router(Arc::new(AppState::with_stage_advancer(
        config,
        stage_advancer,
    )))
}

pub(super) fn test_router_with_stage_rerunner(
    desktop_id: &str,
    desktop_name: &str,
    stage_rerunner: TestStageRerunner,
) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-rerun-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings-rerun", "json"),
    };
    seed_test_mutation_tasks(&config, &["task-1"]);
    router(Arc::new(AppState::with_stage_rerunner(
        config,
        stage_rerunner,
    )))
}

pub(super) fn test_router_with_stage_completer(
    desktop_id: &str,
    desktop_name: &str,
    stage_completer: TestStageCompleter,
) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings", "json"),
    };
    seed_test_mutation_tasks(&config, &["task-1"]);
    router(Arc::new(AppState::with_stage_completer(
        config,
        stage_completer,
    )))
}

pub(super) fn test_router_with_revision_requester(
    desktop_id: &str,
    desktop_name: &str,
    revision_requester: TestRevisionRequester,
) -> Router {
    let config = Config {
        relay_url: "wss://relay.example".to_string(),
        device_token: "device-token".to_string(),
        firebase_project_id: "kanna-local".to_string(),
        firebase_auth_emulator_url: None,
        firebase_firestore_emulator_host: None,
        daemon_dir: crate::test_paths::unique_test_path_string("kanna-daemon"),
        db_path: Db::test_db_path(&format!("http-api-{desktop_id}")),
        kanna_cli_path: None,
        desktop_id: desktop_id.to_string(),
        desktop_secret: Some("desktop-secret".to_string()),
        desktop_name: desktop_name.to_string(),
        version: "test-version".to_string(),
        environment: "development".to_string(),
        lan_host: "0.0.0.0".to_string(),
        lan_port: 48120,
        transfer_port: 4455,
        activity_event_debounce_seconds: 300,
        pairing_store_path: crate::test_paths::unique_test_file("kanna-pairings", "json"),
    };
    seed_test_mutation_tasks(&config, &["review-task"]);
    router(Arc::new(AppState::with_revision_requester(
        config,
        revision_requester,
    )))
}
