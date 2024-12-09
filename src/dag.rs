use std::{
    borrow::Borrow,
    collections::HashMap,
    error::Error,
    fmt::{Debug, Display},
    hash::Hash,
};

#[allow(unused_imports)]
use log::debug;

pub trait GraphNode {
    type NodeId: Hash + Eq + Clone + Debug;

    // Identifier for a node, unique among nodes in the set under consideration.
    fn id(&self) -> impl Borrow<Self::NodeId>;
    // IDs of nodes that have an edge from this node to that node.
    fn child_ids(&self) -> Vec<impl Borrow<Self::NodeId>>;
}

// Ajacency-list for a directed acyclic "graph" (dunno maybe incorrect
// terminology, it doesn't make any promises about connectedness so it might be
// zero or several actual "graphs"), where nodes are identified with a usize.
#[derive(Debug)]
pub struct Dag<G: GraphNode> {
    nodes: Vec<G>,
    // maps ids that nodes know about themselves to their index in `nodes`.
    id_to_idx: HashMap<G::NodeId, usize>,
    // edges[i] contains the destinations of the edges originating from node i.
    edges: Vec<Vec<usize>>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum DagError<I> {
    // Two nodes had the same ID
    DuplicateId(I),
    // Node identified by `parent` referred to `child`, but the latter didn't exist.
    NoSuchChild { parent: I, child: I },
    // A cycle existed containing the node with this ID,
    Cycle(I),
}

impl<I: Debug> Display for DagError<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            Self::DuplicateId(id) => write!(f, "duplicate key {:?}", id),
            Self::NoSuchChild { parent, child } => {
                write!(f, "{:?} refers to nonexistent {:?}", parent, child)
            }
            Self::Cycle(id) => write!(f, "cycle in graph, containing {:?}", id),
        }
    }
}

impl<I: Debug> Error for DagError<I> {}

impl<G: GraphNode> Dag<G> {
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            id_to_idx: HashMap::new(),
            edges: Vec::new(),
        }
    }

    pub fn new(nodes: impl IntoIterator<Item = G>) -> Result<Self, DagError<G::NodeId>> {
        let nodes: Vec<G> = nodes.into_iter().collect();

        // We eventually wanna have a vector and just index it by an integer, so
        // start by mapping the arbitrary "node IDs" to vec indexes.
        // At this point we also reject duplicates (this is why we don't just
        // wanna use `collect`).
        let mut id_to_idx = HashMap::new();
        for (idx, node) in nodes.iter().enumerate() {
            let id = node.id();
            let id = id.borrow();
            if id_to_idx.contains_key(id) {
                return Err(DagError::DuplicateId(id.clone()));
            }
            id_to_idx.insert(id.clone(), idx);
        }

        // Now build the adjacency list.
        let mut edges = vec![Vec::new(); nodes.len()];
        for (idx, node) in nodes.iter().enumerate() {
            for child_id in node.child_ids() {
                let child_idx =
                    id_to_idx
                        .get(child_id.borrow())
                        .ok_or_else(|| DagError::NoSuchChild {
                            parent: node.id().borrow().clone(),
                            child: child_id.borrow().clone(),
                        })?;
                edges[idx].push(*child_idx);
            }
        }

        let dag = Self {
            nodes,
            edges,
            id_to_idx,
        };
        dag.bottom_up().check_cycles()?;
        Ok(dag)
    }

    // Return a new graph with a node added.
    pub fn with_node(mut self, node: G) -> Result<Self, DagError<G::NodeId>> {
        let new_idx = self.nodes.len();
        self.id_to_idx.insert(node.id().borrow().clone(), new_idx);
        self.edges.push(
            node.child_ids()
                .into_iter()
                .map(|id| {
                    self.id_to_idx
                        .get(id.borrow())
                        .ok_or(DagError::NoSuchChild {
                            parent: node.id().borrow().clone(),
                            child: id.borrow().clone(),
                        })
                        .copied()
                })
                .collect::<Result<Vec<_>, DagError<G::NodeId>>>()?,
        );
        self.nodes.push(node);
        Ok(self)
    }

    // Iterate over nodes, visiting children before their parents.
    pub fn bottom_up(&self) -> TopologicalSort<'_, G> {
        TopologicalSort::new(&self, (0..self.nodes.len()).collect())
    }

    pub fn nodes(&self) -> impl Iterator<Item = &G> + Clone {
        self.nodes.iter()
    }

    pub fn node(&self, id: &G::NodeId) -> Option<&G> {
        // TODO this is dumb lol get rid of id_to_idx
        Some(&self.nodes[*self.id_to_idx.get(id.borrow())?])
    }

    // Iterate all the descendants of the relevant node, visiting parents before
    // their children.
    pub fn top_down_from(&self, id: &G::NodeId) -> Option<impl Iterator<Item = &G>> {
        // Mindlessly recycle `TopologicalSort`, just reverse it and BAM!
        // The back-and-forth of iterators is super awkward but necessary,
        // because we need something with `DoubleEndedIterator` trait (i.e. Vec::<_>).
        // Maybe there's a better way.
        Some(
            TopologicalSort::new(&self, vec![*self.id_to_idx.get(id.borrow())?])
                .into_iter()
                .collect::<Vec<&G>>()
                .into_iter()
                .rev(),
        )
    }
}

// Possible states of a node during DFS.
#[derive(Clone, PartialEq, Eq)]
enum NodeState {
    New,    // Initial state of every node.
    Opened, // Node visited (pushed to the stack) but not yet closed.
    Closed, // Node popped from the stack, all descendants visited.
}

// Struct that iterates over the nodes reachable by a set of given sources
// in topological order (https://en.wikipedia.org/wiki/Topological_sorting),
// that is, every node comes _after_ all its children (yes, techinically
// this is reverse toposort, but here it kinda makes sense to call it that
// since edges are meant to represent dependencies).
#[derive(Clone)]
pub struct TopologicalSort<'a, G: GraphNode> {
    dag: &'a Dag<G>,
    dfs_stack: Vec<usize>,
    node_state: Vec<NodeState>,
}

impl<'a, G: GraphNode> TopologicalSort<'a, G> {
    // The constructed instance will iterate over all nodes that are reachable
    // from at least one of the given `sources`.
    fn new(dag: &'a Dag<G>, sources: Vec<usize>) -> Self {
        TopologicalSort {
            dag,
            dfs_stack: sources,
            node_state: vec![NodeState::New; dag.nodes.len()],
        }
    }

    // Private method that advances the iterator while checking for cycles.
    //
    // This is the iterative version of the DFS-based toposort implementation
    // (https://en.wikipedia.org/wiki/Topological_sorting#Depth-first_search).
    // Basically: do a normal DFS starting from the sources, and a node is
    // appended to the toposort as soon as it is closed.
    //
    // If cycles are present, `try_next()` will find one eventually and
    // return a `DagError::Cycle`. Subsequent calls will return `Ok(None)`.
    fn try_next(&mut self) -> Result<Option<&'a G>, DagError<G::NodeId>> {
        while let Some(v) = self.dfs_stack.pop() {
            match self.node_state[v] {
                NodeState::New => {
                    self.node_state[v] = NodeState::Opened;
                    // Don't actually pop yet.
                    self.dfs_stack.push(v);
                    // If any child of v is `Opened`, we fonud a cycle!
                    // This includes the case of a self-loop at v.
                    if self.dag.edges[v]
                        .iter()
                        .any(|&u| self.node_state[u] == NodeState::Opened)
                    {
                        // Some cleanup + subsequent calls will return `None`.
                        self.dfs_stack.clear();
                        return Err(DagError::Cycle(self.dag.nodes[v].id().borrow().clone()));
                    }
                    self.dfs_stack.extend(
                        self.dag.edges[v]
                            .iter()
                            .copied()
                            .filter(|&u| self.node_state[u] == NodeState::New),
                    );
                }
                NodeState::Opened => {
                    self.node_state[v] = NodeState::Closed;
                    return Ok(Some(&self.dag.nodes[v]));
                }
                NodeState::Closed => {}
            }
        }
        Ok(None)
    }

    fn check_cycles(mut self) -> Result<(), DagError<G::NodeId>> {
        while self.try_next()?.is_some() {}
        Ok(())
    }
}

impl<'a, G: GraphNode> Iterator for TopologicalSort<'a, G> {
    type Item = &'a G;

    fn next(&mut self) -> Option<Self::Item> {
        self.try_next().expect("found cycle while iterating DAG")
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    // We don't have any actual need to clone these but for weird Rust reasons
    // (https://users.rust-lang.org/t/unnecessary-trait-bound-requirement-for-clone/110045)
    // the Clone implementation derived for BottomUp has a bound that the graph
    // node type is Clone.
    #[derive(Debug, Eq, PartialEq, Hash, Clone)]
    struct TestGraphNode {
        id: usize,
        child_ids: Vec<usize>,
    }

    impl GraphNode for TestGraphNode {
        type NodeId = usize;

        fn id(&self) -> impl Borrow<usize> {
            self.id
        }

        fn child_ids(&self) -> Vec<impl Borrow<usize>> {
            self.child_ids.iter().collect()
        }
    }

    fn nodes(edges: impl IntoIterator<Item = Vec<usize>>) -> Vec<TestGraphNode> {
        edges
            .into_iter()
            .enumerate()
            .map(|(id, child_ids)| TestGraphNode { id, child_ids })
            .collect()
    }

    #[test_case(vec![], None; "empty")]
    #[test_case(nodes([vec![1], vec![]]), None; "one edge")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![]]), None; "tree")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![],
                       vec![5], vec![6, 7], vec![], vec![]]), None; "trees")]
    #[test_case(nodes([vec![0]]), Some(DagError::Cycle(0)); "self-link")]
    // Note we don't actually care that the Cycle is reported on node 2, but
    // luckily that's stable behaviour so it's just easy to assert it that way.
    #[test_case(nodes([vec![1], vec![2], vec![3], vec![0]]), Some(DagError::Cycle(2)); "a loop")]
    #[test_case(nodes([vec![1]]), Some(DagError::NoSuchChild{parent: 0, child: 1}); "no child")]
    fn test_graph_validity(edges: Vec<TestGraphNode>, want_err: Option<DagError<usize>>) {
        assert_eq!(Dag::new(edges).err(), want_err);
    }

    // Most of the "want" values here are just one of many possible valid
    // orders, but the algorithm is stable and I think it would be easier
    // to just rewrite all the test cases if the algorithm changes,
    // than have a clever (a.k.a buggy) test that tries to really just
    // assert what mattters.
    #[test_case(vec![], vec![]; "empty")]
    #[test_case(nodes([vec![1], vec![]]), vec![1, 0]; "one edge")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![]]), vec![3, 2, 1, 0]; "tree")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![],
                       vec![5], vec![6, 7], vec![], vec![]]),
                vec![7, 6, 5, 4, 3, 2, 1, 0]; "trees")]
    #[test_case(nodes([vec![1, 2], vec![3], vec![3], vec![]]),
                vec![3, 2, 1, 0]; "diamond")]
    fn test_bottom_up(edges: Vec<TestGraphNode>, want_order: Vec<usize>) {
        let dag = Dag::new(edges).unwrap();
        let order = dag.bottom_up();
        assert_eq!(
            order.map(|node| node.id).collect::<Vec<_>>(),
            want_order,
            "Some nodes have been visited more than once"
        );
    }

    #[test_case(nodes([vec![1], vec![]]), 0, vec![0, 1]; "one edge")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![]]),
                0, vec![0, 1, 2, 3]; "tree")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![]]),
                1, vec![1, 2, 3]; "tree non root")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![],
                       vec![5], vec![6, 7], vec![], vec![]]),
                0, vec![0, 1, 2, 3]; "trees 1")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![],
                       vec![5], vec![6, 7], vec![], vec![]]),
                4, vec![4, 5, 6, 7]; "trees 2")]
    #[test_case(nodes([vec![1, 2], vec![4], vec![3], vec![4], vec![]]),
                0, vec![0, 1, 2, 3, 4]; "asymmetric diamond")]
    fn test_top_down(edges: Vec<TestGraphNode>, from: usize, want_order: Vec<usize>) {
        let dag = Dag::new(edges).unwrap();
        let order = dag.top_down_from(&from).unwrap();
        assert_eq!(order.map(|node| node.id).collect::<Vec<_>>(), want_order);
    }
}
