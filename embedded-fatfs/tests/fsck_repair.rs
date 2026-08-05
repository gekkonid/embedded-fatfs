//! `FileSystem::repair` against the corruption corpus in `tests/corpus/`.
//!
//! The contract is strict: for every case, repairing the corrupt image must
//! produce the corpus's `fixed` image byte for byte, outside the ranges the
//! case declares it does not care about.

mod common;
mod corpus;

use corpus::Fat32Image;
use corpus::pristine::MemDisk;
use embedded_fatfs::{
    scratch_size_for_depth, Error, FsOptions, LossyOemCpConverter, NullTimeProvider, RepairMode,
    RepairScratch, RepairStats,
};

/// The corpus tree is three levels deep; give repair room for more so the
/// depth-limit behaviour is tested on purpose rather than by accident.
const DEPTH: usize = 8;

type FileSystem = common::TestFs;

async fn mount(img: &Fat32Image) -> (FileSystem, std::rc::Rc<std::cell::RefCell<Vec<u8>>>) {
    // So that `RUST_LOG=debug cargo test ... -- --nocapture` shows the progress
    // and per-repair lines. Idempotent, so every test may call it.
    let _ = env_logger::builder().is_test(true).try_init();
    common::mount_or_panic(img).await
}

/// Mount `img`, repair it with a `depth`-level scratch buffer, unmount, and
/// return the resulting image alongside the stats.
async fn repair_with(img: &Fat32Image, mode: RepairMode, depth: usize) -> (Fat32Image, RepairStats) {
    let (fs, buffer) = mount(img).await;
    let mut buf = vec![0_u8; scratch_size_for_depth(depth)];
    let stats = fs
        .repair(mode, &mut RepairScratch::new(&mut buf))
        .await
        .expect("repair failed");
    fs.unmount().await.expect("unmount");
    let data = buffer.borrow().clone();
    (Fat32Image::parse(data), stats)
}

async fn repair(img: &Fat32Image) -> (Fat32Image, RepairStats) {
    repair_with(img, RepairMode::All, DEPTH).await
}

#[tokio::test]
async fn repairing_a_healthy_volume_changes_nothing() {
    let pristine = corpus::pristine::build().await;
    let (after, stats) = repair(&pristine).await;

    corpus::assert_images_eq(&after, &pristine, &[]);
    // The corpus has 3 dirs and 7 files; assert at least those were examined.
    assert!(
        stats.dirs_scanned >= 3,
        "dirs_scanned {} < 3", stats.dirs_scanned
    );
    assert!(
        stats.files_examined >= 7,
        "files_examined {} < 7", stats.files_examined
    );
    assert_eq!(
        stats.modes_applied, RepairMode::All,
        "a clean volume should be walked and left alone"
    );
}

#[tokio::test]
async fn every_case_repairs_to_the_expected_image() {
    let pristine = corpus::pristine::build().await;

    for spec in corpus::CASES {
        let case = corpus::build_case(&pristine, spec);
        let (after, stats) = repair(&case.corrupt).await;

        let remaining = corpus::verify::check(&after);
        assert!(
            remaining.is_empty(),
            "case {:?}: repair left the volume inconsistent: {:#?}\nstats: {:?}",
            spec.name,
            remaining,
            stats
        );

        let ignore = (spec.dont_care)(&case.fixed);
        let diffs = corpus::diff_ranges(after.as_bytes(), case.fixed.as_bytes(), &ignore);
        assert!(
            diffs.is_empty(),
            "case {:?}: repair did not produce the expected image ({:?})",
            spec.name,
            stats
        );
        corpus::assert_images_eq(&after, &case.fixed, &ignore);
    }
}

/// Repair must be safe to run again — including on a volume it has already
/// repaired, and on one where a previous run was cut short.
#[tokio::test]
async fn repair_is_idempotent() {
    let pristine = corpus::pristine::build().await;

    for spec in corpus::CASES {
        let case = corpus::build_case(&pristine, spec);
        let (once, _) = repair(&case.corrupt).await;
        let (twice, stats) = repair(&once).await;

        corpus::assert_images_eq(&twice, &once, &[]);
        assert_eq!(
            stats.clusters_freed, 0,
            "case {:?}: second pass freed clusters",
            spec.name
        );
        assert_eq!(
            stats.entries_removed, 0,
            "case {:?}: second pass removed entries",
            spec.name
        );
    }
}

/// The marker bit lives in the FAT, so a repair interrupted part way through
/// leaves markers behind. The next run must clear them rather than mistake them
/// for a claim on a cluster.
#[tokio::test]
async fn leftover_markers_from_an_interrupted_run_are_cleared() {
    let pristine = corpus::pristine::build().await;

    // Simulate an interrupted repair: set the marker bit on every entry,
    // including free ones and ones that are genuinely in use.
    let mut interrupted = pristine.clone();
    for cluster in 2..=interrupted.geom().max_valid_cluster() {
        let off = interrupted.geom().fat_entry_offset(0, cluster);
        let raw = interrupted.read_u32(off);
        interrupted.write_u32(off, raw | 0x1000_0000);
    }

    let (after, _) = repair(&interrupted).await;
    assert!(
        corpus::verify::check(&after).is_empty(),
        "leftover markers were not cleaned up"
    );
    corpus::assert_images_eq(&after, &pristine, &[]);
}

/// Running out of scratch depth must cost coverage, never data.
#[tokio::test]
async fn a_shallow_scratch_buffer_skips_subtrees_without_losing_them() {
    let pristine = corpus::pristine::build().await;

    // Depth 1 reaches the root only, so DATA (and everything under it) is
    // skipped. Nothing under DATA may be freed as a result.
    let (after, stats) = repair_with(&pristine, RepairMode::All, 1).await;
    assert_eq!(stats.dirs_scanned, 1);
    assert_eq!(stats.dirs_skipped, 1, "DATA should have been skipped");
    assert_eq!(stats.clusters_freed, 0, "a skipped subtree must not be freed");
    corpus::assert_images_eq(&after, &pristine, &[]);

    // Depth 2 reaches DATA but not DATA/SUB.
    let (after, stats) = repair_with(&pristine, RepairMode::All, 2).await;
    assert_eq!(stats.dirs_scanned, 2);
    assert_eq!(stats.dirs_skipped, 1, "DATA/SUB should have been skipped");
    assert_eq!(stats.clusters_freed, 0);
    corpus::assert_images_eq(&after, &pristine, &[]);
}

#[tokio::test]
async fn a_scratch_buffer_with_no_room_is_rejected() {
    let pristine = corpus::pristine::build().await;
    let (fs, _buffer) = mount(&pristine).await;

    let mut buf = [0_u8; 0];
    assert!(matches!(
        fs.repair(RepairMode::All, &mut RepairScratch::new(&mut buf)).await,
        Err(Error::InvalidInput)
    ));

    // One byte short of a frame is still no room.
    let mut buf = vec![0_u8; scratch_size_for_depth(1) - 1];
    assert_eq!(RepairScratch::new(&mut buf).max_depth(), 0);
}

#[tokio::test]
async fn repair_declines_non_fat32_volumes() {
    let _ = env_logger::builder().is_test(true).try_init();
    let img = tokio::fs::read("resources/fat16.img").await.expect("fat16.img");
    let disk = MemDisk::from_bytes(img);
    let options = FsOptions::new()
        .time_provider(NullTimeProvider::new())
        .oem_cp_converter(LossyOemCpConverter::new());
    let fs = embedded_fatfs::FileSystem::new(disk, options).await.expect("mount");

    let mut buf = [0_u8; scratch_size_for_depth(4)];
    assert!(matches!(
        fs.repair(RepairMode::All, &mut RepairScratch::new(&mut buf)).await,
        Err(Error::InvalidInput)
    ));
}

// -- modes -------------------------------------------------------------------

/// A dry run must report without touching anything. This is what a device runs
/// at boot to decide whether spending flash writes is warranted at all.
#[tokio::test]
async fn a_dry_run_changes_nothing() {
    let pristine = corpus::pristine::build().await;

    for spec in corpus::CASES {
        let case = corpus::build_case(&pristine, spec);
        let (after, stats) = repair_with(&case.corrupt, RepairMode::All | RepairMode::DryRun, DEPTH).await;

        corpus::assert_images_eq(&after, &case.corrupt, &[]);
        assert!(
            !stats.modes_applied.contains(RepairMode::OrphanClusters),
            "case {:?}: orphan detection cannot run without writing markers",
            spec.name
        );
        assert_eq!(
            stats.clusters_freed, 0,
            "case {:?}: a dry run freed clusters",
            spec.name
        );
    }
}

/// The counters have to mean something in a dry run, or there is no way to tell
/// whether a repair is needed. Every case that is not purely about orphans must
/// register as *something*.
#[tokio::test]
async fn a_dry_run_still_reports_what_it_found() {
    let pristine = corpus::pristine::build().await;

    // Orphaned clusters are the one class a dry run genuinely cannot see: the
    // reachability marker is a write.
    let invisible = ["fat_lost_chain", "fat_tail_beyond_size"];

    for spec in corpus::CASES.iter().filter(|s| !invisible.contains(&s.name)) {
        let case = corpus::build_case(&pristine, spec);
        let (_, stats) = repair_with(&case.corrupt, RepairMode::All | RepairMode::DryRun, DEPTH).await;

        let found = stats.chains_truncated
            + stats.sizes_corrected
            + stats.entries_removed
            + stats.entries_recovered
            + stats.dot_entries_fixed
            + stats.reserved_entries_restored
            + stats.mirror_entries_synced
            + stats.fsinfo_corrected;
        assert!(found > 0, "case {:?}: a dry run reported nothing wrong", spec.name);
    }

    // ... and a healthy volume must report nothing, or the check is useless.
    let (_, stats) = repair_with(&pristine, RepairMode::All | RepairMode::DryRun, DEPTH).await;
    let found = stats.chains_truncated
        + stats.sizes_corrected
        + stats.entries_removed
        + stats.entries_recovered
        + stats.dot_entries_fixed
        + stats.reserved_entries_restored
        + stats.mirror_entries_synced
        + stats.fsinfo_corrected;
    assert_eq!(found, 0, "dry run on healthy volume reported {found} problems");
    assert!(
        stats.dirs_scanned >= 3 && stats.files_examined >= 7,
        "dry run should have scanned the corpus tree"
    );
}

/// `Minimal` skips reclamation, so leaked clusters survive — but nothing else
/// may be left behind, and no file may lose data that `All` would have kept.
#[tokio::test]
async fn minimal_mode_fixes_everything_except_leaks() {
    use corpus::verify::Problem;
    let pristine = corpus::pristine::build().await;

    for spec in corpus::CASES {
        let case = corpus::build_case(&pristine, spec);
        let (minimal, stats) = repair_with(&case.corrupt, RepairMode::Minimal, DEPTH).await;
        let (full, _) = repair(&case.corrupt).await;

        assert_eq!(
            stats.clusters_freed, 0,
            "case {:?}: Minimal must not free clusters",
            spec.name
        );

        // The only complaints allowed are about clusters nothing points at.
        let remaining = corpus::verify::check(&minimal);
        let unexpected: Vec<_> = remaining
            .iter()
            .filter(|p| !matches!(p, Problem::LostCluster { .. } | Problem::FsInfoFreeCount { .. }))
            .collect();
        assert!(
            unexpected.is_empty(),
            "case {:?}: Minimal left something other than leaked clusters: {:#?}",
            spec.name,
            unexpected
        );

        // Whatever `All` chose to keep, `Minimal` must have kept too: the two
        // differ in what they reclaim, never in what they preserve.
        for cluster in 2..=minimal.geom().max_valid_cluster() {
            if full.fat_get(0, cluster) != 0 {
                assert_eq!(
                    minimal.fat_get(0, cluster),
                    full.fat_get(0, cluster),
                    "case {:?}: Minimal and All disagree about cluster {}",
                    spec.name,
                    cluster
                );
            }
        }
    }
}

/// Markers left by an interrupted `All` run must not mislead a later `Minimal`
/// run, which does not clear them. This is the safety argument for tying the
/// clear pass to `OrphanClusters` rather than running it always.
#[tokio::test]
async fn minimal_mode_is_unmoved_by_stale_markers() {
    let pristine = corpus::pristine::build().await;

    // A real interrupted run writes markers through the mirroring FAT accessor,
    // so they land in every copy. Setting them in one copy only would leave a
    // divergence for the mirror pass to fix, which is a different test.
    let mut interrupted = pristine.clone();
    for cluster in 2..=interrupted.geom().max_valid_cluster() {
        for fat in 0..interrupted.geom().num_fats {
            let off = interrupted.geom().fat_entry_offset(fat, cluster);
            let raw = interrupted.read_u32(off);
            interrupted.write_u32(off, raw | 0x1000_0000);
        }
    }

    let (after, stats) = repair_with(&interrupted, RepairMode::Minimal, DEPTH).await;
    assert_eq!(
        stats.chains_truncated, 0,
        "stale markers made Minimal cut healthy chains short"
    );
    assert_eq!(stats.sizes_corrected, 0, "stale markers made Minimal shrink a file");
    corpus::assert_images_eq(&after, &interrupted, &[]);
}

/// A FAT chain that loops back on itself must not be followed forever. Without
/// the hop limit this hangs, and a device that hangs here watchdogs, reboots,
/// and hangs again.
#[tokio::test]
async fn a_cyclic_chain_does_not_hang() {
    use std::time::Duration;
    let pristine = corpus::pristine::build().await;

    // Point the last cluster of a multi-cluster file back at its first.
    let mut looped = pristine.clone();
    let root = looped.geom().root_cluster;
    let first = looped.entry_first_cluster(looped.sfn(root, "BOOT.BIN"));
    let last = *looped.chain(first).last().unwrap();
    looped.fat_set_all(last, first);

    for mode in [RepairMode::Minimal, RepairMode::All] {
        let img = looped.clone();
        let result = tokio::time::timeout(Duration::from_secs(60), repair_with(&img, mode, DEPTH)).await;
        let (after, stats) = result.unwrap_or_else(|_| panic!("{:?} did not terminate on a cyclic chain", mode));
        assert!(stats.chains_truncated > 0, "{:?} did not report cutting the loop", mode);
        // The loop must actually be gone, not merely survived.
        assert!(
            corpus::verify::check(&after)
                .iter()
                .all(|p| !matches!(p, corpus::verify::Problem::CrossLinked { .. })),
            "{:?} left the loop in place",
            mode
        );
    }
}

/// Each flag must repair its own class of damage and leave the others alone.
#[tokio::test]
async fn flags_are_independent() {
    let pristine = corpus::pristine::build().await;

    // (case, the one flag that should fix it)
    let cases = [
        ("fat_reserved_entries_clobbered", RepairMode::FatSignatures),
        ("fat_chain_unterminated", RepairMode::Chains),
        ("dir_size_without_cluster", RepairMode::EntryValidity),
        ("dir_dotdot_wrong_cluster", RepairMode::EntryValidity),
        ("dir_orphan_lfn", RepairMode::LongNames),
        ("fat_lost_chain", RepairMode::OrphanClusters),
    ];

    for (name, flag) in cases {
        let spec = corpus::CASES.iter().find(|s| s.name == name).expect("unknown case");
        let case = corpus::build_case(&pristine, spec);

        // With the flag: fixed (bar the FSInfo count, which needs its own flag).
        let (fixed, _) = repair_with(&case.corrupt, flag | RepairMode::FsInfo, DEPTH).await;
        assert!(
            corpus::verify::check(&fixed).is_empty(),
            "{:?} alone did not repair {:?}: {:#?}",
            flag,
            name,
            corpus::verify::check(&fixed)
        );

        // Without it: untouched. `FsInfo` is excluded here too, since it would
        // rewrite the count on its own.
        let others = (RepairMode::All - flag) - RepairMode::FsInfo;
        let (untouched, _) = repair_with(&case.corrupt, others, DEPTH).await;
        corpus::assert_images_eq(&untouched, &case.corrupt, &[]);
    }
}

// -- containment -------------------------------------------------------------

/// Fill `clusters` with recognisable non-directory data and chain them
/// together, returning the first cluster.
///
/// The bytes are deliberately never zero, so nothing in them can be mistaken
/// for an end-of-directory marker — this is the shape of data (logs, text,
/// compressed payloads) that a runaway directory walk chews through furthest.
fn plant_file_data(img: &mut Fat32Image, first: u32, count: u32) -> u32 {
    let mut state = 0x1234_5678_u32;
    for cluster in first..first + count {
        let next = if cluster + 1 == first + count {
            0x0FFF_FFFF
        } else {
            cluster + 1
        };
        img.fat_set_all(cluster, next);
        let offset = img.geom().cluster_offset(cluster);
        let bytes: Vec<u8> = (0..img.geom().bytes_per_cluster())
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((state >> 24) as u8).max(1)
            })
            .collect();
        img.write(offset, &bytes);
    }
    first
}

/// Bytes of every cluster in `first..first + count`, for before/after
/// comparison.
fn cluster_bytes(img: &Fat32Image, first: u32, count: u32) -> Vec<u8> {
    (first..first + count)
        .flat_map(|c| {
            let offset = img.geom().cluster_offset(c);
            img.read(offset, img.geom().bytes_per_cluster() as usize).to_vec()
        })
        .collect()
}

/// A directory entry whose first cluster has been corrupted into pointing at
/// file data must not send the walk into that data.
///
/// This is the failure that destroyed a test card: every 32 bytes of the file
/// looked like a corrupt directory entry, so repair renamed and deleted its way
/// through the lot. A directory is identified by its `.` and `..` entries, and
/// data that has none is not one.
#[tokio::test]
async fn an_entry_pointing_at_file_data_is_not_walked() {
    let pristine = corpus::pristine::build().await;
    let mut img = pristine.clone();

    let planted_at = 20_000;
    let planted = 300;
    plant_file_data(&mut img, planted_at, planted);
    let data = img.dir_cluster("DATA");
    let sub = img.sfn(data, "SUB");
    img.set_entry_first_cluster(sub, planted_at);
    let before = cluster_bytes(&img, planted_at, planted);

    for mode in [RepairMode::Minimal, RepairMode::All] {
        let (after, stats) = repair_with(&img, mode, DEPTH).await;

        assert_eq!(
            cluster_bytes(&after, planted_at, planted),
            before,
            "{:?} rewrote data that was never a directory",
            mode
        );
        assert_eq!(stats.dirs_rejected, 1, "{:?} did not report rejecting it", mode);
        assert_eq!(
            stats.entries_recovered, 0,
            "{:?} renamed entries inside something that is not a directory",
            mode
        );
        assert_eq!(
            stats.entries_removed, 0,
            "{:?} deleted entries inside something that is not a directory",
            mode
        );
        // The clusters are still claimed by the (bogus) entry, so even a full
        // run must leave them allocated: they may be a real file whose
        // attribute byte took a bit flip. (SUB's own former cluster does get
        // reclaimed under `All`, correctly — repointing the entry orphaned it.)
        for cluster in planted_at..planted_at + planted {
            assert_ne!(
                after.fat_get(0, cluster),
                0,
                "{:?} freed cluster {} of data it declined to walk",
                mode,
                cluster
            );
        }
    }
}

/// The cap is on the chain, not on the entry that named it: a chain that a
/// directory entry points at, but which is longer than any directory can be,
/// must not be followed to its end.
#[tokio::test]
async fn a_directory_chain_is_capped_at_what_a_directory_can_hold() {
    let pristine = corpus::pristine::build().await;
    let mut img = pristine.clone();

    // 512-byte clusters, so a 2 MiB directory is 4096 clusters. Chain well past
    // that, starting from the real DATA/SUB so the `.`/`..` gate is satisfied
    // and the length cap is what has to stop us.
    let sub = img.dir_cluster("DATA/SUB");
    let tail_at = 30_000;
    let tail = 5_000;
    plant_file_data(&mut img, tail_at, tail);
    img.fat_set_all(sub, tail_at);

    let max_dir_clusters = 65_536 * 32 / img.geom().bytes_per_cluster();
    let (after, _) = repair_with(&img, RepairMode::Minimal, DEPTH).await;

    let walked = after.chain(sub).len() as u32;
    assert!(
        walked <= max_dir_clusters,
        "followed {} clusters of directory, cap is {}",
        walked,
        max_dir_clusters
    );
}

/// The general property, checked across the whole corpus: repair must only ever
/// write inside the reserved area, the FATs, or a cluster that belongs to a
/// directory. File data is never its business.
#[tokio::test]
async fn repair_never_writes_to_file_data() {
    let pristine = corpus::pristine::build().await;

    // Clusters holding the contents of the corpus's files, as opposed to its
    // directories.
    let mut file_clusters: Vec<u32> = Vec::new();
    for &(path, _) in corpus::pristine::FILES {
        let (dir, name) = match path.rsplit_once('/') {
            Some((dir, name)) => (dir, name),
            None => ("", path),
        };
        let dir_cluster = pristine.dir_cluster(dir);
        // Long-named files are found under the short name the library derived.
        let sfn = if name.contains(' ') {
            corpus::cases::LONG_NAME_SFN
        } else {
            name
        };
        if let Some(entry) = pristine.find_sfn(dir_cluster, sfn) {
            let first = pristine.entry_first_cluster(entry);
            if first >= 2 {
                file_clusters.extend(pristine.chain(first));
            }
        }
    }
    assert!(file_clusters.len() >= 5, "expected the corpus to have file data");

    for spec in corpus::CASES {
        let case = corpus::build_case(&pristine, spec);
        for mode in [RepairMode::Minimal, RepairMode::All] {
            let (after, _) = repair_with(&case.corrupt, mode, DEPTH).await;
            for &cluster in &file_clusters {
                let offset = after.geom().cluster_offset(cluster);
                let len = after.geom().bytes_per_cluster() as usize;
                assert_eq!(
                    after.read(offset, len),
                    case.corrupt.read(offset, len),
                    "case {:?} under {:?} rewrote file data in cluster {}",
                    spec.name,
                    mode,
                    cluster
                );
            }
        }
    }
}

// -- the dirty flag ----------------------------------------------------------

/// Mark a volume unclean in *both* the places FAT records it, as an interrupted
/// writer would: the boot sector flag and the clean-shutdown bit in FAT entry 1.
fn mark_dirty(img: &mut Fat32Image) {
    // Boot sector: `reserved_1` at 0x41 on FAT32, bit 0 is "dirty", bit 1 is
    // "hard error".
    img.write(0x41, &[0b01]);
    // FAT entry 1: the flag is inverted, so clearing bit 27 says "unclean".
    let off = img.geom().fat_entry_offset(0, 1);
    let raw = img.read_u32(off);
    img.write_u32(off, raw & !(1 << 27));
}

async fn status_of(img: &Fat32Image) -> embedded_fatfs::FsStatusFlags {
    let (fs, _buffer) = mount(img).await;
    fs.read_status_flags().await.expect("status")
}

/// A repair that finished must leave the volume clean, or a device that repairs
/// whenever it mounts dirty repairs on every boot until the card wears out.
#[tokio::test]
async fn a_completed_repair_clears_the_dirty_flag() {
    let pristine = corpus::pristine::build().await;

    for mode in [RepairMode::Minimal, RepairMode::All] {
        let mut dirty = pristine.clone();
        mark_dirty(&mut dirty);
        assert!(status_of(&dirty).await.dirty(), "the fixture is not dirty");

        let (after, _) = repair_with(&dirty, mode, DEPTH).await;
        assert!(
            !status_of(&after).await.dirty(),
            "{:?} left the volume dirty, so the next boot will repair it again",
            mode
        );
    }

    // The same must hold when there was real damage to fix, not just a flag.
    for spec in corpus::CASES {
        let mut case = corpus::build_case(&pristine, spec).corrupt;
        mark_dirty(&mut case);
        let (after, _) = repair_with(&case, RepairMode::Minimal, DEPTH).await;
        assert!(
            !status_of(&after).await.dirty(),
            "case {:?} left the volume dirty",
            spec.name
        );
    }
}

/// Both indicators have to be cleared: `read_status_flags` reports them ORed
/// together, so clearing one and not the other still reads as dirty.
#[tokio::test]
async fn both_dirty_indicators_are_cleared() {
    let pristine = corpus::pristine::build().await;
    let mut dirty = pristine.clone();
    mark_dirty(&mut dirty);

    let (after, _) = repair_with(&dirty, RepairMode::Minimal, DEPTH).await;

    assert_eq!(after.read(0x41, 1)[0] & 0b01, 0, "boot sector flag still set");
    let fat1 = after.read_u32(after.geom().fat_entry_offset(0, 1));
    assert_ne!(fat1 & (1 << 27), 0, "FAT[1] clean-shutdown bit still clear");
}

/// The hard-error bit says the *device* returned an error once. A filesystem
/// check neither verifies nor refutes that, so it must survive.
#[tokio::test]
async fn the_hard_error_flag_is_left_alone() {
    let pristine = corpus::pristine::build().await;
    let mut dirty = pristine.clone();
    mark_dirty(&mut dirty);
    // Set the hard-error bit in both places too.
    dirty.write(0x41, &[0b11]);
    let off = dirty.geom().fat_entry_offset(0, 1);
    let raw = dirty.read_u32(off);
    dirty.write_u32(off, raw & !(1 << 26));

    let (after, _) = repair_with(&dirty, RepairMode::Minimal, DEPTH).await;

    let status = status_of(&after).await;
    assert!(!status.dirty(), "dirty flag should have been cleared");
    assert!(status.io_error(), "hard-error flag should have been preserved");
}

/// A dry run establishes nothing about the volume, so it must not claim it is
/// clean.
#[tokio::test]
async fn a_dry_run_leaves_the_dirty_flag_set() {
    let pristine = corpus::pristine::build().await;
    let mut dirty = pristine.clone();
    mark_dirty(&mut dirty);

    let (after, _) = repair_with(&dirty, RepairMode::All | RepairMode::DryRun, DEPTH).await;
    corpus::assert_images_eq(&after, &dirty, &[]);
    assert!(status_of(&after).await.dirty(), "a dry run marked the volume clean");
}

/// A run that could not see the whole tree, or that was not allowed to repair
/// every class of damage, has not earned a clean flag.
#[tokio::test]
async fn an_incomplete_repair_leaves_the_dirty_flag_set() {
    let pristine = corpus::pristine::build().await;
    let mut dirty = pristine.clone();
    mark_dirty(&mut dirty);

    // Out of scratch depth: part of the tree was never looked at.
    let (after, stats) = repair_with(&dirty, RepairMode::All, 1).await;
    assert!(stats.dirs_skipped > 0, "expected the fixture to run out of depth");
    assert!(
        status_of(&after).await.dirty(),
        "a run that skipped subtrees marked the volume clean"
    );
    // Checked at the byte as well, because `read_status_flags` ORs the two
    // indicators: FAT[1] staying dirty would mask the boot-sector flag being
    // cleared behind repair's back — by the `unmount` that follows it, which
    // takes a recomputed free-cluster count as licence to declare the volume
    // sound.
    assert_eq!(
        after.read(0x41, 1)[0] & 0b01,
        0b01,
        "the boot-sector dirty flag was cleared by a run that did not earn it"
    );

    // A hand-picked subset can leave damage of the classes it was not asked to
    // repair, so it does not get to declare the volume sound either.
    let (after, _) = repair_with(&dirty, RepairMode::Chains, DEPTH).await;
    assert!(
        status_of(&after).await.dirty(),
        "a partial mode marked the volume clean"
    );
}
