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

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::sync::Arc;

use itertools::Itertools;
use quickwit_doc_mapper::{DocMapper, WarmupInfo};
use quickwit_proto::{LeafSearchResponse, PartialHit, SearchRequest, SortOrder};
use serde::Deserialize;
use tantivy::aggregation::agg_req::{get_fast_field_names, Aggregations};
use tantivy::aggregation::intermediate_agg_result::IntermediateAggregationResults;
use tantivy::aggregation::{AggregationLimits, AggregationSegmentCollector};
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::ColumnType;
use tantivy::fastfield::Column;
use tantivy::{DocId, Score, SegmentOrdinal, SegmentReader, TantivyError};

use crate::filters::{create_timestamp_filter_builder, TimestampFilter, TimestampFilterBuilder};
use crate::find_trace_ids_collector::{FindTraceIdsCollector, FindTraceIdsSegmentCollector};
use crate::partial_hit_sorting_key;
use crate::service::SearcherContext;

#[derive(Clone, Debug)]
pub(crate) enum SortBy {
    DocId,
    FastField {
        field_name: String,
        order: SortOrder,
    },
    Score {
        order: SortOrder,
    },
}

/// The `SortingFieldComputer` can be seen as the specialization of `SortBy` applied to a specific
/// `SegmentReader`. Its role is to compute the sorting field given a `DocId`.
enum SortingFieldComputer {
    /// If undefined, we simply sort by DocIds.
    DocId,
    FastField {
        sort_column: Column<u64>,
        order: SortOrder,
    },
    Score {
        order: SortOrder,
    },
}

impl SortingFieldComputer {
    /// Returns the ranking key for the given element
    fn compute_sorting_field(&self, doc_id: DocId, score: Score) -> u64 {
        match self {
            SortingFieldComputer::FastField {
                sort_column: fast_field_reader,
                order,
            } => {
                if let Some(field_val) = fast_field_reader.first(doc_id) {
                    match order {
                        // Descending is our most common case.
                        SortOrder::Desc => field_val,
                        // We get Ascending order by using a decreasing mapping over u64 as the
                        // sorting_field.
                        SortOrder::Asc => u64::MAX - field_val,
                    }
                } else {
                    0u64
                }
            }
            SortingFieldComputer::DocId => doc_id as u64,
            SortingFieldComputer::Score { order } => {
                let u64_score = f32_to_u64(score);
                match order {
                    SortOrder::Desc => u64_score,
                    SortOrder::Asc => u64::MAX - u64_score,
                }
            }
        }
    }
}

/// Converts a float to an unsigned integer while preserving order.
/// See `<https://lemire.me/blog/2020/12/14/converting-floating-point-numbers-to-integers-while-preserving-order/>`
fn f32_to_u64(value: f32) -> u64 {
    let value_u32 = u32::from_le_bytes(value.to_le_bytes());
    let mut mask = (value_u32 as i32 >> 31) as u32;
    mask |= 0x80000000;
    (value_u32 ^ mask) as u64
}

/// Takes a user-defined sorting criteria and resolves it to a
/// segment specific `SortFieldComputer`.
fn resolve_sort_by(
    sort_by: &SortBy,
    segment_reader: &SegmentReader,
) -> tantivy::Result<SortingFieldComputer> {
    match sort_by {
        SortBy::DocId => Ok(SortingFieldComputer::DocId),
        SortBy::FastField { field_name, order } => {
            let sort_column_opt: Option<(Column<u64>, ColumnType)> =
                segment_reader.fast_fields().u64_lenient(field_name)?;
            let sort_column = if let Some((sort_column, _column_type)) = sort_column_opt {
                sort_column
            } else {
                Column::build_empty_column(segment_reader.max_doc())
            };
            Ok(SortingFieldComputer::FastField {
                sort_column,
                order: *order,
            })
        }
        SortBy::Score { order } => Ok(SortingFieldComputer::Score { order: *order }),
    }
}

/// PartialHitHeapItem order is the inverse of the natural order
/// so that we actually have a min-heap.
#[derive(Clone, Copy)]
struct PartialHitHeapItem {
    sorting_field_value: u64,
    doc_id: DocId,
}

impl PartialOrd for PartialHitHeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PartialHitHeapItem {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        let by_sorting_field = other
            .sorting_field_value
            .partial_cmp(&self.sorting_field_value)
            .unwrap_or(Ordering::Equal);

        let lazy_order_by_doc_id = || {
            self.doc_id
                .partial_cmp(&other.doc_id)
                .unwrap_or(Ordering::Equal)
        };

        // In case of a tie on the feature, we sort by ascending `DocId`.
        by_sorting_field.then_with(lazy_order_by_doc_id)
    }
}

impl PartialEq for PartialHitHeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for PartialHitHeapItem {}

enum AggregationSegmentCollectors {
    FindTraceIdsSegmentCollector(Box<FindTraceIdsSegmentCollector>),
    TantivyAggregationSegmentCollector(AggregationSegmentCollector),
}

/// Quickwit collector working at the scale of the segment.
pub struct QuickwitSegmentCollector {
    num_hits: u64,
    split_id: String,
    sort_by: SortingFieldComputer,
    hits: BinaryHeap<PartialHitHeapItem>,
    max_hits: usize,
    segment_ord: u32,
    timestamp_filter_opt: Option<TimestampFilter>,
    aggregation: Option<AggregationSegmentCollectors>,
}

impl QuickwitSegmentCollector {
    #[inline]
    fn at_capacity(&self) -> bool {
        self.hits.len() >= self.max_hits
    }

    #[inline]
    fn collect_top_k(&mut self, doc_id: DocId, score: Score) {
        let sorting_field_value: u64 = self.sort_by.compute_sorting_field(doc_id, score);
        if self.at_capacity() {
            if let Some(limit_sorting_field) = self.hits.peek().map(|head| head.sorting_field_value)
            {
                // In case of a tie, we keep the document with a lower `DocId`.
                if limit_sorting_field < sorting_field_value {
                    if let Some(mut head) = self.hits.peek_mut() {
                        head.sorting_field_value = sorting_field_value;
                        head.doc_id = doc_id;
                    }
                }
            }
        } else {
            // we have not reached capacity yet, so we can just push the
            // element.
            self.hits.push(PartialHitHeapItem {
                sorting_field_value,
                doc_id,
            });
        }
    }

    #[inline]
    fn accept_document(&self, doc_id: DocId) -> bool {
        if let Some(ref timestamp_filter) = self.timestamp_filter_opt {
            return timestamp_filter.is_within_range(doc_id);
        }
        true
    }
}

impl SegmentCollector for QuickwitSegmentCollector {
    type Fruit = tantivy::Result<LeafSearchResponse>;

    #[inline]
    fn collect(&mut self, doc_id: DocId, score: Score) {
        if !self.accept_document(doc_id) {
            return;
        }

        self.num_hits += 1;
        self.collect_top_k(doc_id, score);

        match self.aggregation.as_mut() {
            Some(AggregationSegmentCollectors::FindTraceIdsSegmentCollector(collector)) => {
                collector.collect(doc_id, score)
            }
            Some(AggregationSegmentCollectors::TantivyAggregationSegmentCollector(collector)) => {
                collector.collect(doc_id, score)
            }
            None => (),
        }
    }

    fn harvest(self) -> Self::Fruit {
        let segment_ord = self.segment_ord;
        // TODO use into_iter_sorted() once it gets stable.
        let split_id = self.split_id;
        let partial_hits: Vec<PartialHit> = self
            .hits
            .into_sorted_vec()
            .into_iter()
            .map(|hit| PartialHit {
                sorting_field_value: hit.sorting_field_value,
                segment_ord,
                doc_id: hit.doc_id,
                split_id: split_id.clone(),
            })
            .collect();

        let intermediate_aggregation_result = match self.aggregation {
            Some(AggregationSegmentCollectors::FindTraceIdsSegmentCollector(collector)) => {
                let fruit = collector.harvest();
                let serialized =
                    postcard::to_allocvec(&fruit).expect("Collector fruit should be serializable.");
                Some(serialized)
            }
            Some(AggregationSegmentCollectors::TantivyAggregationSegmentCollector(collector)) => {
                let serialized = postcard::to_allocvec(&collector.harvest()?)
                    .expect("Collector fruit should be serializable.");
                Some(serialized)
            }
            None => None,
        };
        Ok(LeafSearchResponse {
            intermediate_aggregation_result,
            num_hits: self.num_hits,
            partial_hits,
            failed_splits: Vec::new(),
            num_attempted_splits: 1,
        })
    }
}

/// Available aggregation types.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum QuickwitAggregations {
    /// Aggregation used by the Jaeger service to find trace IDs that match a
    /// [`quickwit_proto::jaeger::storage::v1::FindTraceIDsRequest`].
    FindTraceIdsAggregation(FindTraceIdsCollector),
    /// Your classic Tantivy aggregation.
    TantivyAggregations(Aggregations),
}

impl QuickwitAggregations {
    fn fast_field_names(&self) -> HashSet<String> {
        match self {
            QuickwitAggregations::FindTraceIdsAggregation(collector) => {
                collector.fast_field_names()
            }
            QuickwitAggregations::TantivyAggregations(aggregations) => {
                get_fast_field_names(aggregations)
            }
        }
    }
}

/// The quickwit collector is the tantivy Collector used in Quickwit.
///
/// It defines the data that should be accumulated about the documents matching
/// the query.
#[derive(Clone)]
pub(crate) struct QuickwitCollector {
    pub split_id: String,
    pub start_offset: usize,
    pub max_hits: usize,
    pub sort_by: SortBy,
    timestamp_filter_builder_opt: Option<TimestampFilterBuilder>,
    pub aggregation: Option<QuickwitAggregations>,
    pub aggregation_limits: AggregationLimits,
}

impl QuickwitCollector {
    pub fn fast_field_names(&self) -> HashSet<String> {
        let mut fast_field_names = HashSet::default();
        match &self.sort_by {
            SortBy::DocId | SortBy::Score { .. } => {}
            SortBy::FastField { field_name, .. } => {
                fast_field_names.insert(field_name.clone());
            }
        }
        if let Some(aggregations) = &self.aggregation {
            fast_field_names.extend(aggregations.fast_field_names());
        }
        if let Some(timestamp_filter_builder) = &self.timestamp_filter_builder_opt {
            fast_field_names.insert(timestamp_filter_builder.timestamp_field_name.clone());
        }
        fast_field_names
    }

    pub fn warmup_info(&self) -> WarmupInfo {
        WarmupInfo {
            term_dict_field_names: Default::default(),
            fast_field_names: self.fast_field_names(),
            field_norms: self.requires_scoring(),
            ..WarmupInfo::default()
        }
    }
}

impl Collector for QuickwitCollector {
    type Child = QuickwitSegmentCollector;
    type Fruit = LeafSearchResponse;

    fn for_segment(
        &self,
        segment_ord: SegmentOrdinal,
        segment_reader: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let sort_by = resolve_sort_by(&self.sort_by, segment_reader)?;
        // Regardless of the start_offset, we need to collect top-K
        // starting from 0 for every leaves.
        let leaf_max_hits = self.max_hits + self.start_offset;

        let timestamp_filter_opt = match &self.timestamp_filter_builder_opt {
            Some(timestamp_filter_builder) => timestamp_filter_builder.build(segment_reader)?,
            None => None,
        };
        let aggregation = match &self.aggregation {
            Some(QuickwitAggregations::FindTraceIdsAggregation(collector)) => {
                Some(AggregationSegmentCollectors::FindTraceIdsSegmentCollector(
                    Box::new(collector.for_segment(0, segment_reader)?),
                ))
            }
            Some(QuickwitAggregations::TantivyAggregations(aggs)) => Some(
                AggregationSegmentCollectors::TantivyAggregationSegmentCollector(
                    AggregationSegmentCollector::from_agg_req_and_reader(
                        aggs,
                        segment_reader,
                        &self.aggregation_limits,
                    )?,
                ),
            ),
            None => None,
        };
        Ok(QuickwitSegmentCollector {
            num_hits: 0u64,
            split_id: self.split_id.clone(),
            sort_by,
            hits: BinaryHeap::with_capacity(leaf_max_hits),
            segment_ord,
            max_hits: leaf_max_hits,
            timestamp_filter_opt,
            aggregation,
        })
    }

    fn requires_scoring(&self) -> bool {
        // We do not need BM25 scoring in Quickwit if it is not opted-in.
        // By returning false, we inform tantivy that it does not need to decompress
        // term frequencies.
        match self.sort_by {
            SortBy::DocId | SortBy::FastField { .. } => false,
            SortBy::Score { .. } => true,
        }
    }

    fn merge_fruits(
        &self,
        segment_fruits: Vec<tantivy::Result<LeafSearchResponse>>,
    ) -> tantivy::Result<Self::Fruit> {
        let segment_fruits: tantivy::Result<Vec<LeafSearchResponse>> =
            segment_fruits.into_iter().collect();
        // We want the hits in [start_offset..start_offset + max_hits).
        // All leaves will return their top [0..max_hits) documents.
        // We compute the overall [0..start_offset + max_hits) documents ...
        let num_hits = self.start_offset + self.max_hits;
        let mut merged_leaf_response =
            merge_leaf_responses(&self.aggregation, segment_fruits?, num_hits)?;
        // ... and drop the first [..start_offsets) hits.
        merged_leaf_response
            .partial_hits
            .drain(
                0..self
                    .start_offset
                    .min(merged_leaf_response.partial_hits.len()),
            )
            .count(); //< we just use count as a way to consume the entire iterator.
        Ok(merged_leaf_response)
    }
}

fn map_error(err: postcard::Error) -> TantivyError {
    TantivyError::InternalError(format!("Merge Result Postcard Error: {}", err))
}

/// Merges a set of Leaf Results.
fn merge_leaf_responses(
    aggregations_opt: &Option<QuickwitAggregations>,
    mut leaf_responses: Vec<LeafSearchResponse>,
    max_hits: usize,
) -> tantivy::Result<LeafSearchResponse> {
    // Optimization: No merging needed if there is only one result.
    if leaf_responses.len() == 1 {
        return Ok(leaf_responses.pop().unwrap());
    }
    let merged_intermediate_aggregation_result = match aggregations_opt {
        Some(QuickwitAggregations::FindTraceIdsAggregation(collector)) => {
            let fruits: Vec<
                <<FindTraceIdsCollector as Collector>::Child as SegmentCollector>::Fruit,
            > = leaf_responses
                .iter()
                .filter_map(|leaf_response| {
                    leaf_response.intermediate_aggregation_result.as_ref().map(
                        |intermediate_aggregation_result| {
                            postcard::from_bytes(intermediate_aggregation_result.as_slice())
                                .map_err(map_error)
                        },
                    )
                })
                .collect::<Result<_, _>>()?;
            let merged_fruit = collector.merge_fruits(fruits)?;
            let serialized = postcard::to_allocvec(&merged_fruit).map_err(map_error)?;
            Some(serialized)
        }
        Some(QuickwitAggregations::TantivyAggregations(_)) => {
            let fruits: Vec<IntermediateAggregationResults> = leaf_responses
                .iter()
                .filter_map(|leaf_response| {
                    leaf_response.intermediate_aggregation_result.as_ref().map(
                        |intermediate_aggregation_result| {
                            postcard::from_bytes(intermediate_aggregation_result.as_slice())
                                .map_err(map_error)
                        },
                    )
                })
                .collect::<Result<_, _>>()?;

            let mut fruit_iter = fruits.into_iter();
            if let Some(first_fruit) = fruit_iter.next() {
                let mut merged_fruit = first_fruit;
                for fruit in fruit_iter {
                    merged_fruit.merge_fruits(fruit)?;
                }
                let serialized = postcard::to_allocvec(&merged_fruit).map_err(map_error)?;

                Some(serialized)
            } else {
                None
            }
        }
        None => None,
    };
    let num_attempted_splits = leaf_responses
        .iter()
        .map(|leaf_response| leaf_response.num_attempted_splits)
        .sum();
    let num_hits: u64 = leaf_responses
        .iter()
        .map(|leaf_response| leaf_response.num_hits)
        .sum();
    let failed_splits = leaf_responses
        .iter()
        .flat_map(|leaf_response| leaf_response.failed_splits.iter())
        .cloned()
        .collect_vec();
    let all_partial_hits: Vec<PartialHit> = leaf_responses
        .into_iter()
        .flat_map(|leaf_response| leaf_response.partial_hits)
        .collect();
    // TODO optimize
    let top_k_partial_hits = top_k_partial_hits(all_partial_hits, max_hits);
    Ok(LeafSearchResponse {
        intermediate_aggregation_result: merged_intermediate_aggregation_result,
        num_hits,
        partial_hits: top_k_partial_hits,
        failed_splits,
        num_attempted_splits,
    })
}

/// Mutates partial_hits so that it contains the top-num_hitso hits,
/// and so that these elements are sorted.
///
/// TODO we could possibly optimize the sort away (but I doubt it matters).
fn top_k_partial_hits(mut partial_hits: Vec<PartialHit>, num_hits: usize) -> Vec<PartialHit> {
    partial_hits.sort_unstable_by(|left, right| {
        let left_key = partial_hit_sorting_key(left);
        let right_key = partial_hit_sorting_key(right);
        left_key.cmp(&right_key)
    });
    partial_hits.truncate(num_hits);
    partial_hits
}

/// Builds the QuickwitCollector, in function of the information that was requested by the user.
pub(crate) fn make_collector_for_split(
    split_id: String,
    doc_mapper: &dyn DocMapper,
    search_request: &SearchRequest,
    aggregation_limits: AggregationLimits,
) -> crate::Result<QuickwitCollector> {
    let aggregation = match &search_request.aggregation_request {
        Some(aggregation) => Some(serde_json::from_str(aggregation)?),
        None => None,
    };
    let timestamp_filter_builder_opt = create_timestamp_filter_builder(
        doc_mapper.timestamp_field_name(),
        search_request.start_timestamp,
        search_request.end_timestamp,
    );
    let sort_order = search_request
        .sort_order
        .and_then(SortOrder::from_i32)
        .unwrap_or(SortOrder::Desc);
    let sort_by = search_request
        .sort_by_field
        .as_ref()
        .map(|field_name| {
            if field_name == "_score" {
                SortBy::Score { order: sort_order }
            } else {
                SortBy::FastField {
                    field_name: field_name.clone(),
                    order: sort_order,
                }
            }
        })
        .unwrap_or(SortBy::DocId);

    Ok(QuickwitCollector {
        split_id,
        start_offset: search_request.start_offset as usize,
        max_hits: search_request.max_hits as usize,
        sort_by,
        timestamp_filter_builder_opt,
        aggregation,
        aggregation_limits,
    })
}

pub fn aggregation_limits_from_searcher_context(
    searcher_context: &Arc<SearcherContext>,
) -> AggregationLimits {
    AggregationLimits::new(
        Some(
            searcher_context
                .searcher_config
                .aggregation_memory_limit
                .get_bytes(),
        ),
        Some(searcher_context.searcher_config.aggregation_bucket_limit),
    )
}

/// Builds a QuickwitCollector that's only useful for merging fruits.
///
/// This collector only needs `start_offset` & `max_hit` so the other attributes
/// can be set to default.
pub(crate) fn make_merge_collector(
    search_request: &SearchRequest,
    searcher_context: &Arc<SearcherContext>,
) -> crate::Result<QuickwitCollector> {
    let aggregation = match &search_request.aggregation_request {
        Some(aggregation) => Some(serde_json::from_str(aggregation)?),
        None => None,
    };
    Ok(QuickwitCollector {
        split_id: String::default(),
        start_offset: search_request.start_offset as usize,
        max_hits: search_request.max_hits as usize,
        sort_by: SortBy::DocId,
        timestamp_filter_builder_opt: None,
        aggregation,
        aggregation_limits: aggregation_limits_from_searcher_context(searcher_context),
    })
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use proptest::prelude::*;
    use quickwit_proto::PartialHit;

    use super::PartialHitHeapItem;
    use crate::collector::{f32_to_u64, top_k_partial_hits};

    #[test]
    fn test_partial_hit_ordered_by_sorting_field() {
        let lesser_score = PartialHitHeapItem {
            sorting_field_value: 1u64,
            doc_id: 1u32,
        };
        let higher_score = PartialHitHeapItem {
            sorting_field_value: 2u64,
            doc_id: 1u32,
        };
        assert_eq!(lesser_score.cmp(&higher_score), Ordering::Greater);
    }

    #[test]
    fn test_merge_partial_hits_no_tie() {
        let make_doc = |sorting_field_value: u64| PartialHit {
            sorting_field_value,
            split_id: "split1".to_string(),
            segment_ord: 0u32,
            doc_id: 0u32,
        };
        assert_eq!(
            top_k_partial_hits(vec![make_doc(1u64), make_doc(3u64), make_doc(2u64),], 2),
            vec![make_doc(3), make_doc(2)]
        );
    }

    #[test]
    fn test_merge_partial_hits_with_tie() {
        let make_hit_given_split_id = |split_id: u64| PartialHit {
            sorting_field_value: 0u64,
            split_id: format!("split_{split_id}"),
            segment_ord: 0u32,
            doc_id: 0u32,
        };
        assert_eq!(
            top_k_partial_hits(
                vec![
                    make_hit_given_split_id(1u64),
                    make_hit_given_split_id(3u64),
                    make_hit_given_split_id(2u64),
                ],
                2
            ),
            vec![make_hit_given_split_id(1), make_hit_given_split_id(2)]
        );
    }

    prop_compose! {
        // Turns out, zero's and negative zero's u64 representation is not same.
        // It is not relevant for our use case. For simplicity we filter the negative
        // zero.
        fn any_f32_without_negative_zero()(val in any::<f32>().prop_filter("Value can't be negative zero", |val| *val != -0.0)) -> f32 {
            val
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10000))]
        #[test]
        fn test_proptest_f32_to_u64_compare_arbitrary(a in any_f32_without_negative_zero(), b in any_f32_without_negative_zero()) {
            prop_assert_eq!(a < b, f32_to_u64(a) < f32_to_u64(b))
        }
    }
}
