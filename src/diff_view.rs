//! Presentation-only row mappings. Unified indices remain the selection identity.

use crate::{DiffKind, DiffLine};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffLayout {
    #[default]
    Unified,
    SideBySide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitRow {
    pub left: Option<usize>,
    pub right: Option<usize>,
}

pub fn split_rows(lines: &[DiffLine]) -> (Vec<SplitRow>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut display_indices = vec![0; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        if lines[index].kind == DiffKind::Context {
            display_indices[index] = rows.len();
            rows.push(SplitRow {
                left: Some(index),
                right: Some(index),
            });
            index += 1;
            continue;
        }
        let hunk = lines[index].hunk;
        let mut removed = Vec::new();
        let mut added = Vec::new();
        while index < lines.len()
            && lines[index].hunk == hunk
            && lines[index].kind != DiffKind::Context
        {
            match lines[index].kind {
                DiffKind::Removed => removed.push(index),
                DiffKind::Added => added.push(index),
                DiffKind::Context => unreachable!(),
            }
            index += 1;
        }
        for offset in 0..removed.len().max(added.len()) {
            let left = removed.get(offset).copied();
            let right = added.get(offset).copied();
            for source_index in [left, right].into_iter().flatten() {
                display_indices[source_index] = rows.len();
            }
            rows.push(SplitRow { left, right });
        }
    }
    (rows, display_indices)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(kinds: &[(DiffKind, usize)]) -> Vec<DiffLine> {
        kinds
            .iter()
            .map(|(kind, hunk)| DiffLine {
                old_number: None,
                new_number: None,
                kind: *kind,
                hunk: *hunk,
                text: "test".into(),
            })
            .collect()
    }

    #[test]
    fn replacement_pairs_keep_both_source_indices_and_pad_unbalanced_changes() {
        let input = lines(&[
            (DiffKind::Context, 1),
            (DiffKind::Removed, 1),
            (DiffKind::Removed, 1),
            (DiffKind::Added, 1),
            (DiffKind::Context, 1),
        ]);
        let (rows, indices) = split_rows(&input);
        assert_eq!(
            rows,
            vec![
                SplitRow {
                    left: Some(0),
                    right: Some(0)
                },
                SplitRow {
                    left: Some(1),
                    right: Some(3)
                },
                SplitRow {
                    left: Some(2),
                    right: None
                },
                SplitRow {
                    left: Some(4),
                    right: Some(4)
                },
            ]
        );
        assert_eq!(indices, vec![0, 1, 2, 1, 3]);
    }

    #[test]
    fn adjacent_hunks_never_pair_unrelated_changes() {
        let input = lines(&[(DiffKind::Removed, 1), (DiffKind::Added, 2)]);
        let (rows, indices) = split_rows(&input);
        assert_eq!(
            rows,
            vec![
                SplitRow {
                    left: Some(0),
                    right: None
                },
                SplitRow {
                    left: None,
                    right: Some(1)
                },
            ]
        );
        assert_eq!(indices, vec![0, 1]);
    }

    #[test]
    fn empty_files_and_addition_only_blocks_are_supported() {
        assert_eq!(split_rows(&[]), (vec![], vec![]));
        let input = lines(&[(DiffKind::Added, 1), (DiffKind::Added, 1)]);
        let (rows, indices) = split_rows(&input);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.left.is_none()));
        assert_eq!(indices, vec![0, 1]);
    }
}
