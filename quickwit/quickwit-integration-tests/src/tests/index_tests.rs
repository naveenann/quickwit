// Copyright (C) 2023 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use bytes::Bytes;
use quickwit_metastore::SplitState;
use quickwit_rest_client::models::IngestSource;
use quickwit_rest_client::rest_client::CommitType;
use quickwit_serve::SearchRequestQueryString;
use serde_json::json;

use crate::test_utils::ClusterSandbox;

#[tokio::test]
async fn test_restarting_standalone_server() {
    quickwit_common::setup_logging_for_tests();
    let sandbox = ClusterSandbox::start_standalone_node().await.unwrap();
    let index_id = "test-index-with-restarting";
    let index_config = Bytes::from(format!(
        r#"
            version: 0.5
            index_id: {}
            doc_mapping:
                field_mappings:
                - name: body
                  type: text
            indexing_settings:
                commit_timeout_secs: 1
                merge_policy:
                    type: stable_log
                    merge_factor: 3
                    max_merge_factor: 3
            "#,
        index_id
    ));

    // Create the index.
    sandbox
        .indexer_rest_client
        .indexes()
        .create(
            index_config.clone(),
            quickwit_config::ConfigFormat::Yaml,
            false,
        )
        .await
        .unwrap();

    // Wait fo the pipeline to start.
    // TODO: there should be a better way to do this.
    sandbox.wait_for_indexing_pipelines(1).await.unwrap();

    let old_incarnation_id = sandbox
        .indexer_rest_client
        .indexes()
        .get(index_id)
        .await
        .unwrap()
        .incarnation_id;

    // Index one record.
    sandbox
        .indexer_rest_client
        .ingest(
            index_id,
            IngestSource::Bytes(json!({"body": "first record"}).to_string().into()),
            None,
            CommitType::Force,
        )
        .await
        .unwrap();

    // Delete the indexq
    sandbox
        .indexer_rest_client
        .indexes()
        .delete(index_id, false)
        .await
        .unwrap();

    // Create the index again.
    sandbox
        .indexer_rest_client
        .indexes()
        .create(index_config, quickwit_config::ConfigFormat::Yaml, false)
        .await
        .unwrap();

    sandbox.wait_for_indexing_pipelines(1).await.unwrap();

    let new_incarnation_id = sandbox
        .indexer_rest_client
        .indexes()
        .get(index_id)
        .await
        .unwrap()
        .incarnation_id;
    assert_ne!(old_incarnation_id, new_incarnation_id);

    // Index a couple of records to create 2 additional splits.
    sandbox
        .indexer_rest_client
        .ingest(
            index_id,
            IngestSource::Bytes(json!({"body": "second record"}).to_string().into()),
            None,
            CommitType::Force,
        )
        .await
        .unwrap();

    sandbox
        .indexer_rest_client
        .ingest(
            index_id,
            IngestSource::Bytes(json!({"body": "third record"}).to_string().into()),
            None,
            CommitType::Force,
        )
        .await
        .unwrap();

    sandbox
        .indexer_rest_client
        .ingest(
            index_id,
            IngestSource::Bytes(json!({"body": "fourth record"}).to_string().into()),
            None,
            CommitType::Force,
        )
        .await
        .unwrap();

    let search_response_empty = sandbox
        .searcher_rest_client
        .search(
            index_id,
            SearchRequestQueryString {
                query: "body:record".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(search_response_empty.num_hits, 3);

    // Wait for splits to merge, since we created 3 splits and merge factor is 3,
    // we should get 1 published split with no staged splits eventually.
    sandbox
        .wait_for_published_splits(
            index_id,
            Some(vec![SplitState::Published, SplitState::Staged]),
            1,
        )
        .await
        .unwrap();

    sandbox.shutdown().await.unwrap();
}
