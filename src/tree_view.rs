use crate::model::Node;
use std::collections::HashSet;

pub fn centered_row_offset(row: usize, viewport_height: f32, row_height: f32, spacing: f32) -> f32 {
    (row as f32 * (row_height + spacing) + row_height * 0.5 - viewport_height * 0.5).max(0.0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Percent,
    Size,
    Items,
    Files,
    Folders,
    Modified,
    Attributes,
}

pub struct TreeView {
    expanded: HashSet<usize>,
    pub column: SortColumn,
    pub descending: bool,
    rows: Vec<(usize, usize)>,
    revision: Option<u64>,
    dirty: bool,
}

impl Default for TreeView {
    fn default() -> Self {
        Self {
            expanded: HashSet::from([0]),
            column: SortColumn::Size,
            descending: true,
            rows: Vec::new(),
            revision: None,
            dirty: true,
        }
    }
}

impl TreeView {
    pub fn collapse_all(&mut self) {
        self.expanded.clear();
        self.expanded.insert(0);
        self.dirty = true;
    }

    pub fn expand(&mut self, index: usize) {
        self.dirty |= self.expanded.insert(index);
    }

    pub fn is_expanded(&self, index: usize) -> bool {
        self.expanded.contains(&index)
    }

    pub fn toggle(&mut self, index: usize) {
        if !self.expanded.remove(&index) {
            self.expanded.insert(index);
        }
        self.dirty = true;
    }

    pub fn sort_by(&mut self, column: SortColumn) {
        self.descending = if self.column == column {
            !self.descending
        } else {
            column != SortColumn::Name
        };
        self.column = column;
        self.dirty = true;
    }

    pub fn update(&mut self, nodes: &[Node], revision: u64) {
        if !self.dirty && self.revision == Some(revision) {
            return;
        }
        self.rows.clear();
        let mut stack = vec![(0, 0)];
        while let Some((index, depth)) = stack.pop() {
            self.rows.push((index, depth));
            if self.expanded.contains(&index) {
                let mut children = nodes[index].children.clone();
                if self.column == SortColumn::Name {
                    children.sort_by_cached_key(|&child| nodes[child].name.to_lowercase());
                    if self.descending {
                        children.reverse();
                    }
                } else if self.column == SortColumn::Size && self.descending {
                    // The immutable scan snapshot already stores this order.
                } else {
                    children.sort_by(|&a, &b| self.compare(&nodes[a], &nodes[b]));
                }
                stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
            }
        }
        self.revision = Some(revision);
        self.dirty = false;
    }

    pub fn rows(&self) -> &[(usize, usize)] {
        &self.rows
    }

    fn compare(&self, a: &Node, b: &Node) -> std::cmp::Ordering {
        let order = match self.column {
            SortColumn::Name => unreachable!("name sort uses cached keys"),
            SortColumn::Percent | SortColumn::Size => a.size.cmp(&b.size),
            SortColumn::Items => (a.files + a.folders).cmp(&(b.files + b.folders)),
            SortColumn::Files => a.files.cmp(&b.files),
            SortColumn::Folders => a.folders.cmp(&b.folders),
            SortColumn::Modified => a.modified.cmp(&b.modified),
            SortColumn::Attributes => a.attributes.cmp(&b.attributes),
        };
        if self.descending {
            order.reverse()
        } else {
            order
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TreeBuilder;
    use std::path::PathBuf;

    #[test]
    fn centering_a_distant_row_includes_the_spacing_used_by_virtualization() {
        let row = 10_000;
        let viewport = 600.0;
        let offset = centered_row_offset(row, viewport, 28.0, 3.0);
        let first_visible = (offset / 31.0).floor() as usize;
        let last_visible = ((offset + viewport) / 31.0).ceil() as usize;
        assert!((first_visible..last_visible).contains(&row));
        assert!((row as f32 * 31.0 + 14.0 - offset - viewport * 0.5).abs() < 0.01);
        assert_eq!(centered_row_offset(0, viewport, 28.0, 3.0), 0.0);
    }

    fn fixture(count: usize) -> Vec<Node> {
        let root = PathBuf::from("root");
        let mut builder = TreeBuilder::new(root.clone());
        for index in 0..count {
            builder.add(
                root.join(format!("file-{:06}.dat", (index * 7919) % 100003)),
                false,
                (count - index) as u64,
                None,
                0,
            );
        }
        builder.finish()
    }

    #[test]
    fn sorting_and_replacing_snapshots_invalidate_cached_rows() {
        let nodes = fixture(100);
        let mut view = TreeView::default();
        view.sort_by(SortColumn::Name);
        view.update(&nodes, 1);
        assert!(
            view.rows()[1..]
                .windows(2)
                .all(|pair| nodes[pair[0].0].name <= nodes[pair[1].0].name)
        );
        let buffer = view.rows().as_ptr();
        view.update(&nodes, 1);
        assert_eq!(view.rows().as_ptr(), buffer);
        let smaller = fixture(2);
        view.update(&smaller, 2);
        assert_eq!(view.rows().len(), 3);
        assert!(view.rows().iter().all(|&(index, _)| index < smaller.len()));
    }

    #[test]
    #[ignore = "manual in-memory benchmark for cached directory rows"]
    fn benchmark_cached_directory_rows() {
        let nodes = fixture(100_000);
        let mut view = TreeView::default();
        view.sort_by(SortColumn::Name);
        let start = std::time::Instant::now();
        view.update(&nodes, 1);
        let first = start.elapsed();
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            view.update(std::hint::black_box(&nodes), std::hint::black_box(1));
            std::hint::black_box(view.rows());
        }
        eprintln!(
            "100k directory rows: initial name sort {:.2} ms; cached update {:.4} ms",
            first.as_secs_f64() * 1000.0,
            start.elapsed().as_secs_f64() * 1000.0 / 10_000.0
        );
    }
}
