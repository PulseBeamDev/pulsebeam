use super::*;

fn pull(reader: &Reader, now: Instant) -> Pull {
    let bytes = reader.pull(now).unwrap();
    let Some(Frame::Pull(pull)) = Frame::decode(&bytes) else {
        panic!("expected a pull")
    };
    pull
}

fn receive(reader: &mut Reader, now: Instant, frame: &[u8]) -> Option<(Vec<u8>, Instant)> {
    let Some(Frame::Chunk(chunk)) = Frame::decode(frame) else {
        panic!("expected a chunk")
    };
    reader.receive(now, chunk)
}

#[test]
fn largest_image_survives_loss_duplication_and_reordering_without_renewing_its_deadline() {
    let start = Instant::now();
    let mut now = start;
    let data: Vec<_> = (0..MAX_IMAGE_BYTES)
        .map(|i| i.to_be_bytes()[0] ^ i.to_le_bytes()[1])
        .collect();
    let mut reader = Reader::default();
    let mut sender = Sender::default();
    assert!(reader.start(now));
    assert!(sender.needs_image(pull(&reader, now)));
    sender.set_image(now, data.clone());
    // Lose one chunk from every window, deliver the others in reverse order,
    // and duplicate one. Only the missing chunk should be retried.
    for _ in 0..MAX_IMAGE_BYTES.div_ceil(CHUNK_BYTES * WINDOW) {
        let request = pull(&reader, now);
        assert!(!sender.needs_image(request));
        let mut flight: Vec<_> = sender.respond(now, request).collect();
        let lost = flight.pop().unwrap();
        while let Some(chunk) = flight.pop() {
            assert!(receive(&mut reader, now, &chunk).is_none());
            assert!(receive(&mut reader, now, &chunk).is_none());
        }
        now += RETRY;
        let retry: Vec<_> = sender.respond(now, pull(&reader, now)).collect();
        assert_eq!(retry, [lost]);
        if let Some((complete, deadline)) = receive(&mut reader, now, retry.first().unwrap()) {
            assert_eq!(complete, data);
            assert_eq!(deadline, start + DEADLINE);
            assert!(now < deadline);
            assert!(reader.pull(now).is_none());
            return;
        }
        now += RETRY;
    }
    panic!("valid maximum-sized image did not converge")
}

#[test]
fn expired_or_superseded_round_cannot_apply_a_delayed_complete_image() {
    let now = Instant::now();
    let mut reader = Reader::default();
    let mut sender = Sender::default();
    assert!(reader.start(now));
    let first = pull(&reader, now);
    assert!(sender.needs_image(first));
    sender.set_image(now, vec![1, 2]);
    let old = sender.respond(now, first).next().unwrap();
    let later = now + DEADLINE;
    assert!(reader.pull(later).is_none());
    assert!(receive(&mut reader, later, &old).is_none());
    assert!(reader.start(later));
    let second = pull(&reader, later);
    assert!(sender.needs_image(second));
    assert!(!sender.needs_image(first));
    assert!(receive(&mut reader, later, &old).is_none());
    sender.set_image(later, vec![3]);
    assert!(sender.respond(later, first).next().is_none());
    let fresh = sender.respond(later, second).next().unwrap();
    assert_eq!(receive(&mut reader, later, &fresh).unwrap().0, vec![3]);
}

#[test]
fn invalid_lengths_and_conflicting_duplicates_do_not_produce_an_image() {
    let now = Instant::now();
    let mut reader = Reader::default();
    let mut sender = Sender::default();
    assert!(reader.start(now));
    let request = pull(&reader, now);
    assert!(sender.needs_image(request));
    sender.set_image(now, vec![0; CHUNK_BYTES + 1]);
    let mut flight = sender.respond(now, request);
    let chunk = flight.next().unwrap();
    let mut malformed = chunk.clone();
    malformed
        .get_mut(10..14)
        .unwrap()
        .copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(Frame::decode(&malformed).is_none());
    assert!(Frame::decode(chunk.get(..chunk.len() - 1).unwrap()).is_none());
    assert!(receive(&mut reader, now, &chunk).is_none());
    let mut conflict = chunk;
    *conflict.last_mut().unwrap() = 1;
    assert!(receive(&mut reader, now, &conflict).is_none());
    assert!(reader.pull(now).is_none());
    assert!(receive(&mut reader, now, &flight.next().unwrap()).is_none());
}
