//! The data-plane layout rules (SPEC.md §11–§12).
use ava1::gen::*;
use ava1::wire::{FrameMessage, Message};

const J: [u8; 16] = [7; 16];

fn first16<M: FrameMessage>(m: M) {
    let b = m.to_bytes().unwrap();
    assert_eq!(&b[..16], &J, "{} must start with job_id", M::NAME);
    assert!((0x20..=0x3F).contains(&M::TYPE), "{} type", M::NAME);
}

#[test]
fn every_data_message_starts_with_the_job_id() {
    first16(JobOpen {
        job_id: J,
        ..Default::default()
    });
    first16(JobOpenAck {
        job_id: J,
        ..Default::default()
    });
    first16(ManifestPage {
        job_id: J,
        ..Default::default()
    });
    first16(ManifestEnd {
        job_id: J,
        ..Default::default()
    });
    first16(JobMap {
        job_id: J,
        ..Default::default()
    });
    first16(Resume {
        job_id: J,
        ..Default::default()
    });
    first16(Chunk {
        job_id: J,
        ..Default::default()
    });
    first16(Bundle {
        job_id: J,
        ..Default::default()
    });
    first16(Received {
        job_id: J,
        ..Default::default()
    });
    first16(Credit {
        job_id: J,
        ..Default::default()
    });
    first16(Durable {
        job_id: J,
        ..Default::default()
    });
    first16(FileRoot {
        job_id: J,
        ..Default::default()
    });
    first16(FileRetry {
        job_id: J,
        ..Default::default()
    });
    first16(Status {
        job_id: J,
        ..Default::default()
    });
    first16(JobDone {
        job_id: J,
        ..Default::default()
    });
    first16(JobCancel {
        job_id: J,
        ..Default::default()
    });
}

#[test]
fn a_full_manifest_page_fits_a_control_frame() {
    // 60 KiB of entries with 1 KiB paths, with the page message's own fields in the slack.
    let e = ManifestEntry {
        path: "p".repeat(1024),
        ..Default::default()
    };
    let per = e.to_bytes().unwrap().len() + 4;
    let n = (60 * 1024 - 64) / per;
    let page = ManifestPage {
        job_id: J,
        entries: vec![e; n],
    };
    let b = page.to_bytes().unwrap();
    // A page rides the control connection, which is sealed from Hs3 on (§5), and the cap
    // is on `body_len`, which counts the MAC (§2, §4.4) — so the body may use
    // CONTROL_MAX_BODY - MAC_LEN.
    assert!(
        b.len() + ava1::keys::MAC_LEN <= ava1::frame::CONTROL_MAX_BODY as usize,
        "{}",
        b.len()
    );
    assert_eq!(ManifestPage::decode(&b).unwrap().entries.len(), n);
}

/// The numbers that travel on the wire, pinned so an edit to the schema cannot silently
/// renumber what a peer already understands. The SPEC states some of these in prose (§1
/// port 9120, §9 eight lanes, §12.2 one-MiB groups, §11.3 job kinds and entry kinds,
/// §11.4 policies, 1 = node.info in §7); the rest are the schema's own numbering, which
/// both implementations are generated from — this test is the tripwire, not a second
/// source of truth.
#[test]
fn the_wire_numbers_are_pinned() {
    assert_eq!(DEFAULT_PORT, 9120); // §1
    assert_eq!(MAX_LANES, 8); // §9: lanes 1..=8
    assert_eq!(1u64 << GROUP_SHIFT, 1 << 20); // §12.2: one verification group
    assert_eq!(METHOD_NODE_INFO, 1); // §7
    assert_eq!(METHOD_PAIRING_OPEN, 2); // §5: pairing.open
    assert_eq!((JOB_UPLOAD, JOB_DOWNLOAD, JOB_COPY), (1, 2, 3)); // §11.3
    assert_eq!((POLICY_REPLACE, POLICY_SKIP_EXISTING, POLICY_VERIFY), (0, 1, 2)); // §11.4
    assert_eq!((ENTRY_FILE, ENTRY_DIR), (0, 1)); // §11.3
    assert_eq!(JF_SINGLE_FILE, 1); // §11.6
    assert_eq!((STATUS_OK, CAP_DATA_PLANE), (0, 1)); // §7, §10
    assert_eq!((RETRY_VERIFY, RETRY_IO, RETRY_CHANGED), (1, 2, 3));
    // Statuses travel in `RpcResponse.status` and as `Error.code`: renumbering these is a
    // protocol break.
    let errors: [u16; 17] = [
        ERR_NOT_PAIRED,
        ERR_PAIRING_CLOSED,
        ERR_UNSUPPORTED_VERSION,
        ERR_PROTOCOL,
        ERR_BAD_JOIN,
        ERR_UNKNOWN_METHOD,
        ERR_INTERNAL,
        ERR_BUSY,
        ERR_PATH,
        ERR_NO_SPACE,
        ERR_UNKNOWN_JOB,
        ERR_IO,
        ERR_VERIFY,
        ERR_EXISTS,
        ERR_CANCELLED,
        ERR_CROSS_DEVICE,
        ERR_CREDIT,
    ];
    assert_eq!(
        errors,
        [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]
    );
}
