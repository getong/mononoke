// Copyright (c) 2017-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

//! Wire packs. The format is currently undocumented.

use std::fmt;

use byteorder::{BigEndian, ByteOrder};
use bytes::BytesMut;

use mercurial_types::{Delta, NodeHash, RepoPath, NULL_HASH};

use delta;
use errors::*;
use utils::BytesExt;

pub mod unpacker;

/// What sort of wirepack this is.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Kind {
    /// A wire pack representing tree manifests.
    Tree,
    /// A wire pack representing file contents.
    File,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Kind::Tree => write!(f, "tree"),
            Kind::File => write!(f, "file"),
        }
    }
}

/// An atomic part returned from the wirepack.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Part {
    HistoryMeta { path: RepoPath, entry_count: usize },
    History(HistoryEntry),
    DataMeta { path: RepoPath, entry_count: usize },
    Data(DataEntry),
    End,
}

#[cfg(test)]
impl Part {
    pub(crate) fn unwrap_history_meta(self) -> (RepoPath, usize) {
        match self {
            Part::HistoryMeta { path, entry_count } => (path, entry_count),
            other => panic!("expected wirepack part to be HistoryMeta, was {:?}", other),
        }
    }

    pub(crate) fn unwrap_history(self) -> HistoryEntry {
        match self {
            Part::History(entry) => entry,
            other => panic!("expected wirepack part to be History, was {:?}", other),
        }
    }

    pub(crate) fn unwrap_data_meta(self) -> (RepoPath, usize) {
        match self {
            Part::DataMeta { path, entry_count } => (path, entry_count),
            other => panic!("expected wirepack part to be HistoryMeta, was {:?}", other),
        }
    }

    pub(crate) fn unwrap_data(self) -> DataEntry {
        match self {
            Part::Data(entry) => entry,
            other => panic!("expected wirepack part to be Data, was {:?}", other),
        }
    }
}

// See the history header definition in this file for the breakdown.
const HISTORY_COPY_FROM_OFFSET: usize = 20 + 20 + 20 + 20;
const HISTORY_HEADER_SIZE: usize = HISTORY_COPY_FROM_OFFSET + 2;

// See the data header definition in this file for the breakdown.
const DATA_DELTA_OFFSET: usize = 20 + 20;
const DATA_HEADER_SIZE: usize = DATA_DELTA_OFFSET + 8;

// TODO: move to mercurial-types
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryEntry {
    pub node: NodeHash,
    // TODO: replace with Parents?
    pub p1: NodeHash,
    pub p2: NodeHash,
    pub linknode: NodeHash,
    pub copy_from: Option<RepoPath>,
}

impl HistoryEntry {
    pub(crate) fn decode(buf: &mut BytesMut, kind: Kind) -> Result<Option<Self>> {
        if buf.len() < HISTORY_HEADER_SIZE {
            return Ok(None);
        }

        // A history revision has:
        // ---
        // node: NodeHash (20 bytes)
        // p1: NodeHash (20 bytes)
        // p2: NodeHash (20 bytes)
        // link node: NodeHash (20 bytes)
        // copy from len: u16 (2 bytes) -- 0 if this revision is not a copy
        // copy from: RepoPath (<copy from len> bytes)
        // ---
        // Tree revisions are never copied, so <copy from len> is always 0.

        let copy_from_len =
            BigEndian::read_u16(&buf[HISTORY_COPY_FROM_OFFSET..HISTORY_HEADER_SIZE]) as usize;
        if buf.len() < HISTORY_HEADER_SIZE + copy_from_len {
            return Ok(None);
        }

        let node = buf.drain_node();
        let p1 = buf.drain_node();
        let p2 = buf.drain_node();
        let linknode = buf.drain_node();
        let _ = buf.drain_u16();
        let copy_from = if copy_from_len > 0 {
            let path = buf.drain_path(copy_from_len)?;
            match kind {
                Kind::Tree => bail_err!(ErrorKind::WirePackDecode(format!(
                    "tree entry {} is marked as copied from path {}, but they cannot be copied",
                    node,
                    path
                ))),
                Kind::File => Some(RepoPath::file(path).with_context(|_| {
                    ErrorKind::WirePackDecode("invalid copy from path".into())
                })?),
            }
        } else {
            None
        };
        Ok(Some(Self {
            node,
            p1,
            p2,
            linknode,
            copy_from,
        }))
    }

    pub fn verify(&self, kind: Kind) -> Result<()> {
        if let Some(ref path) = self.copy_from {
            match *path {
                RepoPath::RootPath => bail_err!(ErrorKind::InvalidWirePackEntry(format!(
                    "history entry for {} is copied from the root path, which isn't allowed",
                    self.node
                ))),
                RepoPath::DirectoryPath(ref path) => {
                    bail_err!(ErrorKind::InvalidWirePackEntry(format!(
                        "history entry for {} is copied from directory {}, which isn't allowed",
                        self.node,
                        path
                    )))
                }
                RepoPath::FilePath(ref path) => {
                    ensure_err!(
                        kind == Kind::File,
                        ErrorKind::InvalidWirePackEntry(format!(
                            "history entry for {} is copied from file {}, but the pack is of \
                             kind {}",
                            self.node,
                            path,
                            kind
                        ))
                    );
                    ensure_err!(
                        path.len() <= (u16::max_value() as usize),
                        ErrorKind::InvalidWirePackEntry(format!(
                            "history entry for {} is copied from a path of length {} -- maximum \
                             length supported is {}",
                            self.node,
                            path.len(),
                            u16::max_value(),
                        ),)
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataEntry {
    pub node: NodeHash,
    pub delta_base: NodeHash,
    pub delta: Delta,
}

impl DataEntry {
    pub(crate) fn decode(buf: &mut BytesMut) -> Result<Option<Self>> {
        if buf.len() < DATA_HEADER_SIZE {
            return Ok(None);
        }

        // A data revision has:
        // ---
        // node: NodeHash (20 bytes)
        // delta base: NodeHash (20 bytes) -- NULL_HASH if full text
        // delta len: u64 (8 bytes)
        // delta: Delta (<delta len> bytes)
        // ---
        // There's a bit of a wart in the current format: if delta base is NULL_HASH, instead of
        // storing a delta with start = 0 and end = 0, we store the full text directly. This
        // should be fixed in a future wire protocol revision.
        let delta_len = BigEndian::read_u64(&buf[DATA_DELTA_OFFSET..DATA_HEADER_SIZE]) as usize;
        if buf.len() < DATA_HEADER_SIZE + delta_len {
            return Ok(None);
        }

        let node = buf.drain_node();
        let delta_base = buf.drain_node();
        let _ = buf.drain_u64();
        let delta = buf.split_to(delta_len);

        let delta = if delta_base == NULL_HASH {
            Delta::new_fulltext(delta.to_vec())
        } else {
            delta::decode_delta(delta)?
        };

        Ok(Some(Self {
            node,
            delta_base,
            delta,
        }))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_history_verify_basic() {
        let foo_dir = RepoPath::dir("foo").unwrap();
        let bar_file = RepoPath::file("bar").unwrap();
        let root = RepoPath::root();

        let valid = hashset! {
            (Kind::Tree, None),
            (Kind::File, None),
            (Kind::File, Some(bar_file.clone())),
        };
        // Can't use arrays here because IntoIterator isn't supported for them:
        // https://github.com/rust-lang/rust/issues/25725
        let kinds = vec![Kind::Tree, Kind::File].into_iter();
        let copy_froms = vec![None, Some(foo_dir), Some(bar_file), Some(root)].into_iter();

        for pair in iproduct!(kinds, copy_froms) {
            let is_valid = valid.contains(&pair);
            let (kind, copy_from) = pair;
            let entry = make_history_entry(copy_from);
            let result = entry.verify(kind);
            if is_valid {
                result.expect("expected history entry to be valid");
            } else {
                result.expect_err("expected history entry to be invalid");
            }
        }
    }

    fn make_history_entry(copy_from: Option<RepoPath>) -> HistoryEntry {
        HistoryEntry {
            node: NULL_HASH,
            p1: NULL_HASH,
            p2: NULL_HASH,
            linknode: NULL_HASH,
            copy_from,
        }
    }
}
