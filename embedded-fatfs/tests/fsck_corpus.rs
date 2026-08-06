//! Tests for the FAT32 fsck corpus in `tests/corpus/`.
//!
//! These tests do not check an fsck — there isn't one here to check. They check
//! that the corpus itself is trustworthy, which is what makes it usable as a
//! target:
//!
//! * the pristine volume mounts and reads back correctly, and a reference
//!   consistency checker finds nothing wrong with it;
//! * every `corrupt` image really is broken (the reference checker complains,
//!   and the damage is or is not visible to a plain mount exactly as the case
//!   declares);
//! * every `fixed` image really is repaired (checker clean, mounts, and the
//!   files the case says survive are readable with their original bytes);
//! * the damage is confined to the structure the case claims to damage.
//!
//! Run `cargo run --example gen-fsck-corpus -- <dir>` to write the images out.

mod common;
mod corpus;

use std::cell::RefCell;
use std::rc::Rc;

use common::TestFs;
use corpus::image::Fat32Image;
use corpus::{CaseSpec, Domain, Visibility};
use embedded_io_async::Read;

/// What a plain mount can see. Used to decide whether a corruption is
/// observable without a checker.
#[derive(Debug, PartialEq, Eq)]
struct Observation {
    /// Every path in the tree with its length, or the error that stopped us.
    tree: Vec<String>,
    /// Contents of every readable file, as `path: length` plus a checksum.
    contents: Vec<String>,
    /// Volume dirty / IO-error flags.
    status: String,
    /// Where `DATA/SUB/..` leads.
    dotdot: String,
}

async fn mount(img: &Fat32Image) -> Result<(TestFs, Rc<RefCell<Vec<u8>>>), String> {
    common::mount(img).await
}

/// Mount `img` and record everything a driver can see, without writing to it.
async fn observe(img: &Fat32Image) -> Observation {
    let (fs, _buffer) = match mount(img).await {
        Ok(v) => v,
        Err(e) => {
            return Observation {
                tree: vec![format!("<mount failed: {}>", e)],
                contents: Vec::new(),
                status: String::new(),
                dotdot: String::new(),
            }
        }
    };

    let status = match fs.read_status_flags().await {
        Ok(flags) => format!("dirty={} io_error={}", flags.dirty(), flags.io_error()),
        Err(e) => format!("<{:?}>", e),
    };

    let mut tree = Vec::new();
    let mut contents = Vec::new();
    walk(&fs, "", &mut tree, &mut contents).await;

    let dotdot = match fs.root_dir().open_dir("DATA/SUB/..").await {
        Ok(dir) => {
            let mut names: Vec<String> = Vec::new();
            let mut iter = dir.iter();
            while let Some(r) = iter.next().await {
                match r {
                    Ok(e) => names.push(e.file_name()),
                    Err(e) => names.push(format!("<{:?}>", e)),
                }
            }
            names.join(",")
        }
        Err(e) => format!("<{:?}>", e),
    };

    Observation {
        tree,
        contents,
        status,
        dotdot,
    }
}

async fn walk(fs: &TestFs, path: &str, tree: &mut Vec<String>, contents: &mut Vec<String>) {
    let dir = if path.is_empty() {
        Ok(fs.root_dir())
    } else {
        fs.root_dir().open_dir(path).await
    };
    let dir = match dir {
        Ok(d) => d,
        Err(e) => {
            tree.push(format!("{}/ <open failed: {:?}>", path, e));
            return;
        }
    };

    let mut children = Vec::new();
    let mut iter = dir.iter();
    while let Some(r) = iter.next().await {
        match r {
            Ok(entry) => {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                let full = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{}", path, name)
                };
                tree.push(format!(
                    "{}{} len={}",
                    full,
                    if entry.is_dir() { "/" } else { "" },
                    entry.len()
                ));
                children.push((full, entry.is_dir()));
            }
            Err(e) => tree.push(format!("{}/ <iteration failed: {:?}>", path, e)),
        }
    }

    for (full, is_dir) in children {
        if is_dir {
            Box::pin(walk(fs, &full, tree, contents)).await;
        } else {
            contents.push(match read_file(fs, &full).await {
                Ok(data) => format!("{} len={} sum={:#010x}", full, data.len(), checksum(&data)),
                Err(e) => format!("{} <read failed: {}>", full, e),
            });
        }
    }
}

async fn read_file(fs: &TestFs, path: &str) -> Result<Vec<u8>, String> {
    let mut file = fs
        .root_dir()
        .open_file(path)
        .await
        .map_err(|e| format!("open: {:?}", e))?;
    let mut out = Vec::new();
    let mut buf = [0_u8; 512];
    loop {
        match file.read(&mut buf).await {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => return Err(format!("read: {:?}", e)),
        }
    }
}

fn checksum(data: &[u8]) -> u32 {
    data.iter()
        .fold(0x811C_9DC5_u32, |h, &b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
}

// -- tests ------------------------------------------------------------------

#[tokio::test]
async fn pristine_image_is_sound() {
    let img = corpus::pristine::build().await;

    let geom = img.geom();
    assert_eq!(geom.bytes_per_sector, u32::from(corpus::pristine::BYTES_PER_SECTOR));
    assert_eq!(geom.bytes_per_cluster(), corpus::pristine::BYTES_PER_CLUSTER);
    assert_eq!(geom.num_fats, 2, "the corpus needs two FATs to model mirror damage");
    assert!(geom.total_clusters >= 65525, "not a real FAT32 volume");

    let problems = corpus::verify::check(&img);
    assert!(problems.is_empty(), "pristine image is not clean: {:#?}", problems);

    let observed = observe(&img).await;
    assert_eq!(
        observed.status, "dirty=false io_error=false",
        "pristine volume is dirty"
    );
    for &(path, len) in corpus::pristine::FILES {
        assert!(
            observed
                .tree
                .iter()
                .any(|line| line.starts_with(&format!("{} len={}", path, len))),
            "{} missing from pristine tree:\n{}",
            path,
            observed.tree.join("\n")
        );
    }
}

#[tokio::test]
async fn pristine_image_is_reproducible() {
    let a = corpus::pristine::build().await;
    let b = corpus::pristine::build().await;
    corpus::assert_images_eq(&a, &b, &[]);
}

/// The short name the library derives for the long-named file is baked into one
/// case's expectations; catch a change in the SFN generator here rather than in
/// a confusing survivor mismatch.
#[tokio::test]
async fn long_name_short_name_is_as_expected() {
    let img = corpus::pristine::build().await;
    let data = img.dir_cluster("DATA");
    let sfn = img.find_sfn(data, corpus::cases::LONG_NAME_SFN).unwrap_or_else(|| {
        panic!(
            "no {} in DATA; the library now generates a different short name for \
             {:?} and cases::LONG_NAME_SFN needs updating:\n{}",
            corpus::cases::LONG_NAME_SFN,
            "sensor readings.txt",
            img.ls(data)
        )
    });
    assert_eq!(
        img.lfn_run(data, sfn).len(),
        2,
        "expected two long-name entries in front of {}",
        corpus::cases::LONG_NAME_SFN
    );
}

#[tokio::test]
async fn every_case_is_broken_and_its_fix_is_sound() {
    let pristine = corpus::pristine::build().await;
    let clean = observe(&pristine).await;

    for spec in corpus::CASES {
        let case = corpus::build_case(&pristine, spec);

        // The corrupt image must actually be inconsistent.
        let found = corpus::verify::check(&case.corrupt);
        assert!(
            !found.is_empty(),
            "case {:?}: corrupt image passes the consistency check, so it does not \
             model anything",
            spec.name
        );

        // ... and the fixed image must not be.
        let remaining = corpus::verify::check(&case.fixed);
        assert!(
            remaining.is_empty(),
            "case {:?}: fixed image is still inconsistent: {:#?}",
            spec.name,
            remaining
        );

        // Visibility is part of the case's documentation; hold it to it.
        match spec.visibility {
            Visibility::Observable => assert_ne!(
                observe(&case.corrupt).await,
                clean,
                "case {:?} is declared Observable but a mount sees nothing wrong",
                spec.name
            ),
            Visibility::Silent => assert_eq!(
                observe(&case.corrupt).await,
                clean,
                "case {:?} is declared Silent but a mount can see the damage",
                spec.name
            ),
        }

        // The fixed image must mount cleanly and still hold the declared files.
        let repaired = observe(&case.fixed).await;
        assert!(
            !repaired.tree.iter().any(|l| l.contains("failed")),
            "case {:?}: fixed image does not walk cleanly:\n{}",
            spec.name,
            repaired.tree.join("\n")
        );
        let (fs, _buf) = mount(&case.fixed).await.unwrap_or_else(|e| {
            panic!("case {:?}: fixed image does not mount: {}", spec.name, e);
        });
        for survivor in spec.survivors {
            let data = read_file(&fs, survivor.path)
                .await
                .unwrap_or_else(|e| panic!("case {:?}: reading {}: {}", spec.name, survivor.path, e));
            assert_eq!(
                data.len(),
                survivor.len,
                "case {:?}: {} has the wrong length",
                spec.name,
                survivor.path
            );
            let original_len = corpus::pristine::FILES
                .iter()
                .find(|(p, _)| *p == survivor.content_from)
                .map(|(_, len)| *len)
                .unwrap_or_else(|| panic!("{} is not a pristine file", survivor.content_from));
            let original = corpus::pristine::file_content(survivor.content_from, original_len);
            assert_eq!(
                data,
                original[..survivor.len],
                "case {:?}: {} does not hold its original bytes",
                spec.name,
                survivor.path
            );
        }
    }
}

#[tokio::test]
async fn damage_stays_inside_the_declared_structure() {
    let pristine = corpus::pristine::build().await;

    for spec in corpus::CASES {
        let case = corpus::build_case(&pristine, spec);
        let diffs = corpus::diff_ranges(pristine.as_bytes(), case.corrupt.as_bytes(), &[]);
        assert!(!diffs.is_empty(), "case {:?} changes nothing", spec.name);
        for range in &diffs {
            let where_ = classify(&pristine, range.start);
            let ok = match spec.domain {
                Domain::Fat => where_ == Where::Fat,
                Domain::Directory => where_ == Where::Data,
                Domain::FsInfo => where_ == Where::FsInfo,
            };
            assert!(
                ok,
                "case {:?} is declared {:?} but touches {:#010x} — {}",
                spec.name,
                spec.domain,
                range.start,
                pristine.describe_offset(range.start)
            );
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Where {
    Reserved,
    FsInfo,
    Fat,
    Data,
}

fn classify(img: &Fat32Image, offset: u64) -> Where {
    let g = img.geom();
    let sector = offset / u64::from(g.bytes_per_sector);
    if sector < u64::from(g.reserved_sectors) {
        return if sector == u64::from(g.fs_info_sector) {
            Where::FsInfo
        } else {
            Where::Reserved
        };
    }
    if (0..g.num_fats).any(|f| g.fat_range(f).contains(&offset)) {
        return Where::Fat;
    }
    Where::Data
}

/// A directory-entry corruption must stay inside a directory. If one of these
/// cases reached into file data it would be testing something else, and the
/// survivor checks would stop meaning what they say.
#[tokio::test]
async fn directory_damage_stays_in_directories() {
    let pristine = corpus::pristine::build().await;
    let dir_clusters: Vec<u32> = ["", "DATA", "DATA/SUB"]
        .iter()
        .flat_map(|p| pristine.chain(pristine.dir_cluster(p)))
        .collect();
    let dir_bytes: Vec<_> = dir_clusters
        .iter()
        .map(|&c| {
            let start = pristine.geom().cluster_offset(c);
            start..start + u64::from(pristine.geom().bytes_per_cluster())
        })
        .collect();

    for spec in corpus::CASES.iter().filter(|s| s.domain == Domain::Directory) {
        let case = corpus::build_case(&pristine, spec);
        for range in corpus::diff_ranges(pristine.as_bytes(), case.corrupt.as_bytes(), &[]) {
            assert!(
                dir_bytes
                    .iter()
                    .any(|d| d.contains(&range.start) && d.contains(&(range.end - 1))),
                "case {:?} changes {:#010x}, which is not inside a directory — {}",
                spec.name,
                range.start,
                pristine.describe_offset(range.start)
            );
        }
    }
}

/// Case names are used as file names by the generator, so they must be unique.
#[test]
fn case_names_are_unique() {
    let mut names: Vec<&str> = corpus::CASES.iter().map(|c: &CaseSpec| c.name).collect();
    names.sort_unstable();
    let count = names.len();
    names.dedup();
    assert_eq!(names.len(), count, "duplicate case names");
}
