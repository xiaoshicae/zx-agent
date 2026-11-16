//! Rebuildable indexes of the append-only tree: ancestor checks and the nearest request
//! configuration, without walking a long branch for every request record.

#[derive(Clone, Copy)]
pub(super) enum RecordKind {
    RequestConfig,
    Other,
}

#[derive(Default)]
pub(super) struct TreeIndex {
    nodes: Vec<Node>,
}

struct Node {
    depth: usize,
    /// Ancestors at distances 1, 2, 4, ... (binary lifting).
    jumps: Vec<usize>,
    request_config: Option<usize>,
}

impl TreeIndex {
    pub(super) fn push(&mut self, parent: Option<usize>, kind: RecordKind) {
        let mut jumps = Vec::new();
        let mut next = parent;
        while let Some(index) = next {
            jumps.push(index);
            next = self.nodes[index].jumps.get(jumps.len() - 1).copied();
        }
        self.nodes.push(Node {
            depth: parent.map_or(0, |index| self.nodes[index].depth + 1),
            jumps,
            request_config: if matches!(kind, RecordKind::RequestConfig) {
                Some(self.nodes.len())
            } else {
                parent.and_then(|index| self.nodes[index].request_config)
            },
        });
    }

    pub(super) fn request_config(&self, parent: usize) -> Option<usize> {
        self.nodes[parent].request_config
    }

    pub(super) fn contains(&self, ancestor: usize, mut leaf: usize) -> bool {
        let Some(mut distance) = self.nodes[leaf].depth.checked_sub(self.nodes[ancestor].depth) else {
            return false;
        };
        let mut level = 0;
        while distance > 0 {
            if distance & 1 != 0 {
                leaf = self.nodes[leaf].jumps[level];
            }
            distance >>= 1;
            level += 1;
        }
        ancestor == leaf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestry_and_request_configs_follow_branches_and_multiple_roots() {
        let mut tree = TreeIndex::default();
        tree.push(None, RecordKind::RequestConfig);
        for index in 1..1024 {
            tree.push(Some(index - 1), RecordKind::Other);
        }
        tree.push(Some(0), RecordKind::RequestConfig);
        tree.push(Some(1024), RecordKind::Other);
        tree.push(None, RecordKind::Other);
        assert!(tree.contains(0, 1023));
        assert!(tree.contains(500, 1023));
        assert!(!tree.contains(1024, 1023));
        assert!(!tree.contains(1, 1025));
        assert!(!tree.contains(0, 1026));
        assert_eq!(tree.request_config(1023), Some(0));
        assert_eq!(tree.request_config(1025), Some(1024));
        assert_eq!(tree.request_config(1026), None);
    }
}
