// A fixture shared between test binaries, each of which uses only the part it
// needs — so anything unused here is unused by that binary, not dead.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use embedded_fatfs::{FsOptions, LossyOemCpConverter, NullTimeProvider};

use super::corpus::pristine::MemDisk;
use super::corpus::Fat32Image;

pub type TestFs = embedded_fatfs::FileSystem<MemDisk, NullTimeProvider, LossyOemCpConverter>;

pub async fn mount(
    img: &Fat32Image,
) -> Result<(TestFs, Rc<RefCell<Vec<u8>>>), String> {
    let disk = MemDisk::from_bytes(img.as_bytes().to_vec());
    let buffer = disk.buffer();
    let options = FsOptions::new()
        .time_provider(NullTimeProvider::new())
        .oem_cp_converter(LossyOemCpConverter::new());
    match embedded_fatfs::FileSystem::new(disk, options).await {
        Ok(fs) => Ok((fs, buffer)),
        Err(e) => Err(format!("{:?}", e)),
    }
}

pub async fn mount_or_panic(img: &Fat32Image) -> (TestFs, Rc<RefCell<Vec<u8>>>) {
    mount(img).await.expect("mount failed")
}
