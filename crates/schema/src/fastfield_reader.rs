// Stract is an open source web search engine.
// Copyright (C) 2023 Stract ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::{collections::HashMap, sync::Arc};

use stdx::enum_map::EnumMap;
use tantivy::{columnar::ColumnValues, DocId, SegmentId};

use crate::{DataType, FastField, ALL_FIELDS};

#[derive(Default, Clone)]
struct InnerFastFieldReader {
    segments: HashMap<SegmentId, Arc<SegmentReader>>,
}

#[derive(Default, Clone)]
pub struct FastFieldReader {
    inner: Arc<InnerFastFieldReader>,
}

impl FastFieldReader {
    pub fn get_segment(&self, segment: &SegmentId) -> Arc<SegmentReader> {
        Arc::clone(self.inner.segments.get(segment).unwrap())
    }
}

impl FastFieldReader {
    pub fn new(tv_searcher: &tantivy::Searcher) -> Self {
        let mut segments = HashMap::new();

        for reader in tv_searcher.segment_readers() {
            let fastfield_readers = reader.fast_fields();

            let mut field_readers = Vec::new();

            for field in ALL_FIELDS.iter().filter_map(|field| field.as_fast()) {
                let field_reader = match field.data_type() {
                    DataType::U64 => {
                        let reader = fastfield_readers.u64(field.name()).unwrap();
                        FieldReader::U64(reader.values)
                    }
                };

                field_readers.push((field, field_reader));
            }

            field_readers.sort_by_key(|(field, _)| *field as usize);

            segments.insert(
                reader.segment_id(),
                Arc::new(SegmentReader {
                    field_readers: field_readers.into_iter().collect(),
                }),
            );
        }

        Self {
            inner: Arc::new(InnerFastFieldReader { segments }),
        }
    }
}

pub enum FieldValue {
    U64(u64),
    U64s(Vec<u64>),
}

impl From<FieldValue> for Option<Vec<u64>> {
    fn from(val: FieldValue) -> Self {
        match val {
            FieldValue::U64s(vec) => Some(vec),
            _ => None,
        }
    }
}

impl From<FieldValue> for Option<u64> {
    fn from(val: FieldValue) -> Self {
        match val {
            FieldValue::U64(res) => Some(res),
            _ => None,
        }
    }
}

pub enum FieldReader {
    U64(Arc<dyn ColumnValues<u64>>),
}

impl FieldReader {
    pub fn get(&self, doc: &DocId) -> FieldValue {
        match self {
            FieldReader::U64(reader) => FieldValue::U64(reader.get_val(*doc)),
        }
    }
}

pub struct SegmentReader {
    field_readers: EnumMap<FastField, FieldReader>,
}

impl SegmentReader {
    pub fn get_field_reader(&self, field: &FastField) -> &FieldReader {
        self.field_readers.get(*field).unwrap()
    }
}