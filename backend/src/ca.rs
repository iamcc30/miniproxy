//! 本地根 CA：首次运行自动生成并持久化到 ~/.miniproxy/，
//! 之后按域名动态签发 MITM 叶子证书（内存缓存）。
//!
//! rcgen 0.11 API：Certificate::from_params + serialize_*_with_signer。

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, ExtendedKeyUsagePurpose, KeyPair,
    KeyUsagePurpose,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub fn base_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".miniproxy")
}

pub struct Ca {
    pub cert_path: PathBuf,
    cert_pem: String,
    ca_cert: Certificate,
    cert_cache: Mutex<HashMap<String, Arc<rustls::sign::CertifiedKey>>>,
}

impl Ca {
    /// 加载已存在的 CA，否则生成新的并写盘。
    pub fn load_or_create() -> Result<Self, Box<dyn std::error::Error>> {
        let dir = base_dir();
        std::fs::create_dir_all(&dir)?;
        let cert_path = dir.join("ca.crt");
        let key_path = dir.join("ca.key");

        let (cert_pem, key_pem) = if cert_path.exists() && key_path.exists() {
            let c = std::fs::read_to_string(&cert_path)?;
            let k = std::fs::read_to_string(&key_path)?;
            (c, k)
        } else {
            let (c, k) = generate_ca()?;
            std::fs::write(&cert_path, &c)?;
            std::fs::write(&key_path, &k)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
            }
            (c, k)
        };

        // 用持久化的私钥重建 CA 证书对象（同 subject + 同密钥，信任链不变）
        let key_pair = KeyPair::from_pem(&key_pem)?;
        let ca_cert = build_ca_params(Some(key_pair))?;
        let _ = cert_pem.len(); // cert_pem 用于对外分发

        Ok(Self {
            cert_path,
            cert_pem,
            ca_cert,
            cert_cache: Mutex::new(HashMap::new()),
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

    /// 构建 rustls ServerConfig（仅提供该 host 的证书，ALPN 只允许 HTTP/1.1）。
    pub fn server_config_for(self: &Arc<Self>, host: &str) -> Arc<rustls::ServerConfig> {
        let ck = {
            let mut cache = self.cert_cache.lock().unwrap();
            cache
                .entry(host.to_string())
                .or_insert_with(|| {
                    let (cert_der, key_der) = self.leaf_der(host);
                    let key = rustls::sign::any_supported_type(&rustls::PrivateKey(key_der))
                        .expect("加载叶子私钥失败");
                    Arc::new(rustls::sign::CertifiedKey {
                        cert: vec![rustls::Certificate(cert_der)],
                        key,
                        ocsp: None,
                        sct_list: None,
                    })
                })
                .clone()
        };

        let mut cfg = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(FixedResolver { ck }));
        // 关闭 ALPN：强制浏览器走 HTTP/1.1（服务端 MITM 仅实现 http1）
        cfg.alpn_protocols = vec![];
        Arc::new(cfg)
    }
}

fn build_ca_params(
    key_pair: Option<KeyPair>,
) -> Result<Certificate, Box<dyn std::error::Error>> {
    let mut params =
        CertificateParams::new(vec!["MiniProxy Root CA".to_string()]);
    params.is_ca = rcgen::IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "MiniProxy Root CA");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "MiniProxy");
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
