use base64::{Engine as _, engine::general_purpose::STANDARD};
use zeroize::Zeroizing;

use crate::error::{Error, Result};

const GITHUB_SECRET_CONTEXT: &[u8] = b"dvup/github-api-key/v1";
const AI_SECRET_CONTEXT: &[u8] = b"dvup/ai-api-key/v1";

#[cfg(windows)]
const ENCRYPTED_PREFIX: &str = "dpapi-v1:";
#[cfg(not(windows))]
const ENCRYPTED_PREFIX: &str = "aead-v1:";

pub(crate) fn encrypt_github_api_key(api_key: &str) -> Result<String> {
    validate_api_key(api_key)?;
    encrypt_platform(api_key.as_bytes(), GITHUB_SECRET_CONTEXT, "GitHub API key")
}

pub(crate) fn github_api_key(encrypted_api_key: Option<&str>) -> Result<Option<Zeroizing<String>>> {
    let Some(encrypted_api_key) = encrypted_api_key else {
        return Ok(None);
    };
    validate_encrypted_github_api_key(encrypted_api_key)?;
    let plaintext = decrypt_platform(encrypted_api_key, GITHUB_SECRET_CONTEXT, "GitHub API key")?;
    let api_key = String::from_utf8(plaintext).map_err(|_| {
        Error::InvalidConfig("encrypted GitHub API key is not valid UTF-8".to_owned())
    })?;
    validate_api_key(&api_key)?;
    Ok(Some(Zeroizing::new(api_key)))
}

pub(crate) fn has_github_api_key(encrypted_api_key: Option<&str>) -> Result<bool> {
    Ok(github_api_key(encrypted_api_key)?.is_some())
}

pub(crate) fn validate_encrypted_github_api_key(encrypted_api_key: &str) -> Result<()> {
    validate_encrypted_api_key(encrypted_api_key)
}

pub(crate) fn encrypt_ai_api_key(api_key: &str) -> Result<String> {
    validate_ai_api_key(api_key)?;
    encrypt_platform(api_key.as_bytes(), AI_SECRET_CONTEXT, "AI API key")
}

pub(crate) fn ai_api_key(encrypted_api_key: Option<&str>) -> Result<Option<Zeroizing<String>>> {
    let Some(encrypted_api_key) = encrypted_api_key else {
        return Ok(None);
    };
    validate_encrypted_ai_api_key(encrypted_api_key)?;
    let plaintext = decrypt_platform(encrypted_api_key, AI_SECRET_CONTEXT, "AI API key")?;
    let api_key = String::from_utf8(plaintext)
        .map_err(|_| Error::InvalidConfig("encrypted AI API key is not valid UTF-8".to_owned()))?;
    validate_ai_api_key(&api_key)?;
    Ok(Some(Zeroizing::new(api_key)))
}

pub(crate) fn validate_encrypted_ai_api_key(encrypted_api_key: &str) -> Result<()> {
    validate_encrypted_api_key(encrypted_api_key)
}

fn validate_encrypted_api_key(encrypted_api_key: &str) -> Result<()> {
    if encrypted_api_key.trim() != encrypted_api_key {
        return Err(encrypted_error());
    }
    let encoded = encrypted_api_key
        .strip_prefix(ENCRYPTED_PREFIX)
        .ok_or_else(encrypted_error)?;
    let decoded = STANDARD.decode(encoded).map_err(|_| encrypted_error())?;
    #[cfg(windows)]
    if decoded.is_empty() {
        return Err(encrypted_error());
    }
    #[cfg(not(windows))]
    if decoded.len() <= 12 + 16 {
        return Err(encrypted_error());
    }
    Ok(())
}

fn validate_ai_api_key(api_key: &str) -> Result<()> {
    if api_key.is_empty()
        || api_key.trim() != api_key
        || !api_key
            .chars()
            .all(|character| character.is_ascii_graphic())
    {
        return Err(Error::InvalidConfig(
            "AI API key must contain visible ASCII characters without whitespace".to_owned(),
        ));
    }
    Ok(())
}

fn validate_api_key(api_key: &str) -> Result<()> {
    if api_key.is_empty()
        || api_key.trim() != api_key
        || !api_key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-".contains(character))
    {
        return Err(Error::InvalidConfig(
            "GitHub API key contains invalid characters".to_owned(),
        ));
    }
    Ok(())
}

fn encrypted_error() -> Error {
    Error::InvalidConfig("encrypted API key has an invalid format".to_owned())
}

#[cfg(windows)]
fn encrypt_platform(plaintext: &[u8], context: &[u8], label: &str) -> Result<String> {
    use std::ptr;

    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData},
    };

    let input_length = u32::try_from(plaintext.len())
        .map_err(|_| Error::InvalidConfig(format!("{label} is too long")))?;
    let entropy_length = u32::try_from(context.len()).expect("small DPAPI entropy");
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_length,
        pbData: plaintext.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: entropy_length,
        pbData: context.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };
    // SAFETY: the input and entropy slices outlive the call, their lengths fit
    // in u32, and output is a valid zero-initialized out parameter owned by
    // the caller after CryptProtectData succeeds.
    let succeeded = unsafe {
        CryptProtectData(
            &input,
            ptr::null(),
            &entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if succeeded == 0 {
        return Err(Error::Message(format!(
            "Windows DPAPI {label} encryption failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: a successful CryptProtectData call returns a readable LocalAlloc
    // buffer of exactly cbData bytes. It remains live until LocalFree below.
    let encrypted = unsafe {
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let encoded = STANDARD.encode(bytes);
        let _ = LocalFree(output.pbData.cast());
        encoded
    };
    Ok(format!("{ENCRYPTED_PREFIX}{encrypted}"))
}

#[cfg(windows)]
fn decrypt_platform(encrypted_api_key: &str, context: &[u8], label: &str) -> Result<Vec<u8>> {
    use std::ptr;

    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
        },
    };

    let mut encrypted = STANDARD
        .decode(
            encrypted_api_key
                .strip_prefix(ENCRYPTED_PREFIX)
                .ok_or_else(encrypted_error)?,
        )
        .map_err(|_| encrypted_error())?;
    let input_length = u32::try_from(encrypted.len()).map_err(|_| encrypted_error())?;
    let entropy_length = u32::try_from(context.len()).expect("small DPAPI entropy");
    let input = CRYPT_INTEGER_BLOB {
        cbData: input_length,
        pbData: encrypted.as_mut_ptr(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: entropy_length,
        pbData: context.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };
    // SAFETY: encrypted and SECRET_CONTEXT outlive the call, their lengths fit
    // in u32, and output is a valid zero-initialized out parameter owned by
    // the caller after CryptUnprotectData succeeds.
    let succeeded = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            &entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if succeeded == 0 {
        return Err(Error::Message(format!(
            "Windows DPAPI {label} decryption failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: a successful CryptUnprotectData call returns a writable
    // LocalAlloc buffer of exactly cbData bytes. Copy the plaintext, wipe the
    // allocation while it is live, and release it exactly once.
    let plaintext = unsafe {
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let plaintext = bytes.to_vec();
        std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        let _ = LocalFree(output.pbData.cast());
        plaintext
    };
    Ok(plaintext)
}

#[cfg(not(windows))]
fn encrypt_platform(plaintext: &[u8], context: &[u8], label: &str) -> Result<String> {
    use aes_gcm::{
        Aes256Gcm, KeyInit, Nonce,
        aead::{Aead, Payload},
    };

    let key = wrapping_key(true)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_slice()).map_err(|_| encrypted_error())?;
    let mut nonce = [0_u8; 12];
    getrandom::fill(&mut nonce)
        .map_err(|error| Error::Message(format!("secure random generation failed: {error}")))?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: context,
            },
        )
        .map_err(|_| Error::Message(format!("{label} encryption failed")))?;
    let mut encrypted = Vec::with_capacity(nonce.len() + ciphertext.len());
    encrypted.extend_from_slice(&nonce);
    encrypted.extend_from_slice(&ciphertext);
    Ok(format!("{ENCRYPTED_PREFIX}{}", STANDARD.encode(encrypted)))
}

#[cfg(not(windows))]
fn decrypt_platform(encrypted_api_key: &str, context: &[u8], label: &str) -> Result<Vec<u8>> {
    use aes_gcm::{
        Aes256Gcm, KeyInit, Nonce,
        aead::{Aead, Payload},
    };

    let encrypted = STANDARD
        .decode(
            encrypted_api_key
                .strip_prefix(ENCRYPTED_PREFIX)
                .ok_or_else(encrypted_error)?,
        )
        .map_err(|_| encrypted_error())?;
    let (nonce, ciphertext) = encrypted.split_at(12);
    let key = wrapping_key(false)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_slice()).map_err(|_| encrypted_error())?;
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: context,
            },
        )
        .map_err(|_| Error::Message(format!("{label} decryption failed")))
}

#[cfg(all(not(windows), test))]
fn wrapping_key(_create: bool) -> Result<Zeroizing<Vec<u8>>> {
    const TEST_WRAPPING_KEY: [u8; 32] = [0xA5; 32];
    Ok(Zeroizing::new(TEST_WRAPPING_KEY.to_vec()))
}

#[cfg(all(not(windows), not(test)))]
fn wrapping_key(create: bool) -> Result<Zeroizing<Vec<u8>>> {
    use keyring::{Entry, Error as KeyringError};

    const SERVICE: &str = "dev.dvup.settings-encryption";
    const USER: &str = "github-api-key";

    let entry = Entry::new(SERVICE, USER).map_err(keyring_error)?;
    let encoded = match entry.get_password() {
        Ok(encoded) => Zeroizing::new(encoded),
        Err(KeyringError::NoEntry) if create => {
            let mut key = Zeroizing::new([0_u8; 32]);
            getrandom::fill(&mut *key).map_err(|error| {
                Error::Message(format!("secure random generation failed: {error}"))
            })?;
            let encoded = Zeroizing::new(STANDARD.encode(*key));
            entry.set_password(&encoded).map_err(keyring_error)?;
            encoded
        }
        Err(KeyringError::NoEntry) => {
            return Err(Error::Message(
                "operating-system settings encryption key is missing".to_owned(),
            ));
        }
        Err(error) => return Err(keyring_error(error)),
    };
    let key = STANDARD.decode(encoded.as_bytes()).map_err(|_| {
        Error::Message("operating-system settings encryption key is invalid".to_owned())
    })?;
    if key.len() != 32 {
        return Err(Error::Message(
            "operating-system settings encryption key is invalid".to_owned(),
        ));
    }
    Ok(Zeroizing::new(key))
}

#[cfg(all(not(windows), not(test)))]
fn keyring_error(error: impl std::fmt::Display) -> Error {
    Error::Message(format!(
        "operating-system settings encryption key failed: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_settings_value_round_trips_without_containing_the_token() {
        let token = "github_pat_temporary_encryption_test";
        let encrypted = encrypt_github_api_key(token).expect("encrypt GitHub API key");

        assert!(encrypted.starts_with(ENCRYPTED_PREFIX));
        assert!(!encrypted.contains(token));
        assert_eq!(
            github_api_key(Some(&encrypted))
                .expect("decrypt GitHub API key")
                .expect("configured GitHub API key")
                .as_str(),
            token
        );
        assert!(
            github_api_key(None)
                .expect("missing GitHub API key")
                .is_none()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unit_tests_use_an_in_process_wrapping_key() {
        assert_eq!(
            wrapping_key(false).expect("test wrapping key").as_slice(),
            &[0xA5; 32]
        );
    }

    #[test]
    fn encrypted_ai_api_key_round_trips_with_its_own_context() {
        let token = "sk-ai.provider:test_123";
        let encrypted = encrypt_ai_api_key(token).expect("encrypt AI API key");

        assert!(encrypted.starts_with(ENCRYPTED_PREFIX));
        assert!(!encrypted.contains(token));
        assert_eq!(
            ai_api_key(Some(&encrypted))
                .expect("decrypt AI API key")
                .expect("configured AI API key")
                .as_str(),
            token
        );
        assert!(github_api_key(Some(&encrypted)).is_err());
    }

    #[test]
    fn github_api_keys_are_strictly_validated_without_echoing_them() {
        assert!(validate_api_key("github_pat_example_123").is_ok());
        assert!(validate_api_key(" secret").is_err());
        assert!(validate_api_key("secret value").is_err());
    }

    #[test]
    fn ai_api_keys_accept_provider_punctuation_but_reject_whitespace() {
        assert!(validate_ai_api_key("sk-provider.token:123").is_ok());
        assert!(validate_ai_api_key(" secret").is_err());
        assert!(validate_ai_api_key("secret value").is_err());
        assert!(validate_ai_api_key("secret\nvalue").is_err());
    }

    #[test]
    fn corrupted_or_plaintext_settings_values_are_rejected() {
        assert!(github_api_key(Some("github_pat_plaintext")).is_err());
        assert!(github_api_key(Some(ENCRYPTED_PREFIX)).is_err());
        assert!(github_api_key(Some(&format!("{ENCRYPTED_PREFIX}not-base64"))).is_err());
    }
}
