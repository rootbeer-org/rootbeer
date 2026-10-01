use crate::Error;
use data_encoding::{BASE64, HEXLOWER};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::Deserialize;

const PAYLOAD_TYPE: &str = "application/vnd.rootbeer.bootstrap.v1+json";

#[derive(Debug)]
pub struct Release(pub(crate) Payload);

#[derive(Debug, Deserialize)]
pub(crate) struct Payload {
    pub(crate) platform: String,
    pub(crate) serial: u64,
    pub(crate) version: String,
    pub(crate) url: String,
    pub(crate) sha256: String,
}

impl Release {
    pub fn serial(&self) -> u64 {
        self.0.serial
    }

    pub fn version(&self) -> &str {
        &self.0.version
    }
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "payloadType")]
    payload_type: String,
    payload: String,
    signatures: Vec<Signature>,
}

#[derive(Deserialize)]
struct Signature {
    sig: String,
}

pub(crate) fn newer(
    document: &[u8],
    keys: &[[u8; 32]],
    platform: &str,
    serial: u64,
) -> Result<Option<Release>, Error> {
    let envelope: Envelope =
        serde_json::from_slice(document).map_err(|error| Error::Invalid(error.to_string()))?;

    if envelope.payload_type != PAYLOAD_TYPE {
        return Err(Error::Invalid(format!(
            "payload type {:?}",
            envelope.payload_type
        )));
    }

    let payload = BASE64
        .decode(envelope.payload.as_bytes())
        .map_err(|_| Error::Invalid("payload encoding".into()))?;

    let message = pae(&envelope.payload_type, &payload);
    let is_trusted = envelope
        .signatures
        .iter()
        .filter_map(|signature| BASE64.decode(signature.sig.as_bytes()).ok())
        .any(|signature| {
            keys.iter().any(|key| {
                UnparsedPublicKey::new(&ED25519, key)
                    .verify(&message, &signature)
                    .is_ok()
            })
        });

    if !is_trusted {
        return Err(Error::Untrusted);
    }

    let payload: Payload =
        serde_json::from_slice(&payload).map_err(|error| Error::Invalid(error.to_string()))?;

    payload.validate(platform)?;
    Ok(Some(Release(payload)).filter(|release| release.serial() > serial))
}

impl Payload {
    pub(crate) fn validate(&self, platform: &str) -> Result<(), Error> {
        if self.platform != platform {
            return Err(Error::Invalid(format!("platform {:?}", self.platform)));
        }

        if !self.url.starts_with("https://") {
            return Err(Error::Invalid(format!("url {:?} must be https", self.url)));
        }

        let is_digest = HEXLOWER
            .decode(self.sha256.as_bytes())
            .is_ok_and(|digest| digest.len() == 32);

        if !is_digest {
            return Err(Error::Invalid(format!("sha256 {:?}", self.sha256)));
        }

        Ok(())
    }
}

fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let header = format!(
        "DSSEv1 {} {payload_type} {} ",
        payload_type.len(),
        payload.len()
    );

    [header.as_bytes(), payload].concat()
}
