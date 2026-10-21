//! Exact service/account credentials in the OS store.
//!
//! buzz-credential-store has `read`, `add` and `delete` on Linux and
//! Windows only; buzz-app's macOS build keeps its keys elsewhere. On macOS
//! they are implemented here over the login keychain's generic passwords,
//! where buzz-credential-store's `read_human` finds the human key.

pub use buzz_credential_store::Error;
pub use files::{private_dir, write_private};

mod files;

#[cfg(not(target_os = "macos"))]
pub use buzz_credential_store::{add, delete, read};

#[cfg(target_os = "macos")]
pub use macos::{add, delete, read};

#[cfg(target_os = "macos")]
mod macos {
    use super::Error;
    use security_framework::passwords;
    use zeroize::Zeroizing;

    type Result<T> = std::result::Result<T, Error>;

    /// The codes buzz-credential-store maps for `read_human`, and
    /// "duplicate item".
    fn error(error: security_framework::base::Error) -> Error {
        match error.code() {
            -25300 => Error::Absent,
            -25299 => Error::Occupied,
            -128 | -25293 | -25308 => Error::Denied,
            _ => Error::Unavailable,
        }
    }

    pub fn read(service: &str, account: &str) -> Result<Zeroizing<Vec<u8>>> {
        passwords::get_generic_password(service, account)
            .map(Zeroizing::new)
            .map_err(error)
    }

    /// Create-only, as on the other platforms: an existing entry is kept.
    pub fn add(service: &str, account: &str, value: &[u8]) -> Result<()> {
        match read(service, account) {
            Ok(_) => return Err(Error::Occupied),
            Err(Error::Absent) => {}
            Err(error) => return Err(error),
        }
        passwords::set_generic_password(service, account, value).map_err(error)
    }

    pub fn delete(service: &str, account: &str) -> Result<()> {
        passwords::delete_generic_password(service, account).map_err(error)
    }
}
