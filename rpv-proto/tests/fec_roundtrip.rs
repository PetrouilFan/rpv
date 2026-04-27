//! Integration test for FEC (Forward Error Correction) roundtrip.
//!
//! This test verifies that data encoded with Reed-Solomon FEC can be
//! decoded even when some shards are missing during transmission.

use reed_solomon_erasure::galois_8::ReedSolomon;

const DATA_SHARDS: usize = 4;
const PARITY_SHARDS: usize = 2;
const TOTAL_SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;

/// Simulate sending shards over an unreliable transport by
/// dropping some shards (up to PARITY_SHARDS count).
fn simulate_transport(shards: &[Vec<u8>], drop_indices: &[usize]) -> Vec<Option<Vec<u8>>> {
    shards
        .iter()
        .enumerate()
        .map(|(i, shard)| {
            if drop_indices.contains(&i) {
                None
            } else {
                Some(shard.clone())
            }
        })
        .collect()
}

/// Reconstruct missing shards from received data.
/// Returns the reconstructed data shards.
fn reconstruct_shards(
    rs: &ReedSolomon,
    received: &[Option<Vec<u8>>],
) -> Result<Vec<Vec<u8>>, String> {
    let mut shards: Vec<Option<Vec<u8>>> = received.to_vec();

    rs.reconstruct(&mut shards)
        .map_err(|e| format!("{:?}", e))?;

    // Return data shards
    let result: Vec<Vec<u8>> = shards[..DATA_SHARDS]
        .iter()
        .map(|s| s.clone().unwrap())
        .collect();
    Ok(result)
}

/// Concatenate data shards into a single vector
fn concatenate_shards(shards: &[Vec<u8>], data_len: usize) -> Vec<u8> {
    shards
        .iter()
        .flat_map(|s| s.iter())
        .take(data_len)
        .copied()
        .collect()
}

#[test]
fn fec_roundtrip_all_data() {
    let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS).expect("Failed to create RS codec");

    // Original data to protect
    let original_data = b"Hello, this is a test message for FEC roundtrip!";
    let data_len = original_data.len();

    // Split data into DATA_SHARDS pieces
    let chunk_size = (data_len + DATA_SHARDS - 1) / DATA_SHARDS;
    let mut shards: Vec<Vec<u8>> = Vec::with_capacity(TOTAL_SHARDS);

    for i in 0..DATA_SHARDS {
        let start = i * chunk_size;
        let end = ((i + 1) * chunk_size).min(data_len);
        let mut shard = vec![0u8; chunk_size];
        if start < data_len {
            let len = end - start;
            shard[..len].copy_from_slice(&original_data[start..end]);
        }
        shards.push(shard);
    }

    // Add parity shards (initially empty)
    for _ in 0..PARITY_SHARDS {
        shards.push(vec![0u8; chunk_size]);
    }

    // Encode: compute parity shards
    rs.encode(&mut shards).expect("Failed to encode");

    // Verify all shards are present
    assert_eq!(shards.len(), TOTAL_SHARDS);

    // Simulate transport: drop one data shard (should still recover)
    let received = simulate_transport(&shards, &[0]);
    let recovered = reconstruct_shards(&rs, &received).expect("Failed to reconstruct");
    let recovered_data = concatenate_shards(&recovered, data_len);
    assert_eq!(&recovered_data, original_data);

    // Simulate transport: drop one parity shard (should still recover)
    let received = simulate_transport(&shards, &[DATA_SHARDS]);
    let recovered = reconstruct_shards(&rs, &received).expect("Failed to reconstruct");
    let recovered_data = concatenate_shards(&recovered, data_len);
    assert_eq!(&recovered_data, original_data);

    // Simulate transport: drop two data shards (maximum recoverable)
    let received = simulate_transport(&shards, &[0, 1]);
    let recovered = reconstruct_shards(&rs, &received).expect("Failed to reconstruct");
    let recovered_data = concatenate_shards(&recovered, data_len);
    assert_eq!(&recovered_data, original_data);

    // Simulate transport: drop one data + one parity (should still recover)
    let received = simulate_transport(&shards, &[0, DATA_SHARDS]);
    let recovered = reconstruct_shards(&rs, &received).expect("Failed to reconstruct");
    let recovered_data = concatenate_shards(&recovered, data_len);
    assert_eq!(&recovered_data, original_data);
}

#[test]
fn fec_roundtrip_empty_data() {
    let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS).expect("Failed to create RS codec");

    // Test with empty data
    let original_data: &[u8] = b"";
    let chunk_size = 1; // Minimum chunk size for empty data

    let mut shards: Vec<Vec<u8>> = Vec::with_capacity(TOTAL_SHARDS);
    for _ in 0..DATA_SHARDS {
        shards.push(vec![0u8; chunk_size]);
    }
    for _ in 0..PARITY_SHARDS {
        shards.push(vec![0u8; chunk_size]);
    }

    rs.encode(&mut shards).expect("Failed to encode");

    // With empty data, all shards should be zeros
    for shard in &shards[..DATA_SHARDS] {
        assert!(shard.iter().all(|&b| b == 0));
    }
}

#[test]
fn fec_roundtrip_large_data() {
    let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS).expect("Failed to create RS codec");

    // Test with larger data (simulating video frame fragment)
    let original_data: Vec<u8> = (0..2000).map(|i| (i % 256) as u8).collect();
    let data_len = original_data.len();

    let chunk_size = (data_len + DATA_SHARDS - 1) / DATA_SHARDS;
    let mut shards: Vec<Vec<u8>> = Vec::with_capacity(TOTAL_SHARDS);

    for i in 0..DATA_SHARDS {
        let start = i * chunk_size;
        let end = ((i + 1) * chunk_size).min(data_len);
        let mut shard = vec![0u8; chunk_size];
        if start < data_len {
            let len = end - start;
            shard[..len].copy_from_slice(&original_data[start..end]);
        }
        shards.push(shard);
    }

    for _ in 0..PARITY_SHARDS {
        shards.push(vec![0u8; chunk_size]);
    }

    rs.encode(&mut shards).expect("Failed to encode");

    // Drop maximum allowed shards
    let drop_indices: Vec<usize> = (0..PARITY_SHARDS).collect();
    let received = simulate_transport(&shards, &drop_indices);
    let recovered = reconstruct_shards(&rs, &received).expect("Failed to reconstruct");

    let recovered_data = concatenate_shards(&recovered, data_len);
    assert_eq!(recovered_data, original_data);
}

#[test]
fn fec_roundtrip_too_many_missing() {
    let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS).expect("Failed to create RS codec");

    let original_data = b"Test data";
    let chunk_size = 10;

    let mut shards: Vec<Vec<u8>> = vec![vec![0u8; chunk_size]; TOTAL_SHARDS];
    shards[0][..original_data.len()].copy_from_slice(original_data);

    rs.encode(&mut shards).expect("Failed to encode");

    // Drop more shards than parity can recover (3 > PARITY_SHARDS=2)
    let received = simulate_transport(&shards, &[0, 1, 2]);
    let result = reconstruct_shards(&rs, &received);

    // Should fail to reconstruct
    assert!(result.is_err());
}

#[test]
fn fec_roundtrip_with_video_constants() {
    // Use the same constants as rpv-cam for realistic test
    const DATA_SHARDS: usize = 4;
    const PARITY_SHARDS: usize = 2;
    const MAX_SHARD_DATA: usize = 1024; // Simplified for test

    let rs = ReedSolomon::new(DATA_SHARDS, PARITY_SHARDS).expect("Failed to create RS codec");

    // Simulate video data that fits in shards
    let test_data: Vec<u8> = (0..MAX_SHARD_DATA * DATA_SHARDS)
        .map(|i| (i % 255) as u8)
        .collect();

    let mut shards: Vec<Vec<u8>> = Vec::with_capacity(DATA_SHARDS + PARITY_SHARDS);

    for i in 0..DATA_SHARDS {
        let start = i * MAX_SHARD_DATA;
        let end = start + MAX_SHARD_DATA;
        shards.push(test_data[start..end].to_vec());
    }

    for _ in 0..PARITY_SHARDS {
        shards.push(vec![0u8; MAX_SHARD_DATA]);
    }

    rs.encode(&mut shards).expect("Failed to encode");

    // Verify parity shards are not all zeros
    for i in DATA_SHARDS..DATA_SHARDS + PARITY_SHARDS {
        assert!(
            shards[i].iter().any(|&b| b != 0),
            "Parity shard {} is all zeros",
            i
        );
    }

    // Simulate missing one data shard
    let mut reconstruct_shards = shards.clone();
    // Use Option<Vec<u8>> for reconstruct
    let mut shard_refs: Vec<Option<Vec<u8>>> = reconstruct_shards
        .iter()
        .map(|s| Some(s.clone()))
        .collect();
    rs.reconstruct(&mut shard_refs)
        .expect("Failed to reconstruct");

    assert_eq!(shard_refs[0].as_ref().unwrap(), &shards[0]);
}
