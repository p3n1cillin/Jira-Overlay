//! Windows DPAPI protection for the Jira API token, byte-compatible with the
//! Python build: `CryptProtectData` with no description, no entropy, user
//! scope, flags 0; stored as standard base64.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData};

fn call(data: &[u8], protect: bool) -> windows::core::Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` borrows `data` for the duration of the call; `output`
    // is allocated by Windows and released with LocalFree below.
    unsafe {
        if protect {
            CryptProtectData(&input, None, None, None, None, 0, &mut output)?;
        } else {
            CryptUnprotectData(&input, None, None, None, None, 0, &mut output)?;
        }
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(bytes)
    }
}

/// Encrypt for storage, tied to this Windows login.
pub fn protect(plaintext: &str) -> windows::core::Result<String> {
    Ok(STANDARD.encode(call(plaintext.as_bytes(), true)?))
}

/// Reverse of [`protect`]. `None` on any failure (another profile's blob,
/// corruption), which callers treat as "no token".
pub fn unprotect(encoded: &str) -> Option<String> {
    let blob = STANDARD.decode(encoded.trim()).ok()?;
    String::from_utf8(call(&blob, false).ok()?).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip_and_garbage() {
        let blob = super::protect("secret-token").unwrap();
        assert_eq!(super::unprotect(&blob).as_deref(), Some("secret-token"));
        assert_eq!(super::unprotect("not base64!"), None);
        assert_eq!(super::unprotect("AAAA"), None);
    }
}
