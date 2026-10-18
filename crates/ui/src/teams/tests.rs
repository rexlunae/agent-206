use super::*;
use uuid::Uuid;

#[test]
fn a_listing_failure_is_kept_whole() {
    let report = ErrorReport::new(ErrorKind::Storage, Uuid::new_v4().to_string());
    assert_eq!(decode(Reply::Error(report.clone())).unwrap_err(), report);
    assert_eq!(decode(Reply::Ok).unwrap_err().kind, ErrorKind::Protocol);
    assert_eq!(
        decode(Reply::AgentTeams(AgentTeams::default())).unwrap(),
        AgentTeams::default()
    );
}
