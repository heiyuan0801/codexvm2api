use gateway_core::account::{
    AccountSlotGeneration, AccountSlotIdentity, AccountSlotInstanceId, ProviderAccountId,
    ProviderAccountSlot,
};

#[test]
fn slot_instance_id_requires_slot_prefix_and_uuid() {
    assert!(AccountSlotInstanceId::parse("slot_0199f4c8-52a8-7aa0-a6d7-f75219e82e3d").is_ok());
    assert!(AccountSlotInstanceId::parse("0199f4c8-52a8-7aa0-a6d7-f75219e82e3d").is_err());
    assert!(AccountSlotInstanceId::parse("slot_not-a-uuid").is_err());
}

#[test]
fn slot_identity_validates_stable_software_facts() {
    let identity = AccountSlotIdentity::new(
        "cpr-slot-0199f4c8".to_owned(),
        "1fc17f765c107a4896c4f29d3d70c327".to_owned(),
        "0199f4c8-52a8-7aa0-a6d7-f75219e82e3d".to_owned(),
        "Asia/Shanghai".to_owned(),
    )
    .expect("valid stable identity");

    assert_eq!(identity.hostname(), "cpr-slot-0199f4c8");
    assert_eq!(identity.timezone(), "Asia/Shanghai");
    assert!(
        AccountSlotIdentity::new(
            "INVALID_HOST".to_owned(),
            "1fc17f765c107a4896c4f29d3d70c327".to_owned(),
            "0199f4c8-52a8-7aa0-a6d7-f75219e82e3d".to_owned(),
            "Asia/Shanghai".to_owned(),
        )
        .is_err()
    );
    assert!(
        AccountSlotIdentity::new(
            "cpr-slot-0199f4c8".to_owned(),
            "short".to_owned(),
            "0199f4c8-52a8-7aa0-a6d7-f75219e82e3d".to_owned(),
            "Asia/Shanghai".to_owned(),
        )
        .is_err()
    );
}

#[test]
fn account_slot_preserves_identity_while_disabled() {
    let slot = ProviderAccountSlot::new(
        ProviderAccountId::new("acct_slot_fixture").expect("account"),
        false,
        AccountSlotInstanceId::parse("slot_0199f4c8-52a8-7aa0-a6d7-f75219e82e3d")
            .expect("instance"),
        AccountSlotIdentity::new(
            "cpr-slot-0199f4c8".to_owned(),
            "1fc17f765c107a4896c4f29d3d70c327".to_owned(),
            "0199f4c8-52a8-7aa0-a6d7-f75219e82e3d".to_owned(),
            "Asia/Shanghai".to_owned(),
        )
        .expect("identity"),
        AccountSlotGeneration::new(1).expect("generation"),
    );

    assert!(!slot.enabled());
    assert_eq!(slot.identity().hostname(), "cpr-slot-0199f4c8");
    assert_eq!(slot.generation().get(), 1);
}
