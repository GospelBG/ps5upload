#![cfg(unix)]
use ava1_ctest::ffi;

#[test]
fn the_c_slot_is_what_the_rust_stamper_looks_for() {
    unsafe {
        let slot = std::ptr::addr_of_mut!(ffi::ava1_trust_slot);
        let mut bytes = std::ptr::read_volatile(slot);
        assert_eq!(ava1::trust::read(&bytes), None);
        let mut out = [0u8; 32];
        assert_eq!(ffi::ava1_trust_slot_key(out.as_mut_ptr()), -1, "unstamped");
        ava1::trust::stamp(&mut bytes, &[0x5a; 32]).unwrap();
        std::ptr::write_volatile(slot, bytes);
        assert_eq!(ffi::ava1_trust_slot_key(out.as_mut_ptr()), 0);
        assert_eq!(out, [0x5a; 32]);
        bytes[9] = 0;
        std::ptr::write_volatile(slot, bytes);
    }
}
