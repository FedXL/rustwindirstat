use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub files: u64,
    pub folders: u64,
    pub modified: Option<SystemTime>,
    pub attributes: u32,
}

#[derive(Debug)]
pub struct TreeBuilder {
    nodes: Vec<Node>,
    by_path: HashMap<String, usize>,
    root: PathBuf,
}

impl TreeBuilder {
    pub fn new(root: PathBuf) -> Self {
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| root.display().to_string());
        let key = path_key(&root);
        let root_node = Node {
            name,
            path: root.clone(),
            size: 0,
            is_dir: true,
            parent: None,
            children: Vec::new(),
            files: 0,
            folders: 0,
            modified: None,
            attributes: 0,
        };
        Self {
            nodes: vec![root_node],
            by_path: HashMap::from([(key, 0)]),
            root,
        }
    }

    pub fn add(
        &mut self,
        path: PathBuf,
        is_dir: bool,
        size: u64,
        modified: Option<SystemTime>,
        attributes: u32,
    ) {
        let path = clean_path(path);
        let key = path_key(&path);
        if let Some(&existing) = self.by_path.get(&key) {
            let node = &mut self.nodes[existing];
            if !is_dir {
                node.size = size;
                node.is_dir = false;
            }
            node.modified = modified.or(node.modified);
            if attributes != 0 {
                node.attributes = attributes;
            }
            return;
        }

        let parent = self.ensure_parent_chain(&path);
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_owned();
        let index = self.nodes.len();
        self.nodes.push(Node {
            name,
            path: path.clone(),
            size: if is_dir { 0 } else { size },
            is_dir,
            parent: Some(parent),
            children: Vec::new(),
            files: 0,
            folders: 0,
            modified,
            attributes,
        });
        self.nodes[parent].children.push(index);
        self.by_path.insert(key, index);
    }

    fn ensure_parent_chain(&mut self, path: &Path) -> usize {
        let mut missing = Vec::new();
        let mut cursor = path.parent();
        let mut known = 0;

        while let Some(parent_path) = cursor {
            if let Some(&index) = self.by_path.get(&path_key(parent_path)) {
                known = index;
                break;
            }
            if parent_path == self.root {
                known = 0;
                break;
            }
            missing.push(parent_path.to_path_buf());
            cursor = parent_path.parent();
        }

        for dir_path in missing.into_iter().rev() {
            let key = path_key(&dir_path);
            if let Some(&index) = self.by_path.get(&key) {
                known = index;
                continue;
            }
            let name = dir_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_owned();
            let index = self.nodes.len();
            self.nodes.push(Node {
                name,
                path: dir_path,
                size: 0,
                is_dir: true,
                parent: Some(known),
                children: Vec::new(),
                files: 0,
                folders: 0,
                modified: None,
                attributes: 0,
            });
            self.nodes[known].children.push(index);
            self.by_path.insert(key, index);
            known = index;
        }
        known
    }

    pub fn finish(mut self) -> Vec<Node> {
        // TreeBuilder always creates parents before descendants. Folding in reverse
        // insertion order is a postorder traversal without a depth array or N log N sort.
        for index in (1..self.nodes.len()).rev() {
            if self.nodes[index].is_dir {
                self.nodes[index].folders = 1;
            } else {
                self.nodes[index].files = 1;
            }
        }
        for index in (1..self.nodes.len()).rev() {
            if let Some(parent) = self.nodes[index].parent {
                let child_size = self.nodes[index].size;
                let child_files = self.nodes[index].files;
                let child_folders = self.nodes[index].folders;
                let child_modified = self.nodes[index].modified;
                let parent_node = &mut self.nodes[parent];
                parent_node.size = parent_node.size.saturating_add(child_size);
                parent_node.files = parent_node.files.saturating_add(child_files);
                parent_node.folders = parent_node.folders.saturating_add(child_folders);
                if let Some(modified) = child_modified {
                    parent_node.modified = Some(
                        parent_node
                            .modified
                            .map_or(modified, |old| old.max(modified)),
                    );
                }
            }
        }
        for index in 0..self.nodes.len() {
            let mut children = std::mem::take(&mut self.nodes[index].children);
            children.sort_by(|&a, &b| {
                self.nodes[b]
                    .size
                    .cmp(&self.nodes[a].size)
                    .then_with(|| {
                        cmp_ascii_case_insensitive(&self.nodes[a].name, &self.nodes[b].name)
                    })
                    .then_with(|| self.nodes[a].name.cmp(&self.nodes[b].name))
            });
            self.nodes[index].children = children;
        }
        self.nodes
    }

    pub fn merge_finished_shards(mut shards: Vec<Vec<Node>>) -> Vec<Node> {
        assert!(!shards.is_empty());
        let mut merged = shards.remove(0);
        for shard in shards {
            if shard.len() <= 1 {
                continue;
            }
            let offset = merged.len() - 1;
            let shard_root = &shard[0];
            merged[0].size = merged[0].size.saturating_add(shard_root.size);
            merged[0].files = merged[0].files.saturating_add(shard_root.files);
            merged[0].folders = merged[0].folders.saturating_add(shard_root.folders);
            if let Some(modified) = shard_root.modified {
                merged[0].modified =
                    Some(merged[0].modified.map_or(modified, |old| old.max(modified)));
            }
            merged[0]
                .children
                .extend(shard_root.children.iter().map(|&child| child + offset));

            for mut node in shard.into_iter().skip(1) {
                if let Some(parent) = &mut node.parent {
                    if *parent != 0 {
                        *parent += offset;
                    }
                }
                for child in &mut node.children {
                    *child += offset;
                }
                merged.push(node);
            }
        }
        let mut root_children = std::mem::take(&mut merged[0].children);
        root_children.sort_by(|&a, &b| {
            merged[b]
                .size
                .cmp(&merged[a].size)
                .then_with(|| cmp_ascii_case_insensitive(&merged[a].name, &merged[b].name))
                .then_with(|| merged[a].name.cmp(&merged[b].name))
        });
        merged[0].children = root_children;
        merged
    }
}

fn cmp_ascii_case_insensitive(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(b.bytes().map(|byte| byte.to_ascii_lowercase()))
}

fn clean_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(stripped) = text.strip_prefix(r"\\.\") {
            return PathBuf::from(stripped);
        }
    }
    path
}

fn path_key(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    value.make_ascii_lowercase();
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn summary(
        nodes: &[Node],
    ) -> HashMap<PathBuf, (u64, bool, u64, u64, Option<PathBuf>, Vec<PathBuf>)> {
        nodes
            .iter()
            .map(|node| {
                let parent = node.parent.map(|index| nodes[index].path.clone());
                let mut children: Vec<_> = node
                    .children
                    .iter()
                    .map(|&index| nodes[index].path.clone())
                    .collect();
                children.sort();
                (
                    node.path.clone(),
                    (
                        node.size,
                        node.is_dir,
                        node.files,
                        node.folders,
                        parent,
                        children,
                    ),
                )
            })
            .collect()
    }

    #[test]
    fn merging_independent_shards_preserves_tree_totals_and_links() {
        let root = PathBuf::from("root");
        let alpha = root.join("Alpha");
        let alpha_file = alpha.join("one.bin");
        let nested_file = alpha.join("Nested").join("two.bin");
        let beta = root.join("Beta");
        let beta_file = beta.join("three.bin");

        let mut sequential = TreeBuilder::new(root.clone());
        sequential.add(alpha_file.clone(), false, 100, None, 0);
        sequential.add(nested_file.clone(), false, 250, None, 0);
        sequential.add(beta_file.clone(), false, 50, None, 0);
        let expected = summary(&sequential.finish());

        let mut alpha_shard = TreeBuilder::new(root.clone());
        alpha_shard.add(alpha_file, false, 100, None, 0);
        alpha_shard.add(nested_file, false, 250, None, 0);
        let mut beta_shard = TreeBuilder::new(root);
        beta_shard.add(beta_file, false, 50, None, 0);
        let actual = summary(&TreeBuilder::merge_finished_shards(vec![
            alpha_shard.finish(),
            beta_shard.finish(),
            TreeBuilder::new(PathBuf::from("root")).finish(),
        ]));

        assert_eq!(actual, expected);
    }
}
