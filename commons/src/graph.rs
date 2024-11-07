use crate::{metadata, policy};
use failure::Fallible;
use serde_derive::{Deserialize, Serialize};
use std::collections::HashMap;

/// Single release entry in the Cincinnati update-graph.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CincinnatiPayload {
    pub version: String,
    pub metadata: HashMap<String, String>,
    pub payload: String,
}

/// Cincinnati update-graph, a DAG with releases (nodes) and update paths (edges).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<CincinnatiPayload>,
    pub edges: Vec<(u64, u64)>,
}

impl Graph {
    /// Assemble a graph from release-index and updates metadata.
    pub fn from_metadata(
        releases: Vec<metadata::Release>,
        updates: metadata::UpdatesJSON,
        scope: GraphScope,
    ) -> Fallible<Self> {
        let nodes: Vec<CincinnatiPayload> = releases
            .into_iter()
            .enumerate()
            .filter_map(|(age_index, entry)| {
                let mut current = CincinnatiPayload {
                    version: entry.version,
                    payload: "".to_string(),
                    metadata: maplit::hashmap! {
                        metadata::AGE_INDEX.to_string() => age_index.to_string(),
                    },
                };
                let mut has_basearch = false;
                if scope.oci {
                    if let Some(oci_images) = entry.oci_images {
                        for oci_image in oci_images {
                            if oci_image.architecture != scope.basearch
                                || oci_image.digest_ref.is_empty()
                            {
                                continue;
                            }
                            has_basearch = true;
                            current.payload = oci_image.digest_ref;
                            current
                                .metadata
                                .insert(metadata::SCHEME.to_string(), "oci".to_string());
                        }
                    } else {
                        // This release doesn't have OCI images, skip it.
                        return None;
                    }
                } else {
                    for commit in entry.commits {
                        if commit.architecture != scope.basearch || commit.checksum.is_empty() {
                            continue;
                        }
                        has_basearch = true;
                        current.payload = commit.checksum;
                        current
                            .metadata
                            .insert(metadata::SCHEME.to_string(), "checksum".to_string());
                    }
                }

                // Not a valid release payload for this graph scope, skip it.
                if !has_basearch {
                    return None;
                }

                // Augment with dead-ends metadata.
                Self::inject_deadend_reason(&updates, &mut current);

                // Augment with barriers metadata.
                Self::inject_barrier_reason(&updates, &mut current);

                // Augment with rollouts metadata.
                Self::inject_throttling_params(&updates, &mut current);

                Some(current)
            })
            .collect();

        // Compute the update graph.
        let edges = Self::compute_edges(&nodes)?;
        let graph = Graph { nodes, edges };

        // Filter deadends.
        let final_graph = policy::filter_deadends(graph);

        Ok(final_graph)
    }

    /// Compute edges based on graph metadata.
    fn compute_edges(nodes: &[CincinnatiPayload]) -> Fallible<Vec<(u64, u64)>> {
        use std::collections::BTreeSet;
        use std::ops::Bound;

        // Collect all rollouts and barriers.
        let mut rollouts = BTreeSet::<u64>::new();
        let mut barriers = BTreeSet::<u64>::new();
        for (index, release) in nodes.iter().enumerate() {
            if release.metadata.contains_key(metadata::ROLLOUT) {
                rollouts.insert(index as u64);
            }
            if release.metadata.contains_key(metadata::BARRIER) {
                barriers.insert(index as u64);
            }
        }

        // Add edges targeting rollouts, back till the previous barrier.
        let mut edges = vec![];
        for (index, _release) in nodes.iter().enumerate().rev() {
            let age = index as u64;
            if !rollouts.contains(&age) {
                continue;
            }

            let previous_barrier = barriers
                .range((Bound::Unbounded, Bound::Excluded(age)))
                .next_back()
                .cloned()
                .unwrap_or(0);

            for i in previous_barrier..age {
                edges.push((i, age))
            }
        }

        // Add edges targeting barriers, back till the previous barrier.
        let mut start = 0;
        for target in barriers {
            if rollouts.contains(&target) {
                // This is an in-progress barrier. Rollout logic already took care
                // of it, nothing to do here.
            } else {
                for i in start..target {
                    edges.push((i, target))
                }
            }
            start = target;
        }

        Ok(edges)
    }

    fn inject_barrier_reason(updates: &metadata::UpdatesJSON, release: &mut CincinnatiPayload) {
        for entry in &updates.releases {
            if entry.version != release.version {
                continue;
            }

            if let Some(barrier) = &entry.metadata.barrier {
                let reason = if barrier.reason.is_empty() {
                    "generic"
                } else {
                    &barrier.reason
                };

                release
                    .metadata
                    .insert(metadata::BARRIER.to_string(), true.to_string());
                release
                    .metadata
                    .insert(metadata::BARRIER_REASON.to_string(), reason.to_string());
            }
        }
    }

    fn inject_deadend_reason(updates: &metadata::UpdatesJSON, release: &mut CincinnatiPayload) {
        for entry in &updates.releases {
            if entry.version != release.version {
                continue;
            }

            if let Some(deadend) = &entry.metadata.deadend {
                let reason = if deadend.reason.is_empty() {
                    "generic"
                } else {
                    &deadend.reason
                };

                release
                    .metadata
                    .insert(metadata::DEADEND.to_string(), true.to_string());
                release
                    .metadata
                    .insert(metadata::DEADEND_REASON.to_string(), reason.to_string());
            }
        }
    }

    fn inject_throttling_params(updates: &metadata::UpdatesJSON, release: &mut CincinnatiPayload) {
        for entry in &updates.releases {
            if entry.version != release.version {
                continue;
            }

            if let Some(rollout) = &entry.metadata.rollout {
                release
                    .metadata
                    .insert(metadata::ROLLOUT.to_string(), true.to_string());
                if let Some(val) = rollout.start_epoch {
                    release
                        .metadata
                        .insert(metadata::START_EPOCH.to_string(), val.to_string());
                }
                if let Some(val) = rollout.start_percentage {
                    release
                        .metadata
                        .insert(metadata::START_VALUE.to_string(), val.to_string());
                }
                if let Some(minutes) = &rollout.duration_minutes {
                    release
                        .metadata
                        .insert(metadata::DURATION.to_string(), minutes.to_string());
                }
            }
        }
    }
}

/// The scope of a cached graph, i.e. the specific stream and basearch that it is valid for.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphScope {
    pub basearch: String,
    pub stream: String,
    pub oci: bool,
}
