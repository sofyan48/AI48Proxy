use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Registry;
use crate::config::Observation;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Aggregate {
    pub credential_id: String,
    pub model: String,
    pub status: String,
    pub count: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Detail {
    pub request_id: String,
    pub credential_id: String,
    pub model: String,
    pub request_kind: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotFrame {
    pub kind: String,
    pub revision: i64,
    pub observed_at: DateTime<Utc>,
    pub barrier_revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_count: Option<usize>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub details_truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aggregates: Vec<Aggregate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<Detail>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub aggregate_group_count: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// Immutable frames should be retried verbatim. Do not mix revisions in a delivery batch.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub revision: i64,
    pub frames: Vec<SnapshotFrame>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    InvalidLimits,
    RevisionExhausted,
    InvalidBarrier,
    /// Even the overflow notification cannot fit the configured wire bounds.
    CannotEncode,
}

impl Registry {
    /// The parent schedules this at Observation::snapshot_interval. Passing the observation
    /// timestamp explicitly makes tests deterministic. barrier_revision comes from Home.
    pub fn snapshot(
        &self,
        limits: &Observation,
        observed_at: DateTime<Utc>,
        barrier_revision: i64,
    ) -> Result<Snapshot, SnapshotError> {
        if barrier_revision < 0 {
            return Err(SnapshotError::InvalidBarrier);
        }
        if limits.max_part_bytes == 0
            || limits.max_part_count == 0
            || limits.max_revision_bytes == 0
            || limits.max_aggregate_groups == 0
            || limits.max_string_bytes == 0
        {
            return Err(SnapshotError::InvalidLimits);
        }
        let mut state = self.state();
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or(SnapshotError::RevisionExhausted)?;
        let revision = state.revision;
        let mut groups = BTreeMap::new();
        let mut details = Vec::new();
        let mut truncated = false;
        let mut invalid_aggregate = false;
        let valid = |s: &str| !s.trim().is_empty() && s.len() <= limits.max_string_bytes;
        for active in state.active.values() {
            invalid_aggregate |= !valid(&active.credential_id) || !valid(&active.model);
            let status = if active.accounted {
                "accounted"
            } else {
                "unaccounted"
            };
            *groups
                .entry((active.credential_id.clone(), active.model.clone(), status))
                .or_insert(0_i64) += 1;
            if details.len() < limits.max_details
                && valid(&active.request_id)
                && valid(&active.request_kind)
                && valid(&active.credential_id)
                && valid(&active.model)
            {
                details.push(Detail {
                    request_id: active.request_id.clone(),
                    credential_id: active.credential_id.clone(),
                    model: active.model.clone(),
                    request_kind: active.request_kind.clone(),
                    started_at: active.started_at,
                });
            } else {
                truncated = true;
            }
        }
        drop(state);
        let group_count = groups.len();
        let base = SnapshotFrame {
            kind: "part".into(),
            revision,
            observed_at,
            barrier_revision,
            part_index: Some(0),
            part_count: Some(limits.max_part_count),
            details_truncated: truncated,
            aggregates: Vec::new(),
            details: Vec::new(),
            aggregate_group_count: 0,
        };
        let overflow = || {
            let frame = SnapshotFrame {
                kind: "overflow".into(),
                details_truncated: false,
                part_index: None,
                part_count: None,
                aggregate_group_count: group_count.max(1),
                ..base.clone()
            };
            let bytes = encoded_len(&frame);
            if bytes > limits.max_part_bytes || bytes > limits.max_revision_bytes {
                Err(SnapshotError::CannotEncode)
            } else {
                Ok(Snapshot {
                    revision,
                    frames: vec![frame],
                })
            }
        };
        if invalid_aggregate || group_count > limits.max_aggregate_groups {
            return overflow();
        }
        let aggregates: Vec<_> = groups
            .into_iter()
            .map(|((credential_id, model, status), count)| Aggregate {
                credential_id,
                model,
                status: status.into(),
                count,
            })
            .collect();
        if let Some(frames) = pack(&base, &aggregates, &details, limits) {
            return Ok(Snapshot { revision, frames });
        }
        // Details are optional; never sacrifice complete aggregates merely to retain details.
        let mut aggregate_base = base.clone();
        aggregate_base.details_truncated |= !details.is_empty();
        if let Some(frames) = pack(&aggregate_base, &aggregates, &[], limits) {
            return Ok(Snapshot { revision, frames });
        }
        overflow()
    }
}

fn encoded_len(frame: &SnapshotFrame) -> usize {
    serde_json::to_vec(frame).map_or(usize::MAX, |bytes| bytes.len())
}

fn pack(
    base: &SnapshotFrame,
    aggregates: &[Aggregate],
    details: &[Detail],
    limits: &Observation,
) -> Option<Vec<SnapshotFrame>> {
    // Reserve the largest allowed index/count widths; replacing them later only shrinks JSON.
    let mut template = base.clone();
    template.part_index = Some(limits.max_part_count - 1);
    let empty_size = encoded_len(&template);
    if empty_size > limits.max_part_bytes {
        return None;
    }
    let mut frames = Vec::new();
    let mut current = template.clone();
    let mut size = empty_size;
    for aggregate in aggregates {
        let item_size = serde_json::to_vec(aggregate).ok()?.len();
        let extra = item_size + if current.aggregates.is_empty() { 16 } else { 1 };
        if size.checked_add(extra)? > limits.max_part_bytes {
            if current.aggregates.is_empty() && current.details.is_empty() {
                return None;
            }
            frames.push(current);
            if frames.len() >= limits.max_part_count {
                return None;
            }
            current = template.clone();
            size = empty_size;
        }
        let extra = item_size + if current.aggregates.is_empty() { 16 } else { 1 };
        size = size.checked_add(extra)?;
        if size > limits.max_part_bytes {
            return None;
        }
        current.aggregates.push(aggregate.clone());
    }
    for detail in details {
        let item_size = serde_json::to_vec(detail).ok()?.len();
        let extra = item_size + if current.details.is_empty() { 13 } else { 1 };
        if size.checked_add(extra)? > limits.max_part_bytes {
            if current.aggregates.is_empty() && current.details.is_empty() {
                return None;
            }
            frames.push(current);
            if frames.len() >= limits.max_part_count {
                return None;
            }
            current = template.clone();
            size = empty_size;
        }
        let extra = item_size + if current.details.is_empty() { 13 } else { 1 };
        size = size.checked_add(extra)?;
        if size > limits.max_part_bytes {
            return None;
        }
        current.details.push(detail.clone());
    }
    frames.push(current);
    let count = frames.len();
    let mut total = 0usize;
    for (index, frame) in frames.iter_mut().enumerate() {
        frame.part_index = Some(index);
        frame.part_count = Some(count);
        let bytes = encoded_len(frame);
        if bytes > limits.max_part_bytes {
            return None;
        }
        total = total.checked_add(bytes)?;
    }
    (total <= limits.max_revision_bytes).then_some(frames)
}
