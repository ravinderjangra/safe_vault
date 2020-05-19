// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::{rpc::Rpc, vault::Init, Result, COST_OF_PUT};
use log::{error, trace};
use pickledb::{PickleDb, PickleDbDumpPolicy};
use rand::{distributions::Standard, CryptoRng, Rng};
use routing::SrcLocation;
use safe_nd::{
    ClientPublicId, Coins, IDataAddress, PublicId, PublicKey, Result as NdResult, XorName,
};
use serde::Serialize;
use std::{fs, path::Path};
use unwrap::unwrap;

pub(crate) fn new_db<D: AsRef<Path>, N: AsRef<Path>>(
    db_dir: D,
    db_name: N,
    init_mode: Init,
) -> Result<PickleDb> {
    let db_path = db_dir.as_ref().join(db_name);
    if init_mode == Init::New {
        trace!("Creating database at {}", db_path.display());
        fs::create_dir_all(db_dir)?;
        let mut db = PickleDb::new_bin(db_path, PickleDbDumpPolicy::AutoDump);
        // Write then delete a value to ensure DB file is actually written to disk.
        db.set("", &"")?;
        let _ = db.rem("")?;
        return Ok(db);
    }
    trace!("Loading database at {}", db_path.display());
    let result = PickleDb::load_bin(db_path.clone(), PickleDbDumpPolicy::AutoDump);
    if let Err(ref error) = &result {
        error!("Failed to load {}: {}", db_path.display(), error);
    }
    Ok(result?)
}

pub(crate) fn random_vec<R: CryptoRng + Rng>(rng: &mut R, size: usize) -> Vec<u8> {
    rng.sample_iter(&Standard).take(size).collect()
}

pub(crate) fn serialise<T: Serialize>(data: &T) -> Vec<u8> {
    unwrap!(bincode::serialize(data))
}

pub(crate) fn db_key_to_idata_address(encoded_address: String) -> IDataAddress {
    let decoded = unwrap!(base64::decode(&encoded_address));
    let deserialized = unwrap!(bincode::deserialize::<IDataAddress>(&decoded));
    deserialized
}

/// Returns the client's public ID, the owner's public ID, or None depending on whether `public_id`
/// represents a Client, App or Node respectively.
pub(crate) fn owner(public_id: &PublicId) -> Option<&ClientPublicId> {
    match public_id {
        PublicId::Node(_) => None,
        PublicId::Client(pub_id) => Some(pub_id),
        PublicId::App(pub_id) => Some(pub_id.owner()),
    }
}

/// Returns the client's ID if `public_id` represents a Client, or None if it represents an App or
/// Node.
pub(crate) fn client(public_id: &PublicId) -> Option<&ClientPublicId> {
    match public_id {
        PublicId::Node(_) | PublicId::App(_) => None,
        PublicId::Client(pub_id) => Some(pub_id),
    }
}

/// Returns the client's or app's public key if `public_id` represents a Client or App respectively,
/// or None if it represents a Node.
pub(crate) fn own_key(public_id: &PublicId) -> Option<&PublicKey> {
    match public_id {
        PublicId::Node(_) => None,
        PublicId::Client(ref client) => Some(client.public_key()),
        PublicId::App(ref app) => Some(app.public_key()),
    }
}

/// Returns the requester's address.  An App's address is the name of its owner.
pub(crate) fn requester_address(rpc: &Rpc) -> &XorName {
    match rpc {
        Rpc::Request { ref requester, .. } | Rpc::Response { ref requester, .. } => {
            requester.name()
        }
    }
}

pub(crate) fn get_refund_for_put<T>(result: &NdResult<T>) -> Option<Coins> {
    if result.is_err() {
        Some(COST_OF_PUT)
    } else {
        None
    }
}

pub(crate) fn get_source_name(src: SrcLocation) -> XorName {
    if let SrcLocation::Node(xorname) = src {
        XorName(xorname.0)
    } else {
        XorName::default()
    }
}
