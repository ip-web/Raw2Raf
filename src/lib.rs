pub mod batch;
pub mod ffi;
pub mod jpeg;
pub mod metadata;
pub mod pipeline;
pub mod repack;
pub mod template;
pub mod tiff;

pub use ffi::*;

use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Creates a `Command` configured to never display a visible console or terminal window on Windows.
pub fn create_hidden_command<S: AsRef<std::ffi::OsStr>>(program: S) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}


