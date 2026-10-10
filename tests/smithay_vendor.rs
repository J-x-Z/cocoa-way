#![cfg(target_os = "macos")]

use smithay::utils::SealedFile;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::MetadataExt;

#[test]
fn darwin_keymap_is_readable_read_only_and_unlinked() {
    let contents = c"test keymap";
    let sealed = SealedFile::with_content(c"smithay-keymap", contents).unwrap();
    assert_eq!(sealed.size(), contents.to_bytes_with_nul().len());
    let fd = sealed.as_fd().try_clone_to_owned().unwrap();
    let mut file = std::fs::File::from(fd);
    let mut actual = Vec::new();
    file.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, contents.to_bytes_with_nul());
    assert!(file.write_all(b"modified").is_err());
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(flags & libc::FD_CLOEXEC, 0);
    assert_eq!(file.metadata().unwrap().nlink(), 0);
}

#[test]
fn darwin_keymaps_are_independent() {
    let first = SealedFile::with_data(c"smithay-keymap", b"first").unwrap();
    let second = SealedFile::with_data(c"smithay-keymap", b"second").unwrap();
    for (sealed, contents) in [(first, b"first".as_slice()), (second, b"second".as_slice())] {
        let mut file = std::fs::File::from(sealed.as_fd().try_clone_to_owned().unwrap());
        let mut actual = Vec::new();
        file.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, contents);
    }
}
