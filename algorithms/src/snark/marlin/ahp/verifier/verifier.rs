// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkVM library.

// The snarkVM library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkVM library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkVM library. If not, see <https://www.gnu.org/licenses/>.

use core::marker::PhantomData;

use crate::{
    fft::EvaluationDomain,
    snark::marlin::{
        ahp::{
            indexer::{Circuit, CircuitInfo},
            verifier::{BatchCombiners, FirstMessage, QuerySet, SecondMessage, State, ThirdMessage},
            AHPError,
            AHPForR1CS,
        },
        MarlinMode,
    },
    AlgebraicSponge,
};
use snarkvm_fields::PrimeField;
use std::collections::BTreeMap;

impl<TargetField: PrimeField, MM: MarlinMode> AHPForR1CS<TargetField, MM> {
    /// Output the first message and next round state.
    pub fn verifier_first_round<'a, BaseField: PrimeField, R: AlgebraicSponge<BaseField, 2>>(
        index_info: CircuitInfo<TargetField>, // TODO: this looks like index_info is just about a single circuit, instead of all?
        batch_sizes: &BTreeMap<&'a Circuit<BaseField, MM>, usize>,
        fs_rng: &mut R,
    ) -> Result<(FirstMessage<'a, TargetField, MM>, State<TargetField, MM>), AHPError> {
        // Check that the R1CS is a square matrix.
        if index_info.num_constraints != index_info.num_variables {
            return Err(AHPError::NonSquareMatrix);
        }

        let constraint_domain_time = start_timer!(|| "Constructing constraint domain");
        let constraint_domain: EvaluationDomain<TargetField> = // TODO: why does vscode complain that this type needs to be annotated?
            EvaluationDomain::new(index_info.num_constraints).ok_or(AHPError::PolynomialDegreeTooLarge)?;
        end_timer!(constraint_domain_time);

        let non_zero_a_time = start_timer!(|| "Constructing non-zero-a domain");
        let non_zero_a_domain =
            EvaluationDomain::new(index_info.num_non_zero_a).ok_or(AHPError::PolynomialDegreeTooLarge)?;
        end_timer!(non_zero_a_time);

        let non_zero_b_time = start_timer!(|| "Constructing non-zero-b domain");
        let non_zero_b_domain =
            EvaluationDomain::new(index_info.num_non_zero_b).ok_or(AHPError::PolynomialDegreeTooLarge)?;
        end_timer!(non_zero_b_time);

        let non_zero_c_time = start_timer!(|| "Constructing non-zero-c domain");
        let non_zero_c_domain =
            EvaluationDomain::new(index_info.num_non_zero_c).ok_or(AHPError::PolynomialDegreeTooLarge)?;
        end_timer!(non_zero_c_time);

        let input_domain_time = start_timer!(|| "Constructing input domain");
        let input_domain =
            EvaluationDomain::new(index_info.num_public_inputs).ok_or(AHPError::PolynomialDegreeTooLarge)?;
        end_timer!(input_domain_time);

        let squeeze_time = start_timer!(|| "Squeezing challenges");
        let elems = fs_rng.squeeze_nonnative_field_elements(3);
        let (first, rest) = elems.split_at(3);
        let [alpha, eta_b, eta_c]: [_; 3] = first.try_into().unwrap();
        let mut batch_combiners = BTreeMap::new();
        let mut circuit_combiners_needed = 0; // the first circuit_combiner is simply TargetField::one()
        // TODO: we should do a review as to what happens when we have more than usize circuit/instance combiners
        for (circuit, batch_size) in batch_sizes {
            let mut combiners = BatchCombiners {
                circuit_combiner: TargetField::one(),
                instance_combiners: vec![TargetField::one()],
            };
            let instances = batch_size;
            let elems = fs_rng.squeeze_nonnative_field_elements(instances - 1 + circuit_combiners_needed);
            let (instance_combiners, circuit_combiner) = elems.split_at(instances - 1);
            assert!(circuit_combiner.len() < 2);
            if circuit_combiner.len() == 1 {
                combiners.circuit_combiner = circuit_combiner[0];
            }
            for instance_combiner in instance_combiners {
                combiners.instance_combiners.push(*instance_combiner);
            }
            batch_combiners.insert(circuit, combiners);
            // TODO: to discuss: this is a bit ugly, but could be avoided if either we use an indexmap to count for us what is the first circuit, or we extract the first loop out of the for-loop
            circuit_combiners_needed = 1; // All circuits after the first need a random circuit combiner
        }
        end_timer!(squeeze_time);

        let check_vanish_poly_time = start_timer!(|| "Evaluating vanishing polynomial");
        assert!(!constraint_domain.evaluate_vanishing_polynomial(alpha).is_zero());
        end_timer!(check_vanish_poly_time);

        let message = FirstMessage { alpha, eta_b, eta_c, batch_combiners};

        let new_state = State {
            batch_sizes,
            input_domain,
            constraint_domain,
            non_zero_a_domain,
            non_zero_b_domain,
            non_zero_c_domain,
            first_round_message: Some(message.clone()),
            second_round_message: None,
            third_round_message: None,
            gamma: None,
            mode: PhantomData,
        };

        Ok((message, new_state))
    }

    /// Output the second message and next round state.
    pub fn verifier_second_round<BaseField: PrimeField, R: AlgebraicSponge<BaseField, 2>>(
        mut state: State<TargetField, MM>,
        fs_rng: &mut R,
    ) -> Result<(SecondMessage<TargetField>, State<TargetField, MM>), AHPError> {
        let elems = fs_rng.squeeze_nonnative_field_elements(1);
        let beta = elems[0];
        assert!(!state.constraint_domain.evaluate_vanishing_polynomial(beta).is_zero());

        let message = SecondMessage { beta };
        state.second_round_message = Some(message);

        Ok((message, state))
    }

    /// Output the third message and next round state.
    pub fn verifier_third_round<BaseField: PrimeField, R: AlgebraicSponge<BaseField, 2>>(
        mut state: State<TargetField, MM>,
        fs_rng: &mut R,
    ) -> Result<(ThirdMessage<TargetField>, State<TargetField, MM>), AHPError> {
        let elems = fs_rng.squeeze_nonnative_field_elements(2);
        let r_b = elems[0];
        let r_c = elems[1];
        // TODO: (maybe) generate more circuit_combiners here.
        let message = ThirdMessage { r_b, r_c };

        state.third_round_message = Some(message);
        Ok((message, state))
    }

    /// Output the third message and next round state.
    pub fn verifier_fourth_round<BaseField: PrimeField, R: AlgebraicSponge<BaseField, 2>>(
        mut state: State<TargetField, MM>,
        fs_rng: &mut R,
    ) -> Result<State<TargetField, MM>, AHPError> {
        let elems = fs_rng.squeeze_nonnative_field_elements(1);
        let gamma = elems[0];

        state.gamma = Some(gamma);
        Ok(state)
    }

    /// Output the query state and next round state.
    pub fn verifier_query_set(state: State<TargetField, MM>) -> (QuerySet<TargetField>, State<TargetField, MM>) {
        (QuerySet::new(&state), state)
    }
}
