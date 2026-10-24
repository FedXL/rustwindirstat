use crate::model::{Node, Snapshot, finalize_nodes};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

const NONE: u32 = u32::MAX;
pub const MODEL_BUDGET: usize = 768 * 1024 * 1024;

#[derive(Debug)]
pub enum BuildError {
    Cancelled,
    MemoryLimit,
}

pub struct Record {
    /// Full NTFS references include the sequence number, not just the record slot.
    pub reference: u64,
    pub parent_reference: u64,
    pub name: OsString,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub attributes: u32,
}

pub struct IndexBuilder {
    root: PathBuf,
    root_reference: u64,
    nodes: Vec<Node>,
    parents: Vec<u64>,
    directories: HashMap<u64, usize>,
    reserved: Vec<bool>,
    names_bytes: usize,
    budget: usize,
}

pub struct BuiltIndex {
    pub snapshot: Snapshot,
    pub omitted: usize,
}

impl IndexBuilder {
    pub fn new(root: PathBuf, root_reference: u64, budget: usize) -> Self {
        let root_node = Node::new(root.as_os_str().to_owned(), true, 0, None, 0);
        Self {
            root,
            root_reference,
            nodes: vec![root_node],
            parents: vec![root_reference],
            directories: HashMap::from([(root_reference, 0)]),
            reserved: vec![true],
            names_bytes: 0,
            budget,
        }
    }

    pub fn push(&mut self, record: Record, reserved: bool) -> Result<(), BuildError> {
        if record.reference == self.root_reference {
            return Ok(());
        }
        let length = self.nodes.len();
        if length >= u32::MAX as usize {
            return Err(BuildError::MemoryLimit);
        }
        let capacity = if length == self.nodes.capacity() {
            (self.nodes.capacity().max(1024) * 2).min(self.nodes.capacity() + 65_536)
        } else {
            self.nodes.capacity()
        };
        let names = self
            .names_bytes
            .saturating_add(record.name.as_encoded_bytes().len());
        // Includes the native names, reference columns, directory map, and finalization
        // workspace. Retained NTFS library buffers are deducted by the caller.
        let projected = capacity
            .saturating_mul(std::mem::size_of::<Node>())
            .saturating_add((length + 1).saturating_mul(48))
            .saturating_add(self.directories.capacity().saturating_mul(32))
            .saturating_add(names);
        if projected > self.budget {
            return Err(BuildError::MemoryLimit);
        }
        if capacity > self.nodes.capacity() {
            self.nodes
                .try_reserve_exact(capacity - length)
                .map_err(|_| BuildError::MemoryLimit)?;
        }
        self.parents
            .try_reserve(1)
            .map_err(|_| BuildError::MemoryLimit)?;
        self.reserved
            .try_reserve(1)
            .map_err(|_| BuildError::MemoryLimit)?;
        if record.is_dir {
            self.directories
                .try_reserve(1)
                .map_err(|_| BuildError::MemoryLimit)?;
            self.directories.insert(record.reference, length);
        }
        self.names_bytes = names;
        self.nodes.push(Node::new(
            record.name,
            record.is_dir,
            record.size,
            record.modified,
            record.attributes,
        ));
        self.parents.push(record.parent_reference);
        self.reserved.push(reserved);
        Ok(())
    }

    pub fn finish(mut self, cancel: &AtomicBool) -> Result<BuiltIndex, BuildError> {
        let count = self.nodes.len();
        let mut first = filled(count, NONE)?;
        let mut next = filled(count, NONE)?;
        for (index, sibling) in next.iter_mut().enumerate().skip(1) {
            check_cancel(cancel, index)?;
            if let Some(&parent) = self.directories.get(&self.parents[index]) {
                self.nodes[index].set_parent(Some(parent));
                *sibling = first[parent];
                first[parent] = index as u32;
            }
        }
        drop(self.parents);
        drop(self.directories);
        let mut order = Vec::new();
        order
            .try_reserve_exact(count)
            .map_err(|_| BuildError::MemoryLimit)?;
        let mut reachable = filled(count, false)?;
        reachable[0] = true;
        order.push(0usize);
        let mut cursor = 0;
        while cursor < order.len() {
            check_cancel(cancel, cursor)?;
            let mut child = first[order[cursor]];
            while child != NONE {
                let index = child as usize;
                if !reachable[index] {
                    reachable[index] = true;
                    order.push(index);
                }
                child = next[index];
            }
            cursor += 1;
        }
        // Only root-reachable records are valid. This also excludes missing/reused
        // parents, non-directory parents, cycles, and every descendant of those records.
        // Reserved NTFS directories are kept only if a normal file/directory needs them.
        let mut keep = filled(count, false)?;
        keep[0] = true;
        for &index in order.iter().rev() {
            keep[index] |= !self.reserved[index];
            if keep[index]
                && let Some(parent) = self.nodes[index].parent()
            {
                keep[parent] = true;
            }
        }
        let omitted = (1..count)
            .filter(|&index| !self.reserved[index] && !keep[index])
            .count();
        drop(reachable);
        drop(self.reserved);
        let mut remap = filled(count, NONE)?;
        let mut new_index = 0u32;
        for index in 0..count {
            check_cancel(cancel, index)?;
            if keep[index] {
                remap[index] = new_index;
                new_index += 1;
            }
        }
        for index in 0..count {
            check_cancel(cancel, index)?;
            if !keep[index] {
                continue;
            }
            let parent = self.nodes[index]
                .parent()
                .map(|parent| remap[parent] as usize);
            self.nodes[index].set_parent(parent);
            if first[index] != NONE {
                let mut children = Vec::new();
                let mut child = first[index];
                while child != NONE {
                    let old = child as usize;
                    if keep[old] {
                        children.push(remap[old] as usize);
                    }
                    child = next[old];
                }
                self.nodes[index].children = children.into_boxed_slice();
            }
        }
        let mut old_index = 0;
        self.nodes.retain(|_| {
            let retain = keep[old_index];
            old_index += 1;
            retain
        });
        order.retain(|&index| keep[index]);
        for index in &mut order {
            *index = remap[*index] as usize;
        }
        drop((first, next, keep, remap));
        if cancel.load(Ordering::Relaxed) {
            return Err(BuildError::Cancelled);
        }
        finalize_nodes(&mut self.nodes, &order);
        if cancel.load(Ordering::Relaxed) {
            return Err(BuildError::Cancelled);
        }
        Ok(BuiltIndex {
            snapshot: Snapshot::new(self.root, self.nodes),
            omitted,
        })
    }
}

fn filled<T: Clone>(length: usize, value: T) -> Result<Vec<T>, BuildError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(length)
        .map_err(|_| BuildError::MemoryLimit)?;
    values.resize(length, value);
    Ok(values)
}

fn check_cancel(cancel: &AtomicBool, index: usize) -> Result<(), BuildError> {
    if index.is_multiple_of(4096) && cancel.load(Ordering::Relaxed) {
        Err(BuildError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
pub fn decode(file: &ntfs_reader::NtfsFile<'_>) -> Option<Record> {
    use ntfs_reader::{NtfsAttributeType, NtfsFileNamespace};
    let mut info = None;
    let mut name = None;
    let mut win32_name = false;
    let mut size = 0;
    for attribute in file.attributes() {
        match attribute.attribute_type() {
            Some(NtfsAttributeType::StandardInformation) => info = attribute.standard_information(),
            Some(NtfsAttributeType::Data) if attribute.is_unnamed() => {
                if let Some(value) = attribute.value_size() {
                    size = value;
                }
            }
            Some(NtfsAttributeType::FileName) if !win32_name => {
                if let Some(candidate) = attribute.file_name() {
                    let preferred = matches!(
                        candidate.namespace(),
                        Some(NtfsFileNamespace::Win32 | NtfsFileNamespace::Win32AndDos)
                    );
                    if preferred || name.is_none() {
                        name = Some(candidate);
                        win32_name = preferred;
                    }
                }
            }
            _ => {}
        }
    }
    let name = name?;
    let native_name = name.to_os_string();
    let mut components = std::path::Path::new(&native_name).components();
    if native_name.as_encoded_bytes().contains(&0)
        || !matches!(components.next(), Some(std::path::Component::Normal(part)) if part == native_name)
        || components.next().is_some()
    {
        return None;
    }
    Some(Record {
        reference: file.reference(),
        parent_reference: name.parent_reference(),
        name: native_name,
        is_dir: file.is_directory(),
        size,
        modified: info.and_then(|info| {
            let seconds = info.modified().unix_timestamp();
            if seconds >= 0 {
                std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(seconds as u64))
            } else {
                std::time::UNIX_EPOCH
                    .checked_sub(std::time::Duration::from_secs(seconds.unsigned_abs()))
            }
        }),
        attributes: info.map_or(0, |info| info.file_attributes()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TreeBuilder;

    fn record(reference: u64, parent: u64, name: &str, is_dir: bool, size: u64) -> Record {
        Record {
            reference,
            parent_reference: parent,
            name: name.into(),
            is_dir,
            size,
            modified: None,
            attributes: 0,
        }
    }

    #[test]
    fn out_of_order_references_match_the_path_builder_and_lazy_paths() {
        let root = PathBuf::from("root");
        let mut graph = IndexBuilder::new(root.clone(), 5, MODEL_BUDGET);
        graph
            .push(record(100, 40, "a.txt", false, 100), false)
            .unwrap();
        graph
            .push(record(40, 30, "nested", true, 0), false)
            .unwrap();
        graph
            .push(record(101, 30, "b.bin", false, 200), false)
            .unwrap();
        graph.push(record(30, 5, "dir", true, 0), false).unwrap();
        let graph = graph.finish(&AtomicBool::new(false)).unwrap().snapshot;
        let mut paths = TreeBuilder::new(root.clone());
        paths.add(root.join("dir/nested/a.txt"), false, 100, None, 0);
        paths.add(root.join("dir/b.bin"), false, 200, None, 0);
        let paths = paths.finish();
        let summarize = |snapshot: &Snapshot| {
            snapshot
                .iter()
                .enumerate()
                .map(|(i, n)| (snapshot.path(i), (n.size, n.files, n.folders)))
                .collect::<HashMap<_, _>>()
        };
        assert_eq!(summarize(&graph), summarize(&paths));
        assert!(std::mem::size_of::<Node>() < 100);
    }

    #[test]
    fn reused_missing_cyclic_and_non_directory_parents_are_excluded() {
        let mut graph = IndexBuilder::new("root".into(), 5, MODEL_BUDGET);
        let old = (1u64 << 48) | 30;
        let reused = (2u64 << 48) | 30;
        graph
            .push(record(reused, 5, "current", true, 0), false)
            .unwrap();
        graph
            .push(record(100, old, "stale.txt", false, 100), false)
            .unwrap();
        graph
            .push(record(101, 999, "missing.txt", false, 100), false)
            .unwrap();
        graph
            .push(record(40, 41, "cycle-a", true, 0), false)
            .unwrap();
        graph
            .push(record(41, 40, "cycle-b", true, 0), false)
            .unwrap();
        graph
            .push(record(42, 40, "cycle-child.txt", false, 100), false)
            .unwrap();
        graph
            .push(record(102, 5, "normal.txt", false, 7), false)
            .unwrap();
        graph
            .push(record(103, 102, "not-a-parent.txt", false, 100), false)
            .unwrap();
        let result = graph.finish(&AtomicBool::new(false)).unwrap();
        assert_eq!(result.omitted, 6);
        assert_eq!(result.snapshot[0].files, 1);
        assert_eq!(result.snapshot[0].size, 7);
    }

    #[test]
    fn reserved_directories_are_kept_only_when_needed_and_limits_are_reported() {
        let mut graph = IndexBuilder::new("root".into(), 5, MODEL_BUDGET);
        graph.push(record(11, 5, "$Extend", true, 0), true).unwrap();
        graph.push(record(12, 5, "unused", true, 0), true).unwrap();
        graph
            .push(record(100, 11, "metadata", false, 10), false)
            .unwrap();
        let result = graph.finish(&AtomicBool::new(false)).unwrap();
        assert_eq!(result.omitted, 0);
        assert_eq!(result.snapshot.len(), 3);
        assert_eq!(
            result.snapshot.path(2),
            PathBuf::from("root").join("$Extend/metadata")
        );
        let mut limited = IndexBuilder::new("root".into(), 5, 1);
        assert!(matches!(
            limited.push(record(100, 5, "a", false, 1), false),
            Err(BuildError::MemoryLimit)
        ));
        let graph = IndexBuilder::new("root".into(), 5, MODEL_BUDGET);
        assert!(matches!(
            graph.finish(&AtomicBool::new(true)),
            Err(BuildError::Cancelled)
        ));
    }

    #[cfg(windows)]
    fn raw_file(number: u64, parent: u64, name: &str, is_dir: bool, size: u64) -> Vec<u8> {
        use ntfs_reader::internals::test_records::*;
        let mut data = new_record(number, 1, 0);
        if is_dir {
            set_record_flags(&mut data, directory_flags());
        }
        let offset = add_standard_information(&mut data, ATTRIBUTES_OFFSET, 0);
        let offset = add_file_name_ex(
            &mut data,
            offset,
            1,
            parent,
            ntfs_reader::NtfsFileNamespace::Win32,
            name,
            0,
        );
        let offset = if is_dir {
            offset
        } else {
            add_nonresident_data(&mut data, offset, size)
        };
        finish_record(&mut data, offset);
        data
    }

    #[cfg(windows)]
    #[test]
    fn raw_ntfs_metadata_matches_file_info_and_cancel_stops_initial_pass() {
        use ntfs_reader::internals::test_records::*;
        use ntfs_reader::{DefaultPathCache, FileInfo, MftScan, NtfsReaderError};
        let records = vec![
            raw_file(24, reference(1, 25), "a.txt", false, 1234),
            raw_file(25, reference(ROOT_SEQUENCE, 5), "dir", true, 0),
        ];
        let (volume, data, bitmap) = raw_parts(records);
        let mut calls = Vec::new();
        let cancelled = MftScan::from_parts_controlled(
            volume.clone(),
            data.clone(),
            bitmap.clone(),
            4,
            |done, total| {
                calls.push((done, total));
                done < 4
            },
        );
        assert!(matches!(cancelled, Err(NtfsReaderError::Cancelled)));
        assert_eq!(calls.len(), 2);
        let mut scan =
            MftScan::from_parts_controlled(volume, data, bitmap, 4, |_, _| true).unwrap();
        let mut cache = DefaultPathCache::new();
        let mut count = 0;
        while let Some(chunk) = scan.next_chunk().unwrap() {
            for file in chunk.files() {
                let metadata = decode(&file).unwrap();
                let info = FileInfo::with_cache(&file, &mut cache);
                assert_eq!(metadata.name.to_string_lossy(), info.name);
                assert_eq!(metadata.size, info.size);
                assert_eq!(metadata.is_dir, info.is_directory);
                assert_eq!(metadata.attributes, info.file_attributes);
                assert!(info.path.is_some());
                count += 1;
            }
        }
        assert_eq!(count, 2);
    }

    #[cfg(windows)]
    #[test]
    fn raw_ntfs_names_cannot_replace_the_root_or_traverse_parents() {
        use ntfs_reader::internals::test_records::*;
        for name in ["..", r"\outside", r"a\b", "C:outside"] {
            let (volume, data, bitmap) = raw_parts(vec![raw_file(
                24,
                reference(ROOT_SEQUENCE, 5),
                name,
                false,
                1,
            )]);
            let mut scan = ntfs_reader::MftScan::from_parts_metadata_controlled(
                volume,
                data,
                bitmap,
                4096,
                |_, _| true,
            )
            .unwrap();
            let chunk = scan.next_chunk().unwrap().unwrap();
            let file = chunk.files().next().unwrap();
            assert!(decode(&file).is_none(), "invalid name {name:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn metadata_only_chunks_match_paths_with_extensions_links_and_stale_parents() {
        use ntfs_reader::internals::test_records::*;
        use ntfs_reader::{DefaultPathCache, FileInfo, MftScan, NtfsFileNamespace};
        let mut base = new_record(24, 1, 0);
        let offset = add_standard_information(&mut base, ATTRIBUTES_OFFSET, 0);
        let offset = add_file_name_ex(
            &mut base,
            offset,
            1,
            reference(1, 25),
            NtfsFileNamespace::Dos,
            "SHORT~1",
            0,
        );
        let offset = add_file_name_ex(
            &mut base,
            offset,
            2,
            reference(1, 25),
            NtfsFileNamespace::Win32,
            "preferred-name.bin",
            0,
        );
        finish_record(&mut base, offset);
        let directory = raw_file(25, reference(ROOT_SEQUENCE, 5), "dir", true, 0);
        let mut extension = new_record(26, 1, reference(1, 24));
        let offset = add_nonresident_data(&mut extension, ATTRIBUTES_OFFSET, 4321);
        finish_record(&mut extension, offset);
        let stale = raw_file(27, reference(2, 25), "stale.txt", false, 999);
        let mut linked = new_record(28, 1, 0);
        let offset = add_standard_information(&mut linked, ATTRIBUTES_OFFSET, 0);
        let offset = add_file_name_ex(
            &mut linked,
            offset,
            1,
            reference(1, 25),
            NtfsFileNamespace::Win32,
            "first-link.txt",
            0,
        );
        let offset = add_file_name_ex(
            &mut linked,
            offset,
            2,
            reference(ROOT_SEQUENCE, 5),
            NtfsFileNamespace::Win32,
            "second-link.txt",
            0,
        );
        let offset = add_nonresident_data(&mut linked, offset, 10);
        let named_offset = offset;
        let offset = add_nonresident_data_runs(
            &mut linked,
            offset,
            ntfs_reader::NtfsAttributeType::Data,
            "alternate",
            0,
            u64::MAX,
            999,
            &[],
        );
        write_u16(&mut linked, named_offset + 10, u16::MAX);
        finish_record(&mut linked, offset);
        let (volume, data, bitmap) = raw_parts(vec![base, directory, extension, stale, linked]);
        let root = PathBuf::from(r"T:\");
        let mut paths = TreeBuilder::new(root.clone());
        let mut scan = MftScan::from_parts_controlled(
            volume.clone(),
            data.clone(),
            bitmap.clone(),
            1,
            |_, _| true,
        )
        .unwrap();
        let mut cache = DefaultPathCache::new();
        while let Some(chunk) = scan.next_chunk().unwrap() {
            for file in chunk.files() {
                let info = FileInfo::with_cache(&file, &mut cache);
                if let Some(path) = info.path {
                    paths.add(
                        path,
                        info.is_directory,
                        info.size,
                        None,
                        info.file_attributes,
                    );
                }
            }
        }
        let paths = paths.finish();
        let mut graph = IndexBuilder::new(root, reference(ROOT_SEQUENCE, 5), MODEL_BUDGET);
        let mut scan =
            MftScan::from_parts_metadata_controlled(volume, data, bitmap, 1, |_, _| true).unwrap();
        while let Some(chunk) = scan.next_chunk().unwrap() {
            for file in chunk.files() {
                graph.push(decode(&file).unwrap(), false).unwrap();
            }
        }
        let graph = graph.finish(&AtomicBool::new(false)).unwrap();
        assert_eq!(graph.omitted, 1);
        let summarize = |snapshot: &Snapshot| {
            snapshot
                .iter()
                .enumerate()
                .map(|(index, node)| (snapshot.path(index), (node.size, node.files, node.folders)))
                .collect::<HashMap<_, _>>()
        };
        assert_eq!(summarize(&paths), summarize(&graph.snapshot));
        assert_eq!(graph.snapshot[0].size, 4331);
        assert_eq!(graph.snapshot[0].files, 2);
    }

    #[test]
    fn deep_parent_chains_do_not_recurse() {
        let mut graph = IndexBuilder::new("root".into(), 5, MODEL_BUDGET);
        for index in (0..10_000u64).rev() {
            let parent = if index == 0 { 5 } else { index + 23 };
            graph
                .push(record(index + 24, parent, "d", true, 0), false)
                .unwrap();
        }
        graph
            .push(record(99_999, 10_023, "file", false, 1), false)
            .unwrap();
        let result = graph.finish(&AtomicBool::new(false)).unwrap();
        assert_eq!(result.snapshot[0].files, 1);
        assert_eq!(result.snapshot[0].folders, 10_000);
        assert_eq!(result.omitted, 0);
    }
}
