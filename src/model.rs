use crate::paths::{normalize, normalize_owned};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::ops::{ControlFlow, Deref};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug)]
pub struct Node {
    pub name: Box<OsStr>,
    pub size: u64,
    pub is_dir: bool,
    parent: Option<NonZeroU32>,
    pub children: Box<[usize]>,
    pub files: u64,
    pub folders: u64,
    pub modified: Option<SystemTime>,
    pub attributes: u32,
}

impl Node {
    pub fn new(
        name: OsString,
        is_dir: bool,
        size: u64,
        modified: Option<SystemTime>,
        attributes: u32,
    ) -> Self {
        Self {
            name: name.into_boxed_os_str(),
            size: if is_dir { 0 } else { size },
            is_dir,
            parent: None,
            children: Box::new([]),
            files: 0,
            folders: 0,
            modified,
            attributes,
        }
    }
    pub fn parent(&self) -> Option<usize> {
        self.parent.map(|value| value.get() as usize - 1)
    }
    pub fn set_parent(&mut self, parent: Option<usize>) {
        self.parent = parent.map(|index| {
            NonZeroU32::new(u32::try_from(index + 1).expect("node index limit checked by builders"))
                .expect("index plus one is nonzero")
        });
    }
}

#[derive(Debug)]
pub struct Snapshot {
    pub(crate) nodes: Vec<Node>,
    root: PathBuf,
}

impl Deref for Snapshot {
    type Target = [Node];
    fn deref(&self) -> &[Node] {
        &self.nodes
    }
}

impl Snapshot {
    pub(crate) fn new(root: PathBuf, nodes: Vec<Node>) -> Self {
        Self { root, nodes }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn path(&self, index: usize) -> PathBuf {
        let mut names = Vec::new();
        let mut cursor = index;
        while let Some(parent) = self.nodes[cursor].parent() {
            names.push(self.nodes[cursor].name.as_ref());
            cursor = parent;
        }
        let mut result = self.root.clone();
        for name in names.into_iter().rev() {
            result.push(name);
        }
        result
    }
    /// Visit every path using one reusable native path buffer and a stack bounded by depth.
    pub fn visit_paths(&self, mut visit: impl FnMut(usize, &Path) -> ControlFlow<()>) {
        if self.nodes.is_empty() {
            return;
        }
        let mut path = self.root.clone();
        if visit(0, &path).is_break() {
            return;
        }
        let mut stack = vec![(0usize, 0usize)];
        while let Some((parent, next)) = stack.last_mut() {
            if let Some(&child) = self.nodes[*parent].children.get(*next) {
                *next += 1;
                path.push(self.nodes[child].name.as_ref());
                if visit(child, &path).is_break() {
                    return;
                }
                if self.nodes[child].children.is_empty() {
                    path.pop();
                } else {
                    stack.push((child, 0));
                }
            } else {
                stack.pop();
                if !stack.is_empty() {
                    path.pop();
                }
            }
        }
    }
    pub fn size_in_memory(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
            + self.root.as_os_str().as_encoded_bytes().len()
            + self
                .nodes
                .iter()
                .map(|node| {
                    node.name.as_encoded_bytes().len()
                        + std::mem::size_of_val(node.children.as_ref())
                })
                .sum::<usize>()
    }
}

type ChildLookup = HashMap<Box<OsStr>, usize>;

#[derive(Debug)]
pub struct TreeBuilder {
    nodes: Vec<Node>,
    by_parent: HashMap<usize, ChildLookup>,
    root: PathBuf,
}

impl TreeBuilder {
    pub fn new(root: PathBuf) -> Self {
        let root = normalize(&root);
        Self {
            nodes: vec![Node::new(root.as_os_str().to_owned(), true, 0, None, 0)],
            by_parent: HashMap::new(),
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
        let path = normalize_owned(path);
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return;
        };
        let mut components = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .peekable();
        let mut parent = 0;
        while let Some(name) = components.next() {
            let is_last = components.peek().is_none();
            let existing = self
                .by_parent
                .get(&parent)
                .and_then(|children| children.get(name))
                .copied();
            let index = if let Some(index) = existing {
                index
            } else {
                assert!(
                    self.nodes.len() < u32::MAX as usize,
                    "node index limit exceeded"
                );
                let index = self.nodes.len();
                let mut node = Node::new(
                    name.to_owned(),
                    !is_last || is_dir,
                    if is_last { size } else { 0 },
                    None,
                    0,
                );
                node.set_parent(Some(parent));
                self.nodes.push(node);
                self.by_parent
                    .entry(parent)
                    .or_default()
                    .insert(name.to_owned().into_boxed_os_str(), index);
                index
            };
            if is_last {
                let node = &mut self.nodes[index];
                node.is_dir = is_dir;
                node.size = if is_dir { 0 } else { size };
                node.modified = modified.or(node.modified);
                if attributes != 0 {
                    node.attributes = attributes;
                }
            }
            parent = index;
        }
    }
    pub fn finish(mut self) -> Snapshot {
        for (parent, children) in self.by_parent.drain() {
            self.nodes[parent].children = children
                .into_values()
                .collect::<Vec<_>>()
                .into_boxed_slice();
        }
        drop(self.by_parent);
        let order: Vec<_> = (0..self.nodes.len()).collect();
        finalize_nodes(&mut self.nodes, &order);
        Snapshot::new(self.root, self.nodes)
    }
    pub fn merge_finished_shards(mut shards: Vec<Snapshot>) -> Snapshot {
        let mut merged = shards.remove(0);
        let mut root_children = merged.nodes[0].children.to_vec();
        for shard in shards {
            if shard.len() <= 1 {
                continue;
            }
            let offset = merged.len() - 1;
            let root = &shard[0];
            merged.nodes[0].size = merged.nodes[0].size.saturating_add(root.size);
            merged.nodes[0].files = merged.nodes[0].files.saturating_add(root.files);
            merged.nodes[0].folders = merged.nodes[0].folders.saturating_add(root.folders);
            if let Some(modified) = root.modified {
                merged.nodes[0].modified = Some(
                    merged.nodes[0]
                        .modified
                        .map_or(modified, |old| old.max(modified)),
                );
            }
            root_children.extend(root.children.iter().map(|&child| child + offset));
            for mut node in shard.nodes.into_iter().skip(1) {
                if let Some(parent) = node.parent()
                    && parent != 0
                {
                    node.set_parent(Some(parent + offset));
                }
                for child in &mut node.children {
                    *child += offset;
                }
                merged.nodes.push(node);
            }
        }
        root_children.sort_unstable_by(|&a, &b| compare_nodes(&merged[a], &merged[b]));
        merged.nodes[0].children = root_children.into_boxed_slice();
        merged
    }
}

/// `order` must put each parent before its descendants; array indices need not do so.
pub(crate) fn finalize_nodes(nodes: &mut [Node], order: &[usize]) {
    for &index in order.iter().skip(1) {
        nodes[index].files = u64::from(!nodes[index].is_dir);
        nodes[index].folders = u64::from(nodes[index].is_dir);
    }
    for &index in order.iter().skip(1).rev() {
        if let Some(parent) = nodes[index].parent() {
            let (size, files, folders, modified) = (
                nodes[index].size,
                nodes[index].files,
                nodes[index].folders,
                nodes[index].modified,
            );
            let node = &mut nodes[parent];
            node.size = node.size.saturating_add(size);
            node.files = node.files.saturating_add(files);
            node.folders = node.folders.saturating_add(folders);
            if let Some(modified) = modified {
                node.modified = Some(node.modified.map_or(modified, |old| old.max(modified)));
            }
        }
    }
    // Detaching adjacency lists permits immutable reads of node metadata while Rayon sorts
    // independent directories; very wide directories also split across workers.
    let children: Vec<_> = nodes
        .iter_mut()
        .enumerate()
        .filter_map(|(index, node)| {
            (!node.children.is_empty()).then(|| (index, std::mem::take(&mut node.children)))
        })
        .collect();
    let sorted = if let Ok(pool) = crate::parallel::build_pool() {
        use rayon::prelude::*;
        pool.install(|| {
            children
                .into_par_iter()
                .map(|(index, mut children)| {
                    if children.len() >= 4096 {
                        children.par_sort_unstable_by(|&a, &b| compare_nodes(&nodes[a], &nodes[b]));
                    } else {
                        children.sort_unstable_by(|&a, &b| compare_nodes(&nodes[a], &nodes[b]));
                    }
                    (index, children)
                })
                .collect::<Vec<_>>()
        })
    } else {
        children
            .into_iter()
            .map(|(index, mut children)| {
                children.sort_unstable_by(|&a, &b| compare_nodes(&nodes[a], &nodes[b]));
                (index, children)
            })
            .collect()
    };
    for (index, children) in sorted {
        nodes[index].children = children;
    }
}

fn compare_nodes(a: &Node, b: &Node) -> std::cmp::Ordering {
    b.size
        .cmp(&a.size)
        .then_with(|| {
            a.name
                .as_encoded_bytes()
                .iter()
                .map(u8::to_ascii_lowercase)
                .cmp(b.name.as_encoded_bytes().iter().map(u8::to_ascii_lowercase))
        })
        .then_with(|| a.name.cmp(&b.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn summary(nodes: &Snapshot) -> HashMap<PathBuf, (u64, u64, u64)> {
        nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (nodes.path(index), (node.size, node.files, node.folders)))
            .collect()
    }
    #[test]
    fn independent_shards_preserve_paths_sizes_and_counts() {
        let root = PathBuf::from("root");
        let mut all = TreeBuilder::new(root.clone());
        let mut a = TreeBuilder::new(root.clone());
        let mut b = TreeBuilder::new(root.clone());
        for (name, size) in [
            ("Alpha/a.bin", 100),
            ("Alpha/Nested/b.bin", 250),
            ("Beta/c.bin", 50),
        ] {
            all.add(root.join(name), false, size, None, 0);
            let shard = if name.starts_with("Alpha") {
                &mut a
            } else {
                &mut b
            };
            shard.add(root.join(name), false, size, None, 0);
        }
        assert_eq!(
            summary(&all.finish()),
            summary(&TreeBuilder::merge_finished_shards(vec![
                a.finish(),
                b.finish()
            ]))
        );
    }
    #[test]
    fn native_case_and_lossless_paths_are_preserved() {
        let root = PathBuf::from("root");
        let mut builder = TreeBuilder::new(root.clone());
        builder.add(root.join("A.txt"), false, 10, None, 0);
        builder.add(root.join("a.txt"), false, 20, None, 0);
        let nodes = builder.finish();
        assert_eq!(nodes[0].files, 2);
        assert_eq!(nodes[0].size, 30);
        let mut visited = HashMap::new();
        nodes.visit_paths(|index, path| {
            visited.insert(index, path.to_owned());
            ControlFlow::Continue(())
        });
        for (index, path) in visited {
            assert_eq!(nodes.path(index), path);
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let mut builder = TreeBuilder::new(PathBuf::from(r"C:\"));
            let a = PathBuf::from(OsString::from_wide(&[67, 58, 92, 0xD800]));
            let b = PathBuf::from(OsString::from_wide(&[67, 58, 92, 0xD801]));
            builder.add(a.clone(), false, 1, None, 0);
            builder.add(b.clone(), false, 2, None, 0);
            let nodes = builder.finish();
            assert_eq!(nodes[0].files, 2);
            assert!(
                nodes
                    .iter()
                    .enumerate()
                    .any(|(index, _)| nodes.path(index) == a)
            );
            assert!(
                nodes
                    .iter()
                    .enumerate()
                    .any(|(index, _)| nodes.path(index) == b)
            );
        }
    }
}
