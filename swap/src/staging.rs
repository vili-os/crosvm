// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![deny(missing_docs)]

use std::ops::Range;

use base::error;
use base::MemoryMapping;
use base::MemoryMappingBuilder;
use base::MmapError;
use data_model::VolatileMemory;
use data_model::VolatileMemoryError;
use data_model::VolatileSlice;
use thiserror::Error as ThisError;

use crate::pagesize::pages_to_bytes;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(ThisError, Debug)]
pub enum Error {
    #[error("failed to mmap operation: {0}")]
    Mmap(MmapError),
    #[error("failed to volatile memory operation: {0}")]
    VolatileMemory(VolatileMemoryError),
    #[error("index is out of range")]
    OutOfRange,
}

impl From<MmapError> for Error {
    fn from(e: MmapError) -> Self {
        Self::Mmap(e)
    }
}

impl From<VolatileMemoryError> for Error {
    fn from(e: VolatileMemoryError) -> Self {
        Self::VolatileMemory(e)
    }
}

/// [StagingMemory] stores active pages from the guest memory in anonymous private memory.
///
/// [StagingMemory] is created per memory region.
///
/// On `crosvm swap enable` command, the monitor process moves all the active pages in the guest
/// memory to this staging memory. [StagingMemory] has several advantages over writing all
/// pages from the guest memory to the swap file directly.
///
/// * Less non-responsive time
///   * While moving the guest memory pages, the monitor process have to freeze whole crosvm
///   * processes to guarantee no updates on the guest memory. Moving memory is faster than writing
///   * them to disk.
/// * Hot pages bypass the disk
///   * The faulting pages between `crosvm swap enable` and `crosvm swap out` are swapped in from
///   * this [StagingMemory] directly without written into the swap file. This saves disk resouces
///   * and latency of page fault handling.
pub struct StagingMemory {
    mmap: MemoryMapping,
    state_list: Vec<bool>,
}

impl StagingMemory {
    /// Creates [StagingMemory] and anonymous private memory.
    ///
    /// # Arguments
    ///
    /// * `num_of_pages` - the number of pages in the region.
    pub fn new(num_of_pages: usize) -> Result<Self> {
        let mmap = MemoryMappingBuilder::new(pages_to_bytes(num_of_pages)).build()?;
        Ok(Self {
            mmap,
            state_list: vec![false; num_of_pages],
        })
    }

    /// Marks that the pages are present in the staging memory.
    ///
    /// # Arguments
    ///
    /// * `idx_range` - the indices of consecutive pages to be marked as present.
    pub fn mark_as_present(&mut self, idx_range: Range<usize>) {
        for idx in idx_range {
            self.state_list[idx] = true;
        }
    }

    /// Returns a content of the page corresponding to the index.
    ///
    /// Returns [Option::None] if no content in the staging memory.
    ///
    /// Returns [Error::OutOfRange] if the `idx` is out of range.
    ///
    /// # Arguments
    ///
    /// * `idx` - the index of the page from the head of the pages.
    pub fn page_content(&self, idx: usize) -> Result<Option<VolatileSlice>> {
        match self.state_list.get(idx) {
            Some(is_present) => {
                if *is_present {
                    let slice = self
                        .mmap
                        .get_slice(pages_to_bytes(idx), pages_to_bytes(1))?;
                    Ok(Some(slice))
                } else {
                    Ok(None)
                }
            }
            None => Err(Error::OutOfRange),
        }
    }

    /// Clears the pages in the staging memory corresponding to the indices.
    ///
    /// # Arguments
    ///
    /// * `idx_range` - the indices of consecutive pages to be cleared.
    pub fn clear_range(&mut self, idx_range: Range<usize>) -> Result<()> {
        if idx_range.end > self.state_list.len() {
            return Err(Error::OutOfRange);
        }
        for is_present in &mut self.state_list[idx_range.clone()] {
            *is_present = false;
        }
        let addr = self.mmap.get_ref::<u8>(pages_to_bytes(idx_range.start))?;
        // Safe because the memory range is within staging memory.
        unsafe {
            libc::madvise(
                addr.as_mut_ptr() as *mut libc::c_void,
                pages_to_bytes(idx_range.end - idx_range.start),
                libc::MADV_REMOVE,
            );
        }
        Ok(())
    }

    /// Returns the range of indices of consecutive pages present in the staging memory after the
    /// `head_idx`.
    ///
    /// If `head_idx` is out of range, this just returns [Option::None].
    pub fn next_data_range(&self, head_idx: usize) -> Option<Range<usize>> {
        if head_idx >= self.state_list.len() {
            return None;
        }
        let head_idx = if let Some(offset) = self.state_list[head_idx..].iter().position(|v| *v) {
            head_idx + offset
        } else {
            return None;
        };
        let tail_idx = self.state_list[head_idx + 1..]
            .iter()
            .position(|v| !*v)
            .map_or(self.state_list.len(), |offset| offset + head_idx + 1);
        Some(head_idx..tail_idx)
    }

    /// Returns the [VolatileSlice] corresponding to the indices.
    ///
    /// * `idx_range` - the indices of the pages.
    pub fn get_slice(&self, idx_range: Range<usize>) -> Result<VolatileSlice> {
        self.mmap
            .get_slice(
                pages_to_bytes(idx_range.start),
                pages_to_bytes(idx_range.end - idx_range.start),
            )
            .map_err(|e| e.into())
    }

    /// Returns the count of present pages in the staging memory.
    pub fn present_pages(&self) -> usize {
        self.state_list
            .iter()
            .fold(0, |acc, v| if *v { acc + 1 } else { acc })
    }
}

#[cfg(test)]
mod tests {
    use base::pagesize;

    use super::*;

    #[test]
    fn new_success() {
        assert!(StagingMemory::new(200).is_ok());
    }

    #[test]
    fn mark_as_present() {
        let mut staging_memory = StagingMemory::new(200).unwrap();

        staging_memory.mark_as_present(1..5);
        // empty
        staging_memory.mark_as_present(10..10);
        // single
        staging_memory.mark_as_present(12..13);

        assert!(staging_memory.page_content(0).unwrap().is_none());
        for i in 1..5 {
            assert!(staging_memory.page_content(i).unwrap().is_some());
        }
        for i in 5..12 {
            assert!(staging_memory.page_content(i).unwrap().is_none());
        }
        assert!(staging_memory.page_content(12).unwrap().is_some());
        for i in 13..200 {
            assert!(staging_memory.page_content(i).unwrap().is_none());
        }
    }

    #[test]
    fn page_content_default_is_none() {
        let staging_memory = StagingMemory::new(200).unwrap();

        assert!(staging_memory.page_content(0).unwrap().is_none());
    }

    #[test]
    fn page_content_returns_content() {
        let mut staging_memory = StagingMemory::new(200).unwrap();

        staging_memory.mark_as_present(0..1);

        let slice = unsafe {
            std::slice::from_raw_parts_mut(
                staging_memory.get_slice(0..1).unwrap().as_mut_ptr(),
                pagesize(),
            )
        };
        for v in slice.iter_mut() {
            *v = 1;
        }

        let page = staging_memory.page_content(0).unwrap().unwrap();
        let result = unsafe { std::slice::from_raw_parts(page.as_ptr() as *const u8, page.size()) };
        assert_eq!(result, &vec![1; pagesize()]);
    }

    #[test]
    fn page_content_out_of_range() {
        let staging_memory = StagingMemory::new(200).unwrap();

        assert!(staging_memory.page_content(199).is_ok());
        match staging_memory.page_content(200) {
            Err(Error::OutOfRange) => {}
            _ => unreachable!("not out of range"),
        }
    }

    #[test]
    fn clear_range() {
        let mut staging_memory = StagingMemory::new(200).unwrap();

        staging_memory.mark_as_present(0..5);
        staging_memory.clear_range(1..3).unwrap();

        assert!(staging_memory.page_content(0).unwrap().is_some());
        assert!(staging_memory.page_content(1).unwrap().is_none());
        assert!(staging_memory.page_content(2).unwrap().is_none());
        assert!(staging_memory.page_content(3).unwrap().is_some());
        assert!(staging_memory.page_content(4).unwrap().is_some());
    }

    #[test]
    fn clear_range_out_of_range() {
        let mut staging_memory = StagingMemory::new(200).unwrap();

        assert!(staging_memory.clear_range(199..200).is_ok());
        match staging_memory.clear_range(199..201) {
            Err(Error::OutOfRange) => {}
            _ => unreachable!("not out of range"),
        };
    }

    #[test]
    fn next_data_range() {
        let mut staging_memory = StagingMemory::new(200).unwrap();

        staging_memory.mark_as_present(1..3);
        staging_memory.mark_as_present(12..13);
        staging_memory.mark_as_present(20..22);
        staging_memory.mark_as_present(22..23);
        staging_memory.mark_as_present(23..30);

        assert_eq!(staging_memory.next_data_range(0).unwrap(), 1..3);
        assert_eq!(staging_memory.next_data_range(1).unwrap(), 1..3);
        assert_eq!(staging_memory.next_data_range(2).unwrap(), 2..3);
        assert_eq!(staging_memory.next_data_range(3).unwrap(), 12..13);
        assert_eq!(staging_memory.next_data_range(12).unwrap(), 12..13);
        assert_eq!(staging_memory.next_data_range(13).unwrap(), 20..30);
        assert_eq!(staging_memory.next_data_range(20).unwrap(), 20..30);
        assert!(staging_memory.next_data_range(30).is_none());
        // out of range
        assert!(staging_memory.next_data_range(200).is_none());
    }

    #[test]
    fn next_data_range_end_is_full() {
        let mut staging_memory = StagingMemory::new(200).unwrap();

        staging_memory.mark_as_present(100..200);

        assert_eq!(staging_memory.next_data_range(0).unwrap(), 100..200);
        assert_eq!(staging_memory.next_data_range(100).unwrap(), 100..200);
        assert!(staging_memory.next_data_range(200).is_none());
    }

    #[test]
    fn present_pages() {
        let mut staging_memory = StagingMemory::new(200).unwrap();

        staging_memory.mark_as_present(1..5);
        staging_memory.mark_as_present(12..13);

        assert_eq!(staging_memory.present_pages(), 5);
    }
}
