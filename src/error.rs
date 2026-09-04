use std::ffi::NulError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum RadosError {
    #[error("librados: {}", std::io::Error::from_raw_os_error(*.0))]
    Rados(i32),

    #[error("object not found")]
    NotFound,

    #[error("object already exists")]
    AlreadyExists,

    #[error("Nul byte in C string: {0}")]
    Nul(#[from] NulError),
}

pub type Result<T> = std::result::Result<T, RadosError>;

pub fn check_err(ret: i32) -> Result<()> {
    if ret >= 0 {
        return Ok(());
    }
    Err(match -ret {
        libc::ENOENT => RadosError::NotFound,
        libc::EEXIST => RadosError::AlreadyExists,
        errno => RadosError::Rados(errno),
    })
}
