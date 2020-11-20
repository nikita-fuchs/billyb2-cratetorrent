use std::{
    collections::{BTreeMap, HashMap},
    fs,
    sync::{self, Arc},
};

use tokio::{sync::mpsc, task};

use super::{Piece, TorrentFile};
use crate::{
    disk::{
        error::*, BatchWrite, TorrentAlert, TorrentAlertReceiver,
        TorrentAlertSender,
    },
    storage_info::{FsStructure, StorageInfo},
    BlockInfo, PieceIndex,
};

/// Torrent information related to disk IO.
///
/// Contains the in-progress pieces (i.e. the write buffer), metadata about
/// torrent's download and piece sizes, etc.
pub(super) struct Torrent {
    /// All information concerning this torrent's storage.
    info: StorageInfo,
    /// The channel used to alert a torrent that a block has been written to
    /// disk and/or a piece was completed.
    alert_chan: TorrentAlertSender,
    /// The in-progress piece downloads and disk writes. This is the torrent's
    /// disk write buffer. Each piece is mapped to its index for faster lookups.
    // TODO(https://github.com/mandreyel/cratetorrent/issues/22): Currently
    // there is no upper bound on this.
    write_buf: HashMap<PieceIndex, Piece>,
    /// Handles of all files in torrent, opened in advance during torrent
    /// creation.
    ///
    /// Each writer thread will get exclusive access to the file handle it
    /// needs, referring to it directly in the vector (hence the arc).
    /// Multiple readers may read from the same file, but not while there is
    /// a pending write.
    ///
    /// Later we will need to make file access more granular, as multiple
    /// concurrent writes to the same file that don't overlap are safe to do.
    // TODO: consider improving concurreny by allowing concurrent reads and
    // writes on different parts of the file using byte-range locking
    // TODO: Is there a way to avoid copying `FileInfo`s here from
    // `self.info.structure`? We could just pass the file info on demand, but
    // that woudl require reallocating this vector every time (to pass a new
    // vector of pairs of `TorrentFile` and `FileInfo`).
    files: Arc<Vec<sync::RwLock<TorrentFile>>>,
    /// The concatenation of all expected piece hashes.
    piece_hashes: Vec<u8>,
    /// Disk IO statistics.
    stats: Stats,
}

impl Torrent {
    /// handles.
    /// Creates the file system structure of the torrent and opens the file
    ///
    /// For a single file, there is a path validity check and then the file is
    /// opened. For multi-file torrents, if there are any subdirectories in the
    /// torrent archive, they are created and all files are opened.
    pub fn new(
        info: StorageInfo,
        piece_hashes: Vec<u8>,
    ) -> Result<(Self, TorrentAlertReceiver), NewTorrentError> {
        // TODO: since this is done as part of a tokio::task, should we use
        // tokio_fs here?
        if !info.download_dir.is_dir() {
            log::warn!(
                "Creating missing download directory {:?}",
                info.download_dir
            );
            fs::create_dir_all(&info.download_dir)?;
            log::info!("Download directory {:?} created", info.download_dir);
        }

        let files = match &info.structure {
            FsStructure::File(file) => {
                log::debug!(
                    "Torrent is single {} bytes long file {:?}",
                    file.len,
                    file.path
                );
                vec![sync::RwLock::new(TorrentFile::new(
                    &info.download_dir,
                    file.clone(),
                )?)]
            }
            FsStructure::Archive { files } => {
                debug_assert!(!files.is_empty());
                log::debug!("Torrent is multi file: {:?}", files);
                log::debug!("Setting up directory structure");

                let mut torrent_files = Vec::with_capacity(files.len());
                for file in files.iter() {
                    let path = info.download_dir.join(&file.path);
                    // file or subdirectory in download root must not exist if
                    // download root does not exists
                    debug_assert!(!path.exists());
                    debug_assert!(path.is_absolute());

                    // get the parent of the file path: if there is one (i.e.
                    // this is not a file in the torrent root), and doesn't
                    // exist, create it
                    if let Some(subdir) = path.parent() {
                        if !subdir.exists() {
                            log::info!("Creating torrent subdir {:?}", subdir);
                            fs::create_dir_all(&subdir).map_err(|e| {
                                log::warn!(
                                    "Failed to create subdir {:?}",
                                    subdir
                                );
                                NewTorrentError::Io(e)
                            })?;
                        }
                    }

                    // open the file and get a handle to it
                    //
                    // TODO: is there a clean way of avoiding creating the path
                    // buffer twice?
                    torrent_files.push(sync::RwLock::new(TorrentFile::new(
                        &info.download_dir,
                        file.clone(),
                    )?));
                }
                torrent_files
            }
        };

        let (alert_chan, alert_port) = mpsc::unbounded_channel();

        Ok((
            Self {
                info,
                alert_chan,
                write_buf: HashMap::new(),
                files: Arc::new(files),
                piece_hashes,
                stats: Stats::default(),
            },
            alert_port,
        ))
    }

    pub async fn write_block(
        &mut self,
        info: BlockInfo,
        data: Vec<u8>,
    ) -> Result<()> {
        log::trace!("Saving block {:?} to disk", info);

        let piece_index = info.piece_index;
        if !self.write_buf.contains_key(&piece_index) {
            if let Err(e) = self.start_new_piece(info) {
                self.alert_chan.send(TorrentAlert::BatchWrite(Err(e)))?;
                // return with ok as the disk task itself shouldn't be aborted
                // due to invalid input
                return Ok(());
            }
        }
        let piece = self
            .write_buf
            .get_mut(&piece_index)
            .expect("Newly inserted piece not present");

        piece.enqueue_block(info.offset, data);

        // if the piece has all its blocks, it means we can hash it and save it
        // to disk and clear its write buffer
        if piece.is_complete() {
            // TODO: remove from in memory store only if the disk write
            // succeeded (otherwise we need to retry later)
            let piece = self.write_buf.remove(&piece_index).unwrap();
            let piece_len = self.info.piece_len;
            let files = Arc::clone(&self.files);

            log::debug!(
                "Piece {} is complete ({} bytes), flushing {} block(s) to disk",
                info.piece_index,
                piece_len,
                piece.blocks.len()
            );

            // don't block the reactor with the potentially expensive hashing
            // and sync file writing
            let write_result = task::spawn_blocking(move || {
                let is_piece_valid = piece.matches_hash();

                // save piece to disk if it's valid
                let blocks = if is_piece_valid {
                    log::debug!("Piece {} is valid", piece_index);
                    let torrent_piece_offset = piece_index as u64 * piece_len as u64;
                    piece.write(torrent_piece_offset, &*files)?;

                    // collect block infos for torrent to identify which
                    // blocks were written to disk
                    let blocks = piece
                        .blocks
                        .iter()
                        .map(|(offset, block)| BlockInfo {
                            piece_index: info.piece_index,
                            offset: *offset,
                            len: block.len() as u32,
                        })
                        .collect();

                    blocks
                } else {
                    log::warn!("Piece {} is NOT valid", info.piece_index);
                    Vec::new()
                };

                Ok((is_piece_valid, blocks))
            })
            .await
            // our code doesn't panic in the task so until better strategies
            // are devised, unwrap here
            .expect("disk IO write task panicked");

            // We don't error out on disk write failure as we don't want to
            // kill the disk task due to potential disk IO errors (which may
            // happen from time to time). We alert torrent of this failure and
            // return normally.
            //
            // TODO(https://github.com/mandreyel/cratetorrent/issues/23): also
            // place back piece write buffer in torrent and retry later
            match write_result {
                Ok((is_piece_valid, blocks)) => {
                    // record write statistics if the piece is valid
                    if is_piece_valid {
                        self.stats.write_count += piece_len as u64;
                    }

                    // alert torrent of block writes and piece completion
                    self.alert_chan.send(TorrentAlert::BatchWrite(Ok(
                        BatchWrite {
                            blocks,
                            is_piece_valid: Some(is_piece_valid),
                        },
                    )))?;
                }
                Err(e) => {
                    log::warn!("Disk write error: {}", e);
                    self.stats.write_failure_count += 1;

                    // alert torrent of block write failure
                    self.alert_chan.send(TorrentAlert::BatchWrite(Err(e)))?;
                }
            }
        }

        Ok(())
    }

    /// Starts a new in-progress piece, creating metadata for it in self.
    ///
    /// This involves getting the expected hash of the piece, its length, and
    /// calculating the files that it intersects.
    fn start_new_piece(&mut self, info: BlockInfo) -> Result<(), WriteError> {
        log::trace!("Creating piece {} write buffer", info.piece_index);

        // get the position of the piece in the concatenated hash string
        let hash_pos = info.piece_index * 20;
        if hash_pos + 20 > self.piece_hashes.len() {
            log::warn!("Piece index {} is invalid", info.piece_index);
            return Err(WriteError::InvalidPieceIndex);
        }

        let hash_slice = &self.piece_hashes[hash_pos..hash_pos + 20];
        let mut expected_hash = [0; 20];
        expected_hash.copy_from_slice(hash_slice);
        log::debug!(
            "Piece {} expected hash {}",
            info.piece_index,
            hex::encode(&expected_hash)
        );

        // TODO: consider using expect here as piece index should be verified in
        // Torrent::write_block
        let len = self
            .info
            .piece_len(info.piece_index)
            .map_err(|_| WriteError::InvalidPieceIndex)?;
        log::debug!("Piece {} is {} bytes long", info.piece_index, len);

        let file_range = self
            .info
            .files_intersecting_piece(info.piece_index)
            .map_err(|_| WriteError::InvalidPieceIndex)?;
        log::debug!(
            "Piece {} intersects files: {:?}",
            info.piece_index,
            file_range
        );

        let piece = Piece {
            expected_hash,
            len,
            blocks: BTreeMap::new(),
            file_range,
        };
        self.write_buf.insert(info.piece_index, piece);

        Ok(())
    }
}

#[derive(Default)]
struct Stats {
    /// The number of bytes successfully written to disk.
    write_count: u64,
    /// The number of times we failed to write to disk.
    write_failure_count: usize,
}