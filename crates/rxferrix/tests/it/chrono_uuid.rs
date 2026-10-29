//! The `chrono` and `uuid` features: their types in a table and in a function signature, round-tripped through the
//! database, typed as `string` in the generated TypeScript, and stored as text in the shape (analytics).

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use rxferrix::testing::TestApp;
use uuid::Uuid;

pub mod events {
    use super::*;
    use rxferrix::prelude::*;

    #[table(name = "cu_events")]
    pub struct Event {
        pub key: Uuid,
        pub at: DateTime<Utc>,
        pub day: NaiveDate,
    }

    #[mutation]
    pub async fn add(ctx: &MutationCtx, key: Uuid, at: DateTime<Utc>, day: NaiveDate) -> Result<Id<Event>> {
        ctx.db.insert(Event { key, at, day }).await
    }

    #[query]
    pub async fn get(ctx: &QueryCtx, id: Id<Event>) -> Result<Option<Doc<Event>>> {
        ctx.db.get(id).await
    }
}

#[tokio::test]
async fn chrono_and_uuid_types_round_trip_and_are_strings_in_typescript() {
    let t = TestApp::new().await.unwrap();
    let key = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    let at = Utc.with_ymd_and_hms(2026, 10, 29, 12, 30, 0).unwrap();
    let day = NaiveDate::from_ymd_opt(2026, 10, 29).unwrap();
    let id = t.mutation::<events::add>((key, at, day)).await.unwrap();
    let doc = t.query::<events::get>((id,)).await.unwrap().unwrap();
    assert_eq!((doc.key, doc.at, doc.day), (key, at, day));

    let files =
        rxferrix::codegen::generate(rxferrix::registry().unwrap(), &rxferrix::runtime::registered_tables()).unwrap();
    assert!(
        files.data_model_ts.contains("\"cu_events\": { key: string, at: string, day: string, };"),
        "{}",
        files.data_model_ts
    );
    let shape = format!("{:?}", (<events::Event as rxferrix::Table>::table_def().shape)());
    assert!(!shape.contains("Json"), "stored as text: {shape}");
}
