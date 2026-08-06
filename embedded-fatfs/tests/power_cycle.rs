//! End-to-end: filling a volume the way a device that keeps losing power does.
//!
//! Each round formats nothing and repairs everything: write a file, drop the
//! filesystem without unmounting so the volume is left dirty exactly as a power
//! cut would leave it, then mount, check it really is dirty, repair, and check
//! the bookkeeping the next round depends on. Repeat until the card is full.
//!
//! What this is looking for is drift: a free-cluster count that wanders away
//! from the truth, an allocation hint that stops being useful, space that leaks
//! a little on every power cut until the volume is full of nothing. None of
//! that shows up in a single-shot test.

mod corpus;

use corpus::image::Geometry;
use corpus::pristine::MemDisk;
use corpus::Fat32Image;
use embedded_fatfs::{
    format_volume, FatType, FormatVolumeOptions, FsOptions, LossyOemCpConverter, NullTimeProvider, RepairMode,
    RepairScratch, RepairStats,
};
use embedded_io_async::{Read, Write};

/// 1 GiB.
const VOLUME_BYTES: u64 = 1024 * 1024 * 1024;
/// 8 KiB clusters, which puts this volume at ~130k clusters.
///
/// The cluster size is what decides the FAT type, not the `fat_type` option:
/// formatting picks the first of FAT32/16/12 whose geometry fits, so asking for
/// FAT32 with clusters big enough to bring the count under 65525 quietly gives
/// you FAT16 instead.
const CLUSTER_BYTES: u32 = 8 * 1024;
/// Written in two halves, so each round has a flushed part and an unflushed one.
const FILE_BYTES: usize = 32 * 1024 * 1024;

const FREE_COUNT: usize = 488;
const NEXT_FREE: usize = 492;
const UNKNOWN: u32 = 0xFFFF_FFFF;

type FileSystem = embedded_fatfs::FileSystem<MemDisk, NullTimeProvider, LossyOemCpConverter>;

fn options() -> FsOptions<NullTimeProvider, LossyOemCpConverter> {
    FsOptions::new()
        .time_provider(NullTimeProvider::new())
        .oem_cp_converter(LossyOemCpConverter::new())
}

/// Deterministic filler, so a mix-up shows up as wrong data rather than zeros.
fn contents(round: usize, len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_u32 ^ (round as u32).wrapping_mul(2_654_435_761);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

// -- raw inspection, without cloning a gigabyte -------------------------------

fn read_u32(disk: &MemDisk, offset: u64) -> u32 {
    let buf = disk.buffer();
    let buf = buf.borrow();
    let at = offset as usize;
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

fn fsinfo(disk: &MemDisk, geom: &Geometry, field: usize) -> u32 {
    read_u32(disk, geom.fs_info_offset() + field as u64)
}

fn fat_entry(disk: &MemDisk, geom: &Geometry, cluster: u32) -> u32 {
    read_u32(disk, geom.fat_entry_offset(0, cluster)) & 0x0FFF_FFFF
}

/// Free clusters according to the FAT itself, which is the only authority.
fn count_free(disk: &MemDisk, geom: &Geometry) -> u32 {
    (2..=geom.max_valid_cluster())
        .filter(|&c| fat_entry(disk, geom, c) == 0)
        .count() as u32
}

// -- the cycle ----------------------------------------------------------------

/// Write one file and abandon the filesystem without unmounting it.
///
/// The write is split around a flush so that part of the file is durable and
/// part of it is only in the driver's hands when the lights go out — which is
/// what makes the round leave real work behind rather than a tidy volume with a
/// flag set.
async fn write_then_lose_power(
    disk: &MemDisk,
    round: usize,
) -> Result<(), embedded_fatfs::Error<embedded_io_async::ErrorKind>> {
    let fs = FileSystem::new(disk.clone(), options()).await?;
    let data = contents(round, FILE_BYTES);
    {
        let root = fs.root_dir();
        let mut file = root.create_file(&format!("LOG{:04}.BIN", round)).await?;
        file.truncate().await?;
        file.write_all(&data[..FILE_BYTES / 2]).await?;
        file.flush().await?;
        file.write_all(&data[FILE_BYTES / 2..]).await?;
        file.close().await?;
    }
    // No `unmount`, no `flush`: the volume keeps its dirty flag and whatever
    // FSInfo held before this round.
    core::mem::drop(fs);
    Ok(())
}

async fn repair(disk: &MemDisk, mode: RepairMode) -> RepairStats {
    let fs = FileSystem::new(disk.clone(), options()).await.expect("mount to repair");
    let mut buf = [0_u8; embedded_fatfs::scratch_size_for_depth(8)];
    let stats = fs
        .repair(mode, &mut RepairScratch::new(&mut buf))
        .await
        .expect("repair");
    fs.unmount().await.expect("unmount");
    stats
}

async fn is_dirty(disk: &MemDisk) -> bool {
    let fs = FileSystem::new(disk.clone(), options()).await.expect("mount");
    fs.read_status_flags().await.expect("status").dirty()
}

/// Every file written so far must still read back exactly.
async fn verify_all_files(disk: &MemDisk, rounds: usize) {
    let fs = FileSystem::new(disk.clone(), options()).await.expect("mount to verify");
    let root = fs.root_dir();
    for round in 0..rounds {
        let name = format!("LOG{:04}.BIN", round);
        let mut file = root
            .open_file(&name)
            .await
            .unwrap_or_else(|e| panic!("open {}: {:?}", name, e));
        let mut got = Vec::new();
        let mut buf = [0_u8; 4096];
        loop {
            match file.read(&mut buf).await.expect("read") {
                0 => break,
                n => got.extend_from_slice(&buf[..n]),
            }
        }
        let want = contents(round, FILE_BYTES);
        assert_eq!(got.len(), want.len(), "{} changed length", name);
        assert!(got == want, "{} came back with different contents", name);
    }
}

#[tokio::test]
async fn power_cycling_while_filling_a_volume_never_drifts() {
    let _ = env_logger::builder().is_test(true).try_init();

    // A fresh volume, formatted the way a device would.
    let disk = MemDisk::new(VOLUME_BYTES as usize);
    format_volume(
        &mut disk.clone(),
        FormatVolumeOptions::new()
            .fat_type(FatType::Fat32)
            .bytes_per_sector(512)
            .bytes_per_cluster(CLUSTER_BYTES)
            .fats(2)
            .volume_id(0x5040_3020),
    )
    .await
    .expect("format");

    let geom = *Fat32Image::parse(disk.buffer().borrow().clone()).geom();
    assert_eq!(geom.bytes_per_cluster(), CLUSTER_BYTES);
    assert!(
        geom.total_clusters >= 65525,
        "{} clusters is a FAT16 volume, not FAT32",
        geom.total_clusters
    );
    let clusters_per_file = (FILE_BYTES as u32).div_ceil(CLUSTER_BYTES);

    let mut round = 0;
    let mut previous_free = count_free(&disk, &geom);
    loop {
        match write_then_lose_power(&disk, round).await {
            Ok(()) => {}
            // The volume is full. That is the end of the loop, not a failure.
            Err(embedded_fatfs::Error::NotEnoughSpace) => break,
            Err(e) => panic!("round {}: unexpected write failure: {:?}", round, e),
        }

        assert!(
            is_dirty(&disk).await,
            "round {}: dropping the filesystem left the volume looking clean",
            round
        );

        let stats = repair(&disk, RepairMode::All).await;

        assert!(
            !is_dirty(&disk).await,
            "round {}: still dirty after repair, so the next boot repairs again",
            round
        );
        assert_eq!(stats.dirs_rejected, 0, "round {}: a directory was rejected", round);
        assert_eq!(stats.entries_removed, 0, "round {}: repair deleted an entry", round);
        assert_eq!(stats.entries_recovered, 0, "round {}: repair renamed an entry", round);

        // -- the bookkeeping the next round depends on --
        let actual_free = count_free(&disk, &geom);
        assert_eq!(
            fsinfo(&disk, &geom, FREE_COUNT),
            actual_free,
            "round {}: FSInfo free count disagrees with the FAT",
            round
        );

        let hint = fsinfo(&disk, &geom, NEXT_FREE);
        assert_ne!(hint, UNKNOWN, "round {}: the allocation hint was lost", round);
        assert!(
            (2..=geom.max_valid_cluster()).contains(&hint),
            "round {}: allocation hint {} is not a cluster number",
            round,
            hint
        );
        if actual_free > 0 {
            // The hint has to be worth having: a forward search from it must
            // find free space without falling back to a full-volume rescan.
            let free_ahead = (hint..=geom.max_valid_cluster()).any(|c| fat_entry(&disk, &geom, c) == 0);
            assert!(
                free_ahead,
                "round {}: no free cluster at or after the hint ({}), so the next \
                 allocation rescans the whole volume",
                round, hint
            );
        }

        // -- no drift --
        let used = previous_free - actual_free;
        assert!(
            used <= clusters_per_file + 2,
            "round {}: consumed {} clusters for a {}-cluster file, so a power cut is leaking space",
            round,
            used,
            clusters_per_file
        );
        previous_free = actual_free;

        round += 1;
        assert!(round < 200, "the volume never filled up");
    }

    assert!(
        round > 8,
        "the volume filled after only {} rounds; test is too weak",
        round
    );

    // Everything written before the last, failed round must have survived every
    // one of those power cuts intact.
    verify_all_files(&disk, round).await;

    // And the volume must be genuinely full rather than merely unable to
    // allocate: nothing left worth reclaiming.
    let final_free = count_free(&disk, &geom);
    assert!(
        final_free < clusters_per_file,
        "write failed with {} clusters still free",
        final_free
    );
}
