// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::errors::{FeatureBuildStage, FeatureGraphWarning};
use crate::graph::feature::{
    FeatureEdge, FeatureGraphImpl, FeatureMetadataImpl, FeatureNode, FeatureType,
};
use crate::graph::{
    DepRequiredOrOptional, FeatureIx, PackageGraph, PackageLink, PackageMetadata,
    PlatformStatusImpl,
};
use cargo_metadata::DependencyKind;
use once_cell::sync::OnceCell;
use petgraph::prelude::*;
use std::collections::HashMap;
use std::iter;

#[derive(Debug)]
pub(super) struct FeatureGraphBuildState<'g> {
    package_graph: &'g PackageGraph,
    graph: Graph<FeatureNode, FeatureEdge, Directed, FeatureIx>,
    // Map from package ixs to the base (first) feature for each package.
    base_ixs: Vec<NodeIndex<FeatureIx>>,
    map: HashMap<FeatureNode, FeatureMetadataImpl>,
    warnings: Vec<FeatureGraphWarning>,
}

impl<'g> FeatureGraphBuildState<'g> {
    pub(super) fn new(package_graph: &'g PackageGraph) -> Self {
        let package_count = package_graph.package_count();
        Self {
            package_graph,
            // Each package corresponds to at least one feature ID.
            graph: Graph::with_capacity(package_count, package_count),
            // Each package corresponds to exactly one base feature ix, and there's one last ix at
            // the end.
            base_ixs: Vec::with_capacity(package_count + 1),
            map: HashMap::with_capacity(package_count),
            warnings: vec![],
        }
    }

    /// Add nodes for every feature in this package + the base package, and add edges from every
    /// feature to the base package.
    pub(super) fn add_nodes(&mut self, package: PackageMetadata<'g>) {
        let base_node = FeatureNode::base(package.package_ix());
        let base_ix = self.add_node(base_node, FeatureType::BasePackage);
        self.base_ixs.push(base_ix);
        FeatureNode::named_features(package).for_each(|feature_node| {
            let feature_ix = self.add_node(feature_node, FeatureType::NamedFeature);
            self.graph
                .update_edge(feature_ix, base_ix, FeatureEdge::FeatureToBase);
        });

        package.optional_deps_full().for_each(|(n, _)| {
            let dep_idx = self.add_node(
                FeatureNode::new(package.package_ix(), n),
                FeatureType::OptionalDep,
            );
            self.graph
                .update_edge(dep_idx, base_ix, FeatureEdge::FeatureToBase);
        });
    }

    /// Mark the end of adding nodes.
    pub(super) fn end_nodes(&mut self) {
        self.base_ixs.push(NodeIndex::new(self.graph.node_count()));
    }

    pub(super) fn add_named_feature_edges(&mut self, metadata: PackageMetadata<'_>) {
        let dep_name_to_metadata: HashMap<_, _> = metadata
            .direct_links()
            .map(|link| (link.dep_name(), link.to()))
            .collect();

        metadata
            .named_features_full()
            .for_each(|(n, named_feature, feature_deps)| {
                let from_node = FeatureNode::new(metadata.package_ix(), n);
                let to_nodes: Vec<_> = feature_deps
                    .iter()
                    .filter_map(|feature_dep| {
                        let (dep_name, to_feature_name) = Self::split_feature_dep(feature_dep);
                        match dep_name {
                            Some(dep_name) => {
                                match dep_name_to_metadata.get(dep_name) {
                                    Some(to_metadata) => {
                                        match to_metadata.get_feature_idx(to_feature_name) {
                                            Some(to_feature_idx) => Some(FeatureNode::new(
                                                to_metadata.package_ix(),
                                                to_feature_idx,
                                            )),
                                            None => {
                                                // It is possible to specify a feature that doesn't
                                                // actually exist, and cargo will accept that if the
                                                // feature isn't resolved. One example is the cfg-if
                                                // crate, where version 0.1.9 has the
                                                // `rustc-dep-of-std` feature commented out, and
                                                // several crates try to enable that feature:
                                                // https://github.com/alexcrichton/cfg-if/issues/22
                                                //
                                                // Since these aren't fatal errors, it seems like
                                                // the best we can do is to store such issues as
                                                // warnings.
                                                self.warnings
                                                    .push(FeatureGraphWarning::MissingFeature {
                                                    stage:
                                                        FeatureBuildStage::AddNamedFeatureEdges {
                                                            package_id: metadata.id().clone(),
                                                            from_feature: named_feature.to_string(),
                                                        },
                                                    package_id: to_metadata.id().clone(),
                                                    feature_name: to_feature_name.to_string(),
                                                });
                                                None
                                            }
                                        }
                                    }
                                    None => {
                                        // This is an unresolved feature -- it won't be included as
                                        // a dependency.
                                        // XXX revisit this if we start modeling unresolved
                                        // dependencies.
                                        None
                                    }
                                }
                            }
                            None => {
                                match metadata.get_feature_idx(to_feature_name) {
                                    Some(to_feature_idx) => Some(FeatureNode::new(
                                        metadata.package_ix(),
                                        to_feature_idx,
                                    )),
                                    None => {
                                        // See blurb above, though maybe this should be tightened a
                                        // bit (errors and not warning?)
                                        self.warnings.push(FeatureGraphWarning::MissingFeature {
                                            stage: FeatureBuildStage::AddNamedFeatureEdges {
                                                package_id: metadata.id().clone(),
                                                from_feature: named_feature.to_string(),
                                            },
                                            package_id: metadata.id().clone(),
                                            feature_name: to_feature_name.to_string(),
                                        });
                                        None
                                    }
                                }
                            }
                        }
                    })
                    // The filter_map above holds an &mut reference to self, which is why it needs to be
                    // collected.
                    .collect();

                // Don't create a map to the base 'from' node since it is already created in
                // add_nodes.
                self.add_edges(
                    from_node,
                    to_nodes
                        .into_iter()
                        .map(|to_node| (to_node, FeatureEdge::FeatureDependency)),
                );
            })
    }

    /// Split a feature dep into package and feature names.
    ///
    /// "foo" -> (None, "foo")
    /// "dep/foo" -> (Some("dep"), "foo")
    fn split_feature_dep(feature_dep: &str) -> (Option<&str>, &str) {
        let mut rsplit = feature_dep.rsplitn(2, '/');
        let to_feature_name = rsplit
            .next()
            .expect("rsplitn should return at least one element");
        let dep_name = rsplit.next();

        (dep_name, to_feature_name)
    }

    pub(super) fn add_dependency_edges(&mut self, link: PackageLink<'_>) {
        let from = link.from();

        // Sometimes the same package is depended on separately in different sections like so:
        //
        // bar/Cargo.toml:
        //
        // [dependencies]
        // foo = { version = "1", features = ["a"] }
        //
        // [build-dependencies]
        // foo = { version = "1", features = ["b"] }
        //
        // Now if you have a crate 'baz' with:
        //
        // [dependencies]
        // bar = { path = "../bar" }
        //
        // ... what features would you expect foo to be built with? You might expect it to just
        // be built with "a", but as it turns out Cargo actually *unifies* the features, such
        // that foo is built with both "a" and "b".
        //
        // Also, feature unification is impacted by whether the dependency is optional.
        //
        // [dependencies]
        // foo = { version = "1", features = ["a"] }
        //
        // [build-dependencies]
        // foo = { version = "1", optional = true, features = ["b"] }
        //
        // This will include 'foo' as a normal dependency but *not* as a build dependency by
        // default.
        // * Without '--features foo', the `foo` dependency will be built with "a".
        // * With '--features foo', `foo` will be both a normal and a build dependency, with
        //   features "a" and "b" in both instances.
        //
        // This means that up to two separate edges have to be represented:
        // * a 'required edge', which will be from the base node for 'from' to the feature nodes
        //   for each required feature in 'to'.
        // * an 'optional edge', which will be from the feature node (from, dep_name) to the
        //   feature nodes for each optional feature in 'to'. This edge is only added if at least
        //   one line is optional.

        let unified_metadata = iter::once((DependencyKind::Normal, link.normal()))
            .chain(iter::once((DependencyKind::Build, link.build())))
            .chain(iter::once((DependencyKind::Development, link.dev())));

        let mut required_req = FeatureReq::new(link);
        let mut optional_req = FeatureReq::new(link);
        for (kind, dependency_req) in unified_metadata {
            required_req.add_features(kind, &dependency_req.inner.required, &mut self.warnings);
            optional_req.add_features(kind, &dependency_req.inner.optional, &mut self.warnings);
        }

        // Add the required edges (base -> features).
        self.add_edges(FeatureNode::base(from.package_ix()), required_req.finish());

        if !optional_req.is_empty() {
            // This means that there is at least one instance of this dependency with optional =
            // true. The dep name should have been added as an optional dependency node to the
            // package metadata.
            let from_node = FeatureNode::new(
                from.package_ix(),
                from.get_feature_idx(link.dep_name()).unwrap_or_else(|| {
                    panic!(
                        "while adding feature edges, for package '{}', optional dep '{}' missing",
                        from.id(),
                        link.dep_name(),
                    );
                }),
            );
            self.add_edges(from_node, optional_req.finish());
        }
    }

    fn add_node(
        &mut self,
        feature_id: FeatureNode,
        feature_type: FeatureType,
    ) -> NodeIndex<FeatureIx> {
        let feature_ix = self.graph.add_node(feature_id.clone());
        self.map.insert(
            feature_id,
            FeatureMetadataImpl {
                feature_ix,
                feature_type,
            },
        );
        feature_ix
    }

    fn add_edges(
        &mut self,
        from_node: FeatureNode,
        to_nodes_edges: impl IntoIterator<Item = (FeatureNode, FeatureEdge)>,
    ) {
        // The from node should always be present because it is a known node.
        let from_ix = self.lookup_node(&from_node).unwrap_or_else(|| {
            panic!(
                "while adding feature edges, missing 'from': {:?}",
                from_node
            );
        });
        to_nodes_edges.into_iter().for_each(|(to_node, edge)| {
            let to_ix = self.lookup_node(&to_node).unwrap_or_else(|| {
                panic!("while adding feature edges, missing 'to': {:?}", to_node)
            });
            self.graph.update_edge(from_ix, to_ix, edge);
        })
    }

    fn lookup_node(&self, node: &FeatureNode) -> Option<NodeIndex<FeatureIx>> {
        self.map.get(node).map(|metadata| metadata.feature_ix)
    }

    pub(super) fn build(self) -> FeatureGraphImpl {
        FeatureGraphImpl {
            graph: self.graph,
            base_ixs: self.base_ixs,
            map: self.map,
            warnings: self.warnings,
            sccs: OnceCell::new(),
        }
    }
}

#[derive(Debug)]
struct FeatureReq<'g> {
    link: PackageLink<'g>,
    to: PackageMetadata<'g>,
    to_default_idx: Option<usize>,
    // This will contain any build states that aren't empty.
    features: HashMap<Option<usize>, DependencyBuildState>,
}

impl<'g> FeatureReq<'g> {
    fn new(link: PackageLink<'g>) -> Self {
        let to = link.to();
        Self {
            link,
            to,
            to_default_idx: to.get_feature_idx("default"),
            features: HashMap::new(),
        }
    }

    fn is_empty(&self) -> bool {
        // self.features only consists of non-empty build states.
        self.features.is_empty()
    }

    fn add_features(
        &mut self,
        dep_kind: DependencyKind,
        req: &DepRequiredOrOptional,
        warnings: &mut Vec<FeatureGraphWarning>,
    ) {
        // Base feature.
        self.extend(None, dep_kind, &req.build_if);
        // Default feature (or base if it isn't present).
        self.extend(self.to_default_idx, dep_kind, &req.default_features_if);

        for (feature, status) in &req.feature_targets {
            match self.to.get_feature_idx(feature) {
                Some(feature_idx) => {
                    self.extend(Some(feature_idx), dep_kind, status);
                }
                None => {
                    // The destination feature is missing -- this is accepted by cargo
                    // in some circumstances, so use a warning rather than an error.
                    warnings.push(FeatureGraphWarning::MissingFeature {
                        stage: FeatureBuildStage::AddDependencyEdges {
                            package_id: self.link.from().id().clone(),
                            dep_name: self.link.dep_name().to_string(),
                        },
                        package_id: self.to.id().clone(),
                        feature_name: feature.to_string(),
                    });
                }
            }
        }
    }

    fn extend(
        &mut self,
        feature_idx: Option<usize>,
        dep_kind: DependencyKind,
        status: &PlatformStatusImpl,
    ) {
        if !status.is_never() {
            self.features
                .entry(feature_idx)
                .or_default()
                .extend(dep_kind, status);
        }
    }

    fn finish(self) -> impl Iterator<Item = (FeatureNode, FeatureEdge)> {
        let package_ix = self.to.package_ix();
        self.features
            .into_iter()
            .map(move |(feature_idx, build_state)| {
                // extend ensures that the build states aren't empty. Double-check that.
                debug_assert!(!build_state.is_empty(), "build states are always non-empty");
                (
                    FeatureNode::new_opt(package_ix, feature_idx),
                    build_state.finish(),
                )
            })
    }
}

#[derive(Debug, Default)]
struct DependencyBuildState {
    normal: PlatformStatusImpl,
    build: PlatformStatusImpl,
    dev: PlatformStatusImpl,
}

impl DependencyBuildState {
    fn extend(&mut self, dep_kind: DependencyKind, status: &PlatformStatusImpl) {
        match dep_kind {
            DependencyKind::Normal => self.normal.extend(status),
            DependencyKind::Build => self.build.extend(status),
            DependencyKind::Development => self.dev.extend(status),
            _ => panic!("unknown dependency kind"),
        }
    }

    fn is_empty(&self) -> bool {
        self.normal.is_never() && self.build.is_never() && self.dev.is_never()
    }

    fn finish(self) -> FeatureEdge {
        FeatureEdge::Dependency {
            normal: self.normal,
            build: self.build,
            dev: self.dev,
        }
    }
}
