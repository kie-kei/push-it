use age::x25519;
use anyhow::Result;
use std::io::{Read, Write};
use std::path::Path;
use age::secrecy::ExposeSecret;
use std::str::FromStr;

/// age-encrypted secret store. Key from PUSHIT_AGE_KEY (X25519 identity
/// string) or auto-generated at `$PUSHIT_DATA/age.key`.
#[derive(Clone)]
pub struct Secrets {
    identity: x25519::Identity,
    recipient: x25519::Recipient,
}

impl Secrets {
    pub fn load(key_path: &Path) -> Result<Self> {
        let identity = match std::env::var("PUSHIT_AGE_KEY") {
            Ok(k) => x25519::Identity::from_str(k.trim())
                .map_err(|e| anyhow::anyhow!("bad PUSHIT_AGE_KEY: {e}"))?,
            Err(_) => {
                if key_path.exists() {
                    x25519::Identity::from_str(std::fs::read_to_string(key_path)?.trim())
                        .map_err(|e| anyhow::anyhow!("bad age.key: {e}"))?
                } else {
                    if let Some(p) = key_path.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    let id = x25519::Identity::generate();
                    std::fs::write(key_path, id.to_string().expose_secret())?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(
                            key_path,
                            std::fs::Permissions::from_mode(0o600),
                        )?;
                    }
                    id
                }
            }
        };
        Ok(Secrets {
            recipient: identity.to_public(),
            identity,
        })
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let enc = age::Encryptor::with_recipients(std::iter::once(
            &self.recipient as &dyn age::Recipient,
        ))
        .expect("recipient provided");
        let mut out = vec![];
        let mut w = enc.wrap_output(&mut out)?;
        w.write_all(plaintext)?;
        w.finish()?;
        Ok(out)
    }

    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let dec = age::Decryptor::new_buffered(ciphertext)?;
        let mut out = vec![];
        let mut r = dec.decrypt(std::iter::once(
            &self.identity as &dyn age::Identity,
        ))?;
        r.read_to_end(&mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = Secrets::load(&dir.path().join("age.key")).unwrap();
        let ct = s.encrypt(b"hunter2").unwrap();
        assert_eq!(s.decrypt(&ct).unwrap(), b"hunter2");
        // key reload from disk yields same identity
        let s2 = Secrets::load(&dir.path().join("age.key")).unwrap();
        assert_eq!(s2.decrypt(&ct).unwrap(), b"hunter2");
    }
}
