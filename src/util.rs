use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

use anyhow::bail;

pub trait GraphNode {
    type NodeId: Hash + Eq + Clone;

    // Identifier for a node, unique among nodes in the set under consideration.
    fn id(&self) -> &Self::NodeId;
    // IDs of nodes that have an edge from this node to that node.
    fn child_ids(&self) -> &Vec<Self::NodeId>;
}

// Starting from the node at start_idx, visit all connected nodes and call f.
// This will hang if there are cycles in the specified graph.
pub fn visit_all<G: GraphNode, F: FnMut(&G)>(nodes: &Vec<G>, start_idx: usize, mut f: F) {
    fn recurse<G: GraphNode, F: FnMut(&G)>(
        nodes: &Vec<G>,
        start_idx: usize,
        id_to_idx: &HashMap<G::NodeId, usize>,
        f: &mut F,
    ) {
        let start_node = &nodes[start_idx];
        f(start_node);
        for child_id in start_node.child_ids().iter() {
            recurse(nodes, id_to_idx[child_id], id_to_idx, f);
        }
    }

    let id_to_idx: HashMap<G::NodeId, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id().clone(), i))
        .collect();
    recurse(nodes, start_idx, &id_to_idx, &mut f);
}

// Return an error if any of the graphs described by the nodes have any cycles.
pub fn check_no_cycles<G: GraphNode>(nodes: &Vec<G>) -> anyhow::Result<()> {
    let id_to_idx: HashMap<G::NodeId, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id().clone(), i))
        .collect();
    // We'll assume the graph is pretty small and not try to do anything
    // clever here. Just do a DFS from each starting point and maintain a
    // set of observed nodes, using the simplest possible code even if it
    // means pointless copies.
    // let check = |start_idx: usize, seen: HashSet<String>| -> anyhow::Result<()> {
    //
    // This is a bit annoying in Rust because you cannot capture
    // environments into a named function but you cannot recurse into a
    // closure, so we just have to pass everything through args explicitly.
    fn check<G: GraphNode>(
        nodes: &Vec<G>,
        start_idx: usize,
        seen: &HashSet<G::NodeId>,
        id_to_idx: &HashMap<G::NodeId, usize>,
    ) -> anyhow::Result<()> {
        let start_node = &nodes[start_idx];
        if seen.contains(start_node.id()) {
            bail!("Cycle in test dependency graph");
        }
        let mut seen = seen.clone();
        seen.insert(start_node.id().clone());
        for child_id in start_node.child_ids().iter() {
            check(nodes, id_to_idx[child_id], &seen, id_to_idx)?;
        }
        Ok(())
    }
    for i in 0..nodes.len() {
        check(nodes, i, &HashSet::new(), &id_to_idx)?;
    }
    Ok(())
}
