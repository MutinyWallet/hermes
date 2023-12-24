use crate::utils::empty_string_as_none;
use fedimint_core::Amount;
use nostr::prelude::XOnlyPublicKey;
use serde::ser::{SerializeTuple, Serializer};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LnurlType {
    PayRequest,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LnurlStatus {
    Ok,
    Error,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum MetadataType {
    TextPlain,
    ImagePngBase64,
    ImageJpegBase64,
    TextEmail,
    TextIdentifier,
}

#[derive(Deserialize)]
pub struct MetadataEntry {
    pub metadata_type: MetadataType,
    pub content: String,
}

impl Serialize for MetadataEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut tup = serializer.serialize_tuple(2)?;
        tup.serialize_element(&format!("{:?}", self.metadata_type))?;
        tup.serialize_element(&self.content)?;
        tup.end()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LnurlWellKnownResponse {
    pub callback: Url,
    pub max_sendable: Amount,
    pub min_sendable: Amount,
    pub metadata: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_allowed: Option<i32>,
    pub tag: LnurlType,
    pub status: LnurlStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nostr_pubkey: Option<XOnlyPublicKey>,
    pub allows_nostr: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LnurlCallbackParams {
    pub amount: u64, // User specified amount in MilliSatoshi
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub nonce: Option<String>, // Optional parameter used to prevent server response caching
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub comment: Option<String>, // Optional parameter to pass the LN WALLET user's comment to LN SERVICE
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub proofofpayer: Option<String>, // Optional ephemeral secp256k1 public key generated by payer
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LnurlCallbackSuccessAction {
    pub tag: String,
    pub message: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LnurlCallbackResponse {
    pub status: LnurlStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub pr: String, // BOLT11 invoice
    pub verify: Url,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_action: Option<LnurlCallbackSuccessAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routes: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LnurlVerifyResponse {
    pub status: LnurlStatus,
    pub settled: bool,
    pub preimage: String,
    pub pr: String,
}
