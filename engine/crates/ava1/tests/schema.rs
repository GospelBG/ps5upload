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
    // 60 KiB of entries with 1 KiB paths still decodes under the 64 KiB control cap.
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
    assert!(
        b.len() + 16 <= ava1::frame::CONTROL_MAX_BODY as usize,
        "{}",
        b.len()
    );
    assert_eq!(ManifestPage::decode(&b).unwrap().entries.len(), n);
}

#[test]
fn the_group_is_one_mebibyte() {
    assert_eq!(1u64 << GROUP_SHIFT, 1 << 20);
}
