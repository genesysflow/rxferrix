//! Backups configured from the dashboard (`_settings` row `backups`, `rxferrix::settings::backups`): the schedule
//! becomes the `ferrix-backup` cron at runtime (changed, kept across restarts, withdrawn), the destination is used by
//! scheduled and manual backups, an S3 destination uses its own credentials (encrypted, masked, never logged), and
//! the command line wins.

use std::time::Duration;

use rxferrix::AppOptions;
use rxferrix::auth::AuthConfig;
use rxferrix::ops::OpsOptions;
use rxferrix::ops::backup::{CRON_NAME, ScheduledBackup};
use rxferrix::testing::TestApp;
use serde_json::{Value, json};

use crate::authkit::{Http, code_of, token};
use crate::storage::fake_s3;

const MASK: &str = "••••";

fn options() -> AppOptions {
    AppOptions { auth: AuthConfig::password().rate_limits(None), dev: true, ..Default::default() }
}

async fn sign_in_admin(t: &TestApp) -> (Http, String) {
    let app = t.app().clone();
    let http = Http::new(t.serve().await.unwrap());
    let _ = app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await;
    let admin = token(&http.admin_sign_in("admin@example.com", "admin password").await, "accessToken");
    (http, admin)
}

async fn update(http: &Http, admin: &str, args: Value) -> (u16, Value) {
    http.call("action", "_system/settings:update", args, Some(admin)).await
}

/// The `ferrix-backup` row on the Scheduler page, if any.
async fn backup_cron(http: &Http, admin: &str) -> Option<Value> {
    let crons = http.ok("query", "_system/scheduler:crons", json!({}), admin).await;
    crons.as_array().unwrap().iter().find(|c| c["name"] == CRON_NAME).cloned()
}

async fn listing(http: &Http, admin: &str) -> Value {
    let (s, b) = http.get("/api/admin/backups", Some(admin)).await;
    assert_eq!(s, 200, "{b}");
    b
}

fn scheduled(listing: &Value) -> usize {
    listing["backups"].as_array().unwrap().iter().filter(|b| b["scheduled"] == true).count()
}

#[tokio::test]
async fn a_schedule_set_from_the_dashboard_runs_changes_survives_a_restart_and_goes_away() {
    let t = TestApp::with_options(options()).await.unwrap();
    let (http, admin) = sign_in_admin(&t).await;
    let elsewhere = tempfile::tempdir().unwrap();
    let path = elsewhere.path().to_str().unwrap().to_string();
    assert!(backup_cron(&http, &admin).await.is_none());

    let (s, view) = update(
        &http,
        &admin,
        json!({ "backups": { "everySecs": 3600, "keep": 2, "includeFiles": true,
                             "destination": { "kind": "local", "path": path } } }),
    )
    .await;
    assert_eq!(s, 200, "{view}");
    let view = &view["value"];
    assert_eq!(view["backups"]["destination"], json!({ "kind": "local", "path": path }));
    assert_eq!(view["effective"]["backups"]["target"], path);
    assert_eq!(view["effective"]["backups"]["everySecs"], 3600);
    assert!(!view["locked"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().starts_with("backups")));

    // The cron is there at once, and runs: three runs keep the newest two.
    let cron = backup_cron(&http, &admin).await.expect("the backup cron");
    assert_eq!(cron["schedule"], "every 1h");
    for _ in 0..3 {
        assert_eq!(t.advance_time(Duration::from_secs(3601)).await.unwrap(), 1);
    }
    let list = listing(&http, &admin).await;
    assert_eq!((list["target"].as_str().unwrap(), scheduled(&list)), (path.as_str(), 2), "{list}");
    // Manual backups from the dashboard go there too.
    let (s, made) = http.post("/api/admin/backups", Some(&admin), json!({})).await;
    assert_eq!(s, 201, "{made}");
    assert!(elsewhere.path().join(made["name"].as_str().unwrap()).exists());

    // A new interval reschedules it.
    let (s, _) = update(
        &http,
        &admin,
        json!({ "backups": { "everySecs": 7200, "keep": 2, "destination": { "kind": "local", "path": path } } }),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(backup_cron(&http, &admin).await.unwrap()["schedule"], "every 2h");

    // After a restart the schedule and the destination still apply.
    let t = t.restart().await.unwrap();
    let (http, admin) = sign_in_admin(&t).await;
    assert_eq!(listing(&http, &admin).await["target"], path);
    t.advance_time(Duration::from_secs(1)).await.unwrap();
    assert_eq!(backup_cron(&http, &admin).await.unwrap()["schedule"], "every 2h");

    // Clearing the setting withdraws the cron and goes back to <data dir>/backups.
    let (s, view) = update(&http, &admin, json!({ "clear": ["backups"] })).await;
    assert_eq!(s, 200, "{view}");
    assert!(backup_cron(&http, &admin).await.is_none());
    assert!(listing(&http, &admin).await["target"].as_str().unwrap().ends_with("backups"));
    assert_eq!(view["value"]["effective"]["backups"]["everySecs"], Value::Null);
}

#[tokio::test]
async fn backup_settings_are_validated_and_admin_only() {
    let t = TestApp::with_options(options()).await.unwrap();
    let (http, admin) = sign_in_admin(&t).await;
    let local = |every: Value, keep: Value, path: &str| json!({ "backups": { "everySecs": every, "keep": keep, "destination": { "kind": "local", "path": path } } });
    let s3 = |d: Value| json!({ "backups": { "everySecs": null, "destination": d } });
    for args in [
        local(json!(30), json!(7), "/tmp/x"),
        local(json!(3600), json!(0), "/tmp/x"),
        local(json!(3600), json!(7), "relative/dir"),
        s3(json!({ "kind": "s3", "bucket": "UPPER_case" })),
        s3(json!({ "kind": "s3", "bucket": "fine-bucket", "prefix": "a/../b" })),
        s3(json!({ "kind": "s3", "bucket": "fine-bucket", "endpoint": "ftp://example.com" })),
        s3(json!({ "kind": "s3", "bucket": "fine-bucket", "accessKeyId": "AKID" })),
    ] {
        let (s, b) = update(&http, &admin, args.clone()).await;
        assert_eq!((s, code_of(&b)), (400, "invalid_settings"), "{args}: {b}");
    }
    let user = token(&http.sign_up("u@example.com", "correct horse").await, "accessToken");
    let (_, b) = http.call("action", "_system/settings:test_backup_destination", json!({}), Some(&user)).await;
    assert_eq!(code_of(&b), "forbidden");
    // The destination in effect (the data directory) passes the check.
    let at = http.ok("action", "_system/settings:test_backup_destination", json!({}), &admin).await;
    assert!(at.as_str().unwrap().ends_with("backups"), "{at}");
}

#[tokio::test]
async fn an_s3_destination_uses_its_own_credentials_encrypted_and_never_shown() {
    let (endpoint, fake) = fake_s3::start().await;
    let t = TestApp::with_options(options()).await.unwrap();
    let (http, admin) = sign_in_admin(&t).await;
    let destination = json!({ "kind": "s3", "bucket": "backups-bucket", "prefix": "/prod/db/", "region": fake_s3::REGION,
                              "endpoint": endpoint, "accessKeyId": fake_s3::KEY_ID });
    let input = json!({ "everySecs": 3600, "keep": 1, "destination": destination, "secretAccessKey": fake_s3::SECRET });

    // Checked before it is saved: a probe object is written and removed.
    let at = http.ok("action", "_system/settings:test_backup_destination", json!({ "backups": input }), &admin).await;
    assert_eq!(at, "s3://backups-bucket/prod/db");
    assert!(fake.objects.lock().unwrap().is_empty());

    let (s, view) = update(&http, &admin, json!({ "backups": input })).await;
    assert_eq!(s, 200, "{view}");
    assert_eq!(view["value"]["backups"]["secretAccessKey"], MASK);
    assert_eq!(view["value"]["backups"]["destination"]["prefix"], "prod/db");
    assert_eq!(view["value"]["effective"]["backups"]["target"], "s3://backups-bucket/prod/db");

    // A manual backup, then two scheduled ones (keep 1): in the bucket, listed, downloadable.
    let (s, made) = http.post("/api/admin/backups", Some(&admin), json!({})).await;
    assert_eq!(s, 201, "{made}");
    t.advance_time(Duration::from_secs(3601)).await.unwrap();
    t.advance_time(Duration::from_secs(3601)).await.unwrap();
    let keys: Vec<String> = fake.objects.lock().unwrap().keys().cloned().collect();
    assert_eq!(keys.len(), 2, "the manual backup and the newest scheduled one: {keys:?}");
    assert!(keys.iter().all(|k| k.starts_with("/backups-bucket/prod/db/ferrix-")), "{keys:?}");
    let list = listing(&http, &admin).await;
    assert_eq!((list["target"].as_str().unwrap(), scheduled(&list)), ("s3://backups-bucket/prod/db", 1), "{list}");
    let name = made["name"].as_str().unwrap();
    let res = reqwest::Client::new()
        .get(format!("{}/api/admin/backups/{name}", http.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let stored = fake.objects.lock().unwrap().get(&format!("/backups-bucket/prod/db/{name}")).unwrap().clone();
    assert_eq!(res.bytes().await.unwrap(), stored);
    // Every request was signed with the dashboard's key.
    let signers = fake.signed_by.lock().unwrap().clone();
    assert!(!signers.is_empty() && signers.iter().all(|k| k == fake_s3::KEY_ID), "{signers:?}");

    // Sending the mask back keeps the secret; the cron's arguments never hold it.
    let mut keep = input.clone();
    keep["secretAccessKey"] = json!(MASK);
    keep["keep"] = json!(3);
    let (s, _) = update(&http, &admin, json!({ "backups": keep })).await;
    assert_eq!(s, 200);
    let cron = backup_cron(&http, &admin).await.unwrap();
    assert!(!cron.to_string().contains(fake_s3::SECRET), "{cron}");
    assert_eq!(cron["args"]["schedule"]["keep"], 3);
    let (s, _) = http.post("/api/admin/backups", Some(&admin), json!({})).await;
    assert_eq!(s, 201, "the kept secret still works");

    // Not in the audit log, the execution log, or any file of the data directory.
    let audit = http
        .ok("query", "_system/audit:list", json!({ "paginationOpts": { "numItems": 50, "cursor": null } }), &admin)
        .await;
    assert!(!audit.to_string().contains(fake_s3::SECRET));
    let (_, logs) = http.get("/api/admin/logs?path=_system/settings:update", Some(&admin)).await;
    assert!(!logs.to_string().contains(fake_s3::SECRET), "{logs}");
    t.app().logs().flush().await.unwrap();
    for entry in std::fs::read_dir(t.dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            let secret = fake_s3::SECRET.as_bytes();
            assert!(!bytes.windows(secret.len()).any(|w| w == secret), "{} holds the secret", path.display());
        }
    }
}

#[tokio::test]
async fn the_command_line_wins_over_the_backup_settings() {
    let cli_dir = tempfile::tempdir().unwrap();
    let mut options = options();
    options.ops = OpsOptions { backup_target: Some(cli_dir.path().to_str().unwrap().into()), ..Default::default() };
    let schedule = ScheduledBackup { to: None, keep: 4, include_aux: false, include_files: false };
    options.scheduler.crons.push(schedule.cron(Duration::from_secs(6 * 3600)));
    let t = TestApp::with_options(options).await.unwrap();
    let (http, admin) = sign_in_admin(&t).await;

    let (s, view) = update(
        &http,
        &admin,
        json!({ "backups": { "everySecs": 600, "keep": 9, "destination": { "kind": "local", "path": "/tmp/ignored" } } }),
    )
    .await;
    assert_eq!(s, 200, "{view}");
    let view = &view["value"];
    let locked: Vec<&str> = view["locked"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
    assert!(locked.contains(&"backups.schedule") && locked.contains(&"backups.destination"), "{locked:?}");
    // Stored, but what runs is the command line's.
    assert_eq!(view["backups"]["everySecs"], 600);
    let effective = &view["effective"]["backups"];
    assert_eq!((effective["everySecs"].as_u64(), effective["keep"].as_u64()), (Some(6 * 3600), Some(4)), "{effective}");
    assert_eq!(effective["target"], cli_dir.path().to_str().unwrap());
    t.advance_time(Duration::from_secs(1)).await.unwrap();
    assert_eq!(backup_cron(&http, &admin).await.unwrap()["schedule"], "every 6h");
    assert_eq!(listing(&http, &admin).await["target"], cli_dir.path().to_str().unwrap());
}
