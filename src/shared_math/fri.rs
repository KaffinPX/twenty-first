use crate::shared_math::traits::{CyclicGroupGenerator, IdentityValues, ModPowU32, PrimeField};
use crate::util_types::merkle_tree::{MerkleTree, PartialAuthenticationPath};
use crate::util_types::proof_stream::ProofStream;
use crate::utils::{blake3_digest, get_index_from_bytes};
use std::error::Error;
use std::fmt;

use super::other::log_2_ceil;
use super::polynomial::Polynomial;
use super::traits::FromVecu8;
use crate::shared_math::ntt::slow_intt;

impl Error for ValidationError {}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Deserialization error for LowDegreeProof: {:?}", self)
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum ValidationError {
    BadMerkleProof,
    BadSizedProof,
    NonPostiveRoundCount,
    NotColinear(usize),
    LastIterationTooHighDegree,
    BadMerkleRootForLastCodeword,
}

// Module for performing FRI on XFieldElement
#[derive(Debug, Clone)]
pub struct Fri<PF: PrimeField> {
    offset: PF::Elem,                // Offset for subgroup generator
    omega: PF::Elem,                 // Generator of the expanded domain
    pub domain_length: usize,        // Size of domain generated by omega
    expansion_factor: usize,         // = domain_length / trace_length
    colinearity_checks_count: usize, //
}

type CodewordEvaluation<T> = (usize, T);

impl<PF: PrimeField> Fri<PF> {
    pub fn new(
        offset: PF::Elem,
        omega: PF::Elem,
        domain_length: usize,
        expansion_factor: usize,
        colinearity_checks_count: usize,
    ) -> Self {
        Self {
            offset,
            omega,
            domain_length,
            expansion_factor,
            colinearity_checks_count,
        }
    }

    pub fn prove(
        &self,
        codeword: &[PF::Elem],
        proof_stream: &mut ProofStream,
    ) -> Result<Vec<usize>, Box<dyn Error>> {
        assert_eq!(
            self.domain_length,
            codeword.len(),
            "Initial codeword length must match that set in FRI object"
        );

        // Commit phase
        let merkle_trees: Vec<MerkleTree<PF::Elem>> = self.commit(codeword, proof_stream)?;
        let codewords: Vec<Vec<PF::Elem>> = merkle_trees.iter().map(|x| x.to_vec()).collect();

        // fiat-shamir phase (get indices)
        let top_level_indices = self.sample_indices(&proof_stream.prover_fiat_shamir());

        // query phase
        let mut c_indices = top_level_indices.clone();
        for i in 0..merkle_trees.len() - 1 {
            c_indices = c_indices
                .clone()
                .iter()
                .map(|x| x % (codewords[i].len() / 2))
                .collect();
            self.query(
                merkle_trees[i].clone(),
                merkle_trees[i + 1].clone(),
                &c_indices,
                proof_stream,
            )?;
        }

        Ok(top_level_indices)
    }

    fn commit(
        &self,
        codeword: &[PF::Elem],
        proof_stream: &mut ProofStream,
    ) -> Result<Vec<MerkleTree<PF::Elem>>, Box<dyn Error>> {
        let mut generator = self.omega;
        let mut offset = self.offset;
        let mut codeword_local = codeword.to_vec();

        let zero: PF::Elem = generator.ring_zero();
        let one: PF::Elem = generator.ring_one();
        let two: PF::Elem = one + one;
        let two_inv = one / two;

        // Compute and send Merkle root
        let mut mt = MerkleTree::from_vec(&codeword_local);
        proof_stream.enqueue(&mt.get_root())?;
        let mut merkle_trees = vec![mt];

        let (num_rounds, _) = self.num_rounds();
        for _ in 0..num_rounds {
            let n = codeword_local.len();

            // Sanity check to verify that generator has the right order; requires ModPowU64
            //assert!(generator.inv() == generator.mod_pow((n - 1).into())); // TODO: REMOVE

            // Get challenge, one just acts as *any* element in this field -- the field element
            // is completely determined from the byte stream.
            let alpha: PF::Elem = one.from_vecu8(proof_stream.prover_fiat_shamir());

            let x_offset: Vec<PF::Elem> = generator
                .get_cyclic_group_elements(None)
                .into_iter()
                .map(|x| x * offset)
                .collect();

            let x_offset_inverses = PF::batch_inversion(x_offset);
            for i in 0..n / 2 {
                codeword_local[i] = two_inv
                    * ((one + alpha * x_offset_inverses[i]) * codeword_local[i]
                        + (one - alpha * x_offset_inverses[i]) * codeword_local[n / 2 + i]);
            }
            codeword_local.resize(n / 2, zero);

            // Compute and send Merkle root
            mt = MerkleTree::from_vec(&codeword_local);
            proof_stream.enqueue(&mt.get_root())?;
            merkle_trees.push(mt);

            // Update subgroup generator and offset
            generator = generator * generator;
            offset = offset * offset;
        }

        // Send the last codeword
        proof_stream.enqueue_length_prepended(&codeword_local)?;

        Ok(merkle_trees)
    }

    // Return the c-indices for the 1st round of FRI
    fn sample_indices(&self, seed: &[u8]) -> Vec<usize> {
        // This algorithm starts with the inner-most indices to pick up
        // to `last_codeword_length` indices from the codeword in the last round.
        // It then calculates the indices in the subsequent rounds by choosing
        // between the two possible next indices in the next round until we get
        // the c-indices for the first round.
        let num_rounds = self.num_rounds().0;
        let last_codeword_length = self.domain_length >> num_rounds;
        assert!(
            self.colinearity_checks_count <= last_codeword_length,
            "Requested number of indices must not exceed length of last codeword"
        );

        let mut last_indices: Vec<usize> = vec![];
        let mut remaining_last_round_exponents: Vec<usize> = (0..last_codeword_length).collect();
        let mut counter = 0u32;
        for _ in 0..self.colinearity_checks_count {
            let mut seed_local: Vec<u8> = seed.to_vec();
            seed_local.append(&mut counter.to_be_bytes().into());
            let hash = blake3_digest(&seed_local);
            let index: usize = get_index_from_bytes(&hash, remaining_last_round_exponents.len());
            last_indices.push(remaining_last_round_exponents.remove(index));
            counter += 1;
        }

        // Use last indices to derive first c-indices
        let mut indices = last_indices;
        for i in 1..num_rounds {
            let codeword_length = last_codeword_length << i;

            let mut new_indices: Vec<usize> = vec![];
            for index in indices {
                let mut seed_local: Vec<u8> = seed.to_vec();
                seed_local.append(&mut counter.to_be_bytes().into());
                let hash = blake3_digest(&seed_local);
                let reduce_modulo: bool = get_index_from_bytes(&hash, 2) == 0;
                let new_index = if reduce_modulo {
                    index + codeword_length / 2
                } else {
                    index
                };
                new_indices.push(new_index);

                counter += 1;
            }

            indices = new_indices;
        }

        indices
    }

    fn query(
        &self,
        current_mt: MerkleTree<PF::Elem>,
        next_mt: MerkleTree<PF::Elem>,
        c_indices: &[usize],
        proof_stream: &mut ProofStream,
    ) -> Result<(), Box<dyn Error>> {
        let a_indices: Vec<usize> = c_indices.to_vec();
        let mut b_indices: Vec<usize> = c_indices
            .iter()
            .map(|x| x + current_mt.get_number_of_leafs() / 2)
            .collect();
        let mut ab_indices = a_indices;
        ab_indices.append(&mut b_indices);

        // Reveal authentication paths
        proof_stream.enqueue_length_prepended(&current_mt.get_multi_proof(&ab_indices))?;
        proof_stream.enqueue_length_prepended(&next_mt.get_multi_proof(c_indices))?;

        Ok(())
    }

    pub fn verify(
        &self,
        proof_stream: &mut ProofStream,
    ) -> Result<Vec<CodewordEvaluation<PF::Elem>>, Box<dyn Error>> {
        let mut omega = self.omega;
        let mut offset = self.offset;
        let (num_rounds, degree_of_last_round) = self.num_rounds();

        // Extract all roots and calculate alpha, the challenges
        let mut roots: Vec<[u8; 32]> = vec![];
        let mut alphas: Vec<PF::Elem> = vec![];
        roots.push(proof_stream.dequeue::<[u8; 32]>(32)?);
        for _ in 0..num_rounds {
            // Get a challenge from the proof stream
            let alpha: PF::Elem = omega.from_vecu8(proof_stream.verifier_fiat_shamir());
            alphas.push(alpha);
            roots.push(proof_stream.dequeue::<[u8; 32]>(32)?);
        }

        // Extract last codeword
        let last_codeword: Vec<PF::Elem> =
            proof_stream.dequeue_length_prepended::<Vec<PF::Elem>>()?;

        // Check if last codeword matches the given root
        if *roots.last().unwrap() != MerkleTree::from_vec(&last_codeword).get_root() {
            return Err(Box::new(ValidationError::BadMerkleRootForLastCodeword));
        }

        // Verify that last codeword is of sufficiently low degree
        let mut last_omega = omega;
        let mut last_offset = offset;
        for _ in 0..num_rounds {
            last_omega = last_omega * last_omega;
            last_offset = last_offset * last_offset;
        }

        // Compute interpolant to get the degree of the last codeword
        // Note that we don't have to scale the polynomial back to the
        // trace subgroup since we only check its degree and don't use
        // it further.
        let coefficients = slow_intt(&last_codeword, &last_omega);
        let last_poly_degree: isize = (Polynomial::<PF> { coefficients }).degree();
        if last_poly_degree > degree_of_last_round as isize {
            return Err(Box::new(ValidationError::LastIterationTooHighDegree));
        }

        let top_level_indices = self.sample_indices(&proof_stream.verifier_fiat_shamir());

        // for every round, check consistency of subsequent layers
        let mut codeword_evaluations: Vec<CodewordEvaluation<PF::Elem>> = vec![];
        for r in 0..num_rounds as usize {
            // Fold c indices
            let c_indices: Vec<usize> = top_level_indices
                .iter()
                .map(|x| x % (self.domain_length >> (r + 1)))
                .collect();

            // Infer a and b indices
            let a_indices = c_indices.clone();
            let b_indices: Vec<usize> = a_indices
                .iter()
                .map(|x| x + (self.domain_length >> (r + 1)))
                .collect();
            let mut ab_indices: Vec<usize> = a_indices.clone();
            ab_indices.append(&mut b_indices.clone());

            // Read values and check colinearity
            let ab_values: Vec<PartialAuthenticationPath<PF::Elem>> =
                proof_stream.dequeue_length_prepended()?;
            let c_values: Vec<PartialAuthenticationPath<PF::Elem>> =
                proof_stream.dequeue_length_prepended()?;

            // verify Merkle authentication paths
            if !MerkleTree::verify_multi_proof(roots[r], &ab_indices, &ab_values)
                || !MerkleTree::verify_multi_proof(roots[r + 1], &c_indices, &c_values)
            {
                return Err(Box::new(ValidationError::BadMerkleProof));
            }

            // Verify that the expected number of samples are present
            if ab_values.len() != 2 * self.colinearity_checks_count
                || c_values.len() != self.colinearity_checks_count
            {
                return Err(Box::new(ValidationError::BadSizedProof));
            }

            // Colinearity check
            let axs: Vec<PF::Elem> = (0..self.colinearity_checks_count)
                .map(|i| offset * omega.mod_pow_u32(a_indices[i] as u32))
                .collect();
            let bxs: Vec<PF::Elem> = (0..self.colinearity_checks_count)
                .map(|i| offset * omega.mod_pow_u32(b_indices[i] as u32))
                .collect();
            let cx: PF::Elem = alphas[r];
            let ays: Vec<PF::Elem> = (0..self.colinearity_checks_count)
                .map(|i| ab_values[i].get_value())
                .collect();
            let bys: Vec<PF::Elem> = (0..self.colinearity_checks_count)
                .map(|i| ab_values[i + self.colinearity_checks_count].get_value())
                .collect();
            let cys: Vec<PF::Elem> = (0..self.colinearity_checks_count)
                .map(|i| c_values[i].get_value())
                .collect();

            if (0..self.colinearity_checks_count).any(|i| {
                !Polynomial::<PF>::are_colinear_3((axs[i], ays[i]), (bxs[i], bys[i]), (cx, cys[i]))
            }) {
                return Err(Box::new(ValidationError::NotColinear(r)));
            }
            // Update subgroup generator and offset
            omega = omega * omega;
            offset = offset * offset;

            // Return top-level values to caller
            if r == 0 {
                for s in 0..self.colinearity_checks_count {
                    codeword_evaluations.push((a_indices[s], ays[s]));
                    codeword_evaluations.push((b_indices[s], bys[s]));
                }
            }
        }

        Ok(codeword_evaluations)
    }

    pub fn get_evaluation_domain(&self) -> Vec<PF::Elem> {
        let omega_domain = self.omega.get_cyclic_group_elements(None);
        omega_domain.into_iter().map(|x| x * self.offset).collect()
    }

    fn num_rounds(&self) -> (u8, u32) {
        let max_degree = (self.domain_length / self.expansion_factor) - 1;
        let mut rounds_count = log_2_ceil(max_degree as u64 + 1) as u8;
        let mut max_degree_of_last_round = 0u32;
        if self.expansion_factor < self.colinearity_checks_count {
            let num_missed_rounds = log_2_ceil(
                (self.colinearity_checks_count as f64 / self.expansion_factor as f64).ceil() as u64,
            ) as u8;
            rounds_count -= num_missed_rounds;
            max_degree_of_last_round = 2u32.pow(num_missed_rounds as u32) - 1;
        }

        (rounds_count, max_degree_of_last_round)
    }
}

#[cfg(test)]
mod test_x_field_fri {
    use itertools::Itertools;

    use crate::shared_math::{
        b_field_element::BFieldElement, traits::GetPrimitiveRootOfUnity,
        x_field_element::XFieldElement,
    };

    use super::*;

    #[test]
    fn get_rounds_count_test() {
        let subgroup_order = 512;
        let expansion_factor = 4;
        let mut fri: Fri<XFieldElement> =
            get_x_field_fri_test_object(subgroup_order, expansion_factor, 2);
        assert_eq!((7, 0), fri.num_rounds());
        fri.colinearity_checks_count = 8;
        assert_eq!((6, 1), fri.num_rounds());
        fri.colinearity_checks_count = 10;
        assert_eq!((5, 3), fri.num_rounds());
        fri.colinearity_checks_count = 16;
        assert_eq!((5, 3), fri.num_rounds());
        fri.colinearity_checks_count = 17;
        assert_eq!((4, 7), fri.num_rounds());
        fri.colinearity_checks_count = 18;
        assert_eq!((4, 7), fri.num_rounds());
        fri.colinearity_checks_count = 31;
        assert_eq!((4, 7), fri.num_rounds());
        fri.colinearity_checks_count = 32;
        assert_eq!((4, 7), fri.num_rounds());
        fri.colinearity_checks_count = 33;
        assert_eq!((3, 15), fri.num_rounds());

        fri.domain_length = 256;
        assert_eq!((2, 15), fri.num_rounds());
        fri.colinearity_checks_count = 32;
        assert_eq!((3, 7), fri.num_rounds());

        fri.colinearity_checks_count = 32;
        fri.domain_length = 1048576;
        fri.expansion_factor = 8;
        assert_eq!((15, 3), fri.num_rounds());

        fri.colinearity_checks_count = 33;
        fri.domain_length = 1048576;
        fri.expansion_factor = 8;
        assert_eq!((14, 7), fri.num_rounds());

        fri.colinearity_checks_count = 63;
        fri.domain_length = 1048576;
        fri.expansion_factor = 8;
        assert_eq!((14, 7), fri.num_rounds());

        fri.colinearity_checks_count = 64;
        fri.domain_length = 1048576;
        fri.expansion_factor = 8;
        assert_eq!((14, 7), fri.num_rounds());

        fri.colinearity_checks_count = 65;
        fri.domain_length = 1048576;
        fri.expansion_factor = 8;
        assert_eq!((13, 15), fri.num_rounds());

        fri.domain_length = 256;
        fri.expansion_factor = 4;
        fri.colinearity_checks_count = 17;
        assert_eq!((3, 7), fri.num_rounds());
    }

    #[test]
    fn fri_on_b_field_test() {
        let fri: Fri<BFieldElement> = get_b_field_fri_test_object();
        let mut proof_stream: ProofStream = ProofStream::default();
        let subgroup = fri.omega.get_cyclic_group_elements(None);

        fri.prove(&subgroup, &mut proof_stream).unwrap();
        let verify_result = fri.verify(&mut proof_stream);
        assert!(verify_result.is_ok(), "FRI verification must succeed");

        println!("{:?}", fri);
    }

    #[test]
    fn fri_on_x_field_test() {
        let subgroup_order = 1024;
        let expansion_factor = 4;
        let colinearity_check_count = 6;
        let fri: Fri<XFieldElement> =
            get_x_field_fri_test_object(subgroup_order, expansion_factor, colinearity_check_count);
        let mut proof_stream: ProofStream = ProofStream::default();
        let subgroup = fri.omega.get_cyclic_group_elements(None);

        fri.prove(&subgroup, &mut proof_stream).unwrap();
        let verify_result = fri.verify(&mut proof_stream);
        assert!(verify_result.is_ok());
    }

    #[test]
    fn fri_x_field_limit_test() {
        let subgroup_order = 1024;
        let expansion_factor = 4;
        let colinearity_check_count = 6;
        let fri: Fri<XFieldElement> =
            get_x_field_fri_test_object(subgroup_order, expansion_factor, colinearity_check_count);
        let subgroup = fri.omega.get_cyclic_group_elements(None);

        let mut points: Vec<XFieldElement>;
        for n in &[1, 10, 50, 100, 255] {
            points = subgroup.clone().iter().map(|p| p.mod_pow_u32(*n)).collect();

            // TODO: Test elsewhere that proof_stream can be re-used for multiple .prove().
            let mut proof_stream: ProofStream = ProofStream::default();
            fri.prove(&points, &mut proof_stream).unwrap();

            let verify_result = fri.verify(&mut proof_stream);
            if verify_result.is_err() {
                println!(
                    "There are {} points, |<1024>^{}| = {}, and verify_result = {:?}",
                    points.len(),
                    n,
                    points.iter().unique().count(),
                    verify_result
                );
            }

            assert!(verify_result.is_ok());
        }

        // Negative test
        let too_high = subgroup_order as u32 / expansion_factor as u32;
        points = subgroup.iter().map(|p| p.mod_pow_u32(too_high)).collect();
        let mut proof_stream: ProofStream = ProofStream::default();
        fri.prove(&points, &mut proof_stream).unwrap();
        let verify_result = fri.verify(&mut proof_stream);
        assert!(verify_result.is_err());
    }

    fn get_b_field_fri_test_object() -> Fri<BFieldElement> {
        let subgroup_order = 1024;
        let (omega, _primes1) =
            BFieldElement::ring_zero().get_primitive_root_of_unity(subgroup_order);
        let (offset, _primes2) =
            BFieldElement::ring_zero().get_primitive_root_of_unity(BFieldElement::QUOTIENT - 1);

        let expansion_factor = 4;
        let colinearity_checks = 6;

        Fri::new(
            offset.unwrap(),
            omega.unwrap(),
            subgroup_order as usize,
            expansion_factor,
            colinearity_checks,
        )
    }

    fn get_x_field_fri_test_object(
        subgroup_order: u128,
        expansion_factor: usize,
        colinearity_checks: usize,
    ) -> Fri<XFieldElement> {
        let (omega, _primes1): (Option<XFieldElement>, Vec<u128>) =
            XFieldElement::ring_zero().get_primitive_root_of_unity(subgroup_order);

        // The following offset was picked arbitrarily by copying the one found in
        // `get_b_field_fri_test_object`. It does not generate the full Z_p\{0}, but
        // we're not sure it needs to, Alan?
        let offset: Option<XFieldElement> = Some(XFieldElement::new_const(BFieldElement::new(7)));

        let fri: Fri<XFieldElement> = Fri::<XFieldElement>::new(
            offset.unwrap(),
            omega.unwrap(),
            subgroup_order as usize,
            expansion_factor,
            colinearity_checks,
        );
        fri
    }
}
