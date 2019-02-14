// Copyright 2019 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use kvproto::coprocessor::KeyRange;

use crate::storage::{Key, Store};

use super::interface::*;
use super::ranges_consumer::{ConsumerResult, RangesConsumer};
use crate::coprocessor::codec::batch::LazyBatchColumnVec;
use crate::coprocessor::dag::Scanner;
use crate::coprocessor::Result;

pub trait ScanExecutorImpl: Send {
    fn build_scanner<S: Store>(&self, store: &S, desc: bool, range: KeyRange)
        -> Result<Scanner<S>>;

    fn build_column_vec(&self, expect_rows: usize) -> LazyBatchColumnVec;

    fn process_kv_pair(
        &mut self,
        key: &[u8],
        value: &[u8],
        columns: &mut LazyBatchColumnVec,
    ) -> Result<()>;
}

pub struct ScanExecutor<S: Store, I: ScanExecutorImpl> {
    imp: I,

    store: S,
    desc: bool,

    /// Consume and produce ranges.
    ranges: RangesConsumer,

    /// Row scanner.
    ///
    /// It is optional because sometimes it is not needed, e.g. when point range is given.
    /// Also, the value may be re-constructed several times if there are multiple key ranges.
    scanner: Option<Scanner<S>>,

    /// A flag indicating whether this executor is ended. When table is drained or there was an
    /// error scanning the table, this flag will be set to `true` and `next_batch` should be never
    /// called again.
    is_ended: bool,
}

impl<S: Store, I: ScanExecutorImpl> ScanExecutor<S, I> {
    pub fn new(
        imp: I,
        store: S,
        desc: bool,
        mut key_ranges: Vec<KeyRange>,
        emit_point_range: bool,
    ) -> Result<Self> {
        crate::coprocessor::codec::table::check_table_ranges(&key_ranges)?;
        if desc {
            key_ranges.reverse();
        }
        Ok(Self {
            imp,
            store,
            desc,
            ranges: RangesConsumer::new(key_ranges, emit_point_range),
            scanner: None,
            is_ended: false,
        })
    }

    /// Creates or resets the range of inner scanner.
    #[inline]
    fn reset_range(&mut self, range: KeyRange) -> Result<()> {
        self.scanner = Some(self.imp.build_scanner(&self.store, self.desc, range)?);
        Ok(())
    }

    /// Scans next row from the scanner.
    #[inline]
    fn scan_next(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        // TODO: Key and value doesn't have to be owned
        if let Some(scanner) = self.scanner.as_mut() {
            Ok(scanner.next_row()?)
        } else {
            // `self.scanner` should never be `None` when this function is being called.
            unreachable!()
        }
    }

    /// Get one row from the store.
    #[inline]
    fn point_get(&mut self, mut range: KeyRange) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut statistics = crate::storage::Statistics::default();
        // TODO: Key and value doesn't have to be owned
        let key = range.take_start();
        let value = self.store.get(&Key::from_raw(&key), &mut statistics)?;
        Ok(value.map(move |v| (key, v)))
    }

    fn fill_batch_rows(
        &mut self,
        expect_rows: usize,
        columns: &mut LazyBatchColumnVec,
    ) -> Result<bool> {
        assert!(expect_rows > 0);

        let mut is_drained = false;

        loop {
            let range = self.ranges.next();
            let some_row = match range {
                ConsumerResult::NewPointRange(r) => self.point_get(r)?,
                ConsumerResult::NewNonPointRange(r) => {
                    self.reset_range(r)?;
                    self.scan_next()?
                }
                ConsumerResult::Continue => self.scan_next()?,
                ConsumerResult::Drained => {
                    is_drained = true;
                    break;
                }
            };
            if let Some((key, value)) = some_row {
                self.imp.process_kv_pair(&key, &value, columns)?;

                columns.debug_assert_columns_equal_length();

                if columns.rows_len() >= expect_rows {
                    break;
                }
            } else {
                self.ranges.consume();
            }
        }

        Ok(is_drained)
    }
}

impl<S: Store, I: ScanExecutorImpl> BatchExecutor for ScanExecutor<S, I> {
    #[inline]
    fn next_batch(&mut self, expect_rows: usize) -> BatchExecuteResult {
        assert!(!self.is_ended);
        assert!(expect_rows > 0);

        let mut data = self.imp.build_column_vec(expect_rows);
        let is_drained = self.fill_batch_rows(expect_rows, &mut data);

        // TODO
        // After calling `fill_batch_rows`, columns' length may not be identical in some special
        // cases, for example, meet decoding errors when decoding the last column. We need to trim
        // extra elements.

        // TODO
        // If `is_drained.is_err()`, it means that there is an error after *successfully* retrieving
        // these rows. After that, if we only consumes some of the rows (TopN / Limit), we should
        // ignore this error.

        match &is_drained {
            Err(_) => self.is_ended = true,
            Ok(true) => self.is_ended = true,
            Ok(false) => {}
        };

        BatchExecuteResult { data, is_drained }
    }
}
