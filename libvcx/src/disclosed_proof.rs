use std::collections::HashMap;
use std::convert::TryInto;

use serde_json;
use serde_json::Value;

use api::VcxStateType;
use connection;
use error::prelude::*;
use messages::{
    get_message::Message,
    payload::Payloads,
    thread::Thread,
};
use messages::proofs::{
    proof_message::CredInfoProver,
    proof_request::{
        ProofRequestData,
        ProofRequestMessage,
        NonRevokedInterval
    },
};
use object_cache::ObjectCache;
use settings;
use utils::constants::GET_MESSAGES_DECRYPTED_RESPONSE;
use utils::error;
use utils::httpclient::AgencyMockDecrypted;
use utils::libindy::anoncreds;
use utils::libindy::anoncreds::{get_rev_reg_def_json, get_rev_reg_delta_json};
use utils::libindy::cache::{get_rev_reg_cache, RevRegCache, RevState, set_rev_reg_cache};
use v3::{
    handlers::proof_presentation::prover::prover::Prover,
    messages::proof_presentation::presentation_request::PresentationRequest,
};
use settings::indy_mocks_enabled;
use utils::mockdata::mockdata_proof::ARIES_PROOF_REQUEST_PRESENTATION;
use utils::mockdata::mock_settings::get_mock_generate_indy_proof;
use proof_utils::{
    build_schemas_json_prover,
    build_cred_defs_json_prover
};

lazy_static! {
    static ref HANDLE_MAP: ObjectCache<Prover> = ObjectCache::<Prover>::new("disclosed-proofs-cache");
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "version", content = "data")]
enum DisclosedProofs {
    #[serde(rename = "2.0")]
    V3(Prover),
}

pub fn credential_def_identifiers(credentials: &str, proof_req: &ProofRequestData) -> VcxResult<Vec<CredInfoProver>> {
    let mut rtn = Vec::new();

    let credentials: Value = serde_json::from_str(credentials)
        .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidJson, format!("Cannot deserialize credentials: {}", err)))?;

    if let Value::Object(ref attrs) = credentials["attrs"] {
        for (requested_attr, value) in attrs {
            if let (Some(referent), Some(schema_id), Some(cred_def_id)) =
            (value["credential"]["cred_info"]["referent"].as_str(),
             value["credential"]["cred_info"]["schema_id"].as_str(),
             value["credential"]["cred_info"]["cred_def_id"].as_str()) {
                let rev_reg_id = value["credential"]["cred_info"]["rev_reg_id"]
                    .as_str()
                    .map(|x| x.to_string());

                let cred_rev_id = value["credential"]["cred_info"]["cred_rev_id"]
                    .as_str()
                    .map(|x| x.to_string());

                let tails_file = value["tails_file"]
                    .as_str()
                    .map(|x| x.to_string());

                rtn.push(
                    CredInfoProver {
                        requested_attr: requested_attr.to_string(),
                        referent: referent.to_string(),
                        schema_id: schema_id.to_string(),
                        cred_def_id: cred_def_id.to_string(),
                        revocation_interval: _get_revocation_interval(&requested_attr, &proof_req)?,
                        timestamp: None,
                        rev_reg_id,
                        cred_rev_id,
                        tails_file,
                    }
                );
            } else { return Err(VcxError::from_msg(VcxErrorKind::InvalidProofCredentialData, "Cannot get identifiers")); }
        }
    }

    Ok(rtn)
}

fn _get_revocation_interval(attr_name: &str, proof_req: &ProofRequestData) -> VcxResult<Option<NonRevokedInterval>> {
    if let Some(attr) = proof_req.requested_attributes.get(attr_name) {
        Ok(attr.non_revoked.clone().or(proof_req.non_revoked.clone().or(None)))
    } else if let Some(attr) = proof_req.requested_predicates.get(attr_name) {
        // Handle case for predicates
        Ok(attr.non_revoked.clone().or(proof_req.non_revoked.clone().or(None)))
    } else {
        Err(VcxError::from_msg(VcxErrorKind::InvalidProofCredentialData, format!("Attribute not found for: {}", attr_name)))
    }
}

pub fn build_rev_states_json(credentials_identifiers: &mut Vec<CredInfoProver>) -> VcxResult<String> {
    let mut rtn: Value = json!({});
    let mut timestamps: HashMap<String, u64> = HashMap::new();

    for cred_info in credentials_identifiers.iter_mut() {
        if let (Some(rev_reg_id), Some(cred_rev_id), Some(tails_file)) =
        (&cred_info.rev_reg_id, &cred_info.cred_rev_id, &cred_info.tails_file) {
            if rtn.get(&rev_reg_id).is_none() {
                let (from, to) = if let Some(ref interval) = cred_info.revocation_interval
                { (interval.from, interval.to) } else { (None, None) };

                let cache = get_rev_reg_cache(&rev_reg_id, &cred_rev_id);

                let (rev_state_json, timestamp) =
                    if let (Some(cached_rev_state), Some(to)) = (cache.rev_state, to) {
                        if cached_rev_state.timestamp >= from.unwrap_or(0)
                            && cached_rev_state.timestamp <= to {
                            (cached_rev_state.value, cached_rev_state.timestamp)
                        } else {
                            let from = match from {
                                Some(from) if from >= cached_rev_state.timestamp => {
                                    Some(cached_rev_state.timestamp)
                                }
                                _ => None
                            };

                            let (_, rev_reg_def_json) = get_rev_reg_def_json(&rev_reg_id)?;

                            let (rev_reg_id, rev_reg_delta_json, timestamp) = get_rev_reg_delta_json(
                                &rev_reg_id,
                                from,
                                Some(to),
                            )?;

                            let rev_state_json = anoncreds::libindy_prover_update_revocation_state(
                                &rev_reg_def_json,
                                &cached_rev_state.value,
                                &rev_reg_delta_json,
                                &cred_rev_id,
                                &tails_file,
                            )?;

                            if timestamp > cached_rev_state.timestamp {
                                let new_cache = RevRegCache {
                                    rev_state: Some(RevState {
                                        timestamp,
                                        value: rev_state_json.clone(),
                                    })
                                };
                                set_rev_reg_cache(&rev_reg_id, &cred_rev_id, &new_cache);
                            }

                            (rev_state_json, timestamp)
                        }
                    } else {
                        let (_, rev_reg_def_json) = get_rev_reg_def_json(&rev_reg_id)?;

                        let (rev_reg_id, rev_reg_delta_json, timestamp) = get_rev_reg_delta_json(
                            &rev_reg_id,
                            None,
                            to,
                        )?;

                        let rev_state_json = anoncreds::libindy_prover_create_revocation_state(
                            &rev_reg_def_json,
                            &rev_reg_delta_json,
                            &cred_rev_id,
                            &tails_file,
                        )?;

                        let new_cache = RevRegCache {
                            rev_state: Some(RevState {
                                timestamp,
                                value: rev_state_json.clone(),
                            })
                        };
                        set_rev_reg_cache(&rev_reg_id, &cred_rev_id, &new_cache);

                        (rev_state_json, timestamp)
                    };

                let rev_state_json: Value = serde_json::from_str(&rev_state_json)
                    .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidJson, format!("Cannot deserialize RevocationState: {}", err)))?;

                // TODO: proover should be able to create multiple states of same revocation policy for different timestamps
                // see ticket IS-1108
                rtn[rev_reg_id.to_string()] = json!({timestamp.to_string(): rev_state_json});
                cred_info.timestamp = Some(timestamp);

                // Cache timestamp for future attributes that have the same rev_reg_id
                timestamps.insert(rev_reg_id.to_string(), timestamp);
            }

            // If the rev_reg_id is already in the map, timestamp may not be updated on cred_info
            if cred_info.timestamp.is_none() {
                cred_info.timestamp = timestamps.get(rev_reg_id).cloned();
            }
        }
    }

    Ok(rtn.to_string())
}

pub fn build_requested_credentials_json(credentials_identifiers: &Vec<CredInfoProver>,
                                        self_attested_attrs: &str,
                                        proof_req: &ProofRequestData) -> VcxResult<String> {
    let mut rtn: Value = json!({
          "self_attested_attributes":{},
          "requested_attributes":{},
          "requested_predicates":{}
    });
    // do same for predicates and self_attested
    if let Value::Object(ref mut map) = rtn["requested_attributes"] {
        for ref cred_info in credentials_identifiers {
            if let Some(_) = proof_req.requested_attributes.get(&cred_info.requested_attr) {
                let insert_val = json!({"cred_id": cred_info.referent, "revealed": true, "timestamp": cred_info.timestamp});
                map.insert(cred_info.requested_attr.to_owned(), insert_val);
            }
        }
    }

    if let Value::Object(ref mut map) = rtn["requested_predicates"] {
        for ref cred_info in credentials_identifiers {
            if let Some(_) = proof_req.requested_predicates.get(&cred_info.requested_attr) {
                let insert_val = json!({"cred_id": cred_info.referent, "timestamp": cred_info.timestamp});
                map.insert(cred_info.requested_attr.to_owned(), insert_val);
            }
        }
    }

    // handle if the attribute is not revealed
    let self_attested_attrs: Value = serde_json::from_str(self_attested_attrs)
        .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidJson, format!("Cannot deserialize self attested attributes: {}", err)))?;
    rtn["self_attested_attributes"] = self_attested_attrs;

    Ok(rtn.to_string())
}

pub fn generate_indy_proof(credentials: &str, self_attested_attrs: &str, proof_req_data_json: &str) -> VcxResult<String> {
    trace!("generate_indy_proof >>> credentials: {}, self_attested_attrs: {}", secret!(&credentials), secret!(&self_attested_attrs));

    match get_mock_generate_indy_proof() {
        None => {}
        Some(mocked_indy_proof) => {
            warn!("generate_indy_proof :: returning mocked response");
            return Ok(mocked_indy_proof)
        }
    }

    let proof_request: ProofRequestData = serde_json::from_str(&proof_req_data_json)
        .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidJson, format!("Cannot deserialize proof request: {}", err)))?;

    let mut credentials_identifiers = credential_def_identifiers(credentials, &proof_request)?;

    let revoc_states_json = build_rev_states_json(&mut credentials_identifiers)?;
    let requested_credentials = build_requested_credentials_json(&credentials_identifiers,
                                                                                 self_attested_attrs,
                                                                                 &proof_request)?;

    let schemas_json = build_schemas_json_prover(&credentials_identifiers)?;
    let credential_defs_json = build_cred_defs_json_prover(&credentials_identifiers)?;

    let proof = anoncreds::libindy_prover_create_proof(&proof_req_data_json,
                                                       &requested_credentials,
                                                       settings::DEFAULT_LINK_SECRET_ALIAS,
                                                       &schemas_json,
                                                       &credential_defs_json,
                                                       Some(&revoc_states_json))?;
    Ok(proof)
}

fn handle_err(err: VcxError) -> VcxError {
    if err.kind() == VcxErrorKind::InvalidHandle {
        VcxError::from(VcxErrorKind::InvalidDisclosedProofHandle)
    } else {
        err
    }
}

pub fn create_proof(source_id: &str, proof_req: &str) -> VcxResult<u32> {
    trace!("create_proof >>> source_id: {}, proof_req: {}", source_id, proof_req);
    debug!("creating disclosed proof with id: {}", source_id);

    let presentation_request: PresentationRequest = serde_json::from_str(proof_req)
        .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidJson,
                                          format!("Strict `aries` protocol is enabled. Can not parse `aries` formatted Presentation Request: {}", err)))?;

    let proof = Prover::create(source_id, presentation_request)?;
    HANDLE_MAP.add(proof)
}

pub fn create_proof_with_msgid(source_id: &str, connection_handle: u32, msg_id: &str) -> VcxResult<(u32, String)> {
    if !connection::is_v3_connection(connection_handle)? {
        return Err(VcxError::from_msg(VcxErrorKind::InvalidConnectionHandle, format!("Connection can not be used for Proprietary Issuance protocol")))
    };

    let proof_request = get_proof_request(connection_handle, &msg_id)?;

    let presentation_request: PresentationRequest = serde_json::from_str(&proof_request)
        .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidJson,
                                          format!("Strict `aries` protocol is enabled. Can not parse `aries` formatted Presentation Request: {}", err)))?;

    let proof = Prover::create(source_id, presentation_request)?;

    let handle = HANDLE_MAP.add(proof)?;

    debug!("inserting disclosed proof {} into handle map", source_id);
    Ok((handle, proof_request))
}

pub fn get_state(handle: u32) -> VcxResult<u32> {
    HANDLE_MAP.get(handle, |proof| {
        Ok(proof.state())
    }).or(Err(VcxError::from(VcxErrorKind::InvalidConnectionHandle)))
}

pub fn update_state(handle: u32, message: Option<String>, connection_handle: Option<u32>) -> VcxResult<u32> {
    HANDLE_MAP.get_mut(handle, |proof| {
        proof.update_state(message.as_ref().map(String::as_str), connection_handle)?;
        Ok(proof.state())
    })
}

pub fn to_string(handle: u32) -> VcxResult<String> {
    HANDLE_MAP.get(handle, |proof| {
        serde_json::to_string(&DisclosedProofs::V3(proof.clone()))
            .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidState, format!("cannot serialize DisclosedProof proofect: {:?}", err)))
    })
}

pub fn from_string(proof_data: &str) -> VcxResult<u32> {
    let proof: DisclosedProofs = serde_json::from_str(proof_data)
        .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidJson, format!("cannot deserialize DisclosedProofs object: {:?}", err)))?;

    match proof {
        DisclosedProofs::V3(proof) => HANDLE_MAP.add(proof),
        _ => Err(VcxError::from_msg(VcxErrorKind::InvalidJson, "Found disclosed proof of unsupported version"))
    }
}

pub fn release(handle: u32) -> VcxResult<()> {
    HANDLE_MAP.release(handle).map_err(handle_err)
}

pub fn release_all() {
    HANDLE_MAP.drain().ok();
}

pub fn generate_proof_msg(handle: u32) -> VcxResult<String> {
    HANDLE_MAP.get(handle, |proof| {
        proof.generate_presentation_msg()
    })
}

pub fn send_proof(handle: u32, connection_handle: u32) -> VcxResult<u32> {
    HANDLE_MAP.get_mut(handle, |proof| {
        proof.send_presentation(connection_handle)?;
        let new_proof = proof.clone();
        *proof = new_proof;
        Ok(error::SUCCESS.code_num)
    })
}

pub fn generate_reject_proof_msg(handle: u32) -> VcxResult<String> {
    HANDLE_MAP.get_mut(handle, |_| {
        Err(VcxError::from_msg(VcxErrorKind::ActionNotSupported,
                               "Action generate_reject_proof_msg is not implemented for V3 disclosed proof."))
    })
}

pub fn reject_proof(handle: u32, connection_handle: u32) -> VcxResult<u32> {
    HANDLE_MAP.get_mut(handle, |proof| {
        proof.decline_presentation_request(connection_handle, Some(String::from("Presentation Request was rejected")), None)?;
        let new_proof = proof.clone();
        *proof = new_proof;
        Ok(error::SUCCESS.code_num)
    })
}

pub fn generate_proof(handle: u32, credentials: String, self_attested_attrs: String) -> VcxResult<u32> {
    HANDLE_MAP.get_mut(handle, |proof| {
        proof.generate_presentation(credentials.clone(), self_attested_attrs.clone())?;
        Ok(error::SUCCESS.code_num)
    }).map(|_| error::SUCCESS.code_num)
}

pub fn decline_presentation_request(handle: u32, connection_handle: u32, reason: Option<String>, proposal: Option<String>) -> VcxResult<u32> {
    HANDLE_MAP.get_mut(handle, |proof| {
        proof.decline_presentation_request(connection_handle, reason.clone(), proposal.clone())?;
        let new_proof = proof.clone();
        *proof = new_proof;
        Ok(error::SUCCESS.code_num)
    }).map(|_| error::SUCCESS.code_num)
}

pub fn retrieve_credentials(handle: u32) -> VcxResult<String> {
    HANDLE_MAP.get_mut(handle, |proof| {
        proof.retrieve_credentials()
    })
}

pub fn is_valid_handle(handle: u32) -> bool {
    HANDLE_MAP.has_handle(handle)
}

//TODO one function with credential
fn get_proof_request(connection_handle: u32, msg_id: &str) -> VcxResult<String> {
    if !connection::is_v3_connection(connection_handle)? {
        return Err(VcxError::from_msg(VcxErrorKind::InvalidConnectionHandle, format!("Connection can not be used for Proprietary Issuance protocol")));
    };

    if indy_mocks_enabled() {
        AgencyMockDecrypted::set_next_decrypted_response(GET_MESSAGES_DECRYPTED_RESPONSE);
        AgencyMockDecrypted::set_next_decrypted_message(ARIES_PROOF_REQUEST_PRESENTATION);
    }

    let presentation_request = Prover::get_presentation_request(connection_handle, msg_id)?;
    return serde_json::to_string_pretty(&presentation_request)
        .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidJson, format!("Cannot serialize message: {}", err)));
}

//TODO one function with credential
pub fn get_proof_request_messages(connection_handle: u32, match_name: Option<&str>) -> VcxResult<String> {
    trace!("get_proof_request_messages >>> connection_handle: {}, match_name: {:?}", connection_handle, match_name);

    if !connection::is_v3_connection(connection_handle)? {
        return Err(VcxError::from_msg(VcxErrorKind::InvalidConnectionHandle, format!("Connection can not be used for Proprietary Issuance protocol")));
    }

    let presentation_requests = Prover::get_presentation_request_messages(connection_handle, match_name)?;

    // strict aries protocol is set. return aries formatted Proof Request.
    if settings::is_strict_aries_protocol_set() {
        return Ok(json!(presentation_requests).to_string());
    }

    let msgs: Vec<ProofRequestMessage> = presentation_requests
        .into_iter()
        .map(|presentation_request| presentation_request.try_into())
        .collect::<VcxResult<Vec<ProofRequestMessage>>>()?;

    serde_json::to_string(&msgs).
        map_err(|err| {
            VcxError::from_msg(VcxErrorKind::InvalidState, format!("Cannot serialize ProofRequestMessage: {:?}", err))
        })
}

fn _parse_proof_req_message(message: &Message, my_vk: &str) -> VcxResult<ProofRequestMessage> {
    let payload = message.payload.as_ref()
        .ok_or(VcxError::from_msg(VcxErrorKind::InvalidHttpResponse, "Cannot get payload"))?;

    let (request, thread) = Payloads::decrypt(&my_vk, payload)?;

    let mut request: ProofRequestMessage = serde_json::from_str(&request)
        .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidHttpResponse, format!("Cannot deserialize proof request: {}", err)))?;

    request.msg_ref_id = Some(message.uid.to_owned());
    request.thread_id = thread.and_then(|tr| tr.thid.clone());

    Ok(request)
}

pub fn get_source_id(handle: u32) -> VcxResult<String> {
    HANDLE_MAP.get(handle, |proof| {
        Ok(proof.get_source_id())
    }).map_err(handle_err)
}

pub fn get_presentation_status(handle: u32) -> VcxResult<u32> {
    HANDLE_MAP.get(handle, |proof| {
        Ok(proof.presentation_status())
    })
}

#[cfg(test)]
mod tests {
    extern crate serde_json;

    use serde_json::Value;
    #[cfg(feature = "pool_tests")]
    use time;

    use utils::{
        constants::{ADDRESS_CRED_ID, LICENCE_CRED_ID, ADDRESS_SCHEMA_ID,
                    ADDRESS_CRED_DEF_ID, CRED_DEF_ID, SCHEMA_ID, ADDRESS_CRED_REV_ID,
                    ADDRESS_REV_REG_ID, REV_REG_ID, CRED_REV_ID, TEST_TAILS_FILE, REV_STATE_JSON,
                    GET_MESSAGES_DECRYPTED_RESPONSE, ARIES_PROVER_CREDENTIALS, ARIES_PROVER_SELF_ATTESTED_ATTRS},
        get_temp_dir_path,
    };
    use utils::mockdata::mockdata_proof;
    use utils::httpclient::AgencyMockDecrypted;
    use utils::devsetup::*;

    use super::*;
    use utils::mockdata::mockdata_proof::{ARIES_PROOF_REQUEST_PRESENTATION, ARIES_PROOF_PRESENTATION_ACK};
    use utils::mockdata::mock_settings::MockBuilder;

    fn proof_req_no_interval() -> ProofRequestData {
        let proof_req = json!({
            "nonce": "123432421212",
            "name": "proof_req_1",
            "version": "0.1",
            "requested_attributes": {
                "address1_1": { "name": "address1" },
                "zip_2": { "name": "zip" },
                "height_1": { "name": "height" }
            },
            "requested_predicates": {},
        }).to_string();

        serde_json::from_str(&proof_req).unwrap()
    }

    fn _get_proof_request_messages(connection_h: u32) -> String {
        let requests = get_proof_request_messages(connection_h, None).unwrap();
        let requests: Value = serde_json::from_str(&requests).unwrap();
        let requests = serde_json::to_string(&requests[0]).unwrap();
        requests
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_create_proof() {
        let _setup = SetupAriesMocks::init();
        settings::set_config_value(settings::CONFIG_PROTOCOL_TYPE, "4.0");

        assert!(create_proof("1", ARIES_PROOF_REQUEST_PRESENTATION).unwrap() > 0);
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_create_fails() {
        let _setup = SetupAriesMocks::init();
        settings::set_config_value(settings::CONFIG_PROTOCOL_TYPE, "4.0");

        assert_eq!(create_proof("1", "{}").unwrap_err().kind(), VcxErrorKind::InvalidJson);
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_proof_cycle() {
        let _setup = SetupAriesMocks::init();
        settings::set_config_value(settings::CONFIG_PROTOCOL_TYPE, "4.0");

        let connection_h = connection::tests::build_test_connection_inviter_requested();

        AgencyMockDecrypted::set_next_decrypted_response(GET_MESSAGES_DECRYPTED_RESPONSE);
        AgencyMockDecrypted::set_next_decrypted_message(ARIES_PROOF_REQUEST_PRESENTATION);

        let request = _get_proof_request_messages(connection_h);

        let handle_proof = create_proof("TEST_CREDENTIAL", &request).unwrap();
        assert_eq!(VcxStateType::VcxStateRequestReceived as u32, get_state(handle_proof).unwrap());

        let _mock_builder = MockBuilder::init().
            set_mock_generate_indy_proof("{\"selected\":\"credentials\"}");

        generate_proof(handle_proof, String::from("{\"selected\":\"credentials\"}"), "{}".to_string()).unwrap();
        send_proof(handle_proof, connection_h).unwrap();
        assert_eq!(VcxStateType::VcxStateOfferSent as u32, get_state(handle_proof).unwrap());

        update_state(handle_proof, Some(String::from(ARIES_PROOF_PRESENTATION_ACK)), Some(connection_h)).unwrap();
        assert_eq!(VcxStateType::VcxStateAccepted as u32, get_state(handle_proof).unwrap());
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_proof_update_state_v2() {
        let _setup = SetupStrictAriesMocks::init();

        let connection_handle = connection::tests::build_test_connection_inviter_requested();

        AgencyMockDecrypted::set_next_decrypted_response(GET_MESSAGES_DECRYPTED_RESPONSE);
        AgencyMockDecrypted::set_next_decrypted_message(mockdata_proof::ARIES_PRESENTATION_REQUEST);

        let request = _get_proof_request_messages(connection_handle);

        let handle = create_proof("TEST_CREDENTIAL", &request).unwrap();
        assert_eq!(VcxStateType::VcxStateRequestReceived as u32, get_state(handle).unwrap());

        generate_proof(handle, ARIES_PROVER_CREDENTIALS.to_string(), ARIES_PROVER_SELF_ATTESTED_ATTRS.to_string());
        assert_eq!(VcxStateType::VcxStateRequestReceived as u32, get_state(handle).unwrap());

        send_proof(handle, connection_handle).unwrap();
        assert_eq!(VcxStateType::VcxStateOfferSent as u32, get_state(handle).unwrap());

        ::connection::release(connection_handle);
        let connection_handle = connection::tests::build_test_connection_inviter_requested();

        AgencyMockDecrypted::set_next_decrypted_response(GET_MESSAGES_DECRYPTED_RESPONSE);
        AgencyMockDecrypted::set_next_decrypted_message(mockdata_proof::ARIES_PROOF_PRESENTATION_ACK);

        update_state(handle, None, Some(connection_handle));
        assert_eq!(VcxStateType::VcxStateAccepted as u32, get_state(handle).unwrap());
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_proof_reject_cycle() {
        let _setup = SetupAriesMocks::init();
        settings::set_config_value(settings::CONFIG_PROTOCOL_TYPE, "4.0");

        let connection_h = connection::tests::build_test_connection_inviter_requested();

        AgencyMockDecrypted::set_next_decrypted_response(GET_MESSAGES_DECRYPTED_RESPONSE);
        AgencyMockDecrypted::set_next_decrypted_message(ARIES_PROOF_REQUEST_PRESENTATION);

        let request = _get_proof_request_messages(connection_h);

        let handle = create_proof("TEST_CREDENTIAL", &request).unwrap();
        assert_eq!(VcxStateType::VcxStateRequestReceived as u32, get_state(handle).unwrap());

        reject_proof(handle, connection_h).unwrap();
        assert_eq!(VcxStateType::VcxStateNone as u32, get_state(handle).unwrap());
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn get_state_test() {
        let _setup = SetupAriesMocks::init();
        ::settings::set_config_value(::settings::CONFIG_PROTOCOL_TYPE, "4.0");

        let handle = create_proof("id", ARIES_PROOF_REQUEST_PRESENTATION).unwrap();
        assert_eq!(VcxStateType::VcxStateRequestReceived as u32, get_state(handle).unwrap())
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn to_string_test() {
        let _setup = SetupAriesMocks::init();
        ::settings::set_config_value(::settings::CONFIG_PROTOCOL_TYPE, "4.0");

        let handle = create_proof("id", ARIES_PROOF_REQUEST_PRESENTATION).unwrap();

        let serialized = to_string(handle).unwrap();
        let j: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(j["version"], ::utils::constants::V3_OBJECT_SERIALIZE_VERSION);

        let handle_2 = from_string(&serialized).unwrap();
        assert_ne!(handle, handle_2);
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_deserialize_fails() {
        let _setup = SetupDefaults::init();

        assert_eq!(from_string("{}").unwrap_err().kind(), VcxErrorKind::InvalidJson);
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_deserialize_succeeds_with_self_attest_allowed() {
        let _setup = SetupDefaults::init();
        ::settings::set_config_value(::settings::CONFIG_PROTOCOL_TYPE, "4.0");

        let handle = create_proof("id", ARIES_PROOF_REQUEST_PRESENTATION).unwrap();

        let serialized = to_string(handle).unwrap();
        from_string(&serialized).unwrap();
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_find_schemas() {
        let _setup = SetupAriesMocks::init();

        assert_eq!(build_schemas_json_prover(&Vec::new()).unwrap(), "{}".to_string());

        let cred1 = CredInfoProver {
            requested_attr: "height_1".to_string(),
            referent: LICENCE_CRED_ID.to_string(),
            schema_id: SCHEMA_ID.to_string(),
            cred_def_id: CRED_DEF_ID.to_string(),
            rev_reg_id: Some(REV_REG_ID.to_string()),
            cred_rev_id: Some(CRED_REV_ID.to_string()),
            revocation_interval: None,
            tails_file: None,
            timestamp: None,
        };
        let cred2 = CredInfoProver {
            requested_attr: "zip_2".to_string(),
            referent: ADDRESS_CRED_ID.to_string(),
            schema_id: ADDRESS_SCHEMA_ID.to_string(),
            cred_def_id: ADDRESS_CRED_DEF_ID.to_string(),
            rev_reg_id: Some(ADDRESS_REV_REG_ID.to_string()),
            cred_rev_id: Some(ADDRESS_CRED_REV_ID.to_string()),
            revocation_interval: None,
            tails_file: None,
            timestamp: None,
        };
        let creds = vec![cred1, cred2];

        let schemas = build_schemas_json_prover(&creds).unwrap();
        assert!(schemas.len() > 0);
        assert!(schemas.contains(r#""id":"2hoqvcwupRTUNkXn6ArYzs:2:test-licence:4.4.4","name":"test-licence""#));
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_find_schemas_fails() {
        let _setup = SetupLibraryWallet::init();

        let credential_ids = vec![CredInfoProver {
            requested_attr: "1".to_string(),
            referent: "2".to_string(),
            schema_id: "3".to_string(),
            cred_def_id: "3".to_string(),
            rev_reg_id: Some("4".to_string()),
            cred_rev_id: Some("5".to_string()),
            revocation_interval: None,
            tails_file: None,
            timestamp: None,
        }];
        assert_eq!(build_schemas_json_prover(&credential_ids).unwrap_err().kind(), VcxErrorKind::InvalidSchema);
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_find_credential_def() {
        let _setup = SetupAriesMocks::init();

        let cred1 = CredInfoProver {
            requested_attr: "height_1".to_string(),
            referent: LICENCE_CRED_ID.to_string(),
            schema_id: SCHEMA_ID.to_string(),
            cred_def_id: CRED_DEF_ID.to_string(),
            rev_reg_id: Some(REV_REG_ID.to_string()),
            cred_rev_id: Some(CRED_REV_ID.to_string()),
            revocation_interval: None,
            tails_file: None,
            timestamp: None,
        };
        let cred2 = CredInfoProver {
            requested_attr: "zip_2".to_string(),
            referent: ADDRESS_CRED_ID.to_string(),
            schema_id: ADDRESS_SCHEMA_ID.to_string(),
            cred_def_id: ADDRESS_CRED_DEF_ID.to_string(),
            rev_reg_id: Some(ADDRESS_REV_REG_ID.to_string()),
            cred_rev_id: Some(ADDRESS_CRED_REV_ID.to_string()),
            revocation_interval: None,
            tails_file: None,
            timestamp: None,
        };
        let creds = vec![cred1, cred2];

        let credential_def = build_cred_defs_json_prover(&creds).unwrap();
        assert!(credential_def.len() > 0);
        assert!(credential_def.contains(r#""id":"2hoqvcwupRTUNkXn6ArYzs:3:CL:2471","schemaId":"2471""#));
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_find_credential_def_fails() {
        let _setup = SetupLibraryWallet::init();

        let credential_ids = vec![CredInfoProver {
            requested_attr: "1".to_string(),
            referent: "2".to_string(),
            schema_id: "3".to_string(),
            cred_def_id: "3".to_string(),
            rev_reg_id: Some("4".to_string()),
            cred_rev_id: Some("5".to_string()),
            revocation_interval: None,
            tails_file: None,
            timestamp: None,
        }];
        assert_eq!(build_cred_defs_json_prover(&credential_ids).unwrap_err().kind(), VcxErrorKind::InvalidProofCredentialData);
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_build_requested_credentials() {
        let _setup = SetupAriesMocks::init();

        let cred1 = CredInfoProver {
            requested_attr: "height_1".to_string(),
            referent: LICENCE_CRED_ID.to_string(),
            schema_id: SCHEMA_ID.to_string(),
            cred_def_id: CRED_DEF_ID.to_string(),
            rev_reg_id: Some(REV_REG_ID.to_string()),
            cred_rev_id: Some(CRED_REV_ID.to_string()),
            revocation_interval: None,
            tails_file: None,
            timestamp: Some(800),
        };
        let cred2 = CredInfoProver {
            requested_attr: "zip_2".to_string(),
            referent: ADDRESS_CRED_ID.to_string(),
            schema_id: ADDRESS_SCHEMA_ID.to_string(),
            cred_def_id: ADDRESS_CRED_DEF_ID.to_string(),
            rev_reg_id: Some(ADDRESS_REV_REG_ID.to_string()),
            cred_rev_id: Some(ADDRESS_CRED_REV_ID.to_string()),
            revocation_interval: None,
            tails_file: None,
            timestamp: Some(800),
        };
        let creds = vec![cred1, cred2];
        let self_attested_attrs = json!({
            "self_attested_attr_3": "my self attested 1",
            "self_attested_attr_4": "my self attested 2",
        }).to_string();

        let test: Value = json!({
              "self_attested_attributes":{
                  "self_attested_attr_3": "my self attested 1",
                  "self_attested_attr_4": "my self attested 2",
              },
              "requested_attributes":{
                  "height_1": {"cred_id": LICENCE_CRED_ID, "revealed": true, "timestamp": 800},
                  "zip_2": {"cred_id": ADDRESS_CRED_ID, "revealed": true, "timestamp": 800},
              },
              "requested_predicates":{}
        });

        let proof_req = json!({
            "nonce": "123432421212",
            "name": "proof_req_1",
            "version": "0.1",
            "requested_attributes": {
                "height_1": {
                    "name": "height_1",
                    "non_revoked":  {"from": 123, "to": 456}
                },
                "zip_2": { "name": "zip_2" }
            },
            "requested_predicates": {},
            "non_revoked": {"from": 098, "to": 123}
        });
        let proof_req: ProofRequestData = serde_json::from_value(proof_req).unwrap();
        let requested_credential = build_requested_credentials_json(&creds, &self_attested_attrs, &proof_req).unwrap();
        assert_eq!(test.to_string(), requested_credential);
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_get_proof_request() {
        let _setup = SetupAriesMocks::init();
        ::settings::set_config_value(::settings::CONFIG_PROTOCOL_TYPE, "4.0");

        let connection_h = connection::tests::build_test_connection_inviter_invited();

        let request = get_proof_request(connection_h, "123").unwrap();
        let _request: PresentationRequest = serde_json::from_str(&request).unwrap();
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_credential_def_identifiers() {
        let _setup = SetupDefaults::init();

        let cred1 = CredInfoProver {
            requested_attr: "height_1".to_string(),
            referent: LICENCE_CRED_ID.to_string(),
            schema_id: SCHEMA_ID.to_string(),
            cred_def_id: CRED_DEF_ID.to_string(),
            rev_reg_id: Some(REV_REG_ID.to_string()),
            cred_rev_id: Some(CRED_REV_ID.to_string()),
            revocation_interval: Some(NonRevokedInterval { from: Some(123), to: Some(456) }),
            tails_file: Some(get_temp_dir_path(TEST_TAILS_FILE).to_str().unwrap().to_string()),
            timestamp: None,
        };
        let cred2 = CredInfoProver {
            requested_attr: "zip_2".to_string(),
            referent: ADDRESS_CRED_ID.to_string(),
            schema_id: ADDRESS_SCHEMA_ID.to_string(),
            cred_def_id: ADDRESS_CRED_DEF_ID.to_string(),
            rev_reg_id: Some(ADDRESS_REV_REG_ID.to_string()),
            cred_rev_id: Some(ADDRESS_CRED_REV_ID.to_string()),
            revocation_interval: Some(NonRevokedInterval { from: None, to: Some(987) }),
            tails_file: None,
            timestamp: None,
        };
        let selected_credentials: Value = json!({
           "attrs":{
              "height_1":{
                "credential": {
                    "cred_info":{
                       "referent":LICENCE_CRED_ID,
                       "attrs":{
                          "sex":"male",
                          "age":"111",
                          "name":"Bob",
                          "height":"4'11"
                       },
                       "schema_id": SCHEMA_ID,
                       "cred_def_id": CRED_DEF_ID,
                       "rev_reg_id":REV_REG_ID,
                       "cred_rev_id":CRED_REV_ID
                    },
                    "interval":null
                },
                "tails_file": get_temp_dir_path(TEST_TAILS_FILE).to_str().unwrap().to_string(),
              },
              "zip_2":{
                "credential": {
                    "cred_info":{
                       "referent":ADDRESS_CRED_ID,
                       "attrs":{
                          "address1":"101 Tela Lane",
                          "address2":"101 Wilson Lane",
                          "zip":"87121",
                          "state":"UT",
                          "city":"SLC"
                       },
                       "schema_id":ADDRESS_SCHEMA_ID,
                       "cred_def_id":ADDRESS_CRED_DEF_ID,
                       "rev_reg_id":ADDRESS_REV_REG_ID,
                       "cred_rev_id":ADDRESS_CRED_REV_ID
                    },
                    "interval":null
                },
             }
           },
           "predicates":{ }
        });
        let proof_req = json!({
            "nonce": "123432421212",
            "name": "proof_req_1",
            "version": "0.1",
            "requested_attributes": {
                "zip_2": { "name": "zip" },
                "height_1": { "name": "height", "non_revoked": {"from": 123, "to": 456} }
            },
            "requested_predicates": {},
            "non_revoked": {"to": 987}
        }).to_string();

        let creds = credential_def_identifiers(&selected_credentials.to_string(), &serde_json::from_str(&proof_req).unwrap()).unwrap();
        assert_eq!(creds, vec![cred1, cred2]);
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_credential_def_identifiers_failure() {
        let _setup = SetupDefaults::init();

        // selected credentials has incorrect json
        assert_eq!(credential_def_identifiers("", &proof_req_no_interval()).unwrap_err().kind(), VcxErrorKind::InvalidJson);


        // No Creds
        assert_eq!(credential_def_identifiers("{}", &proof_req_no_interval()).unwrap(), Vec::new());
        assert_eq!(credential_def_identifiers(r#"{"attrs":{}}"#, &proof_req_no_interval()).unwrap(), Vec::new());

        // missing cred info
        let selected_credentials: Value = json!({
           "attrs":{
              "height_1":{ "interval":null }
           },
           "predicates":{

           }
        });
        assert_eq!(credential_def_identifiers(&selected_credentials.to_string(), &proof_req_no_interval()).unwrap_err().kind(), VcxErrorKind::InvalidProofCredentialData);

        // Optional Revocation
        let mut selected_credentials: Value = json!({
           "attrs":{
              "height_1":{
                "credential": {
                    "cred_info":{
                       "referent":LICENCE_CRED_ID,
                       "attrs":{
                          "sex":"male",
                          "age":"111",
                          "name":"Bob",
                          "height":"4'11"
                       },
                       "schema_id": SCHEMA_ID,
                       "cred_def_id": CRED_DEF_ID,
                       "cred_rev_id":CRED_REV_ID
                    },
                    "interval":null
                },
                "tails_file": get_temp_dir_path(TEST_TAILS_FILE).to_str().unwrap().to_string(),
              },
           },
           "predicates":{ }
        });
        let creds = vec![CredInfoProver {
            requested_attr: "height_1".to_string(),
            referent: LICENCE_CRED_ID.to_string(),
            schema_id: SCHEMA_ID.to_string(),
            cred_def_id: CRED_DEF_ID.to_string(),
            rev_reg_id: None,
            cred_rev_id: Some(CRED_REV_ID.to_string()),
            revocation_interval: None,
            tails_file: Some(get_temp_dir_path(TEST_TAILS_FILE).to_str().unwrap().to_string()),
            timestamp: None,
        }];
        assert_eq!(&credential_def_identifiers(&selected_credentials.to_string(), &proof_req_no_interval()).unwrap(), &creds);

        // rev_reg_id is null
        selected_credentials["attrs"]["height_1"]["cred_info"]["rev_reg_id"] = serde_json::Value::Null;
        assert_eq!(&credential_def_identifiers(&selected_credentials.to_string(), &proof_req_no_interval()).unwrap(), &creds);

        // Missing schema ID
        let mut selected_credentials: Value = json!({
           "attrs":{
              "height_1":{
                "credential": {
                    "cred_info":{
                       "referent":LICENCE_CRED_ID,
                       "attrs":{
                          "sex":"male",
                          "age":"111",
                          "name":"Bob",
                          "height":"4'11"
                       },
                       "cred_def_id": CRED_DEF_ID,
                       "rev_reg_id":REV_REG_ID,
                       "cred_rev_id":CRED_REV_ID
                    },
                    "interval":null
                },
                "tails_file": get_temp_dir_path(TEST_TAILS_FILE).to_str().unwrap().to_string()
              },
           },
           "predicates":{ }
        });
        assert_eq!(credential_def_identifiers(&selected_credentials.to_string(), &proof_req_no_interval()).unwrap_err().kind(), VcxErrorKind::InvalidProofCredentialData);

        // Schema Id is null
        selected_credentials["attrs"]["height_1"]["cred_info"]["schema_id"] = serde_json::Value::Null;
        assert_eq!(credential_def_identifiers(&selected_credentials.to_string(), &proof_req_no_interval()).unwrap_err().kind(), VcxErrorKind::InvalidProofCredentialData);
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_build_rev_states_json() {
        let _setup = SetupAriesMocks::init();

        let cred1 = CredInfoProver {
            requested_attr: "height".to_string(),
            referent: "abc".to_string(),
            schema_id: SCHEMA_ID.to_string(),
            cred_def_id: CRED_DEF_ID.to_string(),
            rev_reg_id: Some(REV_REG_ID.to_string()),
            cred_rev_id: Some(CRED_REV_ID.to_string()),
            tails_file: Some(get_temp_dir_path(TEST_TAILS_FILE).to_str().unwrap().to_string()),
            revocation_interval: None,
            timestamp: None,
        };
        let mut cred_info = vec![cred1];
        let states = build_rev_states_json(cred_info.as_mut()).unwrap();
        let rev_state_json: Value = serde_json::from_str(REV_STATE_JSON).unwrap();
        let expected = json!({REV_REG_ID: {"1": rev_state_json}}).to_string();
        assert_eq!(states, expected);
        assert!(cred_info[0].timestamp.is_some());
    }

    #[cfg(feature = "pool_tests")]
    #[test]
    fn test_build_rev_states_json_empty() {
        let _setup = SetupLibraryWalletPoolZeroFees::init();

        // empty vector
        assert_eq!(build_rev_states_json(Vec::new().as_mut()).unwrap(), "{}".to_string());

        // no rev_reg_id
        let cred1 = CredInfoProver {
            requested_attr: "height_1".to_string(),
            referent: LICENCE_CRED_ID.to_string(),
            schema_id: SCHEMA_ID.to_string(),
            cred_def_id: CRED_DEF_ID.to_string(),
            rev_reg_id: None,
            cred_rev_id: Some(CRED_REV_ID.to_string()),
            tails_file: Some(get_temp_dir_path(TEST_TAILS_FILE).to_str().unwrap().to_string()),
            revocation_interval: None,
            timestamp: None,
        };
        assert_eq!(build_rev_states_json(vec![cred1].as_mut()).unwrap(), "{}".to_string());
    }

    #[cfg(feature = "pool_tests")]
    #[test]
    fn test_build_rev_states_json_real_no_cache() {
        let _setup = SetupLibraryWalletPoolZeroFees::init();

        let attrs = r#"["address1","address2","city","state","zip"]"#;
        let (schema_id, _, cred_def_id, _, _, _, _, cred_id, rev_reg_id, cred_rev_id) =
            ::utils::libindy::anoncreds::tests::create_and_store_credential(attrs, true);
        let cred2 = CredInfoProver {
            requested_attr: "height".to_string(),
            referent: cred_id,
            schema_id,
            cred_def_id,
            rev_reg_id: rev_reg_id.clone(),
            cred_rev_id: cred_rev_id.clone(),
            tails_file: Some(get_temp_dir_path(TEST_TAILS_FILE).to_str().unwrap().to_string()),
            revocation_interval: None,
            timestamp: None,
        };
        let rev_reg_id = rev_reg_id.unwrap();
        let rev_id = cred_rev_id.unwrap();

        // assert cache is empty
        let cache = get_rev_reg_cache(&rev_reg_id, &rev_id);
        assert_eq!(cache.rev_state, None);

        let states = build_rev_states_json(vec![cred2].as_mut()).unwrap();
        assert!(states.contains(&rev_reg_id));

        // check if this value is in cache now.
        let states: Value = serde_json::from_str(&states).unwrap();
        let state: HashMap<String, Value> = serde_json::from_value(states[&rev_reg_id].clone()).unwrap();

        let cache = get_rev_reg_cache(&rev_reg_id, &rev_id);
        let cache_rev_state = cache.rev_state.unwrap();
        let cache_rev_state_value: Value = serde_json::from_str(&cache_rev_state.value).unwrap();
        assert_eq!(cache_rev_state.timestamp, state.keys().next().unwrap().parse::<u64>().unwrap());
        assert_eq!(cache_rev_state_value.to_string(), state.values().next().unwrap().to_string());
    }

    #[cfg(feature = "pool_tests")]
    #[test]
    fn test_build_rev_states_json_real_cached() {
        let _setup = SetupLibraryWalletPoolZeroFees::init();

        let current_timestamp = time::get_time().sec as u64;
        let cached_rev_state = "{\"some\": \"json\"}".to_string();

        let attrs = r#"["address1","address2","city","state","zip"]"#;
        let (schema_id, _, cred_def_id, _, _, _, _, cred_id, rev_reg_id, cred_rev_id) =
            ::utils::libindy::anoncreds::tests::create_and_store_credential(attrs, true);
        let cred2 = CredInfoProver {
            requested_attr: "height".to_string(),
            referent: cred_id,
            schema_id,
            cred_def_id,
            rev_reg_id: rev_reg_id.clone(),
            cred_rev_id: cred_rev_id.clone(),
            tails_file: Some(get_temp_dir_path(TEST_TAILS_FILE).to_str().unwrap().to_string()),
            revocation_interval: None,
            timestamp: None,
        };
        let rev_reg_id = rev_reg_id.unwrap();
        let rev_id = cred_rev_id.unwrap();

        let cached_data = RevRegCache {
            rev_state: Some(RevState {
                timestamp: current_timestamp,
                value: cached_rev_state.clone(),
            })
        };
        set_rev_reg_cache(&rev_reg_id, &rev_id, &cached_data);

        // assert data is successfully cached.
        let cache = get_rev_reg_cache(&rev_reg_id, &rev_id);
        assert_eq!(cache, cached_data);

        let states = build_rev_states_json(vec![cred2].as_mut()).unwrap();
        assert!(states.contains(&rev_reg_id));

        let cache = get_rev_reg_cache(&rev_reg_id, &rev_id);
        // no revocation interval set -> assumed infinite --> the cache is updated with new value
        // assert_eq!(cache, cached_data);

        // check if this value is in cache now.
        let states: Value = serde_json::from_str(&states).unwrap();
        let state: HashMap<String, Value> = serde_json::from_value(states[&rev_reg_id].clone()).unwrap();

        let cache_rev_state = cache.rev_state.unwrap();
        let cache_rev_state_value: Value = serde_json::from_str(&cache_rev_state.value).unwrap();
        assert_eq!(cache_rev_state.timestamp, state.keys().next().unwrap().parse::<u64>().unwrap());
        assert_eq!(cache_rev_state_value.to_string(), state.values().next().unwrap().to_string());
    }

    #[cfg(feature = "pool_tests")]
    #[test]
    fn test_build_rev_states_json_real_with_older_cache() {
        let _setup = SetupLibraryWalletPoolZeroFees::init();

        let current_timestamp = time::get_time().sec as u64;
        let cached_timestamp = current_timestamp - 100;
        let cached_rev_state = "{\"witness\":{\"omega\":\"2 0BB3DE371F14384496D1F4FEB47B86A935C858BC21033B16251442FCBC5370A1 2 026F2848F2972B74079BEE16CDA9D48AD2FF7C7E39087515CB9B6E9B38D73BCB 2 10C48056D8C226141A8D7030E9FA17B7F02A39B414B9B64B6AECDDA5AFD1E538 2 11DCECD73A8FA6CFCD0468C659C2F845A9215842B69BA10355C1F4BF2D9A9557 2 095E45DDF417D05FB10933FFC63D474548B7FFFF7888802F07FFFFFF7D07A8A8 1 0000000000000000000000000000000000000000000000000000000000000000\"},\"rev_reg\":{\"accum\":\"2 033C0E6FAC660DF3582EF46021FAFDD93E111D1DC9DA59C4EA9B92BB21F8E0A4 2 02E0F749312228A93CF67BB5F86CA263FAE535A0F1CA449237D736939518EFF0 2 19BB82474D0BD0A1DDE72D377C8A965D6393071118B79D4220D4C9B93D090314 2 1895AAFD8050A8FAE4A93770C6C82881AB13134EE082C64CF6A7A379B3F6B217 2 095E45DDF417D05FB10933FFC63D474548B7FFFF7888802F07FFFFFF7D07A8A8 1 0000000000000000000000000000000000000000000000000000000000000000\"},\"timestamp\":100}".to_string();

        let attrs = r#"["address1","address2","city","state","zip"]"#;
        let (schema_id, _, cred_def_id, _, _, _, _, cred_id, rev_reg_id, cred_rev_id) =
            ::utils::libindy::anoncreds::tests::create_and_store_credential(attrs, true);
        let cred2 = CredInfoProver {
            requested_attr: "height".to_string(),
            referent: cred_id,
            schema_id,
            cred_def_id,
            rev_reg_id: rev_reg_id.clone(),
            cred_rev_id: cred_rev_id.clone(),
            tails_file: Some(get_temp_dir_path(TEST_TAILS_FILE).to_str().unwrap().to_string()),
            revocation_interval: Some(NonRevokedInterval { from: Some(cached_timestamp + 1), to: None }),
            timestamp: None,
        };
        let rev_reg_id = rev_reg_id.unwrap();

        let cached_data = RevRegCache {
            rev_state: Some(RevState {
                timestamp: cached_timestamp,
                value: cached_rev_state.clone(),
            })
        };
        let rev_id = cred_rev_id.unwrap();
        set_rev_reg_cache(&rev_reg_id, &rev_id, &cached_data);

        // assert data is successfully cached.
        let cache = get_rev_reg_cache(&rev_reg_id, &rev_id);
        assert_eq!(cache, cached_data);

        let states = build_rev_states_json(vec![cred2].as_mut()).unwrap();
        assert!(states.contains(&rev_reg_id));

        // assert cached data is updated.
        let cache = get_rev_reg_cache(&rev_reg_id, &rev_id);
        assert_ne!(cache, cached_data);

        // check if this value is in cache now.
        let states: Value = serde_json::from_str(&states).unwrap();
        let state: HashMap<String, Value> = serde_json::from_value(states[&rev_reg_id].clone()).unwrap();

        let cache_rev_state = cache.rev_state.unwrap();
        let cache_rev_state_value: Value = serde_json::from_str(&cache_rev_state.value).unwrap();
        assert_eq!(cache_rev_state.timestamp, state.keys().next().unwrap().parse::<u64>().unwrap());
        assert_eq!(cache_rev_state_value.to_string(), state.values().next().unwrap().to_string());
    }

    #[cfg(feature = "pool_tests")]
    #[test]
    fn test_build_rev_states_json_real_with_newer_cache() {
        let _setup = SetupLibraryWalletPoolZeroFees::init();

        let current_timestamp = time::get_time().sec as u64;
        let cached_timestamp = current_timestamp + 100;
        let cached_rev_state = "{\"witness\":{\"omega\":\"2 0BB3DE371F14384496D1F4FEB47B86A935C858BC21033B16251442FCBC5370A1 2 026F2848F2972B74079BEE16CDA9D48AD2FF7C7E39087515CB9B6E9B38D73BCB 2 10C48056D8C226141A8D7030E9FA17B7F02A39B414B9B64B6AECDDA5AFD1E538 2 11DCECD73A8FA6CFCD0468C659C2F845A9215842B69BA10355C1F4BF2D9A9557 2 095E45DDF417D05FB10933FFC63D474548B7FFFF7888802F07FFFFFF7D07A8A8 1 0000000000000000000000000000000000000000000000000000000000000000\"},\"rev_reg\":{\"accum\":\"2 033C0E6FAC660DF3582EF46021FAFDD93E111D1DC9DA59C4EA9B92BB21F8E0A4 2 02E0F749312228A93CF67BB5F86CA263FAE535A0F1CA449237D736939518EFF0 2 19BB82474D0BD0A1DDE72D377C8A965D6393071118B79D4220D4C9B93D090314 2 1895AAFD8050A8FAE4A93770C6C82881AB13134EE082C64CF6A7A379B3F6B217 2 095E45DDF417D05FB10933FFC63D474548B7FFFF7888802F07FFFFFF7D07A8A8 1 0000000000000000000000000000000000000000000000000000000000000000\"},\"timestamp\":100}".to_string();

        let attrs = r#"["address1","address2","city","state","zip"]"#;
        let (schema_id, _, cred_def_id, _, _, _, _, cred_id, rev_reg_id, cred_rev_id) =
            ::utils::libindy::anoncreds::tests::create_and_store_credential(attrs, true);
        let cred2 = CredInfoProver {
            requested_attr: "height".to_string(),
            referent: cred_id,
            schema_id,
            cred_def_id,
            rev_reg_id: rev_reg_id.clone(),
            cred_rev_id: cred_rev_id.clone(),
            tails_file: Some(get_temp_dir_path(TEST_TAILS_FILE).to_str().unwrap().to_string()),
            revocation_interval: Some(NonRevokedInterval { from: None, to: Some(cached_timestamp - 1) }),
            timestamp: None,
        };
        let rev_reg_id = rev_reg_id.unwrap();

        let cached_data = RevRegCache {
            rev_state: Some(RevState {
                timestamp: cached_timestamp,
                value: cached_rev_state.clone(),
            })
        };
        let rev_id = cred_rev_id.unwrap();
        set_rev_reg_cache(&rev_reg_id, &rev_id, &cached_data);

        // assert data is successfully cached.
        let cache = get_rev_reg_cache(&rev_reg_id, &rev_id);
        assert_eq!(cache, cached_data);

        let states = build_rev_states_json(vec![cred2].as_mut()).unwrap();
        assert!(states.contains(&rev_reg_id));

        // assert cached data is unchanged.
        let cache = get_rev_reg_cache(&rev_reg_id, &rev_id);
        assert_eq!(cache, cached_data);

        // check if this value is not in cache.
        let states: Value = serde_json::from_str(&states).unwrap();
        let state: HashMap<String, Value> = serde_json::from_value(states[&rev_reg_id].clone()).unwrap();

        let cache_rev_state = cache.rev_state.unwrap();
        let cache_rev_state_value: Value = serde_json::from_str(&cache_rev_state.value).unwrap();
        assert_ne!(cache_rev_state.timestamp, state.keys().next().unwrap().parse::<u64>().unwrap());
        assert_ne!(cache_rev_state_value.to_string(), state.values().next().unwrap().to_string());
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_get_credential_intervals_from_proof_req() {
        let _setup = SetupDefaults::init();

        let proof_req = json!({
            "nonce": "123432421212",
            "name": "proof_req_1",
            "version": "0.1",
            "requested_attributes": {
                "address1_1": {
                    "name": "address1",
                    "non_revoked":  {"from": 123, "to": 456}
                },
                "zip_2": { "name": "zip" }
            },
            "requested_predicates": {},
            "non_revoked": {"from": 098, "to": 123}
        });
        let proof_req: ProofRequestData = serde_json::from_value(proof_req).unwrap();

        // Attribute not found in proof req
        assert_eq!(_get_revocation_interval("not here", &proof_req).unwrap_err().kind(), VcxErrorKind::InvalidProofCredentialData);

        // attribute interval overrides proof request interval
        let interval = Some(NonRevokedInterval { from: Some(123), to: Some(456) });
        assert_eq!(_get_revocation_interval("address1_1", &proof_req).unwrap(), interval);

        // when attribute interval is None, defaults to proof req interval
        let interval = Some(NonRevokedInterval { from: Some(098), to: Some(123) });
        assert_eq!(_get_revocation_interval("zip_2", &proof_req).unwrap(), interval);

        // No interval provided for attribute or proof req
        assert_eq!(_get_revocation_interval("address1_1", &proof_req_no_interval()).unwrap(), None);
    }
}
