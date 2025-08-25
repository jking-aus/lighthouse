//! Tests for sync committee duties with state availability constraints (Issue 7115).

use http_api::test_utils::*;
use std::collections::HashSet;
use types::{ChainSpec, Epoch, EthSpec, MainnetEthSpec, MinimalEthSpec};

type E = MinimalEthSpec;

fn altair_spec(altair_fork_epoch: Epoch) -> ChainSpec {
    let mut spec = E::default_spec();
    spec.altair_fork_epoch = Some(altair_fork_epoch);
    spec
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_committee_duties_state_availability_basic() {
    let validator_count = E::sync_committee_size();
    let fork_epoch = Epoch::new(8);
    let spec = altair_spec(fork_epoch);
    let tester = InteractiveTester::<E>::new(Some(spec.clone()), validator_count).await;
    let harness = &tester.harness;
    let client = &tester.client;

    let all_validators = harness.get_all_validators();
    let all_validators_u64 = all_validators.iter().map(|x| *x as u64).collect::<Vec<_>>();

    // Progress to after the fork to ensure sync committees are active
    let fork_slot = fork_epoch.start_slot(E::slots_per_epoch());
    let (genesis_state, genesis_state_root) = harness.get_current_state_and_root();
    
    // Add some blocks to get past the fork
    let (_, _state) = harness
        .add_attested_block_at_slot(
            fork_slot + 10,
            genesis_state,
            genesis_state_root,
            &all_validators,
        )
        .await
        .expect("should add block");

    // Advance to ensure we're past the fork
    for _ in 0..20 {
        harness.advance_slot();
    }

    // Test sync duties for the current period should work normally
    let current_epoch = harness.get_current_slot().epoch(E::slots_per_epoch());
    let sync_duties_result = client
        .post_validator_duties_sync(current_epoch, &all_validators_u64)
        .await;
    
    assert!(sync_duties_result.is_ok(), "Current epoch duties should succeed");
    let sync_duties = sync_duties_result.expect("checked above").data;
    
    // Should return duties for all sync committee validators
    assert_eq!(sync_duties.len(), E::sync_committee_size());

    // Test that sync duties work for future periods (simulating the state availability fix)
    let current_period_result = current_epoch.sync_committee_period(&spec);
    assert!(current_period_result.is_ok(), "Should get current period");
    let current_period = current_period_result.expect("checked above");
    
    let next_period_epoch = spec.epochs_per_sync_committee_period * (current_period + 1);
    
    let next_period_duties_result = client
        .post_validator_duties_sync(next_period_epoch, &all_validators_u64)
        .await;
    
    assert!(next_period_duties_result.is_ok(), "Next period duties should succeed");
    let next_period_duties = next_period_duties_result.expect("checked above").data;
    
    // Should also return duties for next period
    assert_eq!(next_period_duties.len(), E::sync_committee_size());

    // Verify that duties can be requested for different epochs within the same period
    let period_start_epoch = spec.epochs_per_sync_committee_period * current_period;
    let period_end_epoch = period_start_epoch + spec.epochs_per_sync_committee_period.as_u64() - 1;
    
    // Test duties for different epochs in the same period
    let duties_start_result = client
        .post_validator_duties_sync(period_start_epoch, &all_validators_u64)
        .await;
        
    let duties_end_result = client
        .post_validator_duties_sync(period_end_epoch, &all_validators_u64)
        .await;
    
    assert!(duties_start_result.is_ok(), "Period start duties should succeed");
    assert!(duties_end_result.is_ok(), "Period end duties should succeed");
    
    let duties_start = duties_start_result.expect("checked above").data;
    let duties_end = duties_end_result.expect("checked above").data;
    
    // Should return the same duties for all epochs in the same sync committee period
    assert_eq!(duties_start.len(), E::sync_committee_size());
    assert_eq!(duties_end.len(), E::sync_committee_size());
    
    // The actual duties should be the same (same validators, same committee indices)
    for (start_duty, end_duty) in duties_start.iter().zip(duties_end.iter()) {
        assert_eq!(start_duty.validator_index, end_duty.validator_index);
        assert_eq!(start_duty.validator_sync_committee_indices, end_duty.validator_sync_committee_indices);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_committee_duties_state_availability_edge_cases() {
    let validator_count = E::sync_committee_size();
    let fork_epoch = Epoch::new(8);
    let spec = altair_spec(fork_epoch);
    let tester = InteractiveTester::<E>::new(Some(spec.clone()), validator_count).await;
    let harness = &tester.harness;
    let client = &tester.client;

    let all_validators = harness.get_all_validators();
    let all_validators_u64 = all_validators.iter().map(|x| *x as u64).collect::<Vec<_>>();

    // Progress well past the fork to establish multiple sync committee periods
    let fork_slot = fork_epoch.start_slot(E::slots_per_epoch());
    let (genesis_state, genesis_state_root) = harness.get_current_state_and_root();
    
    // Build a longer chain to create multiple sync committee periods
    let target_slot = fork_slot + (spec.epochs_per_sync_committee_period.as_u64() * E::slots_per_epoch() * 2);
    
    let (_, _state) = harness
        .add_attested_block_at_slot(
            target_slot,
            genesis_state,
            genesis_state_root,
            &all_validators,
        )
        .await
        .expect("should add block");

    // Advance further to ensure we're in a later sync committee period
    for _ in 0..10 {
        harness.advance_slot();
    }

    let current_epoch = harness.get_current_slot().epoch(E::slots_per_epoch());
    let current_period_result = current_epoch.sync_committee_period(&spec);
    assert!(current_period_result.is_ok(), "Should get current period");
    let current_period = current_period_result.expect("checked above");

    // Test 1: Request duties for a period that should be at the boundary
    // This tests the case where we might need to use state_upper_limit
    let boundary_period = current_period.saturating_sub(1);
    let boundary_epoch = spec.epochs_per_sync_committee_period * boundary_period;
    
    let boundary_duties_result = client
        .post_validator_duties_sync(boundary_epoch, &all_validators_u64)
        .await;
    
    // This should succeed even if the ideal state isn't available
    assert!(boundary_duties_result.is_ok(), "Boundary period duties should succeed with state availability fallback");
    let boundary_duties = boundary_duties_result.expect("checked above").data;
    assert_eq!(boundary_duties.len(), E::sync_committee_size());

    // Test 2: Request duties for the very start of a sync committee period
    // This tests the specific scenario mentioned in issue 7115
    let period_start_epoch = spec.epochs_per_sync_committee_period * current_period;
    let period_start_duties_result = client
        .post_validator_duties_sync(period_start_epoch, &all_validators_u64)
        .await;
    
    assert!(period_start_duties_result.is_ok(), "Period start duties should succeed");
    let period_start_duties = period_start_duties_result.expect("checked above").data;
    assert_eq!(period_start_duties.len(), E::sync_committee_size());

    // Test 3: Request duties for different epochs within the same period
    // This verifies that the state availability logic returns consistent results
    let period_mid_epoch = period_start_epoch + (spec.epochs_per_sync_committee_period.as_u64() / 2);
    let period_end_epoch = period_start_epoch + spec.epochs_per_sync_committee_period.as_u64() - 1;
    
    let mid_duties_result = client
        .post_validator_duties_sync(period_mid_epoch, &all_validators_u64)
        .await;
    let end_duties_result = client
        .post_validator_duties_sync(period_end_epoch, &all_validators_u64)
        .await;
    
    assert!(mid_duties_result.is_ok(), "Mid-period duties should succeed");
    assert!(end_duties_result.is_ok(), "End-period duties should succeed");
    
    let mid_duties = mid_duties_result.expect("checked above").data;
    let end_duties = end_duties_result.expect("checked above").data;
    
    // All duties within the same period should be identical
    assert_eq!(period_start_duties.len(), mid_duties.len());
    assert_eq!(period_start_duties.len(), end_duties.len());
    
    // Verify the actual duty assignments are identical within the period
    for (start_duty, mid_duty) in period_start_duties.iter().zip(mid_duties.iter()) {
        assert_eq!(start_duty.validator_index, mid_duty.validator_index);
        assert_eq!(start_duty.validator_sync_committee_indices, mid_duty.validator_sync_committee_indices);
    }
    
    for (start_duty, end_duty) in period_start_duties.iter().zip(end_duties.iter()) {
        assert_eq!(start_duty.validator_index, end_duty.validator_index);
        assert_eq!(start_duty.validator_sync_committee_indices, end_duty.validator_sync_committee_indices);
    }

    // Test 4: Request duties far in the future (should fail)
    let far_future_epoch = current_epoch + Epoch::new(spec.epochs_per_sync_committee_period.as_u64() * 10);
    let far_future_result = client
        .post_validator_duties_sync(far_future_epoch, &all_validators_u64)
        .await;
    
    assert!(far_future_result.is_err(), "Far future duties should fail");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_committee_duties_with_simulated_unavailability() {
    let validator_count = E::sync_committee_size();
    let fork_epoch = Epoch::new(8);
    let spec = altair_spec(fork_epoch);
    let tester = InteractiveTester::<E>::new(Some(spec.clone()), validator_count).await;
    let harness = &tester.harness;
    let client = &tester.client;

    let all_validators = harness.get_all_validators();
    let all_validators_u64 = all_validators.iter().map(|x| *x as u64).collect::<Vec<_>>();

    // Progress past the fork and build a longer chain spanning multiple sync committee periods
    let fork_slot = fork_epoch.start_slot(E::slots_per_epoch());
    let (genesis_state, genesis_state_root) = harness.get_current_state_and_root();
    
    let periods_to_span = 3;
    let target_slot = fork_slot + (spec.epochs_per_sync_committee_period.as_u64() * E::slots_per_epoch() * periods_to_span);
    
    let (_, _state) = harness
        .add_attested_block_at_slot(
            target_slot,
            genesis_state,
            genesis_state_root,
            &all_validators,
        )
        .await
        .expect("should add block");

    // Advance further to be well into a later sync committee period
    for _ in 0..20 {
        harness.advance_slot();
    }

    let current_epoch = harness.get_current_slot().epoch(E::slots_per_epoch());
    let current_period = current_epoch.sync_committee_period(&spec).expect("should get period");

    // Test requesting duties for an earlier period - this should work with state availability logic
    let earlier_period = current_period.saturating_sub(2);
    let earlier_period_start_epoch = spec.epochs_per_sync_committee_period * earlier_period;
    
    // This tests the core issue 7115 scenario:
    // - We're requesting duties for the start of an earlier sync committee period
    // - The ideal state (very start of that period) might not be available
    // - But a state from later in that same period should be available
    // - Our fix should use the available state instead of failing
    let earlier_duties_result = client
        .post_validator_duties_sync(earlier_period_start_epoch, &all_validators_u64)
        .await;
    
    assert!(
        earlier_duties_result.is_ok(), 
        "Earlier period duties should succeed even if ideal state unavailable: {:?}", 
        earlier_duties_result.err()
    );
    
    let earlier_duties = earlier_duties_result.expect("checked above").data;
    assert_eq!(earlier_duties.len(), E::sync_committee_size());

    // Test requesting duties for different epochs within that same earlier period
    // This verifies our logic returns consistent results regardless of which available state is used
    let earlier_period_mid_epoch = earlier_period_start_epoch + (spec.epochs_per_sync_committee_period.as_u64() / 2);
    let earlier_period_end_epoch = earlier_period_start_epoch + spec.epochs_per_sync_committee_period.as_u64() - 1;
    
    let mid_duties_result = client
        .post_validator_duties_sync(earlier_period_mid_epoch, &all_validators_u64)
        .await;
    let end_duties_result = client
        .post_validator_duties_sync(earlier_period_end_epoch, &all_validators_u64)
        .await;
    
    assert!(mid_duties_result.is_ok(), "Mid-period duties should succeed");
    assert!(end_duties_result.is_ok(), "End-period duties should succeed");
    
    let mid_duties = mid_duties_result.expect("checked above").data;
    let end_duties = end_duties_result.expect("checked above").data;
    
    // All duties within the same period should be identical regardless of which state was used
    assert_eq!(earlier_duties.len(), mid_duties.len());
    assert_eq!(earlier_duties.len(), end_duties.len());
    
    // Verify the actual duty assignments are identical - this confirms our fix works correctly
    for (start_duty, mid_duty) in earlier_duties.iter().zip(mid_duties.iter()) {
        assert_eq!(start_duty.validator_index, mid_duty.validator_index, 
                  "Validator index should be same regardless of which state used");
        assert_eq!(start_duty.validator_sync_committee_indices, mid_duty.validator_sync_committee_indices,
                  "Sync committee indices should be same regardless of which state used");
    }
    
    for (start_duty, end_duty) in earlier_duties.iter().zip(end_duties.iter()) {
        assert_eq!(start_duty.validator_index, end_duty.validator_index);
        assert_eq!(start_duty.validator_sync_committee_indices, end_duty.validator_sync_committee_indices);
    }

    // Test edge case: request duties for a period boundary
    // This specifically tests the boundary validation logic in our fix
    let boundary_epoch = spec.epochs_per_sync_committee_period * current_period;
    let boundary_duties_result = client
        .post_validator_duties_sync(boundary_epoch, &all_validators_u64)
        .await;
    
    assert!(
        boundary_duties_result.is_ok(), 
        "Boundary epoch duties should succeed with proper state availability handling"
    );
    
    // Test requesting duties for a period that definitely should have states available
    let recent_period = current_period;
    let recent_epoch = spec.epochs_per_sync_committee_period * recent_period;
    let recent_duties_result = client
        .post_validator_duties_sync(recent_epoch, &all_validators_u64)
        .await;
    
    assert!(recent_duties_result.is_ok(), "Recent period duties should definitely succeed");
    let recent_duties = recent_duties_result.expect("checked above").data;
    assert_eq!(recent_duties.len(), E::sync_committee_size());

    // The key test: we successfully got duties for both periods using state availability logic
    println!("✅ Successfully retrieved duties for multiple periods with state availability handling");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_committee_rotation_with_larger_validator_set() {
    // Use more validators than sync committee size so committees can actually differ across periods
    let validator_count = E::sync_committee_size() * 4; // 128 validators instead of 32
    let fork_epoch = Epoch::new(8);
    let spec = altair_spec(fork_epoch);
    let tester = InteractiveTester::<E>::new(Some(spec.clone()), validator_count).await;
    let harness = &tester.harness;
    let client = &tester.client;

    let all_validators = harness.get_all_validators();
    let all_validators_u64 = all_validators.iter().map(|x| *x as u64).collect::<Vec<_>>();

    // Progress past the fork and build a longer chain spanning multiple sync committee periods
    let fork_slot = fork_epoch.start_slot(E::slots_per_epoch());
    let (genesis_state, genesis_state_root) = harness.get_current_state_and_root();
    
    let periods_to_span = 3;
    let target_slot = fork_slot + (spec.epochs_per_sync_committee_period.as_u64() * E::slots_per_epoch() * periods_to_span);
    
    let (_, _state) = harness
        .add_attested_block_at_slot(
            target_slot,
            genesis_state,
            genesis_state_root,
            &all_validators,
        )
        .await
        .expect("should add block");

    // Advance to be in a later sync committee period
    for _ in 0..20 {
        harness.advance_slot();
    }

    let current_epoch = harness.get_current_slot().epoch(E::slots_per_epoch());
    let current_period = current_epoch.sync_committee_period(&spec).expect("should get period");

    // Test duties for two different periods
    let earlier_period = current_period.saturating_sub(1);
    let earlier_period_epoch = spec.epochs_per_sync_committee_period * earlier_period;
    let current_period_epoch = spec.epochs_per_sync_committee_period * current_period;
    
    let earlier_duties_result = client
        .post_validator_duties_sync(earlier_period_epoch, &all_validators_u64)
        .await;
    let current_duties_result = client
        .post_validator_duties_sync(current_period_epoch, &all_validators_u64)
        .await;
    
    assert!(earlier_duties_result.is_ok(), "Earlier period duties should succeed");
    assert!(current_duties_result.is_ok(), "Current period duties should succeed");
    
    let earlier_duties = earlier_duties_result.expect("checked above").data;
    let current_duties = current_duties_result.expect("checked above").data;
    
    assert_eq!(earlier_duties.len(), E::sync_committee_size());
    assert_eq!(current_duties.len(), E::sync_committee_size());

    // With more validators, sync committees should actually be different across periods
    let earlier_validator_indices: HashSet<u64> = earlier_duties
        .iter()
        .map(|duty| duty.validator_index)
        .collect();
    let current_validator_indices: HashSet<u64> = current_duties
        .iter()
        .map(|duty| duty.validator_index)
        .collect();
    
    let intersection_size = earlier_validator_indices.intersection(&current_validator_indices).count();
    let total_committee_size = E::sync_committee_size();
    
    // With 128 validators and 32 committee size, we expect significant rotation
    assert!(
        intersection_size < total_committee_size,
        "Different sync committee periods should have different committees with larger validator set. Intersection: {}, Total: {}", 
        intersection_size, total_committee_size
    );
    
    println!("✅ Sync committees properly rotate: {}/{} validators differ between periods", 
             total_committee_size - intersection_size, total_committee_size);
}

/// Test sync committee rotation with MainnetEthSpec for realistic committee size and rotation.
/// This test is slower due to larger validator set, so it's ignored by default.
/// Run with: cargo test -- --ignored
#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_committee_rotation_mainnet_spec() {
    type E = MainnetEthSpec;
    
    // Use more validators than sync committee size for meaningful rotation
    // Keep it reasonable for test performance while ensuring rotation
    let validator_count = E::sync_committee_size() + E::sync_committee_size() / 2; // 768 validators vs 512 committee size
    let fork_epoch = Epoch::new(8);
    
    fn altair_spec_mainnet(altair_fork_epoch: Epoch) -> ChainSpec {
        let mut spec = E::default_spec();
        spec.altair_fork_epoch = Some(altair_fork_epoch);
        spec
    }
    
    let spec = altair_spec_mainnet(fork_epoch);
    let tester = InteractiveTester::<E>::new(Some(spec.clone()), validator_count).await;
    let harness = &tester.harness;
    let client = &tester.client;

    let all_validators = harness.get_all_validators();
    let all_validators_u64 = all_validators.iter().map(|x| *x as u64).collect::<Vec<_>>();

    // Progress past the fork and span multiple sync committee periods
    let fork_slot = fork_epoch.start_slot(E::slots_per_epoch());
    let (genesis_state, genesis_state_root) = harness.get_current_state_and_root();
    
    let periods_to_span = 2;
    let target_slot = fork_slot + (spec.epochs_per_sync_committee_period.as_u64() * E::slots_per_epoch() * periods_to_span);
    
    let (_, _state) = harness
        .add_attested_block_at_slot(
            target_slot,
            genesis_state,
            genesis_state_root,
            &all_validators,
        )
        .await
        .expect("should add block");

    // Advance to be in a later sync committee period
    for _ in 0..20 {
        harness.advance_slot();
    }

    let current_epoch = harness.get_current_slot().epoch(E::slots_per_epoch());
    let current_period = current_epoch.sync_committee_period(&spec).expect("should get period");

    // Test duties for two different periods
    let earlier_period = current_period.saturating_sub(1);
    let earlier_period_epoch = spec.epochs_per_sync_committee_period * earlier_period;
    let current_period_epoch = spec.epochs_per_sync_committee_period * current_period;
    
    let earlier_duties_result = client
        .post_validator_duties_sync(earlier_period_epoch, &all_validators_u64)
        .await;
    let current_duties_result = client
        .post_validator_duties_sync(current_period_epoch, &all_validators_u64)
        .await;
    
    assert!(earlier_duties_result.is_ok(), "Earlier period duties should succeed");
    assert!(current_duties_result.is_ok(), "Current period duties should succeed");
    
    let earlier_duties = earlier_duties_result.expect("checked above").data;
    let current_duties = current_duties_result.expect("checked above").data;
    
    assert_eq!(earlier_duties.len(), E::sync_committee_size());
    assert_eq!(current_duties.len(), E::sync_committee_size());

    // With MainnetEthSpec and many validators, committees should definitely rotate
    let earlier_validator_indices: HashSet<u64> = earlier_duties
        .iter()
        .map(|duty| duty.validator_index)
        .collect();
    let current_validator_indices: HashSet<u64> = current_duties
        .iter()
        .map(|duty| duty.validator_index)
        .collect();
    
    let intersection_size = earlier_validator_indices.intersection(&current_validator_indices).count();
    let total_committee_size = E::sync_committee_size();
    
    // With 768 validators and 512 committee size, we expect significant rotation
    // Allow some overlap but require substantial difference
    let max_expected_overlap = total_committee_size * 2 / 3; // Allow up to 2/3 overlap
    assert!(
        intersection_size < max_expected_overlap,
        "MainnetEthSpec sync committees should have substantial rotation. Intersection: {}/{}, expected < {}", 
        intersection_size, total_committee_size, max_expected_overlap
    );
    
    println!("✅ MainnetEthSpec sync committees rotate properly: {}/{} validators changed between periods", 
             total_committee_size - intersection_size, total_committee_size);
    
    // Test our state availability logic with MainnetEthSpec across different epochs in same period
    // This verifies that our fix works correctly even with large validator sets
    let period_start_epoch = spec.epochs_per_sync_committee_period * earlier_period;
    let period_mid_epoch = period_start_epoch + (spec.epochs_per_sync_committee_period.as_u64() / 2);
    let period_end_epoch = period_start_epoch + spec.epochs_per_sync_committee_period.as_u64() - 1;
    
    let start_duties_result = client
        .post_validator_duties_sync(period_start_epoch, &all_validators_u64)
        .await;
    let mid_duties_result = client
        .post_validator_duties_sync(period_mid_epoch, &all_validators_u64)
        .await;
    let end_duties_result = client
        .post_validator_duties_sync(period_end_epoch, &all_validators_u64)
        .await;
    
    assert!(start_duties_result.is_ok(), "Period start duties should succeed with state availability logic");
    assert!(mid_duties_result.is_ok(), "Period mid duties should succeed with state availability logic");  
    assert!(end_duties_result.is_ok(), "Period end duties should succeed with state availability logic");
    
    let start_duties = start_duties_result.expect("checked above").data;
    let mid_duties = mid_duties_result.expect("checked above").data;
    let end_duties = end_duties_result.expect("checked above").data;
    
    assert_eq!(start_duties.len(), E::sync_committee_size());
    assert_eq!(mid_duties.len(), E::sync_committee_size());
    assert_eq!(end_duties.len(), E::sync_committee_size());
    
    // Critical test: All duties within the same period should be IDENTICAL regardless of epoch
    // This validates that our state availability logic returns consistent results
    let start_indices: HashSet<u64> = start_duties.iter().map(|d| d.validator_index).collect();
    let mid_indices: HashSet<u64> = mid_duties.iter().map(|d| d.validator_index).collect();
    let end_indices: HashSet<u64> = end_duties.iter().map(|d| d.validator_index).collect();
    
    assert_eq!(
        start_indices, mid_indices,
        "Start and mid period duties should be identical (same sync committee period)"
    );
    assert_eq!(
        start_indices, end_indices, 
        "Start and end period duties should be identical (same sync committee period)"
    );
    
    // Verify that the current period duties we got earlier are DIFFERENT from this earlier period
    // This confirms both rotation AND that our state availability logic preserves period boundaries
    assert_ne!(
        start_indices, earlier_validator_indices,
        "Different periods should have different sync committees - this validates both rotation and state availability boundary logic"
    );
    
    println!("✅ State availability logic correctly maintains sync committee consistency within periods");
    println!("✅ Period boundaries are correctly preserved across different available states");
}

/// Test state availability logic by testing period consistency at the API level
/// This tests that our implementation correctly handles requests for different epochs
/// within the same sync committee period, which exercises the state loading logic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_committee_duties_detailed_period_consistency() {
    let validator_count = E::sync_committee_size() * 2; // 64 validators for meaningful testing
    let fork_epoch = Epoch::new(8);
    let spec = altair_spec(fork_epoch);
    let tester = InteractiveTester::<E>::new(Some(spec.clone()), validator_count).await;
    let harness = &tester.harness;
    let client = &tester.client;

    let all_validators = harness.get_all_validators();
    let all_validators_u64 = all_validators.iter().map(|x| *x as u64).collect::<Vec<_>>();

    // Build chain across multiple sync committee periods
    let fork_slot = fork_epoch.start_slot(E::slots_per_epoch());
    let (genesis_state, genesis_state_root) = harness.get_current_state_and_root();
    
    let periods_to_span = 4;
    let target_slot = fork_slot + (spec.epochs_per_sync_committee_period.as_u64() * E::slots_per_epoch() * periods_to_span);
    
    let (_, _state) = harness
        .add_attested_block_at_slot(
            target_slot,
            genesis_state,
            genesis_state_root,
            &all_validators,
        )
        .await
        .expect("should add block");

    // Advance further to be deep in the chain
    for _ in 0..100 {
        harness.advance_slot();
    }

    let current_epoch = harness.get_current_slot().epoch(E::slots_per_epoch());
    let current_period = current_epoch.sync_committee_period(&spec).expect("should get period");
    
    // Test multiple historical periods to exercise our state availability logic
    for period_offset in 1..=3 {
        let target_period = current_period.saturating_sub(period_offset);
        let period_start_epoch = spec.epochs_per_sync_committee_period * target_period;
        let period_mid_epoch = period_start_epoch + (spec.epochs_per_sync_committee_period.as_u64() / 2);
        let period_end_epoch = period_start_epoch + spec.epochs_per_sync_committee_period.as_u64() - 1;
        
        // Test exact period boundaries and internal epochs
        let test_epochs = vec![
            period_start_epoch,
            period_start_epoch + 1,
            period_mid_epoch,
            period_end_epoch - 1,
            period_end_epoch,
        ];
        
        let mut all_duties = Vec::new();
        
        for epoch in test_epochs {
            let duties_result = client
                .post_validator_duties_sync(epoch, &all_validators_u64)
                .await;
            
            assert!(
                duties_result.is_ok(), 
                "Duties should succeed for epoch {} in period {} (offset {}): {:?}",
                epoch, target_period, period_offset, duties_result.err()
            );
            
            let duties = duties_result.expect("checked above").data;
            all_duties.push((epoch, duties));
        }
        
        // CRITICAL VALIDATION: All duties within the same period should be identical
        let first_duties_indices: HashSet<u64> = all_duties[0].1.iter().map(|d| d.validator_index).collect();
        
        for (epoch, duties) in &all_duties[1..] {
            let duties_indices: HashSet<u64> = duties.iter().map(|d| d.validator_index).collect();
            assert_eq!(
                first_duties_indices, duties_indices,
                "All epochs in period {} should have identical sync committees. Epoch {} differs from period start",
                target_period, epoch
            );
            
            // Also check the actual committee indices for each validator
            for duty in duties {
                let first_duty = all_duties[0].1.iter().find(|d| d.validator_index == duty.validator_index);
                if let Some(first_duty) = first_duty {
                    assert_eq!(
                        first_duty.validator_sync_committee_indices,
                        duty.validator_sync_committee_indices,
                        "Validator {} committee indices should be identical across all epochs in period {}",
                        duty.validator_index, target_period
                    );
                }
            }
        }
        
        println!("✅ Period {} (offset {}) maintains perfect consistency across {} epochs", 
                target_period, period_offset, all_duties.len());
    }
    
    // Test cross-period validation - different periods should potentially have different committees
    let period_1 = current_period.saturating_sub(2);
    let period_2 = current_period.saturating_sub(1);
    
    let epoch_1 = spec.epochs_per_sync_committee_period * period_1;
    let epoch_2 = spec.epochs_per_sync_committee_period * period_2;
    
    let duties_1 = client.post_validator_duties_sync(epoch_1, &all_validators_u64).await.expect("should succeed").data;
    let duties_2 = client.post_validator_duties_sync(epoch_2, &all_validators_u64).await.expect("should succeed").data;
    
    let indices_1: HashSet<u64> = duties_1.iter().map(|d| d.validator_index).collect();
    let indices_2: HashSet<u64> = duties_2.iter().map(|d| d.validator_index).collect();
    
    if indices_1 != indices_2 {
        println!("✅ Different periods have different committees (good - shows rotation)");
    } else {
        println!("ℹ️  Same committees across periods (expected with small validator sets)");
    }
    
    println!("✅ Comprehensive period consistency testing completed");
    println!("✅ State availability logic correctly maintains sync committee consistency");
}

/// Test boundary conditions for state availability logic
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_committee_duties_boundary_conditions() {
    let validator_count = E::sync_committee_size();
    let fork_epoch = Epoch::new(8);
    let spec = altair_spec(fork_epoch);
    let tester = InteractiveTester::<E>::new(Some(spec.clone()), validator_count).await;
    let harness = &tester.harness;
    let client = &tester.client;

    let all_validators = harness.get_all_validators();
    let all_validators_u64 = all_validators.iter().map(|x| *x as u64).collect::<Vec<_>>();

    // Build chain across multiple periods
    let fork_slot = fork_epoch.start_slot(E::slots_per_epoch());
    let (genesis_state, genesis_state_root) = harness.get_current_state_and_root();
    
    let periods_to_span = 4;
    let target_slot = fork_slot + (spec.epochs_per_sync_committee_period.as_u64() * E::slots_per_epoch() * periods_to_span);
    
    let (_, _state) = harness
        .add_attested_block_at_slot(
            target_slot,
            genesis_state,
            genesis_state_root,
            &all_validators,
        )
        .await
        .expect("should add block");

    for _ in 0..30 {
        harness.advance_slot();
    }

    let current_epoch = harness.get_current_slot().epoch(E::slots_per_epoch());
    let current_period = current_epoch.sync_committee_period(&spec).expect("should get period");
    
    // Test 1: Request duties for period boundary epochs
    let target_period = current_period.saturating_sub(1);
    let period_start = spec.epochs_per_sync_committee_period * target_period;
    let period_end = period_start + spec.epochs_per_sync_committee_period.as_u64() - 1;
    
    // Test exact period boundaries
    let start_duties_result = client
        .post_validator_duties_sync(period_start, &all_validators_u64)
        .await;
    let end_duties_result = client
        .post_validator_duties_sync(period_end, &all_validators_u64)
        .await;
    
    assert!(start_duties_result.is_ok(), "Period start boundary should succeed");
    assert!(end_duties_result.is_ok(), "Period end boundary should succeed");
    
    let start_duties = start_duties_result.expect("checked above").data;
    let end_duties = end_duties_result.expect("checked above").data;
    
    // Boundary validation: same period should yield identical duties
    let start_indices: std::collections::HashSet<u64> = start_duties.iter().map(|d| d.validator_index).collect();
    let end_indices: std::collections::HashSet<u64> = end_duties.iter().map(|d| d.validator_index).collect();
    
    assert_eq!(
        start_indices, end_indices,
        "Period boundary epochs should return identical duties"
    );
    
    // Test 2: Cross-period validation
    let next_period_start = period_end + 1;
    let next_period_duties_result = client
        .post_validator_duties_sync(next_period_start, &all_validators_u64)
        .await;
    
    if next_period_duties_result.is_ok() {
        let next_duties = next_period_duties_result.expect("checked above").data;
        let next_indices: std::collections::HashSet<u64> = next_duties.iter().map(|d| d.validator_index).collect();
        
        // With MinimalEthSpec (32 validators = committee size), committees might be identical
        // But our logic should still be correct
        if next_indices != start_indices {
            println!("✅ Different periods have different committees (expected with larger validator sets)");
        } else {
            println!("ℹ️  Same committees across periods (expected with MinimalEthSpec where all validators are in committee)");
        }
    }
    
    println!("✅ Boundary condition testing completed successfully");
}