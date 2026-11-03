use super::*;

const NOW: i64 = 1_800_000_000;

#[test]
fn only_owner_can_approve_and_tokens_are_scoped_revocable_and_hashed() {
    let dir = tempfile::tempdir().unwrap();
    let auth = AuthStore::new(dir.path());
    auth.configure_owner(Some(42)).unwrap();
    auth.grant_test_user(7).unwrap();
    let request = auth.start_device_pairing("Laptop", NOW).unwrap();
    assert_eq!(request.expires_at, NOW + 300);
    assert_eq!(request.code.len(), 8);
    assert!(
        auth.approve_device_pairing(
            request.request_id,
            &request.code,
            &Identity::Telegram(7),
            NOW
        )
        .is_err()
    );
    assert!(
        auth.approve_device_pairing(request.request_id, "WRONG123", &Identity::Owner, NOW)
            .is_err()
    );
    auth.approve_device_pairing(
        request.request_id,
        &request.code,
        &Identity::Telegram(42),
        NOW,
    )
    .unwrap();
    assert!(
        auth.poll_device_pairing(request.request_id, &request.code, NOW)
            .is_err()
    );
    let PairingPoll::Authorized { device_id, token } = auth
        .poll_device_pairing(request.request_id, &request.polling_secret, NOW)
        .unwrap()
    else {
        panic!("not authorized")
    };
    assert_eq!(
        auth.device_identity(&token, NOW).unwrap(),
        Some(DeviceIdentity { device_id })
    );
    assert!(
        auth.identity(&token, NOW).unwrap().is_none(),
        "device token became browser owner"
    );
    assert!(
        auth.device_identity(&token, NOW + 30 * 86400)
            .unwrap()
            .is_none()
    );
    let journal = fs::read_to_string(auth.root.join("auth.json")).unwrap();
    for secret in [&token, &request.code, &request.polling_secret] {
        assert!(!journal.contains(secret));
    }
    assert!(matches!(
        auth.poll_device_pairing(request.request_id, &request.polling_secret, NOW + 2)
            .unwrap(),
        PairingPoll::Expired
    ));
    auth.revoke_device(device_id).unwrap();
    assert!(
        AuthStore::new(dir.path())
            .device_identity(&token, NOW + 1)
            .unwrap()
            .is_none()
    );
}

#[test]
fn pairing_expires_and_polls_are_throttled_without_consuming_request() {
    let dir = tempfile::tempdir().unwrap();
    let auth = AuthStore::new(dir.path());
    let request = auth.start_device_pairing("Laptop", NOW).unwrap();
    assert!(matches!(
        auth.poll_device_pairing(request.request_id, &request.polling_secret, NOW)
            .unwrap(),
        PairingPoll::Pending
    ));
    assert!(
        auth.poll_device_pairing(request.request_id, &request.polling_secret, NOW + 1)
            .is_err()
    );
    assert!(matches!(
        auth.poll_device_pairing(request.request_id, &request.polling_secret, NOW + 2)
            .unwrap(),
        PairingPoll::Pending
    ));
    assert!(
        auth.approve_device_pairing(
            request.request_id,
            &request.code,
            &Identity::Owner,
            NOW + 300
        )
        .is_err()
    );
    assert!(matches!(
        auth.poll_device_pairing(request.request_id, &request.polling_secret, NOW + 300)
            .unwrap(),
        PairingPoll::Expired
    ));
}

#[test]
fn capacity_and_owner_approval_budget_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let auth = AuthStore::new(dir.path());
    let request = auth.start_device_pairing("Laptop", NOW).unwrap();
    for _ in 0..5 {
        assert!(
            auth.approve_device_pairing(request.request_id, "WRONG123", &Identity::Owner, NOW)
                .is_err()
        );
    }
    assert!(
        auth.approve_device_pairing(
            request.request_id,
            &request.code,
            &Identity::Owner,
            NOW + 59
        )
        .is_err()
    );
    auth.approve_device_pairing(
        request.request_id,
        &request.code,
        &Identity::Owner,
        NOW + 60,
    )
    .unwrap();
    for _ in 1..32 {
        auth.start_device_pairing("Laptop", NOW).unwrap();
    }
    assert!(auth.start_device_pairing("Laptop", NOW).is_err());
    assert!(auth.start_device_pairing("\n", NOW + 301).is_err());
    assert!(auth.start_device_pairing("Laptop", NOW + 301).is_ok());
}

#[test]
fn device_limit_and_owner_rotation_revoke_old_authority() {
    let dir = tempfile::tempdir().unwrap();
    let auth = AuthStore::new(dir.path());
    auth.configure_owner(Some(42)).unwrap();
    let mut last_token = String::new();
    for i in 0..17 {
        let now = NOW + i * 61;
        let request = auth.start_device_pairing("Laptop", now).unwrap();
        auth.approve_device_pairing(request.request_id, &request.code, &Identity::Owner, now)
            .unwrap();
        let result = auth.poll_device_pairing(request.request_id, &request.polling_secret, now);
        if i == 16 {
            assert!(result.is_err(), "device capacity bypassed");
        } else {
            let PairingPoll::Authorized { token, .. } = result.unwrap() else {
                panic!()
            };
            last_token = token;
        }
    }
    auth.configure_owner(Some(43)).unwrap();
    assert!(
        auth.device_identity(&last_token, NOW + 1000)
            .unwrap()
            .is_none()
    );
    let pending = auth.start_device_pairing("Laptop", NOW + 1000).unwrap();
    auth.set_password("a new owner password").unwrap();
    assert!(
        auth.approve_device_pairing(
            pending.request_id,
            &pending.code,
            &Identity::Owner,
            NOW + 1000
        )
        .is_err()
    );
}
