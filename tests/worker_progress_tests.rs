// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use duroxide::providers::{SessionFetchConfig, TagFilter, WorkItem};
use std::time::Duration;

mod common;

#[tokio::test]
async fn expired_deferrals_yield_to_already_ready_work() {
    for session_bound in [false, true] {
        for tagged in [false, true] {
            let (store, schema) = common::create_postgres_store().await;
            let session = session_bound.then(|| SessionFetchConfig {
                owner_id: "progress-worker".to_string(),
                lock_timeout: Duration::from_secs(30),
            });
            let tag_filter = if tagged {
                TagFilter::tags(["limited"])
            } else {
                TagFilter::DefaultOnly
            };
            for id in [1, 2] {
                store
                    .enqueue_for_worker(WorkItem::ActivityExecute {
                        instance: "progress".to_string(),
                        execution_id: 1,
                        id,
                        name: "Activity".to_string(),
                        input: String::new(),
                        session_id: session_bound.then(|| "progress-session".to_string()),
                        tag: tagged.then(|| "limited".to_string()),
                    })
                    .await
                    .unwrap();
            }
            let (item, token, _) = store
                .fetch_work_item(
                    Duration::from_secs(30),
                    Duration::ZERO,
                    session.as_ref(),
                    &tag_filter,
                )
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(item, WorkItem::ActivityExecute { id: 1, .. }));
            store
                .abandon_work_item(&token, Some(Duration::from_millis(1)), true)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;

            let (item, token, _) = store
                .fetch_work_item(
                    Duration::from_secs(30),
                    Duration::ZERO,
                    session.as_ref(),
                    &tag_filter,
                )
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(item, WorkItem::ActivityExecute { id: 2, .. }),
                "expired deferral overtook ready work: session={session_bound}, tagged={tagged}"
            );
            store.ack_work_item(&token, None).await.unwrap();

            let (item, token, attempts) = store
                .fetch_work_item(
                    Duration::from_secs(30),
                    Duration::ZERO,
                    session.as_ref(),
                    &tag_filter,
                )
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(item, WorkItem::ActivityExecute { id: 1, .. }));
            assert_eq!(attempts, 1);
            store.ack_work_item(&token, None).await.unwrap();
            common::cleanup_schema(&schema).await;
        }
    }
}
