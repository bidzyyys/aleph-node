use codec::{Decode, Encode};
use log::info;
use pallet_elections::CommitteeSeats;
use primitives::SessionIndex;
use sp_core::{Pair, H256};
use substrate_api_client::{
    compose_call, compose_extrinsic, AccountId, ExtrinsicParams, FromHexString, XtStatus,
};

use crate::{
    send_xt, waiting::wait_for_event, AnyConnection, BlockNumber, RootConnection, SignedConnection,
};

// Using custom struct and rely on default Encode trait from Parity's codec
// it works since byte arrays are encoded in a straight forward way, it as-is
#[derive(Debug, Encode, Clone)]
pub struct Keys {
    pub aura: [u8; 32],
    pub aleph: [u8; 32],
}

// Manually implementing decoding
impl From<Vec<u8>> for Keys {
    fn from(bytes: Vec<u8>) -> Self {
        assert_eq!(bytes.len(), 64);
        Self {
            aura: bytes[0..32].try_into().unwrap(),
            aleph: bytes[32..64].try_into().unwrap(),
        }
    }
}

impl TryFrom<String> for Keys {
    type Error = ();

    fn try_from(keys: String) -> Result<Self, Self::Error> {
        let bytes: Vec<u8> = match FromHexString::from_hex(keys) {
            Ok(bytes) => bytes,
            Err(_) => return Err(()),
        };
        Ok(Keys::from(bytes))
    }
}

pub fn change_validators(
    sudo_connection: &RootConnection,
    new_reserved_validators: Option<Vec<AccountId>>,
    new_non_reserved_validators: Option<Vec<AccountId>>,
    committee_size: Option<CommitteeSeats>,
    status: XtStatus,
) {
    info!(target: "aleph-client", "New validators: reserved: {:#?}, non_reserved: {:#?}, validators_per_session: {:?}", new_reserved_validators, new_non_reserved_validators, committee_size);
    let call = compose_call!(
        sudo_connection.as_connection().metadata,
        "Elections",
        "change_validators",
        new_reserved_validators,
        new_non_reserved_validators,
        committee_size
    );
    let xt = compose_extrinsic!(
        sudo_connection.as_connection(),
        "Sudo",
        "sudo_unchecked_weight",
        call,
        0_u64
    );
    send_xt(sudo_connection, xt, Some("change_validators"), status);
}

pub fn change_next_era_reserved_validators(
    sudo_connection: &RootConnection,
    new_validators: Vec<AccountId>,
    status: XtStatus,
) {
    change_validators(sudo_connection, Some(new_validators), None, None, status)
}

pub fn set_keys(connection: &SignedConnection, new_keys: Keys, status: XtStatus) {
    let xt = compose_extrinsic!(
        connection.as_connection(),
        "Session",
        "set_keys",
        new_keys,
        0u8
    );
    send_xt(connection, xt, Some("set_keys"), status);
}

pub fn get_current_session<C: AnyConnection>(connection: &C) -> SessionIndex {
    get_session(connection, None)
}

pub fn get_session<C: AnyConnection>(connection: &C, block_hash: Option<H256>) -> SessionIndex {
    connection
        .as_connection()
        .get_storage_value("Session", "CurrentIndex", block_hash)
        .unwrap()
        .unwrap_or(0)
}

pub fn wait_for_predicate<C: AnyConnection, P: Fn(SessionIndex) -> bool>(
    connection: &C,
    session_predicate: P,
) -> anyhow::Result<BlockNumber> {
    info!(target: "aleph-client", "Waiting for session");

    #[derive(Debug, Decode, Clone)]
    struct NewSessionEvent {
        session_index: SessionIndex,
    }
    let result = wait_for_event(
        connection,
        ("Session", "NewSession"),
        |e: NewSessionEvent| {
            info!(target: "aleph-client", "New session {}", e.session_index);

            session_predicate(e.session_index)
        },
    )?;
    Ok(result.session_index)
}

pub fn wait_for<C: AnyConnection>(
    connection: &C,
    session_index: SessionIndex,
) -> anyhow::Result<BlockNumber> {
    wait_for_predicate(connection, |session_ix| session_ix == session_index)
}

pub fn wait_for_at_least<C: AnyConnection>(
    connection: &C,
    session_index: SessionIndex,
) -> anyhow::Result<BlockNumber> {
    wait_for_predicate(connection, |session_ix| session_ix >= session_index)
}

pub fn get_session_period<C: AnyConnection>(connection: &C) -> u32 {
    connection.read_constant("Elections", "SessionPeriod")
}
