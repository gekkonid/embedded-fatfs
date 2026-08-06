//! The corruption cases.
//!
//! Each case is a torn update: a FAT sector or a directory entry that was only
//! half written when the power went away. Nothing here needs a volume-wide
//! cluster ownership map to diagnose, so a checker can handle all of them in
//! constant RAM.
//!
//! Offsets are all discovered from the image at build time — no cluster numbers
//! or entry indices are hard-coded — so the corpus survives changes to the
//! library's allocation order.

use super::image::{dirent, fsinfo, lfn_checksum, pad_sfn, Fat32Image, FAT_EOC, FAT_FREE};
use super::{fsinfo_is_a_hint, no_edit, nothing, CaseSpec, Domain, Survivor, Visibility};

/// Short name the library derives for `DATA/sensor readings.txt`. Asserted in
/// `tests/fsck_corpus.rs`, so a change in the library's SFN generator shows up
/// as a clear failure rather than a mystery.
pub const LONG_NAME_SFN: &str = "SENSOR~1.TXT";

/// The pristine tree, which is what most repairs leave behind.
const ALL_INTACT: &[Survivor] = &[
    Survivor::intact("BOOT.BIN", 1536),
    Survivor::intact("LOG.TXT", 512),
    Survivor::intact("EMPTY.TXT", 0),
    Survivor::intact("DATA/READINGS.CSV", 1000),
    Survivor::intact("DATA/sensor readings.txt", 200),
];

/// As above but `BOOT.BIN` lost the tail of its chain: one cluster.
const BOOT_TRUNCATED_2: &[Survivor] = &[
    Survivor::truncated("BOOT.BIN", 1024),
    Survivor::intact("LOG.TXT", 512),
    Survivor::intact("EMPTY.TXT", 0),
    Survivor::intact("DATA/READINGS.CSV", 1000),
    Survivor::intact("DATA/sensor readings.txt", 200),
];

/// As above but `BOOT.BIN` lost two of its three clusters.
const BOOT_TRUNCATED_1: &[Survivor] = &[
    Survivor::truncated("BOOT.BIN", 512),
    Survivor::intact("LOG.TXT", 512),
    Survivor::intact("EMPTY.TXT", 0),
    Survivor::intact("DATA/READINGS.CSV", 1000),
    Survivor::intact("DATA/sensor readings.txt", 200),
];

/// As above but the long-name file is reachable only by its short name.
const LONG_NAME_LOST: &[Survivor] = &[
    Survivor::intact("BOOT.BIN", 1536),
    Survivor::intact("LOG.TXT", 512),
    Survivor::intact("EMPTY.TXT", 0),
    Survivor::intact("DATA/READINGS.CSV", 1000),
    Survivor::renamed("DATA/SENSOR~1.TXT", 200, "DATA/sensor readings.txt"),
];

pub const CASES: &[CaseSpec] = &[
    // -- FAT: torn writes inside the allocation table ---------------------
    CaseSpec {
        name: "fat_mirror_stale",
        domain: Domain::Fat,
        scenario: "Power was lost after the chain for BOOT.BIN was written to FAT #1 but \
                   before the same update reached the FAT #2 mirror, so the two copies \
                   disagree about clusters that are genuinely in use.",
        repair: "Copy the disputed entries from FAT #1 over FAT #2. FAT #1 wins: it is the \
                 copy the driver reads, so it is the copy the rest of the volume is \
                 consistent with.",
        visibility: Visibility::Silent,
        corrupt: |img| {
            let root = img.geom().root_cluster;
            let first = img.entry_first_cluster(img.sfn(root, "BOOT.BIN"));
            for cluster in img.chain(first) {
                img.fat_set(1, cluster, FAT_FREE);
            }
        },
        expected: no_edit,
        dont_care: nothing,
        survivors: ALL_INTACT,
    },
    CaseSpec {
        name: "fat_chain_unterminated",
        domain: Domain::Fat,
        scenario: "A file was being extended. The driver linked the new cluster into the \
                   previous entry, then lost power before marking the new cluster as the \
                   end of the chain, so BOOT.BIN's last cluster still reads as free. \
                   Reads are unaffected — the file ends exactly at the end of that cluster, \
                   so nothing ever consults its FAT entry — but the allocator considers the \
                   cluster free and will hand it to the next file that needs one, at which \
                   point BOOT.BIN silently acquires someone else's data.",
        repair: "Stop the chain at the last cluster that is actually allocated and shrink \
                 the size to match, leaving the free cluster free. Adopting it instead \
                 would be a guess: the data write that was supposed to fill it may never \
                 have happened, and by the time the checker runs the allocator may already \
                 have given the cluster to somebody else.",
        visibility: Visibility::Silent,
        corrupt: |img| {
            let root = img.geom().root_cluster;
            let first = img.entry_first_cluster(img.sfn(root, "BOOT.BIN"));
            let last = *img.chain(first).last().unwrap();
            img.fat_set_all(last, FAT_FREE);
        },
        expected: |img| {
            let root = img.geom().root_cluster;
            let entry = img.sfn(root, "BOOT.BIN");
            let chain = img.chain(img.entry_first_cluster(entry));
            let (keep, drop) = chain.split_at(chain.len() - 1);
            img.fat_set_all(*keep.last().unwrap(), FAT_EOC);
            img.fat_set_all(drop[0], FAT_FREE);
            img.set_entry_size(entry, keep.len() as u32 * img.geom().bytes_per_cluster());
            img.set_fsinfo_free_count(img.count_free_clusters());
        },
        dont_care: fsinfo_is_a_hint,
        survivors: BOOT_TRUNCATED_2,
    },
    CaseSpec {
        name: "fat_entry_out_of_range",
        domain: Domain::Fat,
        scenario: "A half-written FAT sector left LOG.TXT's only cluster pointing at a \
                   cluster number past the end of the volume instead of at an \
                   end-of-chain marker. LOG.TXT is exactly one cluster long, so reading it \
                   never follows the bad link — but appending to it would.",
        repair: "The link cannot be followed and the file's size needs no further clusters, \
                 so terminate the chain. No cluster is leaked because the bogus link never \
                 named a real one.",
        visibility: Visibility::Silent,
        corrupt: |img| {
            let root = img.geom().root_cluster;
            let first = img.entry_first_cluster(img.sfn(root, "LOG.TXT"));
            let past_end = img.geom().max_valid_cluster() + 50;
            img.fat_set_all(first, past_end);
        },
        expected: no_edit,
        dont_care: nothing,
        survivors: ALL_INTACT,
    },
    CaseSpec {
        name: "fat_entry_reserved_cluster",
        domain: Domain::Fat,
        scenario: "As above, but the half-written entry landed on cluster number 1, which \
                   is reserved and can never appear in a chain. This one is not silent: the \
                   library rejects cluster numbers below 2 while walking a chain, so \
                   reading LOG.TXT fails outright with CorruptedFileSystem.",
        repair: "Terminate the chain. Worth having as its own case: 0 and 1 are the values \
                 a partially erased or partially written FAT sector produces most often, \
                 and 0 is indistinguishable from 'free'.",
        visibility: Visibility::Observable,
        corrupt: |img| {
            let root = img.geom().root_cluster;
            let first = img.entry_first_cluster(img.sfn(root, "LOG.TXT"));
            img.fat_set_all(first, 1);
        },
        expected: no_edit,
        dont_care: nothing,
        survivors: ALL_INTACT,
    },
    CaseSpec {
        name: "fat_lost_chain",
        domain: Domain::Fat,
        scenario: "A file was being created. Two clusters were allocated and chained, then \
                   power was lost before the directory entry naming them was written, so \
                   the clusters are marked in use but nothing references them.",
        repair: "Free both clusters. This is the one case that needs reachability, but it \
                 only needs a mark bit per cluster, not an ownership map: walk the \
                 directory tree marking chains, then free everything still unmarked.",
        visibility: Visibility::Silent,
        corrupt: |img| {
            let free = img.free_clusters(2, 2);
            img.fat_set_all(free[0], free[1]);
            img.fat_set_all(free[1], FAT_EOC);
        },
        expected: no_edit,
        dont_care: fsinfo_is_a_hint,
        survivors: ALL_INTACT,
    },
    CaseSpec {
        name: "fat_reserved_entries_clobbered",
        domain: Domain::Fat,
        scenario: "The first sector of FAT #1 was being rewritten when the power went. \
                   Entries 0 and 1 — the media descriptor and the end-of-chain prototype, \
                   which also carries the clean-shutdown and hard-error flags — came back \
                   as zeros. FAT #2 still has them.",
        repair: "Restore entry 0 to 0x0FFFFF00 | media byte (media comes from the BPB) and \
                 entry 1 to 0x0FFFFFFF. The intact mirror is the other valid source.",
        visibility: Visibility::Observable,
        corrupt: |img| {
            let off = img.geom().fat_offset(0);
            img.write(off, &[0; 8]);
        },
        expected: no_edit,
        dont_care: nothing,
        survivors: ALL_INTACT,
    },
    CaseSpec {
        name: "fat_tail_beyond_size",
        domain: Domain::Fat,
        scenario: "A cluster was allocated and linked onto LOG.TXT's chain, then power was \
                   lost before the larger file size reached the directory entry. The chain \
                   is one cluster longer than the size accounts for.",
        repair: "Size wins: terminate the chain at the last cluster the size covers and free \
                 the tail. The data in the extra cluster was never accounted for by any \
                 size, so there is nothing to preserve.",
        visibility: Visibility::Silent,
        corrupt: |img| {
            let root = img.geom().root_cluster;
            let first = img.entry_first_cluster(img.sfn(root, "LOG.TXT"));
            let last = *img.chain(first).last().unwrap();
            let extra = img.free_clusters(2, 1)[0];
            img.fat_set_all(last, extra);
            img.fat_set_all(extra, FAT_EOC);
        },
        expected: no_edit,
        dont_care: fsinfo_is_a_hint,
        survivors: ALL_INTACT,
    },
    CaseSpec {
        name: "fat_broken_midchain",
        domain: Domain::Fat,
        scenario: "A torn FAT sector write zeroed the entry for BOOT.BIN's middle cluster. \
                   That entry did two jobs — it marked the cluster allocated and it named \
                   the third cluster — so the chain now stops after one cluster while the \
                   size still claims three, and the third cluster is stranded: allocated, \
                   but reachable from nothing.",
        repair: "Terminate the chain at the first cluster and shrink the size to 512, then \
                 free the stranded third cluster. Same rule as `fat_chain_unterminated`, \
                 but here it also leaves work for the orphan sweep, which is why the two \
                 are separate cases.",
        visibility: Visibility::Observable,
        corrupt: |img| {
            let root = img.geom().root_cluster;
            let first = img.entry_first_cluster(img.sfn(root, "BOOT.BIN"));
            let second = img.chain(first)[1];
            img.fat_set_all(second, FAT_FREE);
        },
        expected: |img| {
            let root = img.geom().root_cluster;
            let entry = img.sfn(root, "BOOT.BIN");
            let chain = img.chain(img.entry_first_cluster(entry));
            for &cluster in &chain[1..] {
                img.fat_set_all(cluster, FAT_FREE);
            }
            img.fat_set_all(chain[0], FAT_EOC);
            img.set_entry_size(entry, img.geom().bytes_per_cluster());
            // Two clusters came back to the pool; `dont_care` lets a checker
            // that does not maintain FSInfo off the hook, but the reference
            // image should still be self-consistent.
            img.set_fsinfo_free_count(img.count_free_clusters());
        },
        dont_care: fsinfo_is_a_hint,
        survivors: BOOT_TRUNCATED_1,
    },
    // -- directory entries -------------------------------------------------
    CaseSpec {
        name: "dir_torn_entry",
        domain: Domain::Directory,
        scenario: "A file was being created in the root directory. The first twelve bytes \
                   of its entry — the short name and the attribute byte — reached the card; \
                   the rest of the entry is still the erase pattern, so the entry claims a \
                   4 GiB file starting at cluster 0x0FFFFFFF.",
        repair: "Mark the entry deleted (0xE5 over the first name byte, nothing else \
                 touched). The file never existed as far as any application is concerned, \
                 and its first cluster is not a real cluster, so nothing needs freeing. \
                 `fsck.vfat` instead renames the entry to FSCK0000.000 and truncates it to \
                 zero bytes, which is the right call on a desktop with a human to inspect \
                 the result and the wrong one on a device that will never be looked at.",
        visibility: Visibility::Observable,
        corrupt: |img| {
            let root = img.geom().root_cluster;
            let slot = img.first_unused_slot(root);
            let mut entry = [0xFF_u8; 32];
            entry[..11].copy_from_slice(&pad_sfn("NEWDATA.BIN"));
            entry[dirent::ATTRS] = dirent::ATTR_ARCHIVE;
            img.write(slot, &entry);
        },
        expected: |img| {
            let root = img.geom().root_cluster;
            let slot = img.first_unused_slot(root);
            let mut entry = [0xFF_u8; 32];
            entry[..11].copy_from_slice(&pad_sfn("NEWDATA.BIN"));
            entry[dirent::ATTRS] = dirent::ATTR_ARCHIVE;
            img.write(slot, &entry);
            img.delete_entry(slot);
        },
        dont_care: nothing,
        survivors: ALL_INTACT,
    },
    CaseSpec {
        name: "dir_size_without_cluster",
        domain: Domain::Directory,
        scenario: "EMPTY.TXT was being written for the first time. The new size reached its \
                   directory entry but the first-cluster field, written by a separate \
                   update, did not — so the entry claims 512 bytes starting at cluster 0.",
        repair: "A file with no first cluster has no data, whatever the size field says: set \
                 the size to zero. The inverse ordering (cluster set, size still zero) is \
                 handled by `fat_lost_chain`.",
        visibility: Visibility::Observable,
        corrupt: |img| {
            let root = img.geom().root_cluster;
            let entry = img.sfn(root, "EMPTY.TXT");
            img.set_entry_size(entry, 512);
        },
        expected: no_edit,
        dont_care: nothing,
        survivors: ALL_INTACT,
    },
    CaseSpec {
        name: "dir_orphan_lfn",
        domain: Domain::Directory,
        scenario: "A long-named file was being created in DATA. Long-name entries are \
                   written before the short-name entry they describe, and power was lost in \
                   between, leaving two long-name entries whose checksum matches no short \
                   name in the directory.",
        repair: "Mark the orphaned long-name entries deleted. They are the tail of an \
                 operation that never completed, and a checker can spot them locally: a \
                 long-name run must be immediately followed by a short-name entry whose \
                 checksum matches.",
        visibility: Visibility::Silent,
        corrupt: |img| {
            let data = img.dir_cluster("DATA");
            let slot = img.first_unused_slot(data);
            write_orphan_lfn(img, slot);
        },
        expected: |img| {
            let data = img.dir_cluster("DATA");
            let slot = img.first_unused_slot(data);
            write_orphan_lfn(img, slot);
            img.delete_entry(slot);
            img.delete_entry(slot + 32);
        },
        dont_care: nothing,
        survivors: ALL_INTACT,
    },
    CaseSpec {
        name: "dir_lfn_checksum_mismatch",
        domain: Domain::Directory,
        scenario: "A rename rewrote the short-name entry of DATA/sensor readings.txt and \
                   lost power before rewriting the long-name entries in front of it, so \
                   their checksum no longer matches the short name they belong to.",
        repair: "Delete the long-name entries and keep the short-name entry. The file's data \
                 is intact and stays reachable as SENSOR~1.TXT; the long name is the only \
                 thing that cannot be trusted, and inventing one would be worse than \
                 dropping it. `fsck.vfat` reports this one and declines to fix it, so the \
                 choice is the corpus's rather than the industry's — but leaving it means \
                 leaving a directory a driver may parse either way.",
        visibility: Visibility::Observable,
        corrupt: |img| {
            let data = img.dir_cluster("DATA");
            let sfn = img.sfn(data, LONG_NAME_SFN);
            for off in img.lfn_run(data, sfn) {
                let checksum = img.read(off + dirent::LFN_CHECKSUM as u64, 1)[0];
                img.write(off + dirent::LFN_CHECKSUM as u64, &[checksum.wrapping_add(1)]);
            }
        },
        expected: |img| {
            // Built from the corrupt state rather than the pristine one: the
            // bytes inside a deleted entry are whatever they were, and a
            // checker has no reason to tidy them.
            let data = img.dir_cluster("DATA");
            let sfn = img.sfn(data, LONG_NAME_SFN);
            for off in img.lfn_run(data, sfn) {
                let checksum = img.read(off + dirent::LFN_CHECKSUM as u64, 1)[0];
                img.write(off + dirent::LFN_CHECKSUM as u64, &[checksum.wrapping_add(1)]);
                img.delete_entry(off);
            }
        },
        dont_care: nothing,
        survivors: LONG_NAME_LOST,
    },
    CaseSpec {
        name: "dir_dotdot_wrong_cluster",
        domain: Domain::Directory,
        scenario: "The `..` entry of DATA/SUB points at the root directory instead of at \
                   DATA — the state left behind when power is lost between writing a new \
                   subdirectory's cluster and fixing up its parent link.",
        repair: "Set `..` to the first cluster of the directory the entry was found in \
                 (0 when that is the root). The parent is known for free during the tree \
                 walk, so this costs nothing to check.",
        visibility: Visibility::Observable,
        corrupt: |img| {
            let sub = img.dir_cluster("DATA/SUB");
            let dotdot = img.sfn(sub, "..");
            img.set_entry_first_cluster(dotdot, 0);
        },
        expected: no_edit,
        dont_care: nothing,
        survivors: ALL_INTACT,
    },
    // -- FSInfo ------------------------------------------------------------
    CaseSpec {
        name: "fsinfo_stale",
        domain: Domain::FsInfo,
        scenario: "FSInfo is a cache that drivers update lazily, so an unclean shutdown \
                   almost always leaves its free-cluster count wrong. Here it over-reports \
                   free space, which is the direction that lets a full volume accept writes \
                   it cannot honour.",
        repair: "Recount free clusters while sweeping the FAT — which a checker is already \
                 doing — and write the result. The next-free hint may be set to anything \
                 valid, including 2, so the corpus does not constrain it.",
        visibility: Visibility::Silent,
        corrupt: |img| {
            let wrong = img.fsinfo_free_count() + 500;
            img.set_fsinfo_free_count(wrong);
            img.set_fsinfo_next_free(2);
        },
        expected: no_edit,
        dont_care: |img| {
            let base = img.geom().fs_info_offset() + fsinfo::NEXT_FREE as u64;
            vec![base..base + 4]
        },
        survivors: ALL_INTACT,
    },
];

/// Two long-name entries for `power was cut here.txt` with a checksum that
/// belongs to no short-name entry in the directory.
fn write_orphan_lfn(img: &mut Fat32Image, slot: u64) {
    const NAME: &str = "power was cut here.txt";
    let checksum = lfn_checksum(&pad_sfn("POWERW~1.TXT"));
    let utf16: Vec<u16> = NAME.encode_utf16().chain(core::iter::once(0)).collect();

    // Long-name entries are stored last-fragment-first, so the entry carrying
    // characters 13.. comes first and is flagged as the last of the run.
    for (i, order) in [(1_usize, 0x42_u8), (0, 0x01)] {
        let mut entry = [0xFF_u8; 32];
        entry[dirent::LFN_ORDER] = order;
        entry[dirent::ATTRS] = dirent::ATTR_LFN;
        entry[12] = 0;
        entry[dirent::LFN_CHECKSUM] = checksum;
        entry[26] = 0;
        entry[27] = 0;
        for (j, dst) in [(0_usize, 1_usize), (1, 3), (2, 5), (3, 7), (4, 9)]
            .into_iter()
            .chain([(5, 14), (6, 16), (7, 18), (8, 20), (9, 22), (10, 24)])
            .chain([(11, 28), (12, 30)])
        {
            let ch = utf16.get(i * 13 + j).copied().unwrap_or(0xFFFF);
            entry[dst..dst + 2].copy_from_slice(&ch.to_le_bytes());
        }
        img.write(slot + (1 - i as u64) * 32, &entry);
    }
}
