//! 本地根 CA：首次运行自动生成并持久化到 ~/.miniproxy/，
//! 之后按域名动态签发 MITM 叶子证书（内存缓存）。
//!
//! rcgen 0.11 API：Certificate::from_params + serialize_*_with_signer。

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, ExtendedKeyUsagePurpose,
    KeyPair, KeyUsagePurpose,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

pub fn base_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".miniproxy")
}

pub struct Ca {
    pub cert_path: PathBuf,
    cert_pem: String,
    ca_cert: Certificate,
    /// 按域名缓存的 MITM ServerConfig（含该域名的叶子证书）
    cfg_cache: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
}

impl Ca {
    /// 加载已存在的 CA，否则生成新的并写盘。
    ///
    /// 若磁盘上的 CA 带 SubjectAlternativeName（历史版本生成的缺陷证书，见
    /// `has_subject_alt_name`），自动备份旧文件并重建——这种证书会让 OpenSSL 系客户端
    /// 以 `unsupported or invalid name syntax` 拒绝整条链，必须换掉。
    pub fn load_or_create() -> Result<Self, Box<dyn std::error::Error>> {
        let dir = base_dir();
        std::fs::create_dir_all(&dir)?;
        let cert_path = dir.join("ca.crt");
        let key_path = dir.join("ca.key");

        let mut cert_pem = String::new();
        let mut key_pem = String::new();
        if cert_path.exists() && key_path.exists() {
            cert_pem = std::fs::read_to_string(&cert_path)?;
            key_pem = std::fs::read_to_string(&key_path)?;
            if crate::util::pem_to_der(&cert_pem)
                .map(|d| has_subject_alt_name(&d))
                .unwrap_or(false)
            {
                let bak_cert = dir.join("ca.crt.legacy-san.bak");
                let bak_key = dir.join("ca.key.legacy-san.bak");
                // 先备份再重建。备份必须成功，否则宁可继续用旧证书也不要静默把它覆盖掉。
                let backed_up = std::fs::rename(&cert_path, &bak_cert)
                    .or_else(|_| std::fs::copy(&cert_path, &bak_cert).map(|_| ()))
                    .is_ok()
                    && std::fs::rename(&key_path, &bak_key)
                        .or_else(|_| std::fs::copy(&key_path, &bak_key).map(|_| ()))
                        .is_ok();
                if !backed_up {
                    eprintln!(
                        "  CA 证书       : 检测到旧证书带有非法的 SubjectAlternativeName 扩展，但备份失败，\n\
                         \x20                 已保留原文件继续运行。请手动备份后删除 {} 与 {}\n\
                         \x20                 再重启，即可自动重建（否则 curl / Node / Go / git 无法解密）",
                        cert_path.display(),
                        key_path.display()
                    );
                    return Err("旧 CA 备份失败，已中止重建".into());
                }
                eprintln!(
                    "  CA 证书       : 检测到旧证书带有非法的 SubjectAlternativeName 扩展\n\
                     \x20                 （会导致 curl / Node / Go / git 等客户端拒绝解密连接），已重建。\n\
                     \x20                 旧文件备份在 {} 和 {}\n\
                     \x20                 ⚠ 请在钥匙串里删除旧的 “MiniProxy Root CA” 并重新信任新的 ca.crt",
                    bak_cert.display(),
                    bak_key.display()
                );
                cert_pem.clear();
                key_pem.clear();
            }
        }
        if cert_pem.is_empty() || key_pem.is_empty() {
            let (c, k) = generate_ca()?;
            std::fs::write(&cert_path, &c)?;
            std::fs::write(&key_path, &k)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
            }
            cert_pem = c;
            key_pem = k;
        }

        // 用持久化的私钥重建 CA 证书对象（同 subject + 同密钥，信任链不变）
        let key_pair = KeyPair::from_pem(&key_pem)?;
        let ca_cert = build_ca_params(Some(key_pair))?;
        let _ = cert_pem.len(); // cert_pem 用于对外分发

        Ok(Self {
            cert_path,
            cert_pem,
            ca_cert,
            cfg_cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// 为指定域名签发叶子证书，返回 (证书 DER, PKCS#8 私钥 DER)。
    fn leaf_der(&self, host: &str) -> (Vec<u8>, Vec<u8>) {
        let leaf_key = KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256)
            .expect("生成叶子密钥失败");
        let mut params = CertificateParams::new(vec![host.to_string()]);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, host.to_string());
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_pair = Some(leaf_key);

        let leaf = Certificate::from_params(params).expect("构建叶子证书失败");
        (
            leaf.serialize_der_with_signer(&self.ca_cert)
                .expect("签发叶子证书失败"),
            leaf.serialize_private_key_der(),
        )
    }

    /// 构建 rustls ServerConfig（仅提供该 host 的证书 + 进程内缓存）。
    ///
    /// 缓存的是整份 ServerConfig（不只是证书），这样高并发下不再每个连接重建一遍。
    pub fn server_config_for(self: &Arc<Self>, host: &str) -> Arc<rustls::ServerConfig> {
        let mut cache = self.cfg_cache.lock().unwrap();
        cache
            .entry(host.to_string())
            .or_insert_with(|| {
                let (cert_der, key_der) = self.leaf_der(host);
                let key = rustls::sign::any_supported_type(&rustls::PrivateKey(key_der))
                    .expect("加载叶子私钥失败");
                let ck = Arc::new(rustls::sign::CertifiedKey {
                    cert: vec![rustls::Certificate(cert_der)],
                    key,
                    ocsp: None,
                    sct_list: None,
                });

                let mut cfg = rustls::ServerConfig::builder()
                    .with_safe_defaults()
                    .with_no_client_auth()
                    .with_cert_resolver(Arc::new(FixedResolver { ck }));
                // ALPN 协商 h2：浏览器对同一域名可以多路复用，不必开满 6 条 H1 连接、
                // 每条都做一次完整握手。hyper 服务端默认 ConnectionMode::Fallback，
                // 会从 h2 前言自动识别协议，服务端无需其它改动。
                // MINIPROXY_NO_H2=1 可退回纯 HTTP/1.1。
                cfg.alpn_protocols = if h2_enabled() {
                    vec![b"h2".to_vec(), b"http/1.1".to_vec()]
                } else {
                    vec![b"http/1.1".to_vec()]
                };
                Arc::new(cfg)
            })
            .clone()
    }
}

/// MITM 是否向客户端提供 HTTP/2（默认开启；`MINIPROXY_NO_H2=1` 关闭）。
fn h2_enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| {
        !matches!(
            std::env::var("MINIPROXY_NO_H2").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
    })
}

/// 构建根 CA 的参数。
///
/// 注意：根 CA **不能**带 SubjectAlternativeName。`CertificateParams::new(vec![...])` 会把
/// 传入的字符串当成 DNS 名写进 SAN，于是得到一个 `DNS:MiniProxy Root CA`（含空格、非法）
/// 的名字，OpenSSL 校链时直接报 `unsupported or invalid name syntax` 并拒绝整条链——
/// 表现为 curl / Node / Go / Python requests / git 全部无法通过这个代理访问 HTTPS，
/// 而 Chrome 因为不检查 CA 的 SAN 所以看起来正常。这里用 `default()` 只设 DN。
fn build_ca_params(
    key_pair: Option<KeyPair>,
) -> Result<Certificate, Box<dyn std::error::Error>> {
    let mut params = CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(BasicConstraints::Unconstrained);
    let mut dn = DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "MiniProxy Root CA");
    dn.push(rcgen::DnType::OrganizationName, "MiniProxy");
    params.distinguished_name = dn;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2044, 12, 31);
    params.key_pair = key_pair;
    Ok(Certificate::from_params(params)?)
}

/// 证书里是否含 SubjectAlternativeName 扩展（OID 2.5.29.17，DER 编码为
/// `06 03 55 1D 11`）。用于识别历史版本生成的、带非法 DNS SAN 的根证书。
fn has_subject_alt_name(der: &[u8]) -> bool {
    const SAN_OID: [u8; 5] = [0x06, 0x03, 0x55, 0x1D, 0x11];
    der.windows(SAN_OID.len()).any(|w| w == SAN_OID)
}

fn generate_ca() -> Result<(String, String), Box<dyn std::error::Error>> {
    let key_pair = KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let cert = build_ca_params(Some(key_pair))?;
    let cert_pem = cert.serialize_pem()?;
    let key_pem = cert.serialize_private_key_pem();
    Ok((cert_pem, key_pem))
}

struct FixedResolver {
    ck: Arc<rustls::sign::CertifiedKey>,
}

impl rustls::server::ResolvesServerCert for FixedResolver {
    fn resolve(
        &self,
        _hello: rustls::server::ClientHello,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.ck.clone())
    }
}
