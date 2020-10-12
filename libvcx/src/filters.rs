use serde_json;

use error::prelude::*;
use utils::error;

use aries::messages::proof_presentation::presentation_request::PresentationRequest;

// TODO: Log errors
fn _filter_proof_requests_by_name(requests: &str, match_name: &str) -> VcxResult<Vec<PresentationRequest>> {
    let presentation_requests: Vec<PresentationRequest> = serde_json::from_str(requests)
        .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidJson, format!("Failed to deserialize Vec<PresentationRequest>: {}\nObtained error: {:?}", requests, err)))?;
    let filtered = presentation_requests
        .into_iter()
        .filter_map(|presentation_request| {
            match presentation_request.request_presentations_attach.content().ok() {
                Some(content) => {
                     match serde_json::from_str(&content).map(|value: serde_json::Value| value.get("name").unwrap_or(&serde_json::Value::Null).as_str().unwrap_or("").to_string()) {
                         Ok(name) if name == String::from(match_name) => Some(presentation_request),
                         _ => None
                     }
                }
                _ => None
            }
        })
        .collect();
    Ok(filtered)
}

pub fn filter_proof_requests_by_name(requests: &str, name: &str) -> VcxResult<String> {
    let presentation_requests: Vec<PresentationRequest> = _filter_proof_requests_by_name(requests, name)?;
    let filtered: String = serde_json::to_string(&presentation_requests)
        .map_err(|err| VcxError::from_msg(VcxErrorKind::InvalidJson, format!("Failed to serialize filtered proof requests: {}\nObtained error: {:?}", requests, err)))?;
    Ok(filtered)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use utils::constants::*;
    use utils::devsetup::*;
    use utils::httpclient::HttpClientMockResponse;
    use utils::mockdata::mockdata_proof;

    #[test]
    #[cfg(feature = "general_test")]
    fn test_filter_proof_requests_by_name() {
        let filtered = _filter_proof_requests_by_name(mockdata_proof::PRESENTATION_REQUEST_MESSAGE_ARRAY, "request1").unwrap();
        assert_eq!(filtered.len(), 1);

        let filtered = _filter_proof_requests_by_name(mockdata_proof::PRESENTATION_REQUEST_MESSAGE_ARRAY, "request2").unwrap();
        assert_eq!(filtered.len(), 1);

        let filtered = _filter_proof_requests_by_name(mockdata_proof::PRESENTATION_REQUEST_MESSAGE_ARRAY, "not there").unwrap();
        assert_eq!(filtered.len(), 0);

        let filtered = _filter_proof_requests_by_name(mockdata_proof::PRESENTATION_REQUEST_MESSAGE_ARRAY, "").unwrap();
        assert_eq!(filtered.len(), 0);

        let filtered = _filter_proof_requests_by_name(mockdata_proof::PRESENTATION_REQUEST_MESSAGE_ARRAY_EMPTY_ATTACH, "not there").unwrap();
        assert_eq!(filtered.len(), 0);

        // TODO: fix the behavior so that this passes
        let filtered = _filter_proof_requests_by_name(mockdata_proof::PRESENTATION_REQUEST_MESSAGE_ARRAY_EMPTY_ATTACH, "").unwrap();
        assert_eq!(filtered.len(), 0);
    }

    #[test]
    #[cfg(feature = "general_test")]
    fn test_filter_proof_requests_by_name_serialize_deserialize() {}
}
