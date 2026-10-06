//! The Slint model behind the transcript `ListView`.
//!
//! Rows are stored already converted, so `row_data` is a cheap clone
//! (`SharedString`, `StyledText`, and `ModelRc` are reference counted).
//! Every mutation sends the narrowest notification (`row_changed`,
//! `row_added`, `row_removed`); there is no full reset.

use std::cell::RefCell;

use slint::Model;
use slint::ModelNotify;
use slint::ModelTracker;

use crate::ui::BlockData;

#[derive(Default)]
pub(crate) struct TranscriptModel {
    rows: RefCell<Vec<BlockData>>,
    notify: ModelNotify,
}

impl TranscriptModel {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.rows.borrow().len()
    }

    pub(crate) fn set(&self, row: usize, data: BlockData) {
        {
            let mut rows = self.rows.borrow_mut();
            let Some(slot) = rows.get_mut(row) else {
                return;
            };
            *slot = data;
        }
        self.notify.row_changed(row);
    }

    pub(crate) fn insert(&self, at: usize, data: Vec<BlockData>) {
        if data.is_empty() {
            return;
        }
        let count = data.len();
        let at = {
            let mut rows = self.rows.borrow_mut();
            let at = at.min(rows.len());
            rows.splice(at..at, data);
            at
        };
        self.notify.row_added(at, count);
    }

    pub(crate) fn remove(&self, at: usize, count: usize) {
        let removed = {
            let mut rows = self.rows.borrow_mut();
            let end = (at + count).min(rows.len());
            if at >= end {
                return;
            }
            rows.drain(at..end);
            end - at
        };
        self.notify.row_removed(at, removed);
    }

    #[cfg(test)]
    pub(crate) fn id_at(&self, row: usize) -> Option<String> {
        self.rows.borrow().get(row).map(|data| data.id.to_string())
    }
}

impl Model for TranscriptModel {
    type Data = BlockData;

    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<BlockData> {
        self.rows.borrow().get(row).cloned()
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn row(id: &str) -> BlockData {
        BlockData {
            id: id.into(),
            ..BlockData::default()
        }
    }

    fn ids(model: &TranscriptModel) -> Vec<String> {
        (0..model.row_count())
            .filter_map(|index| model.id_at(index))
            .collect()
    }

    #[test]
    fn insert_set_remove() {
        let model = TranscriptModel::default();
        model.insert(0, vec![row("a"), row("c")]);
        model.insert(1, vec![row("b")]);
        assert_eq!(ids(&model), vec!["a", "b", "c"]);
        model.set(2, row("C"));
        model.remove(0, 1);
        assert_eq!(ids(&model), vec!["b", "C"]);
        model.remove(5, 1);
        model.insert(99, vec![row("d")]);
        assert_eq!(ids(&model), vec!["b", "C", "d"]);
        assert_eq!(model.len(), 3);
    }
}
