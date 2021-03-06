// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::debug_ignore::DebugIgnore;
use crate::graph::query_core::{all_visit_map, reachable_map, reachable_map_filtered, QueryParams};
use crate::graph::{DependencyDirection, GraphSpec};
use crate::petgraph_support::scc::{NodeIter, Sccs};
use crate::petgraph_support::walk::EdgeDfs;
use fixedbitset::FixedBitSet;
use petgraph::prelude::*;
use petgraph::visit::{NodeFiltered, Reversed, VisitMap};
use serde::export::PhantomData;

/// Core logic for queries that have been resolved into a known set of packages.
///
/// The `G` param ensures that package and feature resolutions aren't mixed up accidentally.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ResolveCore<G> {
    pub(super) included: FixedBitSet,
    pub(super) len: usize,
    _phantom: PhantomData<G>,
}

impl<G: GraphSpec> ResolveCore<G> {
    pub(super) fn new(
        graph: &Graph<G::Node, G::Edge, Directed, G::Ix>,
        params: QueryParams<G>,
    ) -> Self {
        let (included, len) = match params {
            QueryParams::Forward(initials) => reachable_map(graph, initials.into_inner()),
            QueryParams::Reverse(initials) => reachable_map(Reversed(graph), initials.into_inner()),
        };
        Self {
            included,
            len,
            _phantom: PhantomData,
        }
    }

    pub(super) fn all_nodes(graph: &Graph<G::Node, G::Edge, Directed, G::Ix>) -> Self {
        let (included, len) = all_visit_map(graph);
        Self {
            included,
            len,
            _phantom: PhantomData,
        }
    }

    /// The arguments to the edge filter are the (source, target, edge ix), unreversed.
    pub(super) fn with_edge_filter(
        graph: &Graph<G::Node, G::Edge, Directed, G::Ix>,
        params: QueryParams<G>,
        mut edge_filter: impl FnMut(NodeIndex<G::Ix>, NodeIndex<G::Ix>, EdgeIndex<G::Ix>) -> bool,
    ) -> Self {
        let (included, len) = match params {
            QueryParams::Forward(initials) => reachable_map_filtered(
                graph,
                |edge_ref| edge_filter(edge_ref.source(), edge_ref.target(), edge_ref.id()),
                initials.into_inner(),
            ),
            QueryParams::Reverse(initials) => reachable_map_filtered(
                Reversed(graph),
                |edge_ref| edge_filter(edge_ref.target(), edge_ref.source(), edge_ref.id()),
                initials.into_inner(),
            ),
        };
        Self {
            included,
            len,
            _phantom: PhantomData,
        }
    }

    pub(super) fn from_included(included: FixedBitSet) -> Self {
        let len = included.count_ones(..);
        Self {
            included,
            len,
            _phantom: PhantomData,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) fn contains(&self, ix: NodeIndex<G::Ix>) -> bool {
        self.included.is_visited(&ix)
    }

    // ---
    // Set operations
    // ---

    pub(super) fn union_with(&mut self, other: &Self) {
        self.included.union_with(&other.included);
        self.invalidate_caches();
    }

    pub(super) fn intersect_with(&mut self, other: &Self) {
        self.included.intersect_with(&other.included);
        self.invalidate_caches();
    }

    // fixedbitset 0.2.0 doesn't have a difference_with :(
    pub(super) fn difference(&self, other: &Self) -> Self {
        Self::from_included(self.included.difference(&other.included).collect())
    }

    pub(super) fn symmetric_difference_with(&mut self, other: &Self) {
        self.included.symmetric_difference_with(&other.included);
        self.invalidate_caches();
    }

    pub(super) fn invalidate_caches(&mut self) {
        self.len = self.included.count_ones(..);
    }

    /// Returns the root metadatas in the specified direction.
    pub(super) fn roots(
        &self,
        graph: &Graph<G::Node, G::Edge, Directed, G::Ix>,
        sccs: &Sccs<G::Ix>,
        direction: DependencyDirection,
    ) -> Vec<NodeIndex<G::Ix>> {
        // If any element of an SCC is in the reachable map, so would every other element. This
        // means that any SCC map computed on the full graph will work on a prefiltered graph. (This
        // will change if we decide to implement edge visiting/filtering.)
        //
        // TODO: petgraph 0.5.1 will allow the closure to be replaced with &self.reachable. Switch
        // to it when it's out.
        match direction {
            DependencyDirection::Forward => sccs
                .externals(&NodeFiltered::from_fn(graph, |x| {
                    self.included.is_visited(&x)
                }))
                .collect(),
            DependencyDirection::Reverse => sccs
                .externals(&NodeFiltered::from_fn(Reversed(graph), |x| {
                    self.included.is_visited(&x)
                }))
                .collect(),
        }
    }

    pub(super) fn topo<'g>(
        &'g self,
        sccs: &'g Sccs<G::Ix>,
        direction: DependencyDirection,
    ) -> Topo<'g, G> {
        // ---
        // IMPORTANT
        // ---
        //
        // This uses the same list of sccs that's computed for the entire graph. This is fine for
        // resolve() -- over there, if one element of an SCC is present all others will be present
        // as well.
        //
        // * However, with resolve_with() and a custom resolver, it is possible that SCCs in the
        //   main graph aren't in the subgraph. That makes the returned order "incorrect", but it's
        //   a very minor sin and probably not worth the extra complexity to deal with.
        // * This requires iterating over every node in the graph even if the set of returned nodes
        //   is very small. There's a tradeoff here between allocating memory to store a custom list
        //   of SCCs and just using the one available. More benchmarking is required to figure out
        //   the best approach.
        //
        // Note that the SCCs can be computed in reachable_map by adapting parts of kosaraju_scc.
        let node_iter = sccs.node_iter(direction.into());

        Topo {
            node_iter,
            included: &self.included,
            remaining: self.len,
        }
    }

    pub(super) fn links<'g>(
        &'g self,
        graph: &'g Graph<G::Node, G::Edge, Directed, G::Ix>,
        sccs: &Sccs<G::Ix>,
        direction: DependencyDirection,
    ) -> Links<'g, G> {
        let edge_dfs = match direction {
            DependencyDirection::Forward => {
                let filtered_graph = NodeFiltered::from_fn(graph, |x| self.included.is_visited(&x));
                EdgeDfs::new(&filtered_graph, sccs.externals(&filtered_graph))
            }
            DependencyDirection::Reverse => {
                let filtered_reversed_graph =
                    NodeFiltered::from_fn(Reversed(graph), |x| self.included.is_visited(&x));
                EdgeDfs::new(
                    &filtered_reversed_graph,
                    sccs.externals(&filtered_reversed_graph),
                )
            }
        };

        Links {
            graph: DebugIgnore(graph),
            included: &self.included,
            edge_dfs,
            direction,
        }
    }
}

/// An iterator over package nodes in topological order.
#[derive(Clone, Debug)]
pub(super) struct Topo<'g, G: GraphSpec> {
    node_iter: NodeIter<'g, G::Ix>,
    included: &'g FixedBitSet,
    remaining: usize,
}

impl<'g, G: GraphSpec> Iterator for Topo<'g, G> {
    type Item = NodeIndex<G::Ix>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(ix) = self.node_iter.next() {
            if !self.included.is_visited(&ix) {
                continue;
            }
            self.remaining -= 1;
            return Some(ix);
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'g, G: GraphSpec> ExactSizeIterator for Topo<'g, G> {
    fn len(&self) -> usize {
        self.remaining
    }
}

/// An iterator over dependency links.
#[derive(Clone, Debug)]
#[allow(clippy::type_complexity)]
pub(super) struct Links<'g, G: GraphSpec> {
    graph: DebugIgnore<&'g Graph<G::Node, G::Edge, Directed, G::Ix>>,
    included: &'g FixedBitSet,
    edge_dfs: EdgeDfs<EdgeIndex<G::Ix>, NodeIndex<G::Ix>, FixedBitSet>,
    direction: DependencyDirection,
}

impl<'g, G: GraphSpec> Iterator for Links<'g, G> {
    #[allow(clippy::type_complexity)]
    type Item = (NodeIndex<G::Ix>, NodeIndex<G::Ix>, EdgeIndex<G::Ix>);

    fn next(&mut self) -> Option<Self::Item> {
        match self.direction {
            DependencyDirection::Forward => {
                let included = self.included;
                let filtered =
                    NodeFiltered::from_fn(self.graph.0, |node_ix| included.is_visited(&node_ix));
                self.edge_dfs.next(&filtered)
            }
            DependencyDirection::Reverse => {
                let included = self.included;
                // TODO: replace with &self.included once petgraph 0.5.1 is out.
                let filtered_reversed = NodeFiltered::from_fn(Reversed(self.graph.0), |node_ix| {
                    included.is_visited(&node_ix)
                });
                self.edge_dfs
                    .next(&filtered_reversed)
                    .map(|(source_ix, target_ix, edge_ix)| {
                        // Flip the source and target around since this is a reversed graph, since the
                        // 'from' and 'to' are always right way up.
                        (target_ix, source_ix, edge_ix)
                    })
            }
        }
    }
}
