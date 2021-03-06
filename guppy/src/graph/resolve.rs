// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::graph::resolve_core::{ResolveCore, Topo};
use crate::graph::{
    DependencyDirection, PackageGraph, PackageIx, PackageLink, PackageLinkImpl, PackageMetadata,
    PackageQuery,
};
use crate::petgraph_support::dot::{DotFmt, DotVisitor, DotWrite};
use crate::petgraph_support::reversed::MaybeReversedEdge;
use crate::PackageId;
use fixedbitset::FixedBitSet;
use petgraph::prelude::*;
use petgraph::visit::{NodeFiltered, NodeRef, VisitMap};
use std::fmt;

impl PackageGraph {
    /// Creates a new `PackageSet` consisting of all members of this package graph.
    ///
    /// This is normally the same as `query_workspace().resolve()`, but can differ in some cases:
    /// * if packages have been replaced with `[patch]` or `[replace]`
    /// * if some edges have been removed from this graph with `retain_edges`.
    ///
    /// In most situations, `query_workspace` is preferred. Use `resolve_all` if you know you need
    /// parts of the graph that aren't accessible from the workspace.
    pub fn resolve_all(&self) -> PackageSet {
        PackageSet {
            graph: self,
            core: ResolveCore::all_nodes(&self.dep_graph),
        }
    }
}

/// A set of resolved packages in a package graph.
///
/// Created by `PackageQuery::resolve`.
#[derive(Clone, Debug)]
pub struct PackageSet<'g> {
    graph: &'g PackageGraph,
    core: ResolveCore<PackageGraph>,
}

impl<'g> PackageSet<'g> {
    pub(super) fn new(query: PackageQuery<'g>) -> Self {
        let graph = query.graph;
        Self {
            graph,
            core: ResolveCore::new(graph.dep_graph(), query.params),
        }
    }

    pub(super) fn from_included(graph: &'g PackageGraph, included: FixedBitSet) -> Self {
        Self {
            graph,
            core: ResolveCore::from_included(included),
        }
    }

    pub(super) fn with_resolver(
        query: PackageQuery<'g>,
        mut resolver: impl PackageResolver<'g>,
    ) -> Self {
        let graph = query.graph;
        let params = query.params.clone();
        Self {
            graph,
            core: ResolveCore::with_edge_filter(
                graph.dep_graph(),
                params,
                |source, target, edge_ix| {
                    let link = graph.edge_to_link(source, target, edge_ix, None);
                    resolver.accept(&query, link)
                },
            ),
        }
    }

    /// Returns the number of packages in this set.
    pub fn len(&self) -> usize {
        self.core.len()
    }

    /// Returns true if no packages were resolved in this set.
    pub fn is_empty(&self) -> bool {
        self.core.is_empty()
    }

    /// Returns true if this package ID is contained in this resolve set, false if it isn't, and
    /// None if the package ID wasn't found.
    pub fn contains(&self, package_id: &PackageId) -> Option<bool> {
        Some(self.core.contains(self.graph.package_ix(package_id)?))
    }

    // ---
    // Set operations
    // ---

    /// Returns a `PackageSet` that contains all packages present in at least one of `self`
    /// and `other`.
    ///
    /// ## Panics
    ///
    /// Panics if the package graphs associated with `self` and `other` don't match.
    pub fn union(&self, other: &Self) -> Self {
        assert!(
            ::std::ptr::eq(self.graph, other.graph),
            "package graphs passed into union() match"
        );
        let mut res = self.clone();
        res.core.union_with(&other.core);
        res
    }

    /// Returns a `PackageSet` that contains all packages present in both `self` and `other`.
    ///
    /// ## Panics
    ///
    /// Panics if the package graphs associated with `self` and `other` don't match.
    pub fn intersection(&self, other: &Self) -> Self {
        assert!(
            ::std::ptr::eq(self.graph, other.graph),
            "package graphs passed into intersection() match"
        );
        let mut res = self.clone();
        res.core.intersect_with(&other.core);
        res
    }

    /// Returns a `PackageSet` that contains all packages present in `self` but not `other`.
    ///
    /// ## Panics
    ///
    /// Panics if the package graphs associated with `self` and `other` don't match.
    pub fn difference(&self, other: &Self) -> Self {
        assert!(
            ::std::ptr::eq(self.graph, other.graph),
            "package graphs passed into difference() match"
        );
        Self {
            graph: self.graph,
            core: self.core.difference(&other.core),
        }
    }

    /// Returns a `PackageSet` that contains all packages present in exactly one of `self` and
    /// `other`.
    ///
    /// ## Panics
    ///
    /// Panics if the package graphs associated with `self` and `other` don't match.
    pub fn symmetric_difference(&self, other: &Self) -> Self {
        assert!(
            ::std::ptr::eq(self.graph, other.graph),
            "package graphs passed into symmetric_difference() match"
        );
        let mut res = self.clone();
        res.core.symmetric_difference_with(&other.core);
        res
    }

    // ---
    // Iterators
    // ---

    /// Iterates over package IDs, in topological order in the direction specified.
    ///
    /// ## Cycles
    ///
    /// The packages within a dependency cycle will be returned in arbitrary order, but overall
    /// topological order will be maintained.
    pub fn package_ids<'a>(
        &'a self,
        direction: DependencyDirection,
    ) -> impl Iterator<Item = &'g PackageId> + ExactSizeIterator + 'a {
        let graph = self.graph;
        self.core
            .topo(self.graph.sccs(), direction)
            .map(move |package_ix| &graph.dep_graph[package_ix])
    }

    pub(super) fn ixs(&'g self, direction: DependencyDirection) -> Topo<'g, PackageGraph> {
        self.core.topo(self.graph.sccs(), direction)
    }

    /// Iterates over package metadatas, in topological order in the direction specified.
    ///
    /// ## Cycles
    ///
    /// The packages within a dependency cycle will be returned in arbitrary order, but overall
    /// topological order will be maintained.
    pub fn packages<'a>(
        &'a self,
        direction: DependencyDirection,
    ) -> impl Iterator<Item = PackageMetadata<'g>> + ExactSizeIterator + 'a {
        let graph = self.graph;
        self.package_ids(direction).map(move |package_id| {
            graph.metadata(package_id).unwrap_or_else(|| {
                panic!(
                    "known package ID '{}' not found in metadata map",
                    package_id
                )
            })
        })
    }

    /// Returns the set of "root package" IDs in the specified direction.
    ///
    /// * If direction is Forward, return the set of packages that do not have any dependencies
    ///   within the selected graph.
    /// * If direction is Reverse, return the set of packages that do not have any dependents within
    ///   the selected graph.
    ///
    /// ## Cycles
    ///
    /// If a root consists of a dependency cycle, all the packages in it will be returned in
    /// arbitrary order.
    pub fn root_ids<'a>(
        &'a self,
        direction: DependencyDirection,
    ) -> impl Iterator<Item = &'g PackageId> + ExactSizeIterator + 'a {
        let dep_graph = &self.graph.dep_graph;
        self.core
            .roots(self.graph.dep_graph(), self.graph.sccs(), direction)
            .into_iter()
            .map(move |package_ix| &dep_graph[package_ix])
    }

    /// Returns the set of "root package" metadatas in the specified direction.
    ///
    /// * If direction is Forward, return the set of packages that do not have any dependencies
    ///   within the selected graph.
    /// * If direction is Reverse, return the set of packages that do not have any dependents within
    ///   the selected graph.
    ///
    /// ## Cycles
    ///
    /// If a root consists of a dependency cycle, all the packages in it will be returned in
    /// arbitrary order.
    pub fn root_packages<'a>(
        &'a self,
        direction: DependencyDirection,
    ) -> impl Iterator<Item = PackageMetadata<'g>> + ExactSizeIterator + 'a {
        let package_graph = self.graph;
        self.core
            .roots(self.graph.dep_graph(), self.graph.sccs(), direction)
            .into_iter()
            .map(move |package_ix| {
                package_graph
                    .metadata(&package_graph.dep_graph[package_ix])
                    .expect("invalid node index")
            })
    }

    /// Creates an iterator over `PackageLink` instances.
    ///
    /// If the iteration is in forward order, for any given package, at least one link where the
    /// package is on the `to` end is returned before any links where the package is on the
    /// `from` end.
    ///
    /// If the iteration is in reverse order, for any given package, at least one link where the
    /// package is on the `from` end is returned before any links where the package is on the `to`
    /// end.
    ///
    /// ## Cycles
    ///
    /// The links in a dependency cycle may be returned in arbitrary order.
    pub fn links<'a>(
        &'a self,
        direction: DependencyDirection,
    ) -> impl Iterator<Item = PackageLink<'g>> + 'a {
        let graph = self.graph;
        self.core
            .links(graph.dep_graph(), graph.sccs(), direction)
            .map(move |(source_ix, target_ix, edge_ix)| {
                graph.edge_to_link(source_ix, target_ix, edge_ix, None)
            })
    }

    /// Constructs a representation of the selected packages in `dot` format.
    pub fn display_dot<'a, V: PackageDotVisitor + 'g>(
        &'a self,
        visitor: V,
    ) -> impl fmt::Display + 'a {
        let included = &self.core.included;
        let node_filtered = NodeFiltered::from_fn(self.graph.dep_graph(), move |package_ix| {
            included.is_visited(&package_ix)
        });
        DotFmt::new(node_filtered, VisitorWrap::new(self.graph, visitor))
    }
}

/// Represents whether a particular link within a package graph should be followed during a
/// resolve operation.
pub trait PackageResolver<'g> {
    /// Returns true if this link should be followed during a resolve operation.
    ///
    /// Returning false does not prevent the `to` package (or `from` package with `query_reverse`)
    /// from being included if it's reachable through other means.
    fn accept(&mut self, query: &PackageQuery<'g>, link: PackageLink<'g>) -> bool;
}

impl<'g, 'a, T> PackageResolver<'g> for &'a mut T
where
    T: PackageResolver<'g>,
{
    fn accept(&mut self, query: &PackageQuery<'g>, link: PackageLink<'g>) -> bool {
        (**self).accept(query, link)
    }
}

impl<'g, 'a> PackageResolver<'g> for Box<dyn PackageResolver<'g> + 'a> {
    fn accept(&mut self, query: &PackageQuery<'g>, link: PackageLink<'g>) -> bool {
        (**self).accept(query, link)
    }
}

impl<'g, 'a> PackageResolver<'g> for &'a mut dyn PackageResolver<'g> {
    fn accept(&mut self, query: &PackageQuery<'g>, link: PackageLink<'g>) -> bool {
        (**self).accept(query, link)
    }
}

pub(super) struct ResolverFn<F>(pub(super) F);

impl<'g, F> PackageResolver<'g> for ResolverFn<F>
where
    F: FnMut(&PackageQuery<'g>, PackageLink<'g>) -> bool,
{
    fn accept(&mut self, query: &PackageQuery<'g>, link: PackageLink<'g>) -> bool {
        (self.0)(query, link)
    }
}

/// A visitor used for formatting `dot` graphs.
pub trait PackageDotVisitor {
    /// Visits this package. The implementation may output a label for this package to the given
    /// `DotWrite`.
    fn visit_package(&self, package: PackageMetadata<'_>, f: &mut DotWrite<'_, '_>) -> fmt::Result;

    /// Visits this dependency link. The implementation may output a label for this link to the
    /// given `DotWrite`.
    fn visit_link(&self, link: PackageLink<'_>, f: &mut DotWrite<'_, '_>) -> fmt::Result;
}

struct VisitorWrap<'g, V> {
    graph: &'g PackageGraph,
    inner: V,
}

impl<'g, V> VisitorWrap<'g, V> {
    fn new(graph: &'g PackageGraph, inner: V) -> Self {
        Self { graph, inner }
    }
}

impl<'g, V, NR, ER> DotVisitor<NR, ER> for VisitorWrap<'g, V>
where
    V: PackageDotVisitor,
    NR: NodeRef<NodeId = NodeIndex<PackageIx>, Weight = PackageId>,
    ER: MaybeReversedEdge<
        NodeId = NodeIndex<PackageIx>,
        EdgeId = EdgeIndex<PackageIx>,
        Weight = PackageLinkImpl,
    >,
{
    fn visit_node(&self, node: NR, f: &mut DotWrite<'_, '_>) -> fmt::Result {
        let metadata = self
            .graph
            .metadata(node.weight())
            .expect("visited node should have associated metadata");
        self.inner.visit_package(metadata, f)
    }

    fn visit_edge(&self, edge: ER, f: &mut DotWrite<'_, '_>) -> fmt::Result {
        let (source_ix, target_ix) = edge.original_endpoints();
        let link = self
            .graph
            .edge_to_link(source_ix, target_ix, edge.id(), Some(edge.weight()));
        self.inner.visit_link(link, f)
    }
}
