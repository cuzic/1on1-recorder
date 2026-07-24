use capture_api::device_diff::DeviceSnapshot;
use capture_api::rebinding::EndpointId;

fn endpoint(id: &str) -> EndpointId {
    EndpointId(id.to_string())
}

#[test]
fn first_diff_from_empty_reports_only_added() {
    let mut snapshot = DeviceSnapshot::default();
    let delta = snapshot.diff_and_update(DeviceSnapshot::from_ids([endpoint("MicA"), endpoint("SpeakerA")]));
    assert_eq!(delta.added, vec![endpoint("MicA"), endpoint("SpeakerA")]);
    assert!(delta.removed.is_empty());
}

#[test]
fn unchanged_list_reports_no_delta() {
    let mut snapshot = DeviceSnapshot::from_ids([endpoint("MicA")]);
    let delta = snapshot.diff_and_update(DeviceSnapshot::from_ids([endpoint("MicA")]));
    assert!(delta.is_empty());
}

#[test]
fn device_unplugged_then_replugged_round_trips() {
    let mut snapshot = DeviceSnapshot::from_ids([endpoint("MicA"), endpoint("SpeakerA")]);

    let delta = snapshot.diff_and_update(DeviceSnapshot::from_ids([endpoint("SpeakerA")]));
    assert_eq!(delta.removed, vec![endpoint("MicA")]);
    assert!(delta.added.is_empty());

    let delta = snapshot.diff_and_update(DeviceSnapshot::from_ids([endpoint("MicA"), endpoint("SpeakerA")]));
    assert_eq!(delta.added, vec![endpoint("MicA")]);
    assert!(delta.removed.is_empty());
}

#[test]
fn is_empty_is_false_when_only_added_is_non_empty() {
    let mut snapshot = DeviceSnapshot::default();
    let delta = snapshot.diff_and_update(DeviceSnapshot::from_ids([endpoint("MicA")]));
    assert!(!delta.is_empty());
}

#[test]
fn is_empty_is_false_when_only_removed_is_non_empty() {
    let mut snapshot = DeviceSnapshot::from_ids([endpoint("MicA")]);
    let delta = snapshot.diff_and_update(DeviceSnapshot::default());
    assert!(!delta.is_empty());
}

#[test]
fn a_different_device_appearing_does_not_mask_another_disappearing() {
    let mut snapshot = DeviceSnapshot::from_ids([endpoint("MicA")]);
    let delta = snapshot.diff_and_update(DeviceSnapshot::from_ids([endpoint("MicB")]));
    assert_eq!(delta.added, vec![endpoint("MicB")]);
    assert_eq!(delta.removed, vec![endpoint("MicA")]);
}
