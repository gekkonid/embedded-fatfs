//! A corpus of corrupted FAT32 images and their repaired counterparts, for
//! developing and testing a lightweight `fsck` for embedded devices.
//!
//! # What is modelled
//!
//! Every case models a *torn update*: the device lost power part way through
//! writing a FAT sector or a directory entry, so the on-disk structures
//! disagree with each other. That is the failure mode an embedded FAT
//! implementation actually has to survive. Deliberately **not** modelled:
//! cross-linked files, cluster double-use, damaged boot sectors, bad-block
//! remapping, and anything else that needs a whole-volume cluster ownership map
//! to diagnose — those cost RAM proportional to the volume size, which is what
//! a lightweight checker cannot afford.
//!
//! # How each case is built
//!
//! ```text
//!   pristine ──(spec.corrupt)──► corrupt.img     what the device wakes up to
//!       │
//!       └──────(spec.expected)──► fixed.img      what fsck must turn it into
//! ```
//!
//! `fixed` is expressed as an edit of the *pristine* image rather than of the
//! corrupt one, so it always describes a consistent volume. For most cases the
//! repair simply undoes the corruption and `fixed == pristine`; where data is
//! genuinely unrecoverable (a chain link that was never written) `fixed`
//! records the truncation that a checker is expected to perform.
//!
//! # The repair rules the corpus assumes
//!
//! A checker that follows these four rules reproduces every `fixed` image:
//!
//! 1. **A file is as long as the shorter of its size field and its cluster
//!    chain.** Trim the size down to what the chain reaches, terminate the chain
//!    where the size ends, and free whatever falls off the end. In particular a
//!    cluster whose FAT entry reads *free* is not part of any chain, however
//!    much the size field wants it: adopting it would resurrect data that may
//!    never have been written, and worse, the allocator may already have handed
//!    that cluster to another file. `fsck.vfat` makes the same call.
//! 2. **Every FAT repair is written to all FAT copies**, and FAT copies that
//!    disagree are resolved in favour of FAT #1.
//! 3. **A directory entry that cannot be trusted is marked deleted** (`0xE5` in
//!    the first name byte), never zeroed or shuffled.
//! 4. **Allocated clusters no directory entry references are freed.**
//!
//! # Cross-checking
//!
//! The images are ordinary FAT32 volumes, so `fsck.vfat -n` is a useful second
//! opinion: it flags all fourteen corruptions, reports nothing on the pristine
//! image or on any of the fixed ones, and agrees with rules 1, 2 and 4. It
//! differs on rule 3, preferring to rename and truncate a damaged entry rather
//! than delete it; the individual cases say so where it matters.
//!
//! # Using the corpus
//!
//! Build the pristine volume once — it takes a moment — then derive each case
//! from it, run the checker over the corrupt image, and compare:
//!
//! ```text
//! let pristine = corpus::pristine::build().await;
//! for spec in corpus::CASES {
//!     let case = corpus::build_case(&pristine, spec);
//!     let mut img = case.corrupt.clone();
//!     my_fsck(&mut img).await;
//!     corpus::assert_images_eq(&img, &case.fixed, &(spec.dont_care)(&case.fixed));
//! }
//! ```
//!
//! `assert_images_eq` reports differences by the structure they land in ("FAT1
//! entry for cluster 7", "cluster 2 +157"), so a failure names the thing the
//! checker got wrong rather than an offset.
//!
//! To drive a checker that works through `FileSystem` rather than on raw bytes,
//! [`pristine::MemDisk`] is a block device over a `Vec<u8>` whose buffer stays
//! readable after `unmount` consumes it.

#![allow(dead_code)]

use std::ops::Range;

pub mod cases;
pub mod image;
pub mod pristine;
pub mod verify;

pub use cases::CASES;
pub use image::Fat32Image;

/// Which structure the corruption lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// The file allocation table.
    Fat,
    /// A directory entry (short name, long name, or `.`/`..`).
    Directory,
    /// The FAT32 FSInfo sector.
    FsInfo,
}

/// Whether the damage is visible to a driver that just mounts and reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Mounting or walking the volume misbehaves: an error, a short read, wrong
    /// data, or a bogus directory entry. The test harness asserts that a walk of
    /// the corrupt image differs from a walk of the pristine one.
    Observable,
    /// Everything reads back correctly; only the metadata is wrong. Lost
    /// clusters and a stale free count look like this — the volume works, it
    /// just leaks space. The harness asserts the walk is *unchanged*.
    ///
    /// Most FAT damage is silent, which is the point of having a checker: the
    /// volume reads fine until the allocator hands out a cluster it should not
    /// have, and by then the damage has spread into file data.
    Silent,
}

/// One corruption scenario.
pub struct CaseSpec {
    /// Stable identifier, also used as the image file name stem.
    pub name: &'static str,
    pub domain: Domain,
    /// The power-loss story that produces this state.
    pub scenario: &'static str,
    /// What a checker must do about it.
    pub repair: &'static str,
    pub visibility: Visibility,
    /// Damages a pristine image.
    pub corrupt: fn(&mut Fat32Image),
    /// Applies to a pristine image the state a correct repair must arrive at.
    /// Empty for the common case where the repair exactly undoes the damage.
    pub expected: fn(&mut Fat32Image),
    /// Byte ranges a conforming checker is allowed to differ in. Used for
    /// bookkeeping a minimal checker may legitimately skip, such as the FSInfo
    /// free-cluster hint.
    pub dont_care: fn(&Fat32Image) -> Vec<Range<u64>>,
    /// Files that must still be readable in the fixed image, as
    /// `(path, expected length)`. Contents are checked against
    /// [`pristine::file_content`] of the *pristine* path in `content_from`.
    pub survivors: &'static [Survivor],
}

/// A file expected to survive the repair.
#[derive(Debug, Clone, Copy)]
pub struct Survivor {
    /// Path in the *fixed* image.
    pub path: &'static str,
    /// Expected length after repair.
    pub len: usize,
    /// Path the content was originally written under; differs from `path` only
    /// when a repair costs a file its long name.
    pub content_from: &'static str,
}

impl Survivor {
    pub const fn intact(path: &'static str, len: usize) -> Self {
        Self {
            path,
            len,
            content_from: path,
        }
    }

    pub const fn truncated(path: &'static str, len: usize) -> Self {
        Self::intact(path, len)
    }

    pub const fn renamed(path: &'static str, len: usize, content_from: &'static str) -> Self {
        Self {
            path,
            len,
            content_from,
        }
    }
}

/// No expected-state edit: the repair exactly undoes the corruption.
pub fn no_edit(_image: &mut Fat32Image) {}

/// No tolerated differences.
pub fn nothing(_image: &Fat32Image) -> Vec<Range<u64>> {
    Vec::new()
}

/// Tolerate any content in the FSInfo sector.
///
/// Used by cases whose repair changes the number of free clusters: a checker
/// that maintains FSInfo and one that just invalidates the hint are both
/// correct, so the whole sector is excluded from comparison.
pub fn fsinfo_is_a_hint(image: &Fat32Image) -> Vec<Range<u64>> {
    vec![image.fsinfo_range()]
}

/// A generated case: the two images plus the spec they came from.
pub struct TestCase {
    pub spec: &'static CaseSpec,
    /// The image as the device finds it after the power cut.
    pub corrupt: Fat32Image,
    /// The image a correct checker must produce from `corrupt`.
    pub fixed: Fat32Image,
}

impl TestCase {
    pub fn name(&self) -> &'static str {
        self.spec.name
    }

    /// Byte ranges where the two images differ, ignoring tolerated ranges.
    pub fn damage(&self) -> Vec<Range<u64>> {
        diff_ranges(
            self.corrupt.as_bytes(),
            self.fixed.as_bytes(),
            &(self.spec.dont_care)(&self.fixed),
        )
    }
}

/// Build one case from a pristine image.
pub fn build_case(pristine: &Fat32Image, spec: &'static CaseSpec) -> TestCase {
    let mut corrupt = pristine.clone();
    (spec.corrupt)(&mut corrupt);

    let mut fixed = pristine.clone();
    (spec.expected)(&mut fixed);

    assert_ne!(
        corrupt.as_bytes(),
        fixed.as_bytes(),
        "case {:?} produced identical corrupt and fixed images",
        spec.name
    );
    TestCase { spec, corrupt, fixed }
}

/// Build every case. Note each image is [`pristine::TOTAL_BYTES`] large, so
/// prefer [`build_case`] in a loop unless you really need them all at once.
pub async fn build_all() -> Vec<TestCase> {
    let pristine = pristine::build().await;
    CASES.iter().map(|spec| build_case(&pristine, spec)).collect()
}

// -- image comparison ------------------------------------------------------

/// Contiguous byte ranges where `a` and `b` differ, skipping `ignore`.
pub fn diff_ranges(a: &[u8], b: &[u8], ignore: &[Range<u64>]) -> Vec<Range<u64>> {
    assert_eq!(a.len(), b.len(), "images differ in length: {} vs {}", a.len(), b.len());
    let ignored = |off: u64| ignore.iter().any(|r| r.contains(&off));

    let mut out: Vec<Range<u64>> = Vec::new();
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let off = i as u64;
        if x == y || ignored(off) {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.end == off => last.end = off + 1,
            _ => out.push(off..off + 1),
        }
    }
    out
}

/// Assert `actual` matches `expected`, reporting the first differences in terms
/// of the FAT structures they land in.
///
/// # Panics
///
/// Panics with a decoded diff if the images differ outside `ignore`.
pub fn assert_images_eq(actual: &Fat32Image, expected: &Fat32Image, ignore: &[Range<u64>]) {
    let diffs = diff_ranges(actual.as_bytes(), expected.as_bytes(), ignore);
    if diffs.is_empty() {
        return;
    }
    let mut report = format!("images differ in {} place(s):\n", diffs.len());
    for range in diffs.iter().take(16) {
        let len = usize::try_from(range.end - range.start).unwrap();
        report.push_str(&format!(
            "  {:#010x}..{:#010x}  {}\n    actual   {:02x?}\n    expected {:02x?}\n",
            range.start,
            range.end,
            expected.describe_offset(range.start),
            actual.read(range.start, len.min(16)),
            expected.read(range.start, len.min(16)),
        ));
    }
    if diffs.len() > 16 {
        report.push_str(&format!("  ... and {} more\n", diffs.len() - 16));
    }
    panic!("{}", report);
}
