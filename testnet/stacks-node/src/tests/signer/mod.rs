// Copyright (C) 2020-2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
mod v0;
mod v1;

use std::collections::HashSet;
// Copyright (C) 2020-2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clarity::boot_util::boot_code_id;
use clarity::vm::types::PrincipalData;
use libsigner::{SignerEntries, SignerEventTrait};
use stacks::chainstate::coordinator::comm::CoordinatorChannels;
use stacks::chainstate::nakamoto::signer_set::NakamotoSigners;
use stacks::chainstate::stacks::boot::{NakamotoSignerEntry, SIGNERS_NAME};
use stacks::chainstate::stacks::{StacksPrivateKey, ThresholdSignature};
use stacks::core::StacksEpoch;
use stacks::net::api::postblock_proposal::{
    BlockValidateOk, BlockValidateReject, BlockValidateResponse,
};
use stacks::types::chainstate::StacksAddress;
use stacks::util::secp256k1::{MessageSignature, Secp256k1PublicKey};
use stacks_common::codec::StacksMessageCodec;
use stacks_common::consts::SIGNER_SLOTS_PER_USER;
use stacks_common::types::StacksEpochId;
use stacks_common::util::hash::{hex_bytes, Sha512Trunc256Sum};
use stacks_signer::client::{ClientError, SignerSlotID, StacksClient};
use stacks_signer::config::{build_signer_config_tomls, GlobalConfig as SignerConfig, Network};
use stacks_signer::runloop::{SignerResult, State, StateInfo};
use stacks_signer::{Signer, SpawnedSigner};
use wsts::state_machine::PublicKeys;

use super::nakamoto_integrations::wait_for;
use crate::config::{Config as NeonConfig, EventKeyType, EventObserverConfig, InitialBalance};
use crate::event_dispatcher::MinedNakamotoBlockEvent;
use crate::neon::{Counters, TestFlag};
use crate::run_loop::boot_nakamoto;
use crate::tests::bitcoin_regtest::BitcoinCoreController;
use crate::tests::nakamoto_integrations::{
    naka_neon_integration_conf, next_block_and_mine_commit, next_block_and_wait_for_commits,
    POX_4_DEFAULT_STACKER_BALANCE,
};
use crate::tests::neon_integrations::{
    get_chain_info, next_block_and_wait, run_until_burnchain_height, test_observer,
    wait_for_runloop,
};
use crate::tests::to_addr;
use crate::{BitcoinRegtestController, BurnchainController};

// Helper struct for holding the btc and stx neon nodes
#[allow(dead_code)]
pub struct RunningNodes {
    pub btc_regtest_controller: BitcoinRegtestController,
    pub btcd_controller: BitcoinCoreController,
    pub run_loop_thread: thread::JoinHandle<()>,
    pub run_loop_stopper: Arc<AtomicBool>,
    pub vrfs_submitted: Arc<AtomicU64>,
    pub commits_submitted: Arc<AtomicU64>,
    pub blocks_processed: Arc<AtomicU64>,
    pub nakamoto_blocks_proposed: Arc<AtomicU64>,
    pub nakamoto_blocks_mined: Arc<AtomicU64>,
    pub nakamoto_blocks_rejected: Arc<AtomicU64>,
    pub nakamoto_blocks_signer_pushed: Arc<AtomicU64>,
    pub nakamoto_test_skip_commit_op: TestFlag,
    pub coord_channel: Arc<Mutex<CoordinatorChannels>>,
    pub conf: NeonConfig,
}

/// A test harness for running a v0 or v1 signer integration test
pub struct SignerTest<S> {
    // The stx and bitcoin nodes and their run loops
    pub running_nodes: RunningNodes,
    // The spawned signers and their threads
    pub spawned_signers: Vec<S>,
    // The spawned signers and their threads
    #[allow(dead_code)]
    pub signer_configs: Vec<SignerConfig>,
    // the private keys of the signers
    pub signer_stacks_private_keys: Vec<StacksPrivateKey>,
    // link to the stacks node
    pub stacks_client: StacksClient,
    // Unique number used to isolate files created during the test
    pub run_stamp: u16,
    /// The number of cycles to stack for
    pub num_stacking_cycles: u64,
}

impl<S: Signer<T> + Send + 'static, T: SignerEventTrait + 'static> SignerTest<SpawnedSigner<S, T>> {
    fn new(
        num_signers: usize,
        initial_balances: Vec<(StacksAddress, u64)>,
        wait_on_signers: Option<Duration>,
    ) -> Self {
        Self::new_with_config_modifications(
            num_signers,
            initial_balances,
            wait_on_signers,
            |_| {},
            |_| {},
            &[],
        )
    }

    fn new_with_config_modifications<
        F: FnMut(&mut SignerConfig) -> (),
        G: FnMut(&mut NeonConfig) -> (),
    >(
        num_signers: usize,
        initial_balances: Vec<(StacksAddress, u64)>,
        wait_on_signers: Option<Duration>,
        mut signer_config_modifier: F,
        mut node_config_modifier: G,
        btc_miner_pubkeys: &[Secp256k1PublicKey],
    ) -> Self {
        // Generate Signer Data
        let signer_stacks_private_keys = (0..num_signers)
            .map(|_| StacksPrivateKey::new())
            .collect::<Vec<StacksPrivateKey>>();

        let (mut naka_conf, _miner_account) = naka_neon_integration_conf(None);

        node_config_modifier(&mut naka_conf);

        // Add initial balances to the config
        for (address, amount) in initial_balances.iter() {
            naka_conf
                .add_initial_balance(PrincipalData::from(address.clone()).to_string(), *amount);
        }

        // So the combination is... one, two, three, four, five? That's the stupidest combination I've ever heard in my life!
        // That's the kind of thing an idiot would have on his luggage!
        let password = "12345";
        naka_conf.connection_options.auth_token = Some(password.to_string());
        if let Some(wait_on_signers) = wait_on_signers {
            naka_conf.miner.wait_on_signers = wait_on_signers;
        } else {
            naka_conf.miner.wait_on_signers = Duration::from_secs(10);
        }
        let run_stamp = rand::random();

        // Setup the signer and coordinator configurations
        let signer_configs: Vec<_> = build_signer_config_tomls(
            &signer_stacks_private_keys,
            &naka_conf.node.rpc_bind,
            Some(Duration::from_millis(128)), // Timeout defaults to 5 seconds. Let's override it to 128 milliseconds.
            &Network::Testnet,
            password,
            run_stamp,
            3000,
            Some(100_000),
            None,
            Some(9000),
        )
        .into_iter()
        .map(|toml| {
            let mut signer_config = SignerConfig::load_from_str(&toml).unwrap();
            signer_config_modifier(&mut signer_config);
            signer_config
        })
        .collect();
        assert_eq!(signer_configs.len(), num_signers);

        let spawned_signers = signer_configs
            .iter()
            .cloned()
            .map(SpawnedSigner::new)
            .collect();

        // Setup the nodes and deploy the contract to it
        let btc_miner_pubkeys = if btc_miner_pubkeys.is_empty() {
            let pk = Secp256k1PublicKey::from_hex(
                naka_conf
                    .burnchain
                    .local_mining_public_key
                    .as_ref()
                    .unwrap(),
            )
            .unwrap();
            vec![pk]
        } else {
            btc_miner_pubkeys.to_vec()
        };
        let node = setup_stx_btc_node(
            naka_conf,
            &signer_stacks_private_keys,
            &signer_configs,
            btc_miner_pubkeys.as_slice(),
            node_config_modifier,
        );
        let config = signer_configs.first().unwrap();
        let stacks_client = StacksClient::from(config);

        Self {
            running_nodes: node,
            spawned_signers,
            signer_stacks_private_keys,
            stacks_client,
            run_stamp,
            num_stacking_cycles: 12_u64,
            signer_configs,
        }
    }

    /// Send a status request to each spawned signer
    pub fn send_status_request(&self, exclude: &HashSet<usize>) {
        for signer_ix in 0..self.spawned_signers.len() {
            if exclude.contains(&signer_ix) {
                continue;
            }
            let port = 3000 + signer_ix;
            let endpoint = format!("http://localhost:{}", port);
            let path = format!("{endpoint}/status");

            debug!("Issue status request to {}", &path);
            let client = reqwest::blocking::Client::new();
            let response = client
                .get(path)
                .send()
                .expect("Failed to send status request");
            assert!(response.status().is_success())
        }
    }

    pub fn wait_for_registered(&mut self, timeout_secs: u64) {
        let mut finished_signers = HashSet::new();
        wait_for(timeout_secs, || {
            self.send_status_request(&finished_signers);
            thread::sleep(Duration::from_secs(1));
            let latest_states = self.get_states(&finished_signers);
            for (ix, state) in latest_states.iter().enumerate() {
                let Some(state) = state else { continue; };
                if state.runloop_state == State::RegisteredSigners {
                    finished_signers.insert(ix);
                } else {
                    warn!("Signer #{ix} returned state = {:?}, will try to wait for a registered signers state from them.", state.runloop_state);
                }
            }
            info!("Finished signers: {:?}", finished_signers.iter().collect::<Vec<_>>());
            Ok(finished_signers.len() == self.spawned_signers.len())
        }).unwrap();
    }

    pub fn wait_for_cycle(&mut self, timeout_secs: u64, reward_cycle: u64) {
        let mut finished_signers = HashSet::new();
        wait_for(timeout_secs, || {
            self.send_status_request(&finished_signers);
            thread::sleep(Duration::from_secs(1));
            let latest_states = self.get_states(&finished_signers);
            for (ix, state) in latest_states.iter().enumerate() {
                let Some(state) = state else { continue; };
                let Some(reward_cycle_info) = state.reward_cycle_info else { continue; };
                if reward_cycle_info.reward_cycle == reward_cycle {
                    finished_signers.insert(ix);
                } else {
                    warn!("Signer #{ix} returned state = {:?}, will try to wait for a cycle = {} state from them.", state, reward_cycle);
                }
            }
            info!("Finished signers: {:?}", finished_signers.iter().collect::<Vec<_>>());
            Ok(finished_signers.len() == self.spawned_signers.len())
        }).unwrap();
    }

    /// Get status check results (if returned) from each signer without blocking
    /// Returns Some() or None() for each signer, in order of `self.spawned_signers`
    pub fn get_states(&mut self, exclude: &HashSet<usize>) -> Vec<Option<StateInfo>> {
        let mut output = Vec::new();
        for (ix, signer) in self.spawned_signers.iter().enumerate() {
            if exclude.contains(&ix) {
                output.push(None);
                continue;
            }
            let Ok(mut results) = signer.res_recv.try_recv() else {
                debug!("Could not receive latest state from signer #{ix}");
                output.push(None);
                continue;
            };
            if results.len() > 1 {
                warn!("Received multiple states from the signer receiver: this test function assumes it should only ever receive 1");
                panic!();
            }
            let Some(result) = results.pop() else {
                debug!("Could not receive latest state from signer #{ix}");
                output.push(None);
                continue;
            };
            match result {
                SignerResult::OperationResult(_operation) => {
                    panic!("Recieved an operation result.");
                }
                SignerResult::StatusCheck(state_info) => {
                    output.push(Some(state_info));
                }
            }
        }
        output
    }

    fn nmb_blocks_to_reward_set_calculation(&mut self) -> u64 {
        let prepare_phase_len = self
            .running_nodes
            .conf
            .get_burnchain()
            .pox_constants
            .prepare_length as u64;
        let current_block_height = self
            .running_nodes
            .btc_regtest_controller
            .get_headers_height()
            .saturating_sub(1); // Must subtract 1 since get_headers_height returns current block height + 1
        let curr_reward_cycle = self.get_current_reward_cycle();
        let next_reward_cycle = curr_reward_cycle.saturating_add(1);
        let next_reward_cycle_height = self
            .running_nodes
            .btc_regtest_controller
            .get_burnchain()
            .reward_cycle_to_block_height(next_reward_cycle);
        let next_reward_cycle_reward_set_calculation = next_reward_cycle_height
            .saturating_sub(prepare_phase_len)
            .saturating_add(1); // +1 as the reward calculation occurs in the SECOND block of the prepare phase/

        next_reward_cycle_reward_set_calculation.saturating_sub(current_block_height)
    }

    fn nmb_blocks_to_reward_cycle_boundary(&mut self, reward_cycle: u64) -> u64 {
        let current_block_height = self
            .running_nodes
            .btc_regtest_controller
            .get_headers_height()
            .saturating_sub(1); // Must subtract 1 since get_headers_height returns current block height + 1
        let reward_cycle_height = self
            .running_nodes
            .btc_regtest_controller
            .get_burnchain()
            .reward_cycle_to_block_height(reward_cycle);
        reward_cycle_height.saturating_sub(current_block_height)
    }

    fn mine_nakamoto_block(&mut self, timeout: Duration) -> MinedNakamotoBlockEvent {
        let commits_submitted = self.running_nodes.commits_submitted.clone();
        let mined_block_time = Instant::now();
        next_block_and_mine_commit(
            &mut self.running_nodes.btc_regtest_controller,
            timeout.as_secs(),
            &self.running_nodes.coord_channel,
            &commits_submitted,
        )
        .unwrap();

        let t_start = Instant::now();
        while test_observer::get_mined_nakamoto_blocks().is_empty() {
            assert!(
                t_start.elapsed() < timeout,
                "Timed out while waiting for mined nakamoto block event"
            );
            thread::sleep(Duration::from_secs(1));
        }
        let mined_block_elapsed_time = mined_block_time.elapsed();
        info!(
            "Nakamoto block mine time elapsed: {:?}",
            mined_block_elapsed_time
        );
        test_observer::get_mined_nakamoto_blocks().pop().unwrap()
    }

    fn mine_block_wait_on_processing(
        &mut self,
        coord_channels: &[&Arc<Mutex<CoordinatorChannels>>],
        commits_submitted: &[&Arc<AtomicU64>],
        timeout: Duration,
    ) {
        let blocks_len = test_observer::get_blocks().len();
        let mined_block_time = Instant::now();
        next_block_and_wait_for_commits(
            &mut self.running_nodes.btc_regtest_controller,
            timeout.as_secs(),
            coord_channels,
            commits_submitted,
        )
        .unwrap();
        let t_start = Instant::now();
        while test_observer::get_blocks().len() <= blocks_len {
            assert!(
                t_start.elapsed() < timeout,
                "Timed out while waiting for nakamoto block to be processed"
            );
            thread::sleep(Duration::from_secs(1));
        }
        let mined_block_elapsed_time = mined_block_time.elapsed();
        info!(
            "Nakamoto block mine time elapsed: {:?}",
            mined_block_elapsed_time
        );
    }

    fn wait_for_confirmed_block_v1(
        &mut self,
        block_signer_sighash: &Sha512Trunc256Sum,
        timeout: Duration,
    ) -> ThresholdSignature {
        let block_obj = self.wait_for_confirmed_block_with_hash(block_signer_sighash, timeout);
        let signer_signature_hex = block_obj.get("signer_signature").unwrap().as_str().unwrap();
        let signer_signature_bytes = hex_bytes(&signer_signature_hex[2..]).unwrap();
        let signer_signature =
            ThresholdSignature::consensus_deserialize(&mut signer_signature_bytes.as_slice())
                .unwrap();
        signer_signature
    }

    /// Wait for a confirmed block and return a list of individual
    /// signer signatures
    fn wait_for_confirmed_block_v0(
        &mut self,
        block_signer_sighash: &Sha512Trunc256Sum,
        timeout: Duration,
    ) -> Vec<MessageSignature> {
        let block_obj = self.wait_for_confirmed_block_with_hash(block_signer_sighash, timeout);
        block_obj
            .get("signer_signature")
            .unwrap()
            .as_array()
            .expect("Expected signer_signature to be an array")
            .iter()
            .cloned()
            .map(serde_json::from_value::<MessageSignature>)
            .collect::<Result<Vec<_>, _>>()
            .expect("Unable to deserialize array of MessageSignature")
    }

    /// Wait for a confirmed block and return a list of individual
    /// signer signatures
    fn wait_for_confirmed_block_with_hash(
        &mut self,
        block_signer_sighash: &Sha512Trunc256Sum,
        timeout: Duration,
    ) -> serde_json::Map<String, serde_json::Value> {
        let t_start = Instant::now();
        while t_start.elapsed() <= timeout {
            let blocks = test_observer::get_blocks();
            if let Some(block) = blocks.iter().find_map(|block_json| {
                let block_obj = block_json.as_object().unwrap();
                let sighash = block_obj
                    // use the try operator because non-nakamoto blocks
                    // do not supply this field
                    .get("signer_signature_hash")?
                    .as_str()
                    .unwrap();
                if sighash != &format!("0x{block_signer_sighash}") {
                    return None;
                }
                Some(block_obj.clone())
            }) {
                return block;
            }
            thread::sleep(Duration::from_millis(500));
        }
        panic!("Timed out while waiting for confirmation of block with signer sighash = {block_signer_sighash}")
    }

    fn wait_for_validate_ok_response(&mut self, timeout: Duration) -> BlockValidateOk {
        // Wait for the block to show up in the test observer
        let t_start = Instant::now();
        loop {
            let responses = test_observer::get_proposal_responses();
            for response in responses {
                let BlockValidateResponse::Ok(validation) = response else {
                    continue;
                };
                return validation;
            }
            assert!(
                t_start.elapsed() < timeout,
                "Timed out while waiting for block proposal ok event"
            );
            thread::sleep(Duration::from_secs(1));
        }
    }

    fn wait_for_validate_reject_response(
        &mut self,
        timeout: Duration,
        signer_signature_hash: Sha512Trunc256Sum,
    ) -> BlockValidateReject {
        // Wait for the block to show up in the test observer
        let t_start = Instant::now();
        loop {
            let responses = test_observer::get_proposal_responses();
            for response in responses {
                let BlockValidateResponse::Reject(rejection) = response else {
                    continue;
                };
                if rejection.signer_signature_hash == signer_signature_hash {
                    return rejection;
                }
            }
            assert!(
                t_start.elapsed() < timeout,
                "Timed out while waiting for block proposal reject event"
            );
            thread::sleep(Duration::from_secs(1));
        }
    }

    // Must be called AFTER booting the chainstate
    fn run_until_epoch_3_boundary(&mut self) {
        let epochs = self.running_nodes.conf.burnchain.epochs.clone().unwrap();
        let epoch_3 =
            &epochs[StacksEpoch::find_epoch_by_id(&epochs, StacksEpochId::Epoch30).unwrap()];

        let epoch_30_boundary = epoch_3.start_height - 1;
        // advance to epoch 3.0 and trigger a sign round (cannot vote on blocks in pre epoch 3.0)
        run_until_burnchain_height(
            &mut self.running_nodes.btc_regtest_controller,
            &self.running_nodes.blocks_processed,
            epoch_30_boundary,
            &self.running_nodes.conf,
        );
        info!("Advanced to Nakamoto epoch 3.0 boundary {epoch_30_boundary}! Ready to Sign Blocks!");
    }

    fn get_current_reward_cycle(&self) -> u64 {
        let block_height = get_chain_info(&self.running_nodes.conf).burn_block_height;
        let rc = self
            .running_nodes
            .btc_regtest_controller
            .get_burnchain()
            .block_height_to_reward_cycle(block_height)
            .unwrap();
        info!("Get current reward cycle: block_height = {block_height}, rc = {rc}");
        rc
    }

    fn get_signer_index(&self, reward_cycle: u64) -> SignerSlotID {
        let valid_signer_set =
            u32::try_from(reward_cycle % 2).expect("FATAL: reward_cycle % 2 exceeds u32::MAX");
        let signer_stackerdb_contract_id = boot_code_id(SIGNERS_NAME, false);

        self.stacks_client
            .get_stackerdb_signer_slots(&signer_stackerdb_contract_id, valid_signer_set)
            .expect("FATAL: failed to get signer slots from stackerdb")
            .iter()
            .position(|(address, _)| address == self.stacks_client.get_signer_address())
            .map(|pos| {
                SignerSlotID(u32::try_from(pos).expect("FATAL: number of signers exceeds u32::MAX"))
            })
            .expect("FATAL: signer not registered")
    }

    fn get_signer_slots(
        &self,
        reward_cycle: u64,
    ) -> Result<Vec<(StacksAddress, u128)>, ClientError> {
        let valid_signer_set =
            u32::try_from(reward_cycle % 2).expect("FATAL: reward_cycle % 2 exceeds u32::MAX");
        let signer_stackerdb_contract_id = boot_code_id(SIGNERS_NAME, false);

        self.stacks_client
            .get_stackerdb_signer_slots(&signer_stackerdb_contract_id, valid_signer_set)
    }

    fn get_signer_indices(&self, reward_cycle: u64) -> Vec<SignerSlotID> {
        self.get_signer_slots(reward_cycle)
            .expect("FATAL: failed to get signer slots from stackerdb")
            .iter()
            .enumerate()
            .map(|(pos, _)| {
                SignerSlotID(u32::try_from(pos).expect("FATAL: number of signers exceeds u32::MAX"))
            })
            .collect::<Vec<_>>()
    }

    /// Get the wsts public keys for the given reward cycle
    fn get_signer_public_keys(&self, reward_cycle: u64) -> PublicKeys {
        let entries = self.get_reward_set_signers(reward_cycle);
        let entries = SignerEntries::parse(false, &entries).unwrap();
        entries.public_keys
    }

    /// Get the signers for the given reward cycle
    pub fn get_reward_set_signers(&self, reward_cycle: u64) -> Vec<NakamotoSignerEntry> {
        self.stacks_client
            .get_reward_set_signers(reward_cycle)
            .unwrap()
            .unwrap()
    }

    #[allow(dead_code)]
    fn get_signer_metrics(&self) -> String {
        #[cfg(feature = "monitoring_prom")]
        {
            let client = reqwest::blocking::Client::new();
            let res = client
                .get("http://localhost:9000/metrics")
                .send()
                .unwrap()
                .text()
                .unwrap();

            return res;
        }
        #[cfg(not(feature = "monitoring_prom"))]
        return String::new();
    }

    /// Kills the signer runloop at index `signer_idx`
    ///  and returns the private key of the killed signer.
    ///
    /// # Panics
    /// Panics if `signer_idx` is out of bounds
    pub fn stop_signer(&mut self, signer_idx: usize) -> StacksPrivateKey {
        let spawned_signer = self.spawned_signers.remove(signer_idx);
        let signer_key = self.signer_stacks_private_keys.remove(signer_idx);

        spawned_signer.stop();
        signer_key
    }

    /// (Re)starts a new signer runloop with the given private key
    pub fn restart_signer(&mut self, signer_idx: usize, signer_private_key: StacksPrivateKey) {
        let signer_config = build_signer_config_tomls(
            &[signer_private_key],
            &self.running_nodes.conf.node.rpc_bind,
            Some(Duration::from_millis(128)), // Timeout defaults to 5 seconds. Let's override it to 128 milliseconds.
            &Network::Testnet,
            "12345", // It worked sir, we have the combination! -Great, what's the combination?
            self.run_stamp,
            3000 + signer_idx,
            Some(100_000),
            None,
            Some(9000 + signer_idx),
        )
        .pop()
        .unwrap();

        info!("Restarting signer");
        let config = SignerConfig::load_from_str(&signer_config).unwrap();
        let signer = SpawnedSigner::new(config);
        self.spawned_signers.insert(signer_idx, signer);
    }

    pub fn shutdown(self) {
        self.running_nodes
            .coord_channel
            .lock()
            .expect("Mutex poisoned")
            .stop_chains_coordinator();

        self.running_nodes
            .run_loop_stopper
            .store(false, Ordering::SeqCst);
        self.running_nodes.run_loop_thread.join().unwrap();
        for signer in self.spawned_signers {
            assert!(signer.stop().is_none());
        }
    }
}

fn setup_stx_btc_node<G: FnMut(&mut NeonConfig) -> ()>(
    mut naka_conf: NeonConfig,
    signer_stacks_private_keys: &[StacksPrivateKey],
    signer_configs: &[SignerConfig],
    btc_miner_pubkeys: &[Secp256k1PublicKey],
    mut node_config_modifier: G,
) -> RunningNodes {
    // Spawn the endpoints for observing signers
    for signer_config in signer_configs {
        naka_conf.events_observers.insert(EventObserverConfig {
            endpoint: signer_config.endpoint.to_string(),
            events_keys: vec![
                EventKeyType::StackerDBChunks,
                EventKeyType::BlockProposal,
                EventKeyType::BurnchainBlocks,
            ],
        });
    }

    // Spawn a test observer for verification purposes
    test_observer::spawn();
    let observer_port = test_observer::EVENT_OBSERVER_PORT;
    naka_conf.events_observers.insert(EventObserverConfig {
        endpoint: format!("localhost:{observer_port}"),
        events_keys: vec![
            EventKeyType::StackerDBChunks,
            EventKeyType::BlockProposal,
            EventKeyType::MinedBlocks,
            EventKeyType::BurnchainBlocks,
        ],
    });

    // The signers need some initial balances in order to pay for epoch 2.5 transaction votes
    let mut initial_balances = Vec::new();

    // TODO: separate keys for stacking and signing (because they'll be different in prod)
    for key in signer_stacks_private_keys {
        initial_balances.push(InitialBalance {
            address: to_addr(key).into(),
            amount: POX_4_DEFAULT_STACKER_BALANCE,
        });
    }
    naka_conf.initial_balances.append(&mut initial_balances);
    naka_conf.node.stacker = true;
    naka_conf.miner.wait_on_interim_blocks = Duration::from_secs(5);

    for signer_set in 0..2 {
        for message_id in 0..SIGNER_SLOTS_PER_USER {
            let contract_id =
                NakamotoSigners::make_signers_db_contract_id(signer_set, message_id, false);
            if !naka_conf.node.stacker_dbs.contains(&contract_id) {
                debug!("A miner/stacker must subscribe to the {contract_id} stacker db contract. Forcibly subscribing...");
                naka_conf.node.stacker_dbs.push(contract_id);
            }
        }
    }
    node_config_modifier(&mut naka_conf);

    info!("Make new BitcoinCoreController");
    let mut btcd_controller = BitcoinCoreController::new(naka_conf.clone());
    btcd_controller
        .start_bitcoind()
        .map_err(|_e| ())
        .expect("Failed starting bitcoind");

    info!("Make new BitcoinRegtestController");
    let mut btc_regtest_controller = BitcoinRegtestController::new(naka_conf.clone(), None);

    info!("Bootstraping...");
    // Should be 201 for other tests?
    btc_regtest_controller.bootstrap_chain_to_pks(195, btc_miner_pubkeys);

    info!("Chain bootstrapped...");

    let mut run_loop = boot_nakamoto::BootRunLoop::new(naka_conf.clone()).unwrap();
    let run_loop_stopper = run_loop.get_termination_switch();
    let Counters {
        blocks_processed,
        naka_submitted_vrfs: vrfs_submitted,
        naka_submitted_commits: commits_submitted,
        naka_proposed_blocks: naka_blocks_proposed,
        naka_mined_blocks: naka_blocks_mined,
        naka_rejected_blocks: naka_blocks_rejected,
        naka_skip_commit_op: nakamoto_test_skip_commit_op,
        naka_signer_pushed_blocks,
        ..
    } = run_loop.counters();

    let coord_channel = run_loop.coordinator_channels();
    let run_loop_thread = thread::spawn(move || run_loop.start(None, 0));

    // Give the run loop some time to start up!
    info!("Wait for runloop...");
    wait_for_runloop(&blocks_processed);

    // First block wakes up the run loop.
    info!("Mine first block...");
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // Second block will hold our VRF registration.
    info!("Mine second block...");
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    // Third block will be the first mined Stacks block.
    info!("Mine third block...");
    next_block_and_wait(&mut btc_regtest_controller, &blocks_processed);

    RunningNodes {
        btcd_controller,
        btc_regtest_controller,
        run_loop_thread,
        run_loop_stopper,
        vrfs_submitted: vrfs_submitted.0,
        commits_submitted: commits_submitted.0,
        blocks_processed: blocks_processed.0,
        nakamoto_blocks_proposed: naka_blocks_proposed.0,
        nakamoto_blocks_mined: naka_blocks_mined.0,
        nakamoto_blocks_rejected: naka_blocks_rejected.0,
        nakamoto_blocks_signer_pushed: naka_signer_pushed_blocks.0,
        nakamoto_test_skip_commit_op,
        coord_channel,
        conf: naka_conf,
    }
}