use core::fmt;
use std::{
    collections::{HashMap, HashSet},
    fmt::{Debug, Display, Formatter},
    hash::Hash,
    ops::Deref,
    path::PathBuf,
    str::FromStr,
};

use anyhow::bail;

pub trait GraphNode<I: Hash + Eq + Clone> {
    // Identifier for a node, unique among nodes in the set under consideration.
    fn id(&self) -> &I;
    // IDs of nodes that have an edge from this node to that node.
    fn child_ids(&self) -> &Vec<I>;
}

// Ajacency-list for a directed acyclic "graph" (dunno maybe incorrect
// terminology, it doesn't make any promises about connectedness so it might be
// zero or several actual "graphs"), where nodes are identified with a usize.
pub struct Dag<I: Hash + Eq + Clone + Debug, G: GraphNode<I>> {
    _nodes: Vec<G>,
    // maps ids that nodes know about themselves to their index in `nodes`.
    _id_to_idx: HashMap<I, usize>,
    // edges[i] contains the destinations of the edges originating from node i.
    _edges: Vec<Vec<usize>>,
}

pub enum DagError<I> {
    // Two nodes had the same ID
    DuplicateId(I),
    // Node identified by `parent` referred to `child`, but the latter didn't exist.
    NoSuchChild { _parent: I, _child: I },
    // A cycle existed containing the node with this ID,
    Cycle(I),
}

impl<I: Hash + Eq + Clone + Debug, G: GraphNode<I>> Dag<I, G> {
    // TODO: unit test this.
    pub fn new(nodes: Vec<G>) -> Result<Self, DagError<I>>
    where
        G: GraphNode<I>,
    {
        // We eventually wanna have a vector and just index it by an integer, so
        // start by mapping the arbitrary "node IDs" to vec indexes.
        // At this point we also reject duplicates (this is why we don't just
        // wanna use `ollect`).
        let mut id_to_idx = HashMap::new();
        for (idx, node) in nodes.iter().enumerate() {
            let id = node.id();
            if id_to_idx.contains_key(id) {
                return Err(DagError::DuplicateId(id.clone()));
            }
            id_to_idx.insert(id.clone(), idx);
        }

        // Now build the adjacency list.
        let mut edges = Vec::new();
        for (idx, node) in nodes.iter().enumerate() {
            if idx >= edges.len() {
                edges.resize(idx + 1, Vec::new())
            }
            for child_id in node.child_ids() {
                let child_idx = id_to_idx
                    .get(child_id)
                    .ok_or_else(|| DagError::NoSuchChild {
                        _parent: node.id().clone(),
                        _child: child_id.clone(),
                    })?;
                edges[idx].push(*child_idx);
            }
        }

        // Check there are no cycles.
        // This set is just used to avoid duplicating work.
        let mut visited: HashSet<usize> = HashSet::new();
        // This one actually detects cycles.
        let mut visited_stack: HashSet<usize> = HashSet::new();
        // This is a bit annoying in Rust because you cannot capture
        // environments into a named function but you cannot recurse into a
        // closure, so we just have to pass everything through args explicitly.
        fn find_cycle(
            visited: &mut HashSet<usize>,
            visited_stack: &mut HashSet<usize>,
            start_idx: usize,
            edges: &Vec<Vec<usize>>,
        ) -> Option<usize> {
            if visited.contains(&start_idx) {
                // Already explored from this node and found no cycles.
                return None;
            }
            if visited_stack.contains(&start_idx) {
                return Some(start_idx);
            }
            visited.insert(start_idx);
            visited_stack.insert(start_idx);
            for child in &edges[start_idx] {
                if let Some(i) = find_cycle(visited, visited_stack, *child, edges) {
                    return Some(i);
                }
            }
            visited_stack.remove(&start_idx);
            None
        }
        for i in 0..edges.len() {
            if let Some(node_in_cycle) = find_cycle(&mut visited, &mut visited_stack, i, &edges) {
                return Err(DagError::Cycle(nodes[node_in_cycle].id().clone()));
            }
        }

        Ok(Self {
            _nodes: nodes,
            _edges: edges,
            _id_to_idx: id_to_idx,
        })
    }
}

// Starting from the node at start_idx, visit all connected nodes and call f.
// This will hang if there are cycles in the specified graph.
pub fn visit_all<'a, I: Hash + Eq + Clone, G: GraphNode<I>, F: FnMut(&'a G)>(
    nodes: &'a Vec<G>,
    start_idx: usize,
    mut f: F,
) {
    fn recurse<'a, I: Hash + Eq + Clone, G: GraphNode<I>, F: FnMut(&'a G)>(
        nodes: &'a Vec<G>,
        start_idx: usize,
        id_to_idx: &HashMap<I, usize>,
        f: &mut F,
    ) {
        let start_node = &nodes[start_idx];
        f(start_node);
        for child_id in start_node.child_ids().iter() {
            recurse(nodes, id_to_idx[child_id], id_to_idx, f);
        }
    }

    let id_to_idx: HashMap<I, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id().clone(), i))
        .collect();
    recurse(nodes, start_idx, &id_to_idx, &mut f);
}

// Return an error if any of the graphs described by the nodes have any cycles.
pub fn check_no_cycles<I: Hash + Eq + Clone>(nodes: &Vec<impl GraphNode<I>>) -> anyhow::Result<()> {
    let id_to_idx: HashMap<I, usize> = nodes
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
    fn check<I: Hash + Eq + Clone>(
        nodes: &Vec<impl GraphNode<I>>,
        start_idx: usize,
        seen: &HashSet<I>,
        id_to_idx: &HashMap<I, usize>,
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

#[derive(Clone)]
pub struct DisplayablePathBuf(pub PathBuf);

impl FromStr for DisplayablePathBuf {
    type Err = <PathBuf as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PathBuf::from_str(s).map(Self)
    }
}

impl Display for DisplayablePathBuf {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(&self.0.display(), f)
    }
}

impl Deref for DisplayablePathBuf {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.0
    }
}
