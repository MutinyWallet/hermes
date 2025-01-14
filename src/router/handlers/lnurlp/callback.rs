use std::{str::FromStr, time::Duration};

use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use fedimint_client::{oplog::UpdateStreamOrOutcome, ClientArc};
use fedimint_core::{config::FederationId, core::OperationId, task::spawn, Amount};
use fedimint_ln_client::{LightningClientModule, LnReceiveState};
use fedimint_mint_client::{MintClientModule, OOBNotes};
use futures::StreamExt;
use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
use nostr::bitcoin::hashes::sha256::Hash as Sha256;
use nostr::hashes::Hash;
use nostr::key::{Secp256k1, SecretKey};
use nostr::prelude::rand::rngs::OsRng;
use nostr::prelude::rand::RngCore;
use nostr::secp256k1::XOnlyPublicKey;
use nostr::{Event, EventBuilder, JsonUtil, Kind};
use nostr_sdk::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info};
use url::Url;
use xmpp::{parsers::message::MessageType, Jid};

use crate::model::zap::{Zap, ZapBmc};
use crate::model::{invoice_state::InvoiceState, ModelManager};
use crate::{
    config::CONFIG,
    error::AppError,
    model::{
        app_user_relays::AppUserRelaysBmc,
        invoice::{InvoiceBmc, InvoiceForCreate},
    },
    router::handlers::{nostr::AppUserRelays, NameOrPubkey},
    state::AppState,
    utils::{create_xmpp_client, empty_string_as_none},
};

use super::LnurlStatus;

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
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub nostr: Option<String>, // Optional zap request
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

const MIN_AMOUNT: u64 = 1000;

#[axum_macros::debug_handler]
pub async fn handle_callback(
    Path(username): Path<String>,
    Query(params): Query<LnurlCallbackParams>,
    State(state): State<AppState>,
) -> Result<Json<LnurlCallbackResponse>, AppError> {
    info!("callback called with username: {}", username);
    if params.amount < MIN_AMOUNT {
        return Err(AppError {
            error: anyhow::anyhow!("Amount < MIN_AMOUNT"),
            status: StatusCode::BAD_REQUEST,
        });
    }

    // verify nostr param is a zap request
    if params
        .nostr
        .as_ref()
        .is_some_and(|n| Event::from_json(n).is_ok_and(|e| e.kind == Kind::ZapRequest))
    {
        return Err(AppError {
            error: anyhow::anyhow!("Invalid nostr event"),
            status: StatusCode::BAD_REQUEST,
        });
    }

    let nip05relays = AppUserRelaysBmc::get_by(&state.mm, NameOrPubkey::Name, &username).await?;
    let federation_id = FederationId::from_str(&nip05relays.federation_id).map_err(|e| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            anyhow::anyhow!("Invalid federation_id: {}", e),
        )
    })?;

    let locked_clients = state.fm.clients.lock().await.clone();
    let client = locked_clients.get(&federation_id).ok_or_else(|| {
        AppError::new(
            StatusCode::BAD_REQUEST,
            anyhow::anyhow!("FederationId not found in multimint map"),
        )
    })?;

    let ln = client.get_first_module::<LightningClientModule>();

    let (op_id, pr) = ln
        .create_bolt11_invoice(
            Amount {
                msats: params.amount,
            },
            "test invoice".to_string(), // todo set description hash properly
            None,
            (),
        )
        .await?;

    // insert invoice into db for later verification
    let id = InvoiceBmc::create(
        &state.mm,
        InvoiceForCreate {
            op_id: op_id.to_string(),
            federation_id: nip05relays.federation_id.clone(),
            app_user_id: nip05relays.app_user_id,
            amount: params.amount as i64,
            bolt11: pr.to_string(),
        },
    )
    .await?;

    // save nostr zap request
    if let Some(request) = params.nostr {
        ZapBmc::create(
            &state.mm,
            Zap {
                id,
                request,
                event_id: None,
            },
        )
        .await?;
    }

    // create subscription to operation
    let subscription = ln
        .subscribe_ln_receive(op_id)
        .await
        .expect("subscribing to a just created operation can't fail");

    spawn_invoice_subscription(state, id, nip05relays, subscription).await;

    let verify_url = format!(
        "http://{}:{}/lnurlp/{}/verify/{}",
        CONFIG.domain, CONFIG.port, username, op_id
    );

    let res = LnurlCallbackResponse {
        pr: pr.to_string(),
        success_action: None,
        status: LnurlStatus::Ok,
        reason: None,
        verify: verify_url.parse()?,
        routes: Some(vec![]),
    };

    Ok(Json(res))
}

pub(crate) async fn spawn_invoice_subscription(
    state: AppState,
    id: i32,
    userrelays: AppUserRelays,
    subscription: UpdateStreamOrOutcome<LnReceiveState>,
) {
    spawn("waiting for invoice being paid", async move {
        let locked_clients = state.fm.clients.lock().await;
        let client = locked_clients
            .get(&FederationId::from_str(&userrelays.federation_id).unwrap())
            .unwrap();
        let nostr = state.nostr.clone();
        let mut stream = subscription.into_stream();
        while let Some(op_state) = stream.next().await {
            match op_state {
                LnReceiveState::Canceled { reason } => {
                    error!("Payment canceled, reason: {:?}", reason);
                    InvoiceBmc::set_state(&state.mm, id, InvoiceState::Cancelled)
                        .await
                        .expect("settling invoice can't fail");
                    break;
                }
                LnReceiveState::Claimed => {
                    info!("Payment claimed");
                    let invoice = InvoiceBmc::set_state(&state.mm, id, InvoiceState::Settled)
                        .await
                        .expect("settling invoice can't fail");
                    notify_user(
                        client,
                        &nostr,
                        &state.mm,
                        id,
                        invoice.amount as u64,
                        userrelays.clone(),
                    )
                    .await
                    .expect("notifying user can't fail");
                    break;
                }
                _ => {}
            }
        }
    });
}

async fn notify_user(
    client: &ClientArc,
    nostr: &Client,
    mm: &ModelManager,
    id: i32,
    amount: u64,
    app_user_relays: AppUserRelays,
) -> Result<(), Box<dyn std::error::Error>> {
    let mint = client.get_first_module::<MintClientModule>();
    let (operation_id, notes) = mint
        .spend_notes(Amount::from_msats(amount), Duration::from_secs(604800), ())
        .await?;
    match app_user_relays.dm_type.as_str() {
        "nostr" => send_nostr_dm(nostr, &app_user_relays, operation_id, amount, notes).await,
        "xmpp" => send_xmpp_msg(&app_user_relays, operation_id, amount, notes).await,
        _ => Err(anyhow::anyhow!("Unsupported dm_type")),
    }?;

    // Send zap if needed
    if let Ok(zap) = ZapBmc::get(&mm, id).await {
        let request = Event::from_json(zap.request)?;
        let event = create_zap_event(request, amount)?;

        let event_id = nostr.send_event(event).await?;
        info!("Broadcasted zap {event_id}!");

        ZapBmc::set_event_id(&mm, id, event_id).await?;
    }

    Ok(())
}

async fn send_nostr_dm(
    nostr: &Client,
    app_user_relays: &AppUserRelays,
    operation_id: OperationId,
    amount: u64,
    notes: OOBNotes,
) -> Result<()> {
    let dm = nostr
        .send_direct_msg(
            XOnlyPublicKey::from_str(&app_user_relays.pubkey).unwrap(),
            json!({
                "operationId": operation_id,
                "amount": amount,
                "notes": notes.to_string(),
            })
            .to_string(),
            None,
        )
        .await?;

    info!("Sent nostr dm: {dm}");
    Ok(())
}

// TODO: add xmpp to registration
async fn send_xmpp_msg(
    app_user_relays: &AppUserRelays,
    operation_id: OperationId,
    amount: u64,
    notes: OOBNotes,
) -> Result<()> {
    let mut xmpp_client = create_xmpp_client()?;
    let recipient = xmpp::BareJid::new(&format!(
        "{}@{}",
        app_user_relays.name, CONFIG.xmpp_chat_server
    ))?;

    xmpp_client
        .send_message(
            Jid::Bare(recipient),
            MessageType::Chat,
            "en",
            &json!({
                "operationId": operation_id,
                "amount": amount,
                "notes": notes.to_string(),
            })
            .to_string(),
        )
        .await;

    Ok(())
}

/// Creates a nostr zap event with a fake invoice
fn create_zap_event(request: Event, amt_msats: u64) -> Result<Event> {
    let preimage = &mut [0u8; 32];
    OsRng.fill_bytes(preimage);
    let invoice_hash = Sha256::hash(preimage);

    let payment_secret = &mut [0u8; 32];
    OsRng.fill_bytes(payment_secret);

    let priv_key_bytes = &mut [0u8; 32];
    OsRng.fill_bytes(priv_key_bytes);
    let private_key = SecretKey::from_slice(priv_key_bytes)?;

    let desc_hash = Sha256::hash(request.as_json().as_bytes());

    let fake_invoice = InvoiceBuilder::new(Currency::Bitcoin)
        .amount_milli_satoshis(amt_msats)
        .description_hash(desc_hash)
        .current_timestamp()
        .payment_hash(invoice_hash)
        .payment_secret(PaymentSecret(*payment_secret))
        .min_final_cltv_expiry_delta(144)
        .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))?;

    let event = EventBuilder::new_zap_receipt(
        fake_invoice.to_string(),
        Some(hex::encode(preimage)),
        request,
    )
    .to_event(&CONFIG.nostr_sk)?;

    Ok(event)
}
