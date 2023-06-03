// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkVM library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod inclusion;
pub use inclusion::*;

mod query;
pub use query::*;

use crate::{
    block::{Execution, Fee, Input, Transition},
    snark::{Proof, ProvingKey, VerifyingKey},
    store::BlockStorage,
};
use circuit::Assignment;
use console::{
    network::prelude::*,
    program::{InputID, Locator},
};

use once_cell::sync::OnceCell;
use std::collections::HashMap;

#[derive(Clone, Debug, Default)]
pub struct Trace<N: Network> {
    /// The list of transitions.
    transitions: Vec<Transition<N>>,
    /// A map of locators to (proving key, assignments) pairs.
    transition_tasks: HashMap<Locator<N>, (ProvingKey<N>, Vec<Assignment<N::Field>>)>,
    /// A tracker for all inclusion tasks.
    inclusion_tasks: Inclusion<N>,

    /// A tracker for the inclusion assignments.
    inclusion_assignments: OnceCell<Vec<InclusionAssignment<N>>>,
    /// A tracker for the global state root.
    global_state_root: OnceCell<N::StateRoot>,
}

impl<N: Network> Trace<N> {
    /// Initializes a new trace.
    pub fn new() -> Self {
        Self {
            transitions: Vec::new(),
            transition_tasks: HashMap::new(),
            inclusion_tasks: Inclusion::new(),
            inclusion_assignments: OnceCell::new(),
            global_state_root: OnceCell::new(),
        }
    }

    /// Inserts the transition into the trace.
    pub fn insert_transition(
        &mut self,
        input_ids: &[InputID<N>],
        transition: &Transition<N>,
        (proving_key, assignment): (ProvingKey<N>, Assignment<N::Field>),
    ) -> Result<()> {
        // Ensure the inclusion assignments and global state root have not been set.
        ensure!(self.inclusion_assignments.get().is_none());
        ensure!(self.global_state_root.get().is_none());

        // Insert the transition into the inclusion tasks.
        self.inclusion_tasks.insert_transition(input_ids, transition)?;

        // Construct the locator.
        let locator = Locator::new(*transition.program_id(), *transition.function_name());
        // Insert the assignment (and proving key if the entry does not exist), for the specified locator.
        self.transition_tasks.entry(locator).or_insert((proving_key, vec![])).1.push(assignment);
        // Insert the transition into the list.
        self.transitions.push(transition.clone());

        Ok(())
    }
}

impl<N: Network> Trace<N> {
    /// Returns `true` if the trace is for a fee transition.
    pub fn is_fee(&self) -> bool {
        match self.transitions.len() {
            // If there is 1 transition, check if the transition is a fee transition.
            1 => {
                &self.transitions[0].program_id().to_string() == "credits.aleo"
                    && &self.transitions[0].function_name().to_string() == "fee"
            }
            // Otherwise, set the indicator to 'false'.
            _ => false,
        }
    }

    /// Returns the inclusion assignments and global state root for the current transition(s).
    pub fn prepare<B: BlockStorage<N>, Q: Into<Query<N, B>>>(&mut self, query: Q) -> Result<()> {
        // Compute the inclusion assignments.
        let (inclusion_assignments, global_state_root) = match self.is_fee() {
            true => self.inclusion_tasks.prepare_fee(&self.transitions[0], query)?,
            false => self.inclusion_tasks.prepare_execution(&self.transitions, query)?,
        };
        // Store the inclusion assignments and global state root.
        self.inclusion_assignments
            .set(inclusion_assignments)
            .map_err(|_| anyhow!("Failed to set inclusion assignments"))?;
        self.global_state_root.set(global_state_root).map_err(|_| anyhow!("Failed to set global state root"))?;
        Ok(())
    }

    /// Returns the inclusion assignments and global state root for the current transition(s).
    pub async fn prepare_async<B: BlockStorage<N>, Q: Into<Query<N, B>>>(&mut self, query: Q) -> Result<()> {
        // Compute the inclusion assignments.
        let (inclusion_assignments, global_state_root) = match self.is_fee() {
            true => self.inclusion_tasks.prepare_fee_async(&self.transitions[0], query).await?,
            false => self.inclusion_tasks.prepare_execution_async(&self.transitions, query).await?,
        };
        // Store the inclusion assignments and global state root.
        self.inclusion_assignments
            .set(inclusion_assignments)
            .map_err(|_| anyhow!("Failed to set inclusion assignments"))?;
        self.global_state_root.set(global_state_root).map_err(|_| anyhow!("Failed to set global state root"))?;
        Ok(())
    }

    /// Returns a new execution with a proof, for the current inclusion assignments and global state root.
    pub fn prove_execution<A: circuit::Aleo<Network = N>, R: Rng + CryptoRng>(
        &self,
        locator: &str,
        rng: &mut R,
    ) -> Result<Execution<N>> {
        // Ensure this is not a fee.
        ensure!(!self.is_fee(), "The trace cannot prove execution for fee");
        // Retrieve the inclusion assignments.
        let inclusion_assignments =
            self.inclusion_assignments.get().ok_or_else(|| anyhow!("Inclusion assignments have not been set"))?;
        // Retrieve the global state root.
        let global_state_root =
            self.global_state_root.get().ok_or_else(|| anyhow!("Global state root has not been set"))?;
        // Compute the proof.
        let (global_state_root, proof) = Self::prove_batch::<A, R>(
            locator,
            self.transition_tasks.clone(),
            inclusion_assignments,
            *global_state_root,
            rng,
        )?;
        // Return the execution.
        Execution::from(self.transitions.iter().cloned(), global_state_root, Some(proof))
    }

    /// Returns a new fee with a proof, for the current inclusion assignment and global state root.
    pub fn prove_fee<A: circuit::Aleo<Network = N>, R: Rng + CryptoRng>(&self, rng: &mut R) -> Result<Fee<N>> {
        // Ensure this is a fee.
        ensure!(self.is_fee(), "The trace cannot prove fee for execution");
        // Retrieve the inclusion assignments.
        let inclusion_assignments =
            self.inclusion_assignments.get().ok_or_else(|| anyhow!("Inclusion assignments have not been set"))?;
        // Ensure there is only 1 inclusion assignment.
        ensure!(inclusion_assignments.len() == 1, "Expected 1 inclusion assignment for proving the fee");
        // Retrieve the global state root.
        let global_state_root =
            self.global_state_root.get().ok_or_else(|| anyhow!("Global state root has not been set"))?;
        // Retrieve the fee transition.
        let fee_transition = match self.transitions.len() {
            1 => &self.transitions[0],
            _ => bail!("Expected 1 transition in the trace for proving the fee"),
        };
        // Compute the proof.
        let (global_state_root, proof) = Self::prove_batch::<A, R>(
            "credits.aleo/fee",
            self.transition_tasks.clone(),
            inclusion_assignments,
            *global_state_root,
            rng,
        )?;
        // Return the fee.
        Ok(Fee::from(fee_transition.clone(), global_state_root, Some(proof)))
    }

    /// Checks the proof for the execution.
    /// Note: This does *not* check that the global state root exists in the ledger.
    pub fn verify_execution_proof(
        locator: &str,
        verifier_inputs: HashMap<Locator<N>, (VerifyingKey<N>, Vec<Vec<N::Field>>)>,
        execution: &Execution<N>,
    ) -> Result<()> {
        // Retrieve the global state root.
        let global_state_root = execution.global_state_root();
        // Retrieve the proof.
        let Some(proof) = execution.proof() else {
            bail!("Expected the execution to contain a proof")
        };
        // Verify the execution proof.
        match Self::verify_batch(locator, verifier_inputs, global_state_root, execution.transitions(), proof) {
            Ok(()) => Ok(()),
            Err(e) => bail!("Execution is invalid - {e}"),
        }
    }

    /// Checks the proof for the fee.
    /// Note: This does *not* check that the global state root exists in the ledger.
    pub fn verify_fee_proof(
        verifier_inputs: (Locator<N>, (VerifyingKey<N>, Vec<Vec<N::Field>>)),
        fee: &Fee<N>,
    ) -> Result<()> {
        // Construct a HashMap for the verifier inputs.
        let verifier_inputs = [verifier_inputs].into_iter().collect();
        // Retrieve the global state root.
        let global_state_root = fee.global_state_root();
        // Ensure the global state root is not zero.
        if global_state_root == N::StateRoot::default() {
            bail!("Inclusion expected the global state root in the fee to *not* be zero")
        }
        // Retrieve the proof.
        let Some(proof) = fee.proof() else {
            bail!("Expected the fee to contain a proof")
        };
        // Ensure the transition contains an input record.
        if fee.transition().inputs().iter().filter(|i| matches!(i, Input::Record(..))).count() != 1 {
            bail!("Inclusion expected the fee to contain an input record")
        }
        // Verify the fee proof.
        match Self::verify_batch(
            "credits.aleo/fee",
            verifier_inputs,
            global_state_root,
            [fee.transition()].into_iter(),
            proof,
        ) {
            Ok(()) => Ok(()),
            Err(e) => bail!("Fee is invalid - {e}"),
        }
    }
}

impl<N: Network> Trace<N> {
    /// Returns the global state root and inclusion proof for the given assignments.
    fn prove_batch<A: circuit::Aleo<Network = N>, R: Rng + CryptoRng>(
        locator: &str,
        mut proving_tasks: HashMap<Locator<N>, (ProvingKey<N>, Vec<Assignment<N::Field>>)>,
        inclusion_assignments: &[InclusionAssignment<N>],
        global_state_root: N::StateRoot,
        rng: &mut R,
    ) -> Result<(N::StateRoot, Proof<N>)> {
        // Ensure the global state root is not zero.
        // Note: To protect user privacy, even when there are *no* inclusion assignments,
        // the user must provide a real global state root (which is checked in consensus).
        if global_state_root == N::StateRoot::default() {
            bail!("Inclusion expected the global state root in the execution to *not* be zero")
        }

        // Initialize a vector for the batch inclusion assignments.
        let mut batch_inclusions = Vec::with_capacity(inclusion_assignments.len());

        for assignment in inclusion_assignments.iter() {
            // Ensure the global state root is the same across iterations.
            if global_state_root != assignment.state_path.global_state_root() {
                bail!("Inclusion expected the global state root to be the same across iterations")
            }
            // Add the assignment to the assignments.
            batch_inclusions.push(assignment.to_circuit_assignment::<A>()?);
        }

        if !batch_inclusions.is_empty() {
            // Fetch the inclusion proving key.
            let proving_key = ProvingKey::<N>::new(N::inclusion_proving_key().clone());
            // Insert the inclusion proving key and assignments.
            proving_tasks.insert(Locator::from_str("inclusion.aleo/state_path")?, (proving_key, batch_inclusions));
        }

        // Compute the proof.
        let proof = ProvingKey::prove_batch(locator, &proving_tasks, rng)?;
        // Return the global state root and proof.
        Ok((global_state_root, proof))
    }

    /// Checks the proof for the given inputs.
    /// Note: This does *not* check that the global state root exists in the ledger.
    fn verify_batch<'a>(
        locator: &str,
        mut verifier_inputs: HashMap<Locator<N>, (VerifyingKey<N>, Vec<Vec<N::Field>>)>,
        global_state_root: N::StateRoot,
        transitions: impl ExactSizeIterator<Item = &'a Transition<N>>,
        proof: &Proof<N>,
    ) -> Result<()> {
        // Construct the batch of inclusion verifier inputs.
        let batch_inclusion_inputs = Inclusion::prepare_verifier_inputs(global_state_root, transitions)?;
        // Insert the batch of inclusion verifier inputs to the verifier inputs.
        if !batch_inclusion_inputs.is_empty() {
            // Construct the locator.
            let locator = Locator::from_str("inclusion.aleo/state_path")?;
            // Fetch the inclusion verifying key.
            let verifying_key = VerifyingKey::<N>::new(N::inclusion_verifying_key().clone());
            // Insert the inclusion verifier inputs.
            verifier_inputs.insert(locator, (verifying_key, batch_inclusion_inputs));
        }
        // Verify the proof.
        match VerifyingKey::verify_batch(locator, verifier_inputs, proof) {
            true => Ok(()),
            false => bail!("Failed to verify proof"),
        }
    }
}

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use snarkvm_utilities::TestRng;
//
//     #[test]
//     fn test_inclusion_verify_execution() {
//         let rng = &mut TestRng::default();
//         // Fetch an execution transaction.
//         let execution_transaction = crate::vm::test_helpers::sample_execution_transaction_with_fee(rng);
//
//         match execution_transaction {
//             Transaction::Execute(_, execution, _) => {
//                 assert!(Trace::verify_execution(&execution).is_ok());
//             }
//             _ => panic!("Expected an execution transaction"),
//         }
//     }
// }

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use snarkvm_utilities::TestRng;
//
//     #[test]
//     fn test_inclusion_verify_fee() {
//         let rng = &mut TestRng::default();
//         // Fetch a deployment transaction.
//         let deployment_transaction = crate::vm::test_helpers::sample_deployment_transaction(rng);
//
//         match deployment_transaction {
//             Transaction::Deploy(_, _, _, fee) => {
//                 assert!(Inclusion::verify_fee(&fee).is_ok());
//             }
//             _ => panic!("Expected a deployment transaction"),
//         }
//     }
// }
