//! Large-scale write tests.
//!
//! These tests create and write files in various intended and pathological patterns, ensuring that file contents are as
//! expected. In this file, we generally write quite large files to larger filesystems (100s MiB), to exercise bugs that
//! only occur e.g. across FAT boundaries. These tests are run even when the fat-cache feature is disabled, as this just
//! tests properties of the underlying FAT FS, and therefore the behaviour at this high level should be identical
//! between fat-cache enabled and disabled.

use embedded_fatfs::{ChronoTimeProvider, FatType, FileSystem, FormatVolumeOptions, FsOptions, LossyOemCpConverter};
use embedded_io_adapters::tokio_1::FromTokio;
use embedded_io_async::{Read, Seek, SeekFrom, Write};
use tokio::fs;
use tokio::io::BufStream;

const KB: u64 = 1024;
const MB: u64 = KB * 1024;
const TMP_DIR: &str = "tmp";

type Fs = FileSystem<FromTokio<BufStream<fs::File>>, ChronoTimeProvider, LossyOemCpConverter>;

async fn open_fs(path: &str) -> Fs {
    let file = fs::OpenOptions::new().read(true).write(true).open(path).await.unwrap();
    let mut stream = FromTokio::new(BufStream::new(file));
    stream.seek(SeekFrom::Start(0)).await.unwrap();
    FileSystem::new(stream, FsOptions::new()).await.expect("mount fs")
}

/// Creates a fresh FAT32 image with `bps` bytes per sector and bytes_per_cluster (sector = cluster)
async fn format_fat32_image(path: &str, total_bytes: u64, bps: u16) {
    fs::create_dir(TMP_DIR).await.ok();
    fs::remove_file(path).await.ok();
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await
        .unwrap();
    file.set_len(total_bytes).await.unwrap();
    let mut stream = FromTokio::new(BufStream::new(file));
    let opts = FormatVolumeOptions::new()
        .fat_type(FatType::Fat32)
        .bytes_per_sector(bps)
        .bytes_per_cluster(bps as u32);
    embedded_fatfs::format_volume(&mut stream, opts)
        .await
        .expect("format volume");
    stream.flush().await.unwrap();
}

fn fill_pattern(buf: &mut [u8], pos: u64, seed: u64) {
    for (k, b) in buf.iter_mut().enumerate() {
        *b = ((pos + k as u64 + seed) % 251) as u8;
    }
}

async fn write_pattern_file(fs: &Fs, name: &str, size: u64, seed: u64) {
    let root_dir = fs.root_dir();
    let mut file = root_dir.create_file(name).await.expect("create file");
    file.truncate().await.unwrap();
    let mut chunk = vec![0u8; 64 * KB as usize];
    let mut pos: u64 = 0;
    while pos < size {
        let n = core::cmp::min(chunk.len() as u64, size - pos) as usize;
        fill_pattern(&mut chunk[..n], pos, seed);
        file.write_all(&chunk[..n]).await.expect("write chunk");
        pos += n as u64;
    }
    file.flush().await.expect("flush file");
}

async fn verify_pattern_file(fs: &Fs, name: &str, size: u64, seed: u64) {
    let root_dir = fs.root_dir();
    let mut file = root_dir.open_file(name).await.expect("open file");
    let mut buf = vec![0u8; 64 * KB as usize];
    let mut pos: u64 = 0;
    loop {
        let n = file.read(&mut buf).await.expect("read chunk");
        if n == 0 {
            break;
        }
        for (k, &b) in buf[..n].iter().enumerate() {
            assert_eq!(b, ((pos + k as u64 + seed) % 251) as u8, "mismatch in {}", name);
        }
        pos += n as u64;
    }
    assert_eq!(pos, size, "{} length", name);
}


async fn do_large_test(bps: u16) {
    let total_bytes = 512 * MB;
    let file_size = 16 * MB + 7;

    fs::create_dir(TMP_DIR).await.ok();
    let path = format!("{}/large_write_fat32_bps{}.img", TMP_DIR, bps);
    fs::remove_file(&path).await.ok();

    // Create and format a fresh FAT32 image.
    {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .await
            .unwrap();
        file.set_len(total_bytes).await.unwrap();
        let mut stream = FromTokio::new(BufStream::new(file));
        let opts = FormatVolumeOptions::new()
            .fat_type(FatType::Fat32)
            .bytes_per_sector(bps)
            .bytes_per_cluster(bps as u32);
        embedded_fatfs::format_volume(&mut stream, opts)
            .await
            .expect("format volume");
        stream.flush().await.unwrap();
    }

    // Mount, write, and check cluster count.
    {
        let fs = open_fs(&path).await;
        assert_eq!(fs.fat_type(), FatType::Fat32);
        let free_before = fs.stats().await.unwrap().free_clusters();

        write_pattern_file(&fs, "big.bin", file_size, 0).await;

        let cluster_size = u64::from(fs.cluster_size());
        let free_after = fs.stats().await.unwrap().free_clusters();
        let expected_clusters = file_size.div_ceil(cluster_size);
        assert_eq!(
            u64::from(free_before - free_after),
            expected_clusters,
            "allocated cluster count must match file size"
        );

        fs.unmount().await.expect("unmount");
    }

    // Re-mount and verify
    {
        let fs = open_fs(&path).await;
        verify_pattern_file(&fs, "big.bin", file_size, 0).await;
        fs.unmount().await.expect("unmount");
    }

    fs::remove_file(&path).await.ok();
}

#[tokio::test]
async fn test_large_write_bps512() {
    let _ = env_logger::builder().is_test(true).try_init();
    do_large_test(512).await;
}

#[tokio::test]
async fn test_large_write_bps1024() {
    let _ = env_logger::builder().is_test(true).try_init();
    do_large_test(1024).await;
}


#[tokio::test]
async fn test_large_write_bps4096() {
    let _ = env_logger::builder().is_test(true).try_init();
    do_large_test(4096).await;
}


/// After a long write + remount, the free space must still be usable: allocate a
/// second file and read it back. This exercises `find_free`/`alloc` starting
/// from a cold cache against an on-disk FAT written by the previous run.
#[tokio::test]
async fn test_large_write_then_second_file_fat32() {
    let _ = env_logger::builder().is_test(true).try_init();

    let total_bytes = 48 * MB;
    let first_size = 8 * MB + 7;
    let second_size = 8 * MB + 13;

    fs::create_dir(TMP_DIR).await.ok();
    let path = format!("{}/large_write_fat32_second.img", TMP_DIR);
    fs::remove_file(&path).await.ok();

    {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .await
            .unwrap();
        file.set_len(total_bytes).await.unwrap();
        let mut stream = FromTokio::new(BufStream::new(file));
        let opts = FormatVolumeOptions::new()
            .fat_type(FatType::Fat32)
            .bytes_per_cluster(512);
        embedded_fatfs::format_volume(&mut stream, opts)
            .await
            .expect("format volume");
        stream.flush().await.unwrap();
    }

    {
        let fs = open_fs(&path).await;
        write_pattern_file(&fs, "first.bin", first_size, 0).await;
        fs.unmount().await.expect("unmount");
    }
    {
        let fs = open_fs(&path).await;
        write_pattern_file(&fs, "second.bin", second_size, 100).await;
        // Both files must read back correctly after the second allocation,
        // confirming the on-disk FAT chains for both are intact.
        verify_pattern_file(&fs, "first.bin", first_size, 0).await;
        verify_pattern_file(&fs, "second.bin", second_size, 100).await;
        fs.unmount().await.expect("unmount");
    }
    {
        // Final cold remount to verify both chains straight from disk.
        let fs = open_fs(&path).await;
        verify_pattern_file(&fs, "first.bin", first_size, 0).await;
        verify_pattern_file(&fs, "second.bin", second_size, 100).await;
        fs.unmount().await.expect("unmount");
    }

    fs::remove_file(&path).await.ok();
}

/// Two files kept open simultaneously and written in an alternating loop. Their
/// cluster allocations interleave through the single shared FAT cache, so a
/// correct implementation must flush-before-evict on every window switch.
/// Verified warm and after a cold remount.
#[tokio::test]
async fn test_two_files_interleaved_writes() {
    let _ = env_logger::builder().is_test(true).try_init();
    let path = format!("{}/two_files_interleaved.img", TMP_DIR);
    format_fat32_image(&path, 48 * MB, 512).await;

    let size = 8 * MB + 7;
    let seed_a = 0;
    let seed_b = 7;

    {
        let fs = open_fs(&path).await;
        assert_eq!(fs.fat_type(), FatType::Fat32);
        {
            let root = fs.root_dir();
            let mut a = root.create_file("a.bin").await.unwrap();
            a.truncate().await.unwrap();
            let mut b = root.create_file("b.bin").await.unwrap();
            b.truncate().await.unwrap();

            let mut chunk = vec![0u8; 64 * KB as usize];
            let mut pos: u64 = 0;
            while pos < size {
                let n = core::cmp::min(chunk.len() as u64, size - pos) as usize;
                fill_pattern(&mut chunk[..n], pos, seed_a);
                a.write_all(&chunk[..n]).await.unwrap();
                fill_pattern(&mut chunk[..n], pos, seed_b);
                b.write_all(&chunk[..n]).await.unwrap();
                pos += n as u64;
            }
            a.flush().await.unwrap();
            b.flush().await.unwrap();
        }
        verify_pattern_file(&fs, "a.bin", size, seed_a).await;
        verify_pattern_file(&fs, "b.bin", size, seed_b).await;
        fs.unmount().await.unwrap();
    }
    {
        let fs = open_fs(&path).await;
        verify_pattern_file(&fs, "a.bin", size, seed_a).await;
        verify_pattern_file(&fs, "b.bin", size, seed_b).await;
        fs.unmount().await.unwrap();
    }

    fs::remove_file(&path).await.ok();
}

/// One file is read while another is written, in the same loop. This interleaves FAT reads and writes through the
/// shared cache, then verifies both chains after a cold remount.
#[tokio::test]
async fn test_read_one_write_another() {
    let _ = env_logger::builder().is_test(true).try_init();
    let path = format!("{}/read_write_interleaved.img", TMP_DIR);
    format_fat32_image(&path, 48 * MB, 512).await;

    let a_size = 6 * MB;
    let c_size = 4 * MB;
    let seed_a = 0;
    let seed_c = 13;

    {
        let fs = open_fs(&path).await;
        write_pattern_file(&fs, "a.bin", a_size, seed_a).await;
        fs.unmount().await.unwrap();
    }
    {
        let fs = open_fs(&path).await;
        {
            let root = fs.root_dir();
            let mut a = root.open_file("a.bin").await.unwrap();
            let mut c = root.create_file("c.bin").await.unwrap();
            c.truncate().await.unwrap();

            let mut rbuf = vec![0u8; 64 * KB as usize];
            let mut wbuf = vec![0u8; 64 * KB as usize];
            let mut rpos: u64 = 0;
            let mut wpos: u64 = 0;
            let mut a_done = false;
            while wpos < c_size || !a_done {
                if !a_done {
                    let n = a.read(&mut rbuf).await.unwrap();
                    if n == 0 {
                        a_done = true;
                    } else {
                        for (k, &x) in rbuf[..n].iter().enumerate() {
                            assert_eq!(x, ((rpos + k as u64 + seed_a) % 251) as u8, "a.bin mismatch");
                        }
                        rpos += n as u64;
                    }
                }
                if wpos < c_size {
                    let n = core::cmp::min(wbuf.len() as u64, c_size - wpos) as usize;
                    fill_pattern(&mut wbuf[..n], wpos, seed_c);
                    c.write_all(&wbuf[..n]).await.unwrap();
                    wpos += n as u64;
                }
            }
            assert_eq!(rpos, a_size, "read back all of a.bin");
            c.flush().await.unwrap();
        }
        verify_pattern_file(&fs, "c.bin", c_size, seed_c).await;
        fs.unmount().await.unwrap();
    }
    {
        let fs = open_fs(&path).await;
        verify_pattern_file(&fs, "a.bin", a_size, seed_a).await;
        verify_pattern_file(&fs, "c.bin", c_size, seed_c).await;
        fs.unmount().await.unwrap();
    }

    fs::remove_file(&path).await.ok();
}

/// Write a file that spans several FAT sectors, call `file.flush()` then drop the `FileSystem` without calling
/// `fs.unmount()` or `fs.flush()`. On remount the file should be complete, even when caching the FAT (ie. when the fat
/// cache is enabled, we test that `file.flush()` also flushes the FAT cache. This happens as a matter of course without
/// the fat cache)
#[tokio::test]
async fn test_fat_cache_file_flush_durability() {
    let _ = env_logger::builder().is_test(true).try_init();

    let path = format!("{}/flush_durability.img", TMP_DIR);
    format_fat32_image(&path, 48 * MB, 512).await;

    // Ensure non power-of-two size, in case of incidental flush when a cluster is complete.
    let file_size = 4 * MB + 7;
    let seed = 42;

    // Write + file.flush() + drop fs WITHOUT unmount / fs.flush()
    {
        let fs = open_fs(&path).await;
        write_pattern_file(&fs, "f.bin", file_size, seed).await;
    }

    // Verify the file is intact
    {
        let fs = open_fs(&path).await;
        verify_pattern_file(&fs, "f.bin", file_size, seed).await;
        fs.unmount().await.unwrap();
    }

    fs::remove_file(&path).await.ok();
}
