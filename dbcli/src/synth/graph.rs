use std::collections::{HashMap, HashSet, VecDeque};

pub fn topological_sort<T: Eq + std::hash::Hash + Clone + std::fmt::Debug>(
    nodes: &[T],
    edges: &[(T, T)],
) -> Result<Vec<T>, String> {
    let mut in_degree: HashMap<&T, usize> = HashMap::new();
    let mut adjacency: HashMap<&T, Vec<&T>> = HashMap::new();

    for node in nodes {
        in_degree.entry(node).or_insert(0);
        adjacency.entry(node).or_insert_with(Vec::new);
    }

    for (from, to) in edges {
        *in_degree.entry(to).or_insert(0) += 1;
        adjacency.entry(from).or_insert_with(Vec::new).push(to);
    }

    let mut queue: VecDeque<&T> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(node, _)| *node)
        .collect();

    let mut order = Vec::new();
    while let Some(node) = queue.pop_front() {
        order.push(node.clone());
        if let Some(neighbors) = adjacency.get(node) {
            for neighbor in neighbors {
                let deg = in_degree.get_mut(neighbor).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(neighbor);
                }
            }
        }
    }

    if order.len() != nodes.len() {
        let cycle_path = find_cycle_path(nodes, edges);
        return Err(format!("cycle detected: {}", cycle_path));
    }

    Ok(order)
}

fn find_cycle_path<T: Eq + std::hash::Hash + Clone + std::fmt::Debug>(
    nodes: &[T],
    edges: &[(T, T)],
) -> String {
    let mut visited = HashSet::new();
    let mut path = Vec::new();

    for node in nodes {
        if !visited.contains(node) {
            if let Some(cycle) = dfs_find_cycle(node, edges, &mut visited, &mut path) {
                return cycle;
            }
        }
    }
    "unknown".to_string()
}

fn dfs_find_cycle<T: Eq + std::hash::Hash + Clone + std::fmt::Debug>(
    node: &T,
    edges: &[(T, T)],
    visited: &mut HashSet<T>,
    path: &mut Vec<T>,
) -> Option<String> {
    if path.contains(node) {
        let cycle_start = path.iter().position(|x| x == node).unwrap();
        let mut cycle: Vec<String> = path[cycle_start..]
            .iter()
            .map(|x| format!("{:?}", x))
            .collect();
        cycle.push(format!("{:?}", node));
        return Some(cycle.join(" -> "));
    }

    path.push(node.clone());
    visited.insert(node.clone());

    for (from, to) in edges {
        if from == node {
            if let Some(cycle) = dfs_find_cycle(to, edges, visited, path) {
                return Some(cycle);
            }
        }
    }

    path.pop();
    None
}

pub fn tarjan_scc<T: Eq + std::hash::Hash + Clone + std::fmt::Debug>(
    nodes: &[T],
    edges: &[(T, T)],
) -> Vec<Vec<T>> {
    let mut state = TarjanState::new(nodes, edges);
    for node in nodes {
        if !state.indices.contains_key(node) {
            state.strongconnect(node);
        }
    }
    state.sccs
}

struct TarjanState<'a, T: Eq + std::hash::Hash + Clone> {
    index_counter: usize,
    stack: Vec<&'a T>,
    indices: HashMap<&'a T, usize>,
    lowlinks: HashMap<&'a T, usize>,
    on_stack: HashSet<&'a T>,
    adjacency: HashMap<&'a T, Vec<&'a T>>,
    sccs: Vec<Vec<T>>,
}

impl<'a, T: Eq + std::hash::Hash + Clone> TarjanState<'a, T> {
    fn new(nodes: &'a [T], edges: &'a [(T, T)]) -> Self {
        let mut adjacency: HashMap<&T, Vec<&T>> = HashMap::new();
        for node in nodes {
            adjacency.entry(node).or_insert_with(Vec::new);
        }
        for (from, to) in edges {
            adjacency.entry(from).or_insert_with(Vec::new).push(to);
        }

        Self {
            index_counter: 0,
            stack: Vec::new(),
            indices: HashMap::new(),
            lowlinks: HashMap::new(),
            on_stack: HashSet::new(),
            adjacency,
            sccs: Vec::new(),
        }
    }

    fn strongconnect(&mut self, v: &'a T) {
        self.indices.insert(v, self.index_counter);
        self.lowlinks.insert(v, self.index_counter);
        self.index_counter += 1;
        self.stack.push(v);
        self.on_stack.insert(v);

        if let Some(neighbors) = self.adjacency.get(v) {
            let neighbors_clone: Vec<&'a T> = neighbors.clone();
            for w in neighbors_clone {
                if !self.indices.contains_key(w) {
                    self.strongconnect(w);
                    let w_low = *self.lowlinks.get(w).unwrap();
                    let v_low = self.lowlinks.get_mut(v).unwrap();
                    if w_low < *v_low {
                        *v_low = w_low;
                    }
                } else if self.on_stack.contains(w) {
                    let w_idx = *self.indices.get(w).unwrap();
                    let v_low = self.lowlinks.get_mut(v).unwrap();
                    if w_idx < *v_low {
                        *v_low = w_idx;
                    }
                }
            }
        }

        let v_low = *self.lowlinks.get(v).unwrap();
        let v_idx = *self.indices.get(v).unwrap();
        if v_low == v_idx {
            let mut scc = Vec::new();
            loop {
                let w = self.stack.pop().unwrap();
                self.on_stack.remove(w);
                scc.push(w.clone());
                if w == v {
                    break;
                }
            }
            self.sccs.push(scc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topological_sort_linear_chain() {
        let edges = vec![("A", "B"), ("B", "C")];
        let order = topological_sort(&["A", "B", "C"], &edges).unwrap();
        assert_eq!(order, vec!["A", "B", "C"]);
    }

    #[test]
    fn topological_sort_with_diamond() {
        let edges = vec![("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")];
        let order = topological_sort(&["A", "B", "C", "D"], &edges).unwrap();
        assert!(order.iter().position(|&x| x == "A") < order.iter().position(|&x| x == "B"));
        assert!(order.iter().position(|&x| x == "A") < order.iter().position(|&x| x == "C"));
        assert!(order.iter().position(|&x| x == "B") < order.iter().position(|&x| x == "D"));
        assert!(order.iter().position(|&x| x == "C") < order.iter().position(|&x| x == "D"));
    }

    #[test]
    fn topological_sort_cycle_detection() {
        let edges = vec![("A", "B"), ("B", "C"), ("C", "A")];
        let result = topological_sort(&["A", "B", "C"], &edges);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("cycle"));
    }

    #[test]
    fn tarjan_scc_finds_cycle_path() {
        let edges = vec![("A", "B"), ("B", "C"), ("C", "A"), ("D", "E")];
        let sccs = tarjan_scc(&["A", "B", "C", "D", "E"], &edges);
        assert_eq!(sccs.len(), 3);
        let cycle_scc = sccs.iter().find(|s| s.len() > 1).unwrap();
        assert!(cycle_scc.contains(&"A"));
        assert!(cycle_scc.contains(&"B"));
        assert!(cycle_scc.contains(&"C"));
    }

    #[test]
    fn topological_sort_projection_reversal() {
        let edges = vec![("orders", "dic_stock")];
        let order = topological_sort(&["orders", "dic_stock"], &edges).unwrap();
        assert_eq!(order, vec!["orders", "dic_stock"]);
    }
}
