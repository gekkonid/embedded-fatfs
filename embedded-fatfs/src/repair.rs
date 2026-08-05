//! # FAT32 filesystem repair.
//!
//! This implements a rudimentary fsck/chkdsk for FAT32 filesystems. It is not a complete `fsck`, it checks only the
//! damage an interrupted write leaves behind, e.g. a FAT sector or a directory entry corrupted mid-update by reset or
//! power loss. It does not rebuild boot sectors, remap bad blocks, or recover files whose directory entry is gone.
//!
//! [`RepairMode`] selects which classes of damage scan for and fix, to balance performance against expected/tolerated
//! failure modes. We clear the filesystem dirty bit in limited circumstances: never for a [`RepairMode::DryRun`], not
//! when the scratch buffer ran out of depth and part of the tree went unvisited, and not for any  subset of flags
//! narrower than [`RepairMode::Minimal`], which could leave behind damage of a class it was never asked to repair. The
//! hard-error bit is never touched. It says the device returned an error once, which a filesystem check neither
//! verifies nor refutes.
//!
//! The caller supplies all working memory, as a mutable byte slice. The required size is given by
//! [`scratch_size_for_depth`]. This buffer holds one small frame per level of directory nesting, and nothing else grows
//! with the size of the volume. Runtime is governed by the [`RepairMode`], see documentation there for details, but
//! briefly, use [`RepairMode::Minimal`] for standard usage, as corrects the majority of problems with the least effort.
//! A full scan ([`RepairMode::OrphanClusters`]) does three passes over the whole FAT, and therefore can take quite some
//! time.
//!
//! If the `log` or `defmt` features are activated, logs are printed roughly every 10% of progress.
//!
//! ## Example
//!
//! ```no_run
//! # async fn run<IO: embedded_fatfs::ReadWriteSeek, TP: embedded_fatfs::TimeProvider, OCC: embedded_fatfs::OemCpConverter>(
//! #     fs: &embedded_fatfs::FileSystem<IO, TP, OCC>
//! # ) -> Result<(), embedded_fatfs::Error<IO::Error>> {
//! use embedded_fatfs::{scratch_size_for_depth, RepairMode, RepairScratch};
//!
//! let mut buf = [0u8; scratch_size_for_depth(8)];
//! let mut scratch = RepairScratch::new(&mut buf);
//!
//! let stats = fs.repair(RepairMode::Minimal, &mut scratch).await?;
//!
//! # Ok(())
//! # }
//! ```


// `RepairMode`'s flags are named in CamelCase, which matches the convention on enum levels. Unfortunately, `bitflags`
// 1.x generates them as associated consts and does not forward attributes onto them, so the allowance has to sit at
// module scope.
#![allow(non_upper_case_globals)]

use bitflags::bitflags;

use crate::dir::{Dir, DirRawStream};
use crate::dir_entry::{DirEntryData, DirEntryEditor, DirFileEntryData, DIR_ENTRY_SIZE, SFN_PADDING, SFN_SIZE};
use crate::error::Error;
use crate::file::File;
use crate::fs::{FatType, FileSystem, OemCpConverter, ReadWriteSeek};
use crate::io::{ReadLeExt, Seek, SeekFrom, Write, WriteLeExt};
use crate::table::{count_free_clusters, RESERVED_FAT_ENTRIES};
use crate::time::TimeProvider;

/// How many times a phase whose size is known reports its progress.
const PROGRESS_REPORTS_PER_PHASE: u32 = 10;

/// How often the directory walk reports, in entries examined.
const PROGRESS_ENTRY_INTERVAL: u32 = 256;

/// The max number of entries a FAT directory can hold.
const MAX_DIR_ENTRIES: u32 = 65_536;

const FAT32_VISITED_BIT: u32 = 0x1000_0000;
const FAT32_VALUE_MASK: u32 = 0x0FFF_FFFF;
const FAT32_FREE: u32 = 0x0000_0000;
const FAT32_BAD: u32 = 0x0FFF_FFF7;
const FAT32_EOC_MIN: u32 = 0x0FFF_FFF8;
const FAT32_EOC: u32 = 0x0FFF_FFFF;

/// The clean-shutdown and hard-error bits of a FAT entry
const FAT32_STATUS_BITS: u32 = (1 << 26) | (1 << 27);


bitflags! {
    /// Which classes of error should [`FileSystem::repair`] check for?
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    pub struct RepairMode: u16 {
        /// FAT entries 0 and 1, which hold fixed signature values
        const FatSignatures = 0x0001;
        /// Cluster chains that run into an invalid cluster (marked free, out-of-range, etc) and/or file sizes that
        /// disagree with the chain. We truncate the chain at the first invalid entry and update the file size accordingly.
        const Chains = 0x0002;
        /// Directory entries that cannot describe a real file: impossible attribute combinations, first clusters
        /// outside the volume, impossible sizes, wrong `.` and `..` targets, short names containing bytes a name cannot
        /// contain, and a damaged volume label.
        const EntryValidity = 0x0004;
        /// Long-name entries with no short name behind them, and long-name runs
        /// whose checksum no longer matches the entry they precede.
        const LongNames = 0x0008;
        /// FAT copies that disagree with each other. We reset the second fat to the first's value.
        ///
        /// Costs one sequential read of every FAT copy, shared with [`RepairMode::FsInfo`] when both are asked for.
        const FatMirrors = 0x0040;
        /// Clusters marked in use that no directory entry can reach.
        ///
        /// This requires marking a reserved bit of each fat entry to track which clustes are reachable. We actually
        /// make three passes: first clear this bit to reset any half-completed repair run, then mark the reachable
        /// clusters, then a last scan to free the unreachable clusters. Cross-linked chains are separated as part of
        /// the same process.
        const OrphanClusters = 0x0010;
        /// The `FSInfo` free-cluster count, which an unclean shutdown almost always leaves stale.
        ///
        /// On its own it costs one read-only pass over the FAT, shared with `FatMirrors`.
        const FsInfo = 0x0020;
        /// Report what is wrong without changing anything.
        const DryRun = 0x0080;
        /// Everything that can be repaired without multiple passes over the FAT.
        const Minimal = Self::FatSignatures.bits
            | Self::Chains.bits
            | Self::EntryValidity.bits
            | Self::LongNames.bits
            | Self::FatMirrors.bits
            | Self::FsInfo.bits;
        /// Everything, including reclaiming unreachable clusters.
        const All = Self::Minimal.bits | Self::OrphanClusters.bits;
    }
}

impl Default for RepairMode {
    /// Minimal is the recommended mode to catch most erors without excessive FAT scans or writing cluster marks.
    fn default() -> Self {
        Self::Minimal
    }
}

impl RepairMode {
    /// Whether anything at all may be written to the volume.
    fn writes(self) -> bool {
        !self.contains(RepairMode::DryRun)
    }

    /// Whether `flag`'s repairs should actually be applied, as opposed to merely counted.
    fn applies(self, flag: RepairMode) -> bool {
        self.contains(flag) && self.writes()
    }

    /// Whether we write the cluster reachability markers.
    fn marks(self) -> bool {
        self.applies(RepairMode::OrphanClusters)
    }
}


/// Bytes of scratch space one level of directory nesting needs.
pub const REPAIR_FRAME_SIZE: usize = 12;

/// Bytes of scratch space needed to descend `depth` levels of directories.
///
/// The root counts as one level, so `scratch_size_for_depth(1)` only lets the repair scan the root directory itself.
#[must_use]
pub const fn scratch_size_for_depth(depth: usize) -> usize {
    depth * REPAIR_FRAME_SIZE
}

/// Working memory for [`FileSystem::repair`], borrowed from the caller.
pub struct RepairScratch<'a> {
    buf: &'a mut [u8],
    depth: usize,
}

/// One directory waiting to be scanned or resumed.
#[derive(Clone, Copy)]
struct DirFrame {
    /// First cluster of the directory, or 0 for the root.
    cluster: u32,
    /// First cluster of the parent directory
    parent: u32,
    /// Byte offset within the directory to resume iteration at.
    resume: u32,
}

impl<'a> RepairScratch<'a> {
    /// Borrow `buf` as repair scratch space.
    ///
    /// Use [`scratch_size_for_depth`] to size the buffer. Any leftover bytes are ignored; a buffer too small for even
    /// one frame makes [`FileSystem::repair`] return [`Error::InvalidInput`].
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, depth: 0 }
    }

    /// How many levels of directory nesting this buffer can hold.
    #[must_use]
    pub fn max_depth(&self) -> usize {
        self.buf.len() / REPAIR_FRAME_SIZE
    }

    fn is_empty(&self) -> bool {
        self.depth == 0
    }

    fn is_full(&self) -> bool {
        self.depth >= self.max_depth()
    }

    fn push(&mut self, frame: DirFrame) {
        debug_assert!(!self.is_full());
        let at = self.depth * REPAIR_FRAME_SIZE;
        self.buf[at..at + 4].copy_from_slice(&frame.cluster.to_le_bytes());
        self.buf[at + 4..at + 8].copy_from_slice(&frame.parent.to_le_bytes());
        self.buf[at + 8..at + 12].copy_from_slice(&frame.resume.to_le_bytes());
        self.depth += 1;
    }

    fn pop(&mut self) {
        debug_assert!(!self.is_empty());
        self.depth -= 1;
    }

    fn top(&self) -> DirFrame {
        debug_assert!(!self.is_empty());
        let at = (self.depth - 1) * REPAIR_FRAME_SIZE;
        let word = |off: usize| {
            let mut b = [0_u8; 4];
            b.copy_from_slice(&self.buf[at + off..at + off + 4]);
            u32::from_le_bytes(b)
        };
        DirFrame {
            cluster: word(0),
            parent: word(4),
            resume: word(8),
        }
    }

    /// Record where to resume the directory currently on top of the stack.
    fn set_top_resume(&mut self, resume: u32) {
        debug_assert!(!self.is_empty());
        let at = (self.depth - 1) * REPAIR_FRAME_SIZE + 8;
        self.buf[at..at + 4].copy_from_slice(&resume.to_le_bytes());
    }
}


/// What [`FileSystem::repair`] found and did.
///
/// Under [`RepairMode::DryRun`] nothing is written and every counter reports
/// what would have been done.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairStats {
    /// The modes that were actually carried out.
    ///
    /// This is the requested mode minus anything that could not be done in a dry run.
    pub modes_applied: RepairMode,
    /// Directories scanned to completion.
    pub dirs_scanned: u32,
    /// Directories left unscanned because the scratch buffer ran out of depth.
    pub dirs_skipped: u32,
    /// Entries that claimed to be directories but did not start one, and were
    /// therefore left untouched.
    ///
    /// Anything other than zero means the volume has damage this tool will not
    /// touch: a first cluster that points into a file, or a file whose
    /// attribute byte has flipped. Both are safer left alone than guessed at.
    pub dirs_rejected: u32,
    /// Directory entries examined.
    pub files_examined: u32,
    /// Entries whose short name was rewritten because it held illegal bytes.
    pub entries_recovered: u32,
    /// Entries marked deleted because nothing about them could be trusted.
    pub entries_removed: u32,
    /// Chains cut short because they ran into something that was not theirs.
    pub chains_truncated: u32,
    /// Chains cut short specifically because another file had already claimed
    /// the cluster.
    pub cross_links_resolved: u32,
    /// Size fields brought into line with the cluster chain.
    pub sizes_corrected: u32,
    /// `.` or `..` entries pointed back at the right directory.
    pub dot_entries_fixed: u32,
    /// Reserved FAT entries (0 and 1) rewritten from their fixed values.
    pub reserved_entries_restored: u32,
    /// FAT entries copied from FAT #1 over a mirror that disagreed.
    pub mirror_entries_synced: u32,
    /// Set when the `FSInfo` free-cluster count disagreed with the FAT.
    pub fsinfo_corrected: u32,
    /// Chains freed because no directory entry could reach them.
    pub orphans_freed: u32,
    /// Total clusters returned to the free pool.
    pub clusters_freed: u32,
}


impl<IO: ReadWriteSeek, TP: TimeProvider, OCC: OemCpConverter> FileSystem<IO, TP, OCC> {
    /// Check a FAT32 volume, and repair any corruption of the requested type(s).
    ///
    /// `mode` selects which classes of damage to act on; see [`RepairMode`].
    ///
    /// `scratch` is a working buffer, it's size for a given depth directory tree is given by
    /// `scratch_size_for_depth(n)`.
    ///
    /// # Errors
    ///
    /// * [`Error::InvalidInput`] if this is not a FAT32 volume, or if `scratch`
    ///   is too small to hold a single directory frame.
    /// * [`Error::Io`] if the underlying storage fails. A repair interrupted by
    ///   an I/O error leaves the volume readable and can be re-run.
    pub async fn repair(
        &self,
        mode: RepairMode,
        scratch: &mut RepairScratch<'_>,
    ) -> Result<RepairStats, Error<IO::Error>> {
        if self.fat_type() != FatType::Fat32 {
            error!("repair is only implemented for FAT32");
            return Err(Error::InvalidInput);
        }
        if scratch.max_depth() == 0 {
            error!(
                "repair scratch buffer holds no directory frames; needs at least {} bytes",
                REPAIR_FRAME_SIZE
            );
            return Err(Error::InvalidInput);
        }

        let mut stats = RepairStats {
            modes_applied: mode,
            ..RepairStats::default()
        };
        info!("FAT repair: starting, max depth {}", scratch.max_depth());

        // Repair addresses the FAT copies directly from here on, so the cached sector has to go: it would otherwise be
        // written back at the end, over whatever the repair had put there.
        #[cfg(feature = "fat-cache")]
        self.drop_fat_cache().await?;

        if !mode.writes() {
            info!("FAT repair: dry run, nothing will be written");
            // Orphans cannot be found without marking, and marking is a write.
            stats.modes_applied.remove(RepairMode::OrphanClusters);
        }

        if mode.marks() {
            // A previous repair may have died half way through and left markers behind, and not clearing them would
            // miscalculate file lengths, leading to erroneous truncation. Force all markers to zero first for
            // idempoentcy.
            clear_visited_flags(self).await?;
        }

        if mode.contains(RepairMode::FatSignatures) {
            fix_reserved_entries(self, mode, &mut stats).await?;
        }

        // First repair FAT mirrors, so the whole repair is made against one agreed copy of the table. Also update the
        // free-cluster count in the same pass.
        let mut counted_free = if mode.contains(RepairMode::FatMirrors) {
            Some(scan_fat_copies(self, mode, &mut stats).await?)
        } else {
            None
        };

        if mode.marks() {
            // The root directory's own chain is reachable by definition, but no directory entry names it, so it has to
            // be claimed explicitly or the orphan sweep would free the entire root.
            scan_chain(self, self.bpb.root_dir_first_cluster, None, mode, &mut stats).await?;
        }

        scan_tree(self, mode, scratch, &mut stats).await?;
        info!(
            "FAT repair: dirs={} skipped={} entries={} renamed={} removed={} dots_fixed={}",
            stats.dirs_scanned,
            stats.dirs_skipped,
            stats.files_examined,
            stats.entries_recovered,
            stats.entries_removed,
            stats.dot_entries_fixed,
        );

        // Freeing a cluster is the one repair that needs to have seen the whole tree: an unvisited cluster is only
        // garbage if every directory was walked. If any subtree was out of reach, sweep the markers away but keep the
        // clusters, and let the caller re-run with more scratch.
        if mode.marks() {
            let reclaim = stats.dirs_skipped == 0;
            if !reclaim {
                warn!("FAT repair: subtrees were skipped, so unreachable clusters are being left alone");
                stats.modes_applied.remove(RepairMode::OrphanClusters);
            }
            counted_free = Some(sweep_orphans(self, &mut stats, reclaim).await?);
            info!(
                "FAT repair: truncated={} sizes_corrected={} orphans={} clusters_freed={}",
                stats.chains_truncated, stats.sizes_corrected, stats.orphans_freed, stats.clusters_freed,
            );
        }

        if mode.contains(RepairMode::FsInfo) {
            // Whichever pass has already walked the FAT will have counted for us. If none did, this is the only
            // full-FAT pass we make, and it yields only a count, so the allocation hint keeps whatever it had.
            let free = match counted_free {
                Some(free) => free,
                None => FreeSpace {
                    count: count_free_clusters(&mut self.fat_slice_uncached(), self.fat_type(), self.total_clusters).await?,
                    first: None,
                },
            };
            if self.free_cluster_count_hint() != Some(free.count) {
                stats.fsinfo_corrected += 1;
                if mode.applies(RepairMode::FsInfo) {
                    self.set_verified_free_cluster_count(free.count);
                }
            }
            // Set allocation hint
            if let Some(first_free) = free.first {
                if mode.applies(RepairMode::FsInfo) {
                    self.set_next_free_cluster_hint(first_free);
                }
            }
        }

        if mode.writes() {
            // Declaring the volume clean is what stops a device that repairs on a dirty mount from repairing on every
            // mount for ever. It is only honest when the run both covered the whole tree and was allowed to fix
            // everything it might have found. `Minimal` is the narrowest mode that repairs every class of structural
            // damage, so that is the bar.
            //
            // `dirs_rejected` deliberately does not disqualify a run. That damage is never going to be repaired by this
            // tool, so holding the flag open for it would guarantee the loop rather than avoid it.
            let earned_clean = stats.dirs_skipped == 0 && mode.contains(RepairMode::Minimal);
            if earned_clean {
                self.mark_clean().await?;
            } else {
                warn!("FAT repair: leaving the volume marked dirty; this run did not cover everything");
                // Not merely declining to clear it: the caller's `unmount` would otherwise clear it for us, because
                // recomputing the free-cluster count grants `flush` that right.
                self.withhold_clean_flag();
            }
            // `flush_state`, not `flush`: the latter clears the dirty flag on its own account whenever it holds a
            // recomputed free-cluster count, which is a weaker claim than the one made above. Letting it run here would
            // mean writing the flag, having it cleared, and writing it back — and a power cut in that window leaves a
            // volume marked clean that was never repaired.
            //
            // After `mark_clean`, because that writes FAT entry 1 through the FAT cache.
            self.flush_state().await?;
        }
        info!("FAT repair: complete");
        Ok(stats)
    }
}


/// Progress ticker
struct Ticker {
    step: u32,
    next: u32,
}

impl Ticker {
    fn new(total: u32) -> Self {
        let step = (total / PROGRESS_REPORTS_PER_PHASE).max(1);
        Self { step, next: step }
    }

    fn every(step: u32) -> Self {
        Self { step, next: step }
    }

    fn tick(&mut self, done: u32) -> bool {
        if done < self.next {
            return false;
        }
        self.next = done.saturating_add(self.step);
        true
    }
}

fn percent(done: u32, total: u32) -> u32 {
    if total == 0 {
        return 100;
    }
    let pct = (u64::from(done) * 100 + u64::from(total) / 2) / u64::from(total);
    (pct as u32).min(100)
}

/// Number of FAT entries on the volume, including the two reserved ones.
fn fat_entry_count<IO: ReadWriteSeek, TP, OCC>(fs: &FileSystem<IO, TP, OCC>) -> u32 {
    fs.total_clusters + RESERVED_FAT_ENTRIES
}

/// Reads a FAT entry verbatim, reserved bits included.
async fn read_fat_raw<IO: ReadWriteSeek, TP, OCC>(
    fs: &FileSystem<IO, TP, OCC>,
    cluster: u32,
) -> Result<u32, Error<IO::Error>> {
    let mut fat = fs.fat_slice_uncached();
    fat.seek(SeekFrom::Start(u64::from(cluster) * 4)).await?;
    Ok(fat.read_u32_le().await?)
}

/// Write a FAT entry verbatim, reserved bits included.
async fn write_fat_raw<IO: ReadWriteSeek, TP, OCC>(
    fs: &FileSystem<IO, TP, OCC>,
    cluster: u32,
    raw: u32,
) -> Result<(), Error<IO::Error>> {
    let mut fat = fs.fat_slice_uncached();
    fat.seek(SeekFrom::Start(u64::from(cluster) * 4)).await?;
    fat.write_u32_le(raw).await?;
    Ok(())
}

/// Write the 28-bit value of a FAT entry, leaving the reserved top nibble as it was.
async fn write_fat_value<IO: ReadWriteSeek, TP, OCC>(
    fs: &FileSystem<IO, TP, OCC>,
    cluster: u32,
    value: u32,
) -> Result<(), Error<IO::Error>> {
    let old = read_fat_raw(fs, cluster).await?;
    let new = (value & FAT32_VALUE_MASK) | (old & !FAT32_VALUE_MASK);
    if new != old {
        write_fat_raw(fs, cluster, new).await?;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum FatEntry {
    Free,
    Data(u32),
    Bad,
    EndOfChain,
}

fn classify(raw: u32) -> FatEntry {
    match raw & FAT32_VALUE_MASK {
        FAT32_FREE => FatEntry::Free,
        FAT32_BAD => FatEntry::Bad,
        FAT32_EOC_MIN..=FAT32_EOC => FatEntry::EndOfChain,
        n => FatEntry::Data(n),
    }
}

fn is_visited(raw: u32) -> bool {
    raw & FAT32_VISITED_BIT != 0
}


async fn clear_visited_flags<IO: ReadWriteSeek, TP, OCC>(fs: &FileSystem<IO, TP, OCC>) -> Result<(), Error<IO::Error>> {
    let count = fat_entry_count(fs);
    trace!("FAT repair: clearing markers over {} entries", count);

    // Entries 0 and 1 are skipped deliberately. They are not clusters, so they are never marked, and entry 1
    // legitimately has status flags in the reserved bits.
    let mut fat = fs.fat_slice_uncached();
    let mut ticker = Ticker::new(count);
    for cluster in RESERVED_FAT_ENTRIES..count {
        if ticker.tick(cluster) {
            debug!("FAT repair: clearing markers, {}%", percent(cluster, count));
        }
        fat.seek(SeekFrom::Start(u64::from(cluster) * 4)).await?;
        let raw = fat.read_u32_le().await?;
        if is_visited(raw) {
            fat.seek(SeekFrom::Start(u64::from(cluster) * 4)).await?;
            fat.write_u32_le(raw & !FAT32_VISITED_BIT).await?;
        }
    }
    Ok(())
}

/// Restore FAT entries 0 and 1, which hold fixed signature values rather than cluster links.
///
/// Entry 0 is the media descriptor from the BPB padded with ones; entry 1 is an end-of-chain mark whose top bits double
/// as the clean-shutdown and hard error flags. When entry 1 is unreadable those flags cannot be recovered, so it is
/// rewritten as clean.
async fn fix_reserved_entries<IO: ReadWriteSeek, TP, OCC>(
    fs: &FileSystem<IO, TP, OCC>,
    mode: RepairMode,
    stats: &mut RepairStats,
) -> Result<(), Error<IO::Error>> {
    let writing = mode.applies(RepairMode::FatSignatures);
    let expected_0 = 0x0FFF_FF00 | u32::from(fs.bpb.media);

    if read_fat_raw(fs, 0).await? & FAT32_VALUE_MASK != expected_0 {
        warn!("FAT repair: restoring FAT entry 0 (media descriptor)");
        stats.reserved_entries_restored += 1;
        if writing {
            write_fat_raw(fs, 0, expected_0).await?;
        }
    }
    // Entry 1 is an end-of-chain mark, but its top two value bits are the clean-shutdown and hard-error flags. If
    // invalid, reset to clean state.
    let entry_1 = read_fat_raw(fs, 1).await? & FAT32_VALUE_MASK;
    if entry_1 | FAT32_STATUS_BITS != FAT32_VALUE_MASK {
        warn!("FAT repair: restoring FAT entry 1 (end-of-chain prototype)");
        stats.reserved_entries_restored += 1;
        if writing {
            write_fat_raw(fs, 1, u32::MAX).await?;
        }
    }
    Ok(())
}

/// Read a FAT entry from a specific copy of the table.
async fn read_fat_copy<IO: ReadWriteSeek, TP, OCC>(
    fs: &FileSystem<IO, TP, OCC>,
    copy: u8,
    cluster: u32,
) -> Result<u32, Error<IO::Error>> {
    let sectors_per_fat = fs.bpb.sectors_per_fat_32;
    let first_sector = u32::from(fs.bpb.reserved_sectors) + u32::from(copy) * sectors_per_fat;
    let offset = fs.bpb.bytes_from_sectors(first_sector) + u64::from(cluster) * 4;
    let mut disk = fs.disk.borrow_mut();
    disk.seek(SeekFrom::Start(offset)).await?;
    Ok(disk.read_u32_le().await?)
}

/// What a pass over the whole FAT learned about free space.
struct FreeSpace {
    /// How many clusters are free.
    count: u32,
    /// The lowest-numbered free cluster, if there is one.
    first: Option<u32>,
}

impl FreeSpace {
    fn note(&mut self, cluster: u32) {
        self.count += 1;
        self.first.get_or_insert(cluster);
    }
}

/// One sequential pass over the FAT that brings the mirrors back into line with FAT #1 and counts free clusters.
///
///  Writes go through the normal FAT accessor, which mirrors them to every copy, so repairing a divergent entry is a
///  single write, not one per copy.
async fn scan_fat_copies<IO: ReadWriteSeek, TP, OCC>(
    fs: &FileSystem<IO, TP, OCC>,
    mode: RepairMode,
    stats: &mut RepairStats,
) -> Result<FreeSpace, Error<IO::Error>> {
    let count = fat_entry_count(fs);
    let sync = mode.contains(RepairMode::FatMirrors) && fs.bpb.fats > 1;
    let mut free = FreeSpace { count: 0, first: None };

    let mut ticker = Ticker::new(count);
    for cluster in RESERVED_FAT_ENTRIES..count {
        if ticker.tick(cluster) {
            debug!("FAT repair: checking FAT copies, {}%", percent(cluster, count));
        }
        let primary = read_fat_raw(fs, cluster).await?;
        if classify(primary) == FatEntry::Free {
            free.note(cluster);
        }
        if !sync {
            continue;
        }
        for copy in 1..fs.bpb.fats {
            if read_fat_copy(fs, copy, cluster).await? != primary {
                stats.mirror_entries_synced += 1;
                if mode.applies(RepairMode::FatMirrors) {
                    // One write, not one per copy: the FAT accessor mirrors it to every copy.
                    write_fat_raw(fs, cluster, primary).await?;
                }
                break;
            }
        }
    }
    Ok(free)
}


/// Outcome of following one cluster chain.
struct ChainScan {
    /// Clusters that are genuinely part of the chain and are now marked in use.
    clusters: u32,
    /// The chain was cut short: whatever followed was not the chain's to keep.
    truncated: bool,
    /// It was cut short because another file had already claimed the cluster.
    cross_linked: bool,
}

/// Follow the chain from `start`, marking each cluster as reached when the mode calls for it.
///
/// Stops early and terminates FAT chain when the next link is free, bad, out of range, already claimed by something
/// else, or when `limit` clusters have been taken. Returns how many clusters the chain really has.
async fn scan_chain<IO: ReadWriteSeek, TP, OCC>(
    fs: &FileSystem<IO, TP, OCC>,
    start: u32,
    limit: Option<u32>,
    mode: RepairMode,
    stats: &mut RepairStats,
) -> Result<ChainScan, Error<IO::Error>> {
    let mut scan = ChainScan {
        clusters: 0,
        truncated: false,
        cross_linked: false,
    };
    if !fs.is_valid_cluster(start) {
        scan.truncated = true;
        return Ok(scan);
    }

    // No chain can be longer than the volume's cluster count. If we find a directory that suggests otherwise, its
    // corrupted.
    let max_hops = fs.total_clusters;

    let mut cluster = start;
    let mut prev: Option<u32> = None;
    loop {
        let raw = read_fat_raw(fs, cluster).await?;

        // Already reached from somewhere else, so it is not ours. Note this also catches a chain that loops back onto
        // itself, because we mark as we go.
        if mode.marks() && is_visited(raw) {
            scan.truncated = true;
            scan.cross_linked = true;
            break;
        }

        // A cluster the FAT calls free is not allocated to anybody, whatever the chain that led here says.
        if classify(raw) == FatEntry::Free || classify(raw) == FatEntry::Bad {
            scan.truncated = true;
            break;
        }

        if mode.marks() {
            write_fat_raw(fs, cluster, raw | FAT32_VISITED_BIT).await?;
        }
        scan.clusters += 1;
        prev = Some(cluster);

        if limit.is_some_and(|max| scan.clusters >= max) {
            // The size field says the file ends here, so truncate and free the leftovers.
            if classify(raw) != FatEntry::EndOfChain {
                scan.truncated = true;
            }
            break;
        }
        if scan.clusters >= max_hops {
            warn!("FAT repair: chain from cluster {} does not end; cutting it here", start);
            scan.truncated = true;
            break;
        }

        match classify(raw) {
            FatEntry::EndOfChain => return Ok(scan),
            FatEntry::Data(next) if fs.is_valid_cluster(next) => cluster = next,
            // A link to cluster 0, 1, or past the end of the volume: a half-written FAT entry. There is no way to guess
            // what it meant.
            FatEntry::Data(_) => {
                scan.truncated = true;
                break;
            }
            FatEntry::Free | FatEntry::Bad => unreachable!("handled above"),
        }
    }

    if scan.truncated {
        if mode.applies(RepairMode::Chains) {
            if let Some(last) = prev {
                write_fat_value(fs, last, FAT32_EOC).await?;
            }
        }
        stats.chains_truncated += 1;
        if scan.cross_linked {
            stats.cross_links_resolved += 1;
        }
    }
    Ok(scan)
}

async fn scan_tree<IO: ReadWriteSeek, TP: TimeProvider, OCC: OemCpConverter>(
    fs: &FileSystem<IO, TP, OCC>,
    mode: RepairMode,
    scratch: &mut RepairScratch<'_>,
    stats: &mut RepairStats,
) -> Result<(), Error<IO::Error>> {
    scratch.push(DirFrame {
        cluster: 0,
        parent: 0,
        resume: 0,
    });

    let mut ticker = Ticker::every(PROGRESS_ENTRY_INTERVAL);

    while !scratch.is_empty() {
        let frame = scratch.top();
        if frame.resume == 0 {
            stats.dirs_scanned += 1;
        }
        if ticker.tick(stats.files_examined) {
            debug!(
                "FAT repair: scanned {} directories, {} entries",
                stats.dirs_scanned, stats.files_examined
            );
        }

        match scan_dir(fs, frame, mode, stats).await? {
            // The directory is exhausted; go back up.
            DirStep::Done => scratch.pop(),
            DirStep::Descend { cluster, resume } => {
                scratch.set_top_resume(resume);
                if scratch.is_full() {
                    // Out of depth.
                    warn!("FAT repair: directory tree deeper than scratch allows, skipping a subtree");
                    stats.dirs_skipped += 1;
                } else {
                    scratch.push(DirFrame {
                        cluster,
                        parent: frame.cluster,
                        resume: 0,
                    });
                }
            }
        }
    }
    Ok(())
}

enum DirStep {
    Done,
    Descend { cluster: u32, resume: u32 },
}

/// One directory entry, located both ways we need it.
struct EntrySite {
    /// Offset within the directory of the short-name entry.
    pos: u64,
    /// Offset within the directory where this entry's run starts, which is the
    /// first of its long-name entries when it has any.
    run_start: u64,
    /// Position of the short-name entry on the storage device, which is what
    /// `DirEntryEditor` writes through.
    abs_pos: u64,
}

/// Scan one directory from `frame.resume` until it ends or until encoutering a subdirectory.
async fn scan_dir<IO: ReadWriteSeek, TP: TimeProvider, OCC: OemCpConverter>(
    fs: &FileSystem<IO, TP, OCC>,
    frame: DirFrame,
    mode: RepairMode,
    stats: &mut RepairStats,
) -> Result<DirStep, Error<IO::Error>> {
    let mut stream = open_dir(fs, frame.cluster).stream;
    stream.seek(SeekFrom::Start(u64::from(frame.resume))).await?;

    // The run of long-name entries we are in the middle of, if any.
    let mut lfn_start: Option<u64> = None;
    let mut lfn_checksum_seen = 0_u8;
    let mut lfn_damaged = false;

    loop {
        let pos = stream.seek(SeekFrom::Current(0)).await?;
        let raw = match DirEntryData::deserialize(&mut stream).await {
            Ok(raw) => raw,
            // Running off the end of the directory's data means its chain was cut short. A directory that stops without
            // an end marker simply stops.
            Err(Error::UnexpectedEof) => {
                warn!("FAT repair: directory ends without an end-of-directory marker");
                return Ok(DirStep::Done);
            }
            Err(e) => return Err(e),
        };
        let next = pos + u64::from(DIR_ENTRY_SIZE);
        // `abs_pos` reports where the stream now is, which is just past the entry we read; the cluster it names is the
        // one holding that entry.
        //
        // A `None` here must never be turned into a default. The repairs below write to this offset directly, so
        // defaulting it to 0 would put a rewritten directory entry over the boot sector.
        let Some(end_of_entry) = stream.abs_pos() else {
            error!("FAT repair: cannot locate a directory entry on the device; refusing to write blind");
            return Err(Error::CorruptedFileSystem);
        };
        let abs_pos = end_of_entry - u64::from(DIR_ENTRY_SIZE);

        if raw.is_end() {
            // Long-name entries with nothing behind them: a file whose creation never got as far as writing its short
            // name.
            if let Some(start) = lfn_start {
                debug!("FAT repair: dropping long-name entries with no file behind them");
                delete_range(fs, &mut stream, start, pos, mode, RepairMode::LongNames, stats).await?;
            }
            return Ok(DirStep::Done);
        }
        if raw.is_deleted() {
            lfn_start = None;
            stream.seek(SeekFrom::Start(next)).await?;
            continue;
        }

        let data = match raw {
            DirEntryData::Lfn(lfn) => {
                match lfn_start {
                    None => {
                        lfn_start = Some(pos);
                        lfn_checksum_seen = lfn.checksum();
                        lfn_damaged = false;
                    }
                    // Every entry of a run carries the same checksum. One that
                    // does not belongs to a run that was overwritten in place.
                    Some(_) if lfn.checksum() != lfn_checksum_seen => lfn_damaged = true,
                    Some(_) => {}
                }
                stream.seek(SeekFrom::Start(next)).await?;
                continue;
            }
            DirEntryData::File(data) => data,
        };

        // A run only belongs to the short name it precedes if the checksums agree. When they do not, the long name is
        // the part that cannot be trusted: drop it and let the file keep its short name.
        let mut run_start = pos;
        if let Some(start) = lfn_start.take() {
            if lfn_damaged || lfn_checksum_seen != lfn_checksum(data.name()) {
                debug!("FAT repair: dropping a long name that does not match its file");
                delete_range(fs, &mut stream, start, pos, mode, RepairMode::LongNames, stats).await?;
            } else {
                run_start = start;
            }
        }
        let site = EntrySite {
            pos,
            run_start,
            abs_pos,
        };

        if is_dot_or_dotdot(data.name()) {
            fix_dot_entry(fs, &data, &site, frame, mode, stats).await?;
        } else if data.is_volume() {
            if is_name_corrupted(data.name()) {
                fix_volume_label(fs, &data, &site, mode).await?;
                stats.entries_recovered += 1;
            }
        } else {
            stats.files_examined += 1;
            if let Some(cluster) = check_entry(fs, &data, &site, &mut stream, mode, stats).await? {
                stream.flush().await?;
                // A directory is capped at `MAX_DIR_ENTRIES`, so this always fits; if it somehow does not, stop rather
                // than resume the parent at a wrapped-around offset and walk it twice.
                let Ok(resume) = u32::try_from(next) else {
                    error!(
                        "FAT repair: directory offset {} is beyond anything a directory can hold",
                        next
                    );
                    return Err(Error::CorruptedFileSystem);
                };
                return Ok(DirStep::Descend { cluster, resume });
            }
        }

        stream.seek(SeekFrom::Start(next)).await?;
    }
}

/// Mark every entry in `[start, end)` deleted, as a driver would.
///
/// `authorised_by` is the flag whose remit this deletion falls under; the entry is only counted, not touched, unless
/// `mode` grants it.
async fn delete_range<IO: ReadWriteSeek, TP: TimeProvider, OCC>(
    fs: &FileSystem<IO, TP, OCC>,
    stream: &mut DirRawStream<'_, IO, TP, OCC>,
    start: u64,
    end: u64,
    mode: RepairMode,
    authorised_by: RepairMode,
    stats: &mut RepairStats,
) -> Result<(), Error<IO::Error>> {
    let _ = fs;
    stats.entries_removed += 1;
    if !mode.applies(authorised_by) {
        return Ok(());
    }
    let resume = stream.seek(SeekFrom::Current(0)).await?;
    stream.seek(SeekFrom::Start(start)).await?;
    let count = (end - start) / u64::from(DIR_ENTRY_SIZE);
    debug!("FAT repair: deleting {} directory entries at {}", count, start);
    for _ in 0..count {
        let mut data = DirEntryData::deserialize(&mut *stream).await?;
        data.set_deleted();
        stream.seek(SeekFrom::Current(-i64::from(DIR_ENTRY_SIZE))).await?;
        data.serialize(&mut *stream).await?;
    }
    stream.flush().await?;
    stream.seek(SeekFrom::Start(resume)).await?;
    Ok(())
}

/// The longest cluster chain that could still be a directory.
fn max_dir_clusters<IO: ReadWriteSeek, TP, OCC>(fs: &FileSystem<IO, TP, OCC>) -> u32 {
    (MAX_DIR_ENTRIES * DIR_ENTRY_SIZE / fs.cluster_size()).max(1)
}

/// Whether `cluster` really is the start of a directory, as opposed to somewhere in the middle of a file that a
/// corrupted entry has pointed us at.
///
/// Every FAT directory except the root begins with `.` and `..`. In theory, nothing else should: random data satisfies
/// this with a probability so small it's practically impossible (on the order of 1e-100), which makes this a cheap,
/// decisive check for whether it is safe to treat what follows as directory entries and start rewriting them.
///
/// Only the names and the directory attribute are checked, not the cluster numbers in those entries: a `..` pointing at
/// the wrong place is ordinary damage that [`fix_dot_entry`] repairs, and is no reason to disown a directory that is
/// plainly a directory.
async fn looks_like_directory<IO: ReadWriteSeek, TP: TimeProvider, OCC: OemCpConverter>(
    fs: &FileSystem<IO, TP, OCC>,
    cluster: u32,
) -> Result<bool, Error<IO::Error>> {
    let mut stream = open_dir(fs, cluster).stream;
    for expected in [b".          ", b"..         "] {
        let raw = match DirEntryData::deserialize(&mut stream).await {
            Ok(raw) => raw,
            // A real I/O failure is the caller's problem. Anything else — a
            // chain that runs out, data that will not parse — just means this
            // is not a directory.
            Err(Error::Io(e)) => return Err(Error::Io(e)),
            Err(_) => return Ok(false),
        };
        match raw {
            DirEntryData::File(data) if data.is_dir() && data.name() == expected => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn open_dir<IO: ReadWriteSeek, TP, OCC>(fs: &FileSystem<IO, TP, OCC>, cluster: u32) -> Dir<'_, IO, TP, OCC> {
    if cluster == 0 {
        fs.root_dir()
    } else {
        Dir::new(DirRawStream::File(File::new(Some(cluster), None, fs)), fs)
    }
}

/// Check one ordinary entry, repairing what is wrong with it.
///
/// Returns the first cluster of a subdirectory that should be descended into.
async fn check_entry<IO: ReadWriteSeek, TP: TimeProvider, OCC: OemCpConverter>(
    fs: &FileSystem<IO, TP, OCC>,
    data: &DirFileEntryData,
    site: &EntrySite,
    stream: &mut DirRawStream<'_, IO, TP, OCC>,
    mode: RepairMode,
    stats: &mut RepairStats,
) -> Result<Option<u32>, Error<IO::Error>> {
    let first_cluster = data.first_cluster(FatType::Fat32);
    let name_corrupt = is_name_corrupted(data.name());
    let is_dir = data.is_dir();
    let end = site.pos + u64::from(DIR_ENTRY_SIZE);

    if !attrs_are_possible(data) || (name_corrupt && first_cluster.is_none()) {
        delete_range(fs, stream, site.run_start, end, mode, RepairMode::EntryValidity, stats).await?;
        return Ok(None);
    }

    let Some(start) = first_cluster else {
        if !is_dir && data.size().unwrap_or(0) != 0 {
            set_size(fs, data, site, 0, mode, RepairMode::EntryValidity, stats).await?;
        }
        if name_corrupt {
            rename_entry(fs, site, mode, stats).await?;
        }
        return Ok(None);
    };

    if !fs.is_valid_cluster(start) {
        delete_range(fs, stream, site.run_start, end, mode, RepairMode::EntryValidity, stats).await?;
        return Ok(None);
    }

    if name_corrupt {
        rename_entry(fs, site, mode, stats).await?;
    }

    let descend = if is_dir {
        let real = looks_like_directory(fs, start).await?;
        if !real {
            warn!(
                "FAT repair: entry claims to be a directory but cluster {} does not start one; leaving it alone",
                start
            );
            stats.dirs_rejected += 1;
        }
        real
    } else {
        false
    };

    // A file is as long as the shorter of its size and its chain. A directory is as long as its chain, up to the most a
    // directory can hold.
    let limit = if is_dir {
        descend.then(|| max_dir_clusters(fs))
    } else {
        Some(data.size().unwrap_or(0).div_ceil(fs.cluster_size()))
    };
    let scan = scan_chain(fs, start, limit, mode, stats).await?;

    if scan.clusters == 0 {
        if is_dir {
            delete_range(fs, stream, site.run_start, end, mode, RepairMode::EntryValidity, stats).await?;
        } else {
            clear_data(fs, data, site, mode, stats).await?;
        }
        return Ok(None);
    }

    if !is_dir {
        let reachable = scan.clusters.saturating_mul(fs.cluster_size());
        if data.size().unwrap_or(0) > reachable {
            set_size(fs, data, site, reachable, mode, RepairMode::Chains, stats).await?;
        }
    }

    Ok(descend.then_some(start))
}


/// Clear the reachability markers, and free every allocated cluster that the walk never reached.
///
/// We only free when `reclaim` is true, i.e. when the walk covered the whole tree. The markers still have to be cleared
/// either way, or the next run would read them as claims.
async fn sweep_orphans<IO: ReadWriteSeek, TP, OCC>(
    fs: &FileSystem<IO, TP, OCC>,
    stats: &mut RepairStats,
    reclaim: bool,
) -> Result<FreeSpace, Error<IO::Error>> {
    let count = fat_entry_count(fs);
    let mut free = FreeSpace { count: 0, first: None };
    let mut in_orphan_run = false;

    let mut fat = fs.fat_slice_uncached();
    let mut ticker = Ticker::new(count);
    for cluster in RESERVED_FAT_ENTRIES..count {
        if ticker.tick(cluster) {
            debug!("FAT repair: sweeping, {}%", percent(cluster, count));
        }
        fat.seek(SeekFrom::Start(u64::from(cluster) * 4)).await?;
        let raw = fat.read_u32_le().await?;

        if is_visited(raw) {
            fat.seek(SeekFrom::Start(u64::from(cluster) * 4)).await?;
            fat.write_u32_le(raw & !FAT32_VISITED_BIT).await?;
            in_orphan_run = false;
            continue;
        }

        match classify(raw) {
            FatEntry::Free => {
                free.note(cluster);
                in_orphan_run = false;
            }
            FatEntry::Data(_) | FatEntry::EndOfChain if reclaim => {
                fat.seek(SeekFrom::Start(u64::from(cluster) * 4)).await?;
                fat.write_u32_le(FAT32_FREE).await?;
                free.note(cluster);
                stats.clusters_freed += 1;
                if !in_orphan_run {
                    stats.orphans_freed += 1;
                    in_orphan_run = true;
                }
            }
            FatEntry::Bad | FatEntry::Data(_) | FatEntry::EndOfChain => in_orphan_run = false,
        }
    }

    // The sweep has just counted every free cluster on the volume, so FSInfo can be made exactly right for free rather
    // than left as the stale guess an unclean shutdown leaves behind.
    Ok(free)
}


fn is_dot_or_dotdot(name: &[u8; SFN_SIZE]) -> bool {
    name[0] == b'.' && name[1..].iter().all(|&b| b == SFN_PADDING || b == b'.')
}

/// Bytes below 0x20 cannot appear in a short name; 0x05 is the escape for a leading 0xE5 and is legal in the first
/// position only.
fn is_name_corrupted(name: &[u8; SFN_SIZE]) -> bool {
    name.iter()
        .enumerate()
        .any(|(i, &b)| b < 0x20 && !(i == 0 && b == 0x05))
}

/// An entry cannot be both a volume label and a directory.
fn attrs_are_possible(data: &DirFileEntryData) -> bool {
    !(data.is_volume() && data.is_dir())
}

/// Checksum tying a run of long-name entries to the short name behind it.
///
/// Defined by the FAT specification; reimpemented here rather than borrowed from `dir` so that repair does not depend
/// on the `lfn` feature being enabled.
fn lfn_checksum(short_name: &[u8; SFN_SIZE]) -> u8 {
    short_name
        .iter()
        .fold(0_u8, |sum, &b| sum.rotate_right(1).wrapping_add(b))
}

/// `.` must name the directory it sits in and `..` must name its parent, with 0 standing for the root. Both are known
/// for free during the walk.
async fn fix_dot_entry<IO: ReadWriteSeek, TP: TimeProvider, OCC: OemCpConverter>(
    fs: &FileSystem<IO, TP, OCC>,
    data: &DirFileEntryData,
    site: &EntrySite,
    frame: DirFrame,
    mode: RepairMode,
    stats: &mut RepairStats,
) -> Result<(), Error<IO::Error>> {
    let is_dotdot = data.name()[1] == b'.';
    let expected = if is_dotdot { frame.parent } else { frame.cluster };
    let found = data.first_cluster(FatType::Fat32).unwrap_or(0);
    if found == expected {
        return Ok(());
    }

    debug!(
        "FAT repair: correcting a dot entry from cluster {} to {}",
        found, expected
    );
    stats.dot_entries_fixed += 1;
    if !mode.applies(RepairMode::EntryValidity) {
        return Ok(());
    }
    let mut editor = DirEntryEditor::new(data.clone(), site.abs_pos);
    editor.set_first_cluster(if expected == 0 { None } else { Some(expected) }, FatType::Fat32);
    editor.flush(fs).await?;
    Ok(())
}

async fn set_size<IO: ReadWriteSeek, TP: TimeProvider, OCC: OemCpConverter>(
    fs: &FileSystem<IO, TP, OCC>,
    data: &DirFileEntryData,
    site: &EntrySite,
    new_size: u32,
    mode: RepairMode,
    authorised_by: RepairMode,
    stats: &mut RepairStats,
) -> Result<(), Error<IO::Error>> {
    debug!(
        "FAT repair: shrinking size from {} to {}",
        data.size().unwrap_or(0),
        new_size
    );
    stats.sizes_corrected += 1;
    if !mode.applies(authorised_by) {
        return Ok(());
    }
    let mut editor = DirEntryEditor::new(data.clone(), site.abs_pos);
    editor.set_size(new_size);
    editor.flush(fs).await?;
    Ok(())
}

/// Make an entry describe an empty file: no size, no first cluster.
async fn clear_data<IO: ReadWriteSeek, TP: TimeProvider, OCC: OemCpConverter>(
    fs: &FileSystem<IO, TP, OCC>,
    data: &DirFileEntryData,
    site: &EntrySite,
    mode: RepairMode,
    stats: &mut RepairStats,
) -> Result<(), Error<IO::Error>> {
    debug!("FAT repair: emptying an entry whose first cluster is not allocated");
    stats.sizes_corrected += 1;
    if !mode.applies(RepairMode::Chains) {
        return Ok(());
    }
    let mut editor = DirEntryEditor::new(data.clone(), site.abs_pos);
    editor.set_size(0);
    editor.set_first_cluster(None, FatType::Fat32);
    editor.flush(fs).await?;
    Ok(())
}

/// Replace a short name that holds bytes a name cannot hold.
async fn rename_entry<IO: ReadWriteSeek, TP, OCC>(
    fs: &FileSystem<IO, TP, OCC>,
    site: &EntrySite,
    mode: RepairMode,
    stats: &mut RepairStats,
) -> Result<(), Error<IO::Error>> {
    let name = recovery_name(stats.entries_recovered);
    debug!("FAT repair: renaming an entry with an illegal short name");
    stats.entries_recovered += 1;
    if !mode.applies(RepairMode::EntryValidity) {
        return Ok(());
    }
    let mut disk = fs.disk.borrow_mut();
    disk.seek(SeekFrom::Start(site.abs_pos)).await?;
    disk.write_all(&name).await?;
    disk.flush().await?;
    Ok(())
}

/// `FSCKnnnn.REC`, numbered so repeated recoveries in one directory do not collide.
fn recovery_name(index: u32) -> [u8; SFN_SIZE] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut name = *b"FSCK0000REC";
    for i in 0..4 {
        name[7 - i] = HEX[(index as usize >> (i * 4)) & 0xF];
    }
    name
}

/// Rewrite a damaged volume-label entry from the label in the boot sector, which is the copy the volume is actually
/// identified by.
async fn fix_volume_label<IO: ReadWriteSeek, TP: TimeProvider, OCC: OemCpConverter>(
    fs: &FileSystem<IO, TP, OCC>,
    data: &DirFileEntryData,
    site: &EntrySite,
    mode: RepairMode,
) -> Result<(), Error<IO::Error>> {
    debug!("FAT repair: restoring the volume label from the boot sector");
    if !mode.applies(RepairMode::EntryValidity) {
        return Ok(());
    }
    let mut label = [SFN_PADDING; SFN_SIZE];
    let from_bpb = fs.volume_label_as_bytes();
    let len = from_bpb.len().min(SFN_SIZE);
    label[..len].copy_from_slice(&from_bpb[..len]);

    let repaired = data.renamed(label);
    let mut disk = fs.disk.borrow_mut();
    disk.seek(SeekFrom::Start(site.abs_pos)).await?;
    repaired.serialize(&mut *disk).await?;
    disk.flush().await?;
    Ok(())
}
