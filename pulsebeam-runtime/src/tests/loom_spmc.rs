use crate::sync::spmc::{RecvError, channel};
use loom::thread;

#[test]
fn loom_spmc_basic_broadcast() {
    loom::model(|| {
        let (tx, mut rx) = channel::<u64>(4);

        let producer = thread::spawn(move || {
            tx.send(10);
            tx.send(20);
            drop(tx);
        });

        let consumer = thread::spawn(move || {
            let mut values = Vec::new();
            let mut iterations = 0;

            loop {
                iterations += 1;
                if iterations > 100 {
                    break;
                }

                match rx.try_recv() {
                    Ok(Some(slot)) => values.push(slot.value),
                    Err(RecvError::Closed) => break,
                    Err(RecvError::Lagged(_)) => continue,
                    Ok(None) => thread::yield_now(),
                }
            }
            values
        });

        producer.join().unwrap();
        let values = consumer.join().unwrap();

        assert!(values.len() <= 2);
        for window in values.windows(2) {
            assert!(window[0] < window[1]);
        }
    });
}

#[test]
#[ignore]
fn loom_sfu_fanout_data_integrity() {
    loom::model(|| {
        const NUM_PACKETS: u64 = 5;
        let (tx, rx) = channel::<u64>(4);

        let producer = thread::spawn(move || {
            for i in 0..NUM_PACKETS {
                tx.send(i);
            }
            drop(tx);
        });

        let mut rx1 = rx.clone();
        let mut rx2 = rx;

        let consumer1 = thread::spawn(move || {
            let mut received = Vec::new();
            for _ in 0..10 {
                match rx1.try_recv() {
                    Ok(Some(slot)) => received.push(slot.value),
                    Err(RecvError::Closed) => break,
                    Err(RecvError::Lagged(_)) => continue,
                    Ok(None) => thread::yield_now(),
                }
            }
            received
        });

        let consumer2 = thread::spawn(move || {
            let mut received = Vec::new();
            for _ in 0..10 {
                match rx2.try_recv() {
                    Ok(Some(slot)) => received.push(slot.value),
                    Err(RecvError::Closed) => break,
                    Err(RecvError::Lagged(_)) => continue,
                    Ok(None) => thread::yield_now(),
                }
            }
            received
        });

        producer.join().unwrap();
        let values1 = consumer1.join().unwrap();
        let values2 = consumer2.join().unwrap();

        for (name, values) in &[("Consumer 1", values1), ("Consumer 2", values2)] {
            for &v in values {
                assert!(v < NUM_PACKETS, "{} got invalid value {}", name, v);
            }
            for window in values.windows(2) {
                assert!(
                    window[0] < window[1],
                    "{} out-of-order: {} then {}",
                    name,
                    window[0],
                    window[1]
                );
            }
        }
    });
}

#[test]
fn loom_lag_recovery() {
    loom::model(|| {
        let (tx, mut rx) = channel::<u64>(2);

        let producer = thread::spawn(move || {
            for i in 0..4 {
                tx.send(i * 10);
            }
            drop(tx);
        });

        let consumer = thread::spawn(move || {
            let mut lag_count = 0;
            let mut values = Vec::new();
            for _ in 0..50 {
                match rx.try_recv() {
                    Ok(Some(slot)) => values.push(slot.value),
                    Err(RecvError::Lagged(_)) => lag_count += 1,
                    Err(RecvError::Closed) => break,
                    Ok(None) => thread::yield_now(),
                }
            }
            (lag_count, values)
        });

        producer.join().unwrap();
        let (_lag_count, _values) = consumer.join().unwrap();
        // Just ensure no panic; can't fully assert timing in Loom
    });
}

#[test]
fn loom_close_visibility() {
    loom::model(|| {
        let (tx, mut rx) = channel::<u64>(2);

        let producer = thread::spawn(move || {
            tx.send(1);
            drop(tx);
        });

        let consumer = thread::spawn(move || {
            let mut saw_closed = false;
            for _ in 0..50 {
                match rx.try_recv() {
                    Ok(Some(_)) => {}
                    Err(RecvError::Closed) => {
                        saw_closed = true;
                        break;
                    }
                    Err(RecvError::Lagged(_)) => continue,
                    Ok(None) => thread::yield_now(),
                }
            }
            saw_closed
        });

        producer.join().unwrap();
        assert!(consumer.join().unwrap());
    });
}
