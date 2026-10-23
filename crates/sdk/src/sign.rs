//! Local driver signing. A key that exists only in memory signs the catalog
//! once and is destroyed with the [`Signer`]; only the public certificate is
//! trusted, so nothing else can ever be signed with it.

use crate::win::{check, hr, wide};
use std::ffi::c_void;
use std::io;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use windows_sys::Win32::Foundation::FALSE;
use windows_sys::Win32::Security::Cryptography::Catalog::{
    CRYPTCATMEMBER, CryptCATCDFClose, CryptCATCDFEnumMembersByCDFTagEx, CryptCATCDFOpen,
};
use windows_sys::Win32::Security::Cryptography::*;
use windows_sys::core::{PCWSTR, PWSTR, w};

pub(crate) const SUBJECT_NAME: &str = "hidshim local driver signing";
const SUBJECT: PCWSTR = w!("CN=hidshim local driver signing");
const TRUST_STORES: [&str; 2] = ["Root", "TrustedPublisher"];

// DER values of the certificate extensions.
const BASIC_CONSTRAINTS_END_ENTITY: &[u8] = &[0x30, 0x00];
const KEY_USAGE_DIGITAL_SIGNATURE: &[u8] = &[0x03, 0x02, 0x07, 0x80];
const EKU_CODE_SIGNING: &[u8] = &[
    0x30, 0x0a, 0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x03,
];

pub(crate) struct Signer {
    key: NCRYPT_KEY_HANDLE,
    cert: *const CERT_CONTEXT,
}

impl Signer {
    pub(crate) fn new() -> io::Result<Self> {
        let key = ephemeral_key()?;
        let mut signer = Self { key, cert: null() };
        signer.cert = self_signed(key)?;
        // A key without a name has no CERT_KEY_PROV_INFO; the signer finds it
        // through the cached key context instead.
        let context = CERT_KEY_CONTEXT {
            cbSize: size_of::<CERT_KEY_CONTEXT>() as u32,
            Anonymous: CERT_KEY_CONTEXT_0 { hNCryptKey: key },
            dwKeySpec: CERT_NCRYPT_KEY_SPEC,
        };
        // SAFETY: cert is valid; NO_CRYPT_RELEASE leaves the key to Drop,
        // which frees the certificate first.
        check(unsafe {
            CertSetCertificateContextProperty(
                signer.cert,
                CERT_KEY_CONTEXT_PROP_ID,
                CERT_STORE_NO_CRYPT_RELEASE_FLAG,
                (&raw const context).cast(),
            )
        })?;
        Ok(signer)
    }

    pub(crate) fn certificate(&self) -> Vec<u8> {
        // SAFETY: cert is a valid context for the lifetime of self.
        unsafe {
            let cert = &*self.cert;
            std::slice::from_raw_parts(cert.pbCertEncoded, cert.cbCertEncoded as usize).to_vec()
        }
    }

    pub(crate) fn sign(&self, file: &Path) -> io::Result<()> {
        let path = wide(file);
        let mut file_info = SIGNER_FILE_INFO {
            cbSize: size_of::<SIGNER_FILE_INFO>() as u32,
            pwszFileName: path.as_ptr(),
            hFile: null_mut(),
        };
        let mut index = 0;
        let subject = SIGNER_SUBJECT_INFO {
            cbSize: size_of::<SIGNER_SUBJECT_INFO>() as u32,
            pdwIndex: &mut index,
            dwSubjectChoice: SIGNER_SUBJECT_FILE,
            Anonymous: SIGNER_SUBJECT_INFO_0 {
                pSignerFileInfo: &mut file_info,
            },
        };
        // CHAIN embeds the self-signed certificate; CHAIN_NO_ROOT would drop it.
        let mut store_info = SIGNER_CERT_STORE_INFO {
            cbSize: size_of::<SIGNER_CERT_STORE_INFO>() as u32,
            pSigningCert: self.cert,
            dwCertPolicy: SIGNER_CERT_POLICY_CHAIN,
            hCertStore: null_mut(),
        };
        let cert = SIGNER_CERT {
            cbSize: size_of::<SIGNER_CERT>() as u32,
            dwCertChoice: SIGNER_CERT_STORE,
            Anonymous: SIGNER_CERT_0 {
                pCertStoreInfo: &mut store_info,
            },
            hwnd: null_mut(),
        };
        let signature = SIGNER_SIGNATURE_INFO {
            cbSize: size_of::<SIGNER_SIGNATURE_INFO>() as u32,
            algidHash: CALG_SHA_256,
            dwAttrChoice: SIGNER_NO_ATTR,
            ..Default::default()
        };
        let mut context = null_mut();
        // SAFETY: every pointer refers to a local that outlives the call.
        hr(unsafe {
            SignerSignEx2(
                0,
                &subject,
                &cert,
                &signature,
                null(),
                0,
                null(),
                null(),
                null(),
                null(),
                &mut context,
                null(),
                null(),
            )
        })?;
        if !context.is_null() {
            // SAFETY: context came from SignerSignEx2.
            unsafe { SignerFreeSignerContext(context) };
        }
        Ok(())
    }
}

impl Drop for Signer {
    fn drop(&mut self) {
        // SAFETY: both were created in new; the certificate refers to the
        // key, so it is freed first.
        unsafe {
            if !self.cert.is_null() {
                CertFreeCertificateContext(self.cert);
            }
            NCryptFreeObject(self.key);
        }
    }
}

fn ephemeral_key() -> io::Result<NCRYPT_KEY_HANDLE> {
    let mut provider = 0;
    // SAFETY: out-pointer to a local.
    hr(unsafe { NCryptOpenStorageProvider(&mut provider, MS_KEY_STORAGE_PROVIDER, 0) })?;
    let mut key = 0;
    let bits: u32 = 3072;
    // SAFETY: a NULL key name makes the key ephemeral; it is never written
    // to the key store.
    let result = unsafe {
        hr(NCryptCreatePersistedKey(
            provider,
            &mut key,
            BCRYPT_RSA_ALGORITHM,
            null(),
            0,
            0,
        ))
        .and_then(|()| {
            hr(NCryptSetProperty(
                key,
                NCRYPT_LENGTH_PROPERTY,
                (&raw const bits).cast(),
                4,
                0,
            ))
        })
        .and_then(|()| hr(NCryptFinalizeKey(key, 0)))
    };
    // SAFETY: provider and key came from the calls above.
    unsafe {
        NCryptFreeObject(provider);
        if result.is_err() && key != 0 {
            NCryptFreeObject(key);
        }
    }
    result.map(|()| key)
}

fn self_signed(key: NCRYPT_KEY_HANDLE) -> io::Result<*const CERT_CONTEXT> {
    let mut name_len = 0;
    // SAFETY: size query, then fill a buffer of that size.
    check(unsafe {
        CertStrToNameW(
            X509_ASN_ENCODING,
            SUBJECT,
            CERT_X500_NAME_STR,
            null(),
            null_mut(),
            &mut name_len,
            null_mut(),
        )
    })?;
    let mut name = vec![0u8; name_len as usize];
    check(unsafe {
        CertStrToNameW(
            X509_ASN_ENCODING,
            SUBJECT,
            CERT_X500_NAME_STR,
            null(),
            name.as_mut_ptr(),
            &mut name_len,
            null_mut(),
        )
    })?;
    let subject = CRYPT_INTEGER_BLOB {
        cbData: name_len,
        pbData: name.as_mut_ptr(),
    };

    let extension = |oid: &'static [u8], critical: bool, value: &'static [u8]| CERT_EXTENSION {
        pszObjId: oid.as_ptr().cast_mut(),
        fCritical: critical.into(),
        Value: CRYPT_INTEGER_BLOB {
            cbData: value.len() as u32,
            pbData: value.as_ptr().cast_mut(),
        },
    };
    let mut extensions = [
        extension(b"2.5.29.19\0", true, BASIC_CONSTRAINTS_END_ENTITY),
        extension(b"2.5.29.15\0", true, KEY_USAGE_DIGITAL_SIGNATURE),
        extension(b"2.5.29.37\0", false, EKU_CODE_SIGNING),
    ];
    let extensions = CERT_EXTENSIONS {
        cExtension: extensions.len() as u32,
        rgExtension: extensions.as_mut_ptr(),
    };
    let algorithm = CRYPT_ALGORITHM_IDENTIFIER {
        pszObjId: c"1.2.840.113549.1.1.11".as_ptr().cast_mut().cast(),
        Parameters: CRYPT_INTEGER_BLOB::default(),
    };

    // The catalog carries no timestamp, so the certificate must outlive
    // every future signature check of the installed package.
    let mut end = Default::default();
    // SAFETY: out-pointer to a local.
    unsafe { windows_sys::Win32::System::SystemInformation::GetSystemTime(&mut end) };
    end.wYear += 100;

    // SAFETY: all pointers refer to locals that outlive the call.
    let cert = unsafe {
        CertCreateSelfSignCertificate(
            key,
            &subject,
            CERT_CREATE_SELFSIGN_NO_KEY_INFO,
            null(),
            &algorithm,
            null(),
            &end,
            &extensions,
        )
    };
    if cert.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(cert)
}

static PARSE_ERROR: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn on_parse_error(area: u32, error: u32, _line: PCWSTR) {
    PARSE_ERROR.store(area << 16 | error, Ordering::Relaxed);
}

/// Writes `<dir>\<name>` covering `files`, which lie in `dir`. The directory
/// path must be ASCII: the CDF it goes through is read as ANSI.
pub(crate) fn make_catalog(dir: &Path, name: &str, files: &[&str]) -> io::Result<PathBuf> {
    use std::fmt::Write;
    let mut cdf = format!(
        "[CatalogHeader]\r\nName={name}\r\nResultDir={}\r\nPublicVersion=0x0000001\r\n\
         EncodingType=0x00010001\r\nCatalogVersion=2\r\nHashAlgorithms=SHA256\r\n\
         CATATTR1=0x10010001:OSAttr:2:10.0\r\n[CatalogFiles]\r\n",
        dir.display()
    );
    for file in files {
        writeln!(cdf, "<hash>{file}={}\r", dir.join(file).display()).unwrap();
    }
    let cdf_path = dir.join("catalog.cdf");
    std::fs::write(&cdf_path, cdf)?;

    PARSE_ERROR.store(0, Ordering::Relaxed);
    let path = wide(&cdf_path);
    // SAFETY: path is NUL-terminated; the CDF handle is closed below.
    unsafe {
        let handle = CryptCATCDFOpen(path.as_ptr(), Some(on_parse_error));
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut tag: PWSTR = null_mut();
        let mut member: *const CRYPTCATMEMBER = null();
        loop {
            tag = CryptCATCDFEnumMembersByCDFTagEx(
                handle,
                tag,
                Some(on_parse_error),
                (&raw mut member).cast_const(),
                FALSE,
                null(),
            );
            if tag.is_null() {
                break;
            }
        }
        check(CryptCATCDFClose(handle))?;
    }
    match PARSE_ERROR.load(Ordering::Relaxed) {
        0 => Ok(dir.join(name)),
        code => Err(io::Error::other(format!(
            "catalog definition rejected: {code:#010x}"
        ))),
    }
}

/// Replaces any earlier hidshim certificate in the machine trust stores.
pub(crate) fn trust(certificate: &[u8]) -> io::Result<()> {
    for name in TRUST_STORES {
        let store = MachineStore::open(name)?;
        store.remove_ours()?;
        // SAFETY: certificate is a DER blob of the given length.
        check(unsafe {
            CertAddEncodedCertificateToStore(
                store.0,
                X509_ASN_ENCODING,
                certificate.as_ptr(),
                certificate.len() as u32,
                CERT_STORE_ADD_REPLACE_EXISTING,
                null_mut(),
            )
        })?;
    }
    Ok(())
}

pub(crate) fn untrust() -> io::Result<()> {
    TRUST_STORES
        .iter()
        .try_for_each(|name| MachineStore::open(name)?.remove_ours())
}

struct MachineStore(HCERTSTORE);

impl MachineStore {
    fn open(name: &str) -> io::Result<Self> {
        let name = wide(name);
        // SAFETY: name is NUL-terminated and read during the call only.
        let store = unsafe {
            CertOpenStore(
                CERT_STORE_PROV_SYSTEM_W,
                0,
                0,
                CERT_SYSTEM_STORE_LOCAL_MACHINE_ID << CERT_SYSTEM_STORE_LOCATION_SHIFT,
                name.as_ptr().cast::<c_void>(),
            )
        };
        if store.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(store))
    }

    fn remove_ours(&self) -> io::Result<()> {
        let subject = wide(SUBJECT_NAME);
        let mut found: *const CERT_CONTEXT = null();
        loop {
            // SAFETY: found is null or the previous result, as the API expects.
            found = unsafe {
                CertFindCertificateInStore(
                    self.0,
                    X509_ASN_ENCODING,
                    0,
                    CERT_FIND_SUBJECT_STR_W,
                    subject.as_ptr().cast(),
                    found,
                )
            };
            if found.is_null() {
                return Ok(());
            }
            if subject_name(found) == SUBJECT_NAME {
                // SAFETY: deleting a duplicate keeps found valid for the
                // next search step.
                check(unsafe {
                    CertDeleteCertificateFromStore(CertDuplicateCertificateContext(found))
                })?;
            }
        }
    }
}

impl Drop for MachineStore {
    fn drop(&mut self) {
        // SAFETY: the store was opened in open.
        unsafe { CertCloseStore(self.0, 0) };
    }
}

fn subject_name(cert: *const CERT_CONTEXT) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: buf length is passed; cert is valid.
    let len = unsafe {
        CertGetNameStringW(
            cert,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            null(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    String::from_utf16_lossy(&buf[..len.saturating_sub(1) as usize])
}

/// `WinVerifyTrust` result for a signed file: 0 when its signature is intact
/// and chains to a trusted root.
pub(crate) fn verify(file: &Path) -> i32 {
    use windows_sys::Win32::Security::WinTrust::*;
    let path = wide(file);
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: path.as_ptr(),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_IGNORE,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: all pointers refer to locals that outlive the call.
    unsafe { WinVerifyTrust(null_mut(), &mut action, (&raw mut data).cast()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_catalog_is_intact_but_untrusted() {
        let dir = std::env::temp_dir().join(format!("hidshim-sign-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.inf"),
            "[Version]\r\nSignature=\"$WINDOWS NT$\"\r\n",
        )
        .unwrap();
        std::fs::write(dir.join("a.dll"), [0u8; 64]).unwrap();

        let catalog = make_catalog(&dir, "a.cat", &["a.dll", "a.inf"]).unwrap();
        let signer = Signer::new().unwrap();
        signer.sign(&catalog).unwrap();
        let certificate = signer.certificate();
        drop(signer);

        const CERT_E_UNTRUSTEDROOT: i32 = 0x800b0109_u32 as i32;
        assert_eq!(verify(&catalog), CERT_E_UNTRUSTEDROOT);
        assert_eq!(chain_errors_with_own_root(&certificate), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Chain status when the certificate is the only trusted root, as it
    /// is after `trust`.
    fn chain_errors_with_own_root(certificate: &[u8]) -> u32 {
        // SAFETY: every handle is created and released here.
        unsafe {
            let cert = CertCreateCertificateContext(
                X509_ASN_ENCODING,
                certificate.as_ptr(),
                certificate.len() as u32,
            );
            let roots = CertOpenStore(CERT_STORE_PROV_MEMORY, 0, 0, 0, null());
            CertAddCertificateContextToStore(roots, cert, CERT_STORE_ADD_ALWAYS, null_mut());
            let config = CERT_CHAIN_ENGINE_CONFIG {
                cbSize: size_of::<CERT_CHAIN_ENGINE_CONFIG>() as u32,
                hExclusiveRoot: roots,
                ..Default::default()
            };
            let mut engine = null_mut();
            assert_ne!(CertCreateCertificateChainEngine(&config, &mut engine), 0);
            let mut usage = c"1.3.6.1.5.5.7.3.3".as_ptr().cast_mut().cast::<u8>();
            let para = CERT_CHAIN_PARA {
                cbSize: size_of::<CERT_CHAIN_PARA>() as u32,
                RequestedUsage: CERT_USAGE_MATCH {
                    dwType: USAGE_MATCH_TYPE_AND,
                    Usage: CTL_USAGE {
                        cUsageIdentifier: 1,
                        rgpszUsageIdentifier: &mut usage,
                    },
                },
                ..std::mem::zeroed()
            };
            let mut chain = null_mut();
            assert_ne!(
                CertGetCertificateChain(
                    engine,
                    cert,
                    null(),
                    null_mut(),
                    &para,
                    0,
                    null(),
                    &mut chain
                ),
                0
            );
            let errors = (*chain).TrustStatus.dwErrorStatus;
            CertFreeCertificateChain(chain);
            CertFreeCertificateChainEngine(engine);
            CertCloseStore(roots, 0);
            CertFreeCertificateContext(cert);
            errors
        }
    }
}
