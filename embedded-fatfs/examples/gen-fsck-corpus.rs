//! Write the FAT32 fsck corpus out as image files.
//!
//! ```sh
//! cargo run --example gen-fsck-corpus -- ./corpus
//! ```
//!
//! For each case you get `<name>.corrupt.img` (what the device wakes up to),
//! `<name>.fixed.img` (what a checker must turn it into) and `<name>.txt` (the
//! scenario, the repair rule, and the byte ranges that differ). `pristine.img`
//! is the undamaged volume every case starts from.
//!
//! Images are written sparsely, so the 40 MiB volumes cost a couple of hundred
//! kilobytes of disk each. They are ordinary FAT32 volumes: `fsck.vfat -n` and
//! `mount -o loop` both understand them, which is a useful second opinion on
//! what a repair should do.

use std::env;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

#[path = "../tests/corpus/mod.rs"]
mod corpus;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let out_dir = env::args().nth(1).unwrap_or_else(|| "corpus".to_string());
    let out_dir = Path::new(&out_dir);
    std::fs::create_dir_all(out_dir)?;

    let pristine = corpus::pristine::build().await;
    write_sparse(&out_dir.join("pristine.img"), pristine.as_bytes())?;
    println!(
        "pristine.img  {} clusters of {} bytes, {} free",
        pristine.geom().total_clusters,
        pristine.geom().bytes_per_cluster(),
        pristine.count_free_clusters(),
    );

    for spec in corpus::CASES {
        let case = corpus::build_case(&pristine, spec);
        write_sparse(
            &out_dir.join(format!("{}.corrupt.img", spec.name)),
            case.corrupt.as_bytes(),
        )?;
        write_sparse(&out_dir.join(format!("{}.fixed.img", spec.name)), case.fixed.as_bytes())?;

        let mut notes = String::new();
        notes.push_str(&format!("{}\n{}\n\n", spec.name, "=".repeat(spec.name.len())));
        notes.push_str(&format!("domain:     {:?}\n", spec.domain));
        notes.push_str(&format!("visibility: {:?}\n\n", spec.visibility));
        notes.push_str(&format!("scenario\n--------\n{}\n\n", wrap(spec.scenario)));
        notes.push_str(&format!("repair\n------\n{}\n\n", wrap(spec.repair)));

        notes.push_str("problems a checker should report\n--------------------------------\n");
        for problem in corpus::verify::check(&case.corrupt) {
            notes.push_str(&format!("  {}\n", problem));
        }

        notes.push_str("\nbytes that must change\n----------------------\n");
        for range in case.damage() {
            let len = usize::try_from(range.end - range.start).unwrap().min(16);
            notes.push_str(&format!(
                "  {:#010x}..{:#010x}  {}\n    corrupt {:02x?}\n    fixed   {:02x?}\n",
                range.start,
                range.end,
                case.fixed.describe_offset(range.start),
                case.corrupt.read(range.start, len),
                case.fixed.read(range.start, len),
            ));
        }

        notes.push_str("\nfiles that must survive\n-----------------------\n");
        for survivor in spec.survivors {
            notes.push_str(&format!("  {} ({} bytes)\n", survivor.path, survivor.len));
        }

        std::fs::write(out_dir.join(format!("{}.txt", spec.name)), &notes)?;
        println!("{:<32} {} byte range(s) to repair", spec.name, case.damage().len());
    }

    println!("\nwrote {} cases to {}", corpus::CASES.len(), out_dir.display());
    Ok(())
}

/// Write `data` skipping all-zero blocks, so the file is sparse on filesystems
/// that support it. The result is byte-identical when read back.
fn write_sparse(path: &Path, data: &[u8]) -> std::io::Result<()> {
    const BLOCK: usize = 4096;
    let mut file = File::create(path)?;
    for (i, block) in data.chunks(BLOCK).enumerate() {
        if block.iter().any(|&b| b != 0) {
            file.seek(SeekFrom::Start((i * BLOCK) as u64))?;
            file.write_all(block)?;
        }
    }
    file.set_len(data.len() as u64)?;
    Ok(())
}

/// Reflow a doc string into 78-column lines for the notes file.
fn wrap(text: &str) -> String {
    let mut out = String::new();
    let mut line_len = 0;
    for word in text.split_whitespace() {
        if line_len + word.len() + 1 > 78 {
            out.push('\n');
            line_len = 0;
        } else if line_len > 0 {
            out.push(' ');
            line_len += 1;
        }
        out.push_str(word);
        line_len += word.len();
    }
    out
}
