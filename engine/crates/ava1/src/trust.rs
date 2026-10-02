//! The trust slot in the payload ELF (SPEC.md §5.1).

pub const MAGIC: &[u8; 9] = b"AVA1TRUST";
pub const SLOT_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    #[error("this ELF has no AVA1 trust slot")]
    NoSlot,
    #[error("this ELF has {0} AVA1 trust slots; expected exactly one")]
    Many(usize),
}

/// Offsets of every slot: magic, state 0 or 1, zeros at 10..16 and 48..64.
fn slots(elf: &[u8]) -> Vec<usize> {
    if elf.len() < SLOT_LEN {
        return Vec::new();
    }
    (0..=elf.len() - SLOT_LEN)
        .filter(|&o| {
            &elf[o..o + 9] == MAGIC
                && elf[o + 9] <= 1
                && elf[o + 10..o + 16].iter().all(|&b| b == 0)
                && elf[o + 48..o + 64].iter().all(|&b| b == 0)
        })
        .collect()
}

/// Writes `key` into the ELF's single trust slot (state 1).
pub fn stamp(elf: &mut [u8], key: &[u8; 32]) -> Result<(), TrustError> {
    match slots(elf).as_slice() {
        [o] => {
            elf[*o + 9] = 1;
            elf[*o + 16..*o + 48].copy_from_slice(key);
            Ok(())
        }
        [] => Err(TrustError::NoSlot),
        many => Err(TrustError::Many(many.len())),
    }
}

/// What every sender of the ps5upload helper does before it sends: stamp `key` (the
/// sending engine's AVA1 public key) so the console trusts that engine without pairing.
/// `Err` is a reason to log, never a reason not to send: an unstamped helper still
/// runs, and the console falls back to its pairing window.
pub fn stamp_helper(elf: &mut [u8], key: Option<&[u8; 32]>) -> Result<(), String> {
    let key = key.ok_or("AVA1 trust not stamped: this engine has no AVA1 identity")?;
    stamp(elf, key).map_err(|e| format!("AVA1 trust not stamped: {e}"))
}

/// The stamped key, if the ELF has exactly one stamped slot.
pub fn read(elf: &[u8]) -> Option<[u8; 32]> {
    match slots(elf).as_slice() {
        [o] if elf[*o + 9] == 1 => elf[*o + 16..*o + 48].try_into().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elf_with(slots: usize) -> Vec<u8> {
        let mut b = vec![0x11u8; 4096];
        b[..4].copy_from_slice(b"\x7fELF");
        for i in 0..slots {
            let at = 1000 + i * 200;
            b[at..at + SLOT_LEN].fill(0);
            b[at..at + 9].copy_from_slice(MAGIC);
        }
        b
    }

    #[test]
    fn stamping_writes_the_key_once_and_can_be_redone() {
        let mut elf = elf_with(1);
        assert_eq!(read(&elf), None);
        stamp(&mut elf, &[7; 32]).unwrap();
        assert_eq!(read(&elf), Some([7; 32]));
        assert_eq!(elf[1009], 1);
        stamp(&mut elf, &[8; 32]).unwrap();
        assert_eq!(read(&elf), Some([8; 32]));
    }

    #[test]
    fn stamp_helper_reports_why_it_did_not_stamp() {
        let mut elf = elf_with(1);
        assert!(stamp_helper(&mut elf, None)
            .unwrap_err()
            .contains("no AVA1 identity"));
        assert_eq!(read(&elf), None);
        stamp_helper(&mut elf, Some(&[3; 32])).unwrap();
        assert_eq!(read(&elf), Some([3; 32]));
        let mut old_build = elf_with(0);
        let before = old_build.clone();
        assert!(stamp_helper(&mut old_build, Some(&[3; 32])).is_err());
        assert_eq!(old_build, before, "an ELF without a slot is sent unchanged");
    }

    #[test]
    fn no_slot_or_two_slots_is_refused() {
        assert_eq!(stamp(&mut elf_with(0), &[1; 32]), Err(TrustError::NoSlot));
        assert_eq!(stamp(&mut elf_with(2), &[1; 32]), Err(TrustError::Many(2)));
    }

    #[test]
    fn the_magic_without_the_zero_padding_is_not_a_slot() {
        let mut elf = elf_with(1);
        elf[1000 + 12] = 0x55; // reserved byte not zero: some other data that happens to start "AVA1TRUST"
        assert_eq!(stamp(&mut elf, &[1; 32]), Err(TrustError::NoSlot));
    }
}
