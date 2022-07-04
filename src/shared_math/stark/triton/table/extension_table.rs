use super::base_table::Table;
use super::challenges_endpoints::{AllChallenges, AllEndpoints};
use crate::shared_math::b_field_element::BFieldElement;
use crate::shared_math::mpolynomial::{Degree, MPolynomial};
use crate::shared_math::polynomial::Polynomial;
use crate::shared_math::stark::triton::fri_domain::{lift_domain, FriDomain};
use crate::shared_math::traits::{Inverse, ModPowU32, PrimeField};
use crate::shared_math::x_field_element::XFieldElement;
use rayon::iter::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator, ParallelIterator,
};

// Generic methods specifically for tables that have been extended

type BWord = BFieldElement;
type XWord = XFieldElement;

pub trait ExtensionTable: Table<XWord> + Sync {
    fn ext_boundary_constraints(&self, challenges: &AllChallenges) -> Vec<MPolynomial<XWord>>;

    fn ext_consistency_constraints(&self, challenges: &AllChallenges) -> Vec<MPolynomial<XWord>>;

    fn ext_transition_constraints(&self, challenges: &AllChallenges) -> Vec<MPolynomial<XWord>>;

    fn ext_terminal_constraints(
        &self,
        challenges: &AllChallenges,
        terminals: &AllEndpoints,
    ) -> Vec<MPolynomial<XWord>>;

    fn max_degree(&self) -> Degree {
        let degree_bounds: Vec<Degree> = vec![self.interpolant_degree(); self.full_width() * 2];

        // 1. Insert dummy challenges
        // 2. Refactor so we can calculate max_degree without specifying challenges
        //    (and possibly without even calling ext_transition_constraints).
        self.ext_transition_constraints(&AllChallenges::dummy())
            .iter()
            .map(|air| {
                let symbolic_degree_bound: Degree = air.symbolic_degree_bound(&degree_bounds);
                let padded_height: Degree = self.padded_height() as Degree;

                symbolic_degree_bound - padded_height + 1
            })
            .max()
            .unwrap_or(-1)
    }

    fn all_quotient_degree_bounds(
        &self,
        challenges: &AllChallenges,
        terminals: &AllEndpoints,
    ) -> Vec<Degree> {
        vec![
            self.boundary_quotient_degree_bounds(challenges),
            self.transition_quotient_degree_bounds(challenges),
            self.terminal_quotient_degree_bounds(challenges, terminals),
        ]
        .concat()
    }

    fn boundary_quotient_degree_bounds(&self, challenges: &AllChallenges) -> Vec<Degree> {
        let max_degrees: Vec<Degree> = vec![self.interpolant_degree(); self.full_width()];

        let degree_bounds: Vec<Degree> = self
            .ext_boundary_constraints(challenges)
            .iter()
            .map(|mpo| mpo.symbolic_degree_bound(&max_degrees) - 1)
            .collect();

        degree_bounds
    }

    fn transition_quotient_degree_bounds(&self, challenges: &AllChallenges) -> Vec<Degree> {
        let max_degrees: Vec<Degree> = vec![self.interpolant_degree(); 2 * self.full_width()];

        let transition_constraints = self.ext_transition_constraints(challenges);

        // Safe because padded height is at most 2^30.
        let padded_height: Degree = self.padded_height().try_into().unwrap();

        transition_constraints
            .iter()
            .map(|mpo| mpo.symbolic_degree_bound(&max_degrees) - padded_height + 1)
            .collect::<Vec<Degree>>()
    }

    fn terminal_quotient_degree_bounds(
        &self,
        challenges: &AllChallenges,
        terminals: &AllEndpoints,
    ) -> Vec<Degree> {
        let max_degrees: Vec<Degree> = vec![self.interpolant_degree(); self.full_width()];
        self.ext_terminal_constraints(challenges, terminals)
            .iter()
            .map(|mpo| mpo.symbolic_degree_bound(&max_degrees) - 1)
            .collect::<Vec<Degree>>()
    }

    fn all_quotients(
        &self,
        fri_domain: &FriDomain<BWord>,
        codewords: &[Vec<XWord>],
        challenges: &AllChallenges,
        terminals: &AllEndpoints,
    ) -> Vec<Vec<XWord>> {
        // println!("TABLENAME: {}", self.name());
        let boundary_quotients = self.boundary_quotients(fri_domain, codewords, challenges);
        let transition_quotients = self.transition_quotients(fri_domain, codewords, challenges);
        let terminal_quotients =
            self.terminal_quotients(fri_domain, codewords, challenges, terminals);

        vec![boundary_quotients, transition_quotients, terminal_quotients].concat()
    }

    fn transition_quotients(
        &self,
        fri_domain: &FriDomain<BWord>,
        codewords: &[Vec<XWord>],
        challenges: &AllChallenges,
    ) -> Vec<Vec<XWord>> {
        let one = BFieldElement::ring_one();
        let x_values: Vec<BFieldElement> = fri_domain.domain_values();
        let subgroup_zerofier: Vec<BFieldElement> = x_values
            .iter()
            .map(|x| x.mod_pow_u32(self.padded_height() as u32) - one)
            .collect();
        let subgroup_zerofier_inverse = if self.padded_height() == 0 {
            subgroup_zerofier
        } else {
            BFieldElement::batch_inversion(subgroup_zerofier)
        };

        let omicron_inverse = self.omicron().unlift().unwrap().inverse();
        let zerofier_inverse: Vec<BFieldElement> = x_values
            .into_iter()
            .enumerate()
            .map(|(i, x)| subgroup_zerofier_inverse[i] * (x - omicron_inverse))
            .collect();

        let transition_constraints = self.ext_transition_constraints(challenges);

        let mut quotients: Vec<Vec<XWord>> = vec![];
        let unit_distance = self.unit_distance(fri_domain.length);
        for tc in transition_constraints.iter() {
            let quotient_codeword: Vec<XWord> = zerofier_inverse
                .par_iter()
                .enumerate()
                .map(|(i, z_inverse)| {
                    let current: Vec<XWord> =
                        (0..self.full_width()).map(|j| codewords[j][i]).collect();

                    let next: Vec<XWord> = (0..self.full_width())
                        .map(|j| codewords[j][(i + unit_distance) % fri_domain.length])
                        .collect();

                    let point = vec![current, next].concat();
                    let composition_evaluation = tc.evaluate(&point);
                    composition_evaluation * z_inverse.lift()
                })
                .collect();

            quotients.push(quotient_codeword);
        }
        // If the `DEBUG` environment variable is set, interpolate the quotient and check the degree

        if std::env::var("DEBUG").is_ok() {
            for (i, qc) in quotients.iter().enumerate() {
                let interpolated: Polynomial<XWord> = lift_domain(fri_domain).interpolate(qc);
                assert!(
                    interpolated.degree() < fri_domain.length as isize - 1,
                    "Degree of transition quotient number {} in {} must not be maximal. Got degree {}, and FRI domain length was {}. Unsatisfied constraint: {}", i, self.name(), interpolated.degree(), fri_domain.length, transition_constraints[i]
                );
            }
        }

        quotients
    }

    fn terminal_quotients(
        &self,
        fri_domain: &FriDomain<BWord>,
        codewords: &[Vec<XWord>],
        challenges: &AllChallenges,
        terminals: &AllEndpoints,
    ) -> Vec<Vec<XWord>> {
        let omicron_inverse = self.omicron().unlift().unwrap().inverse();

        // The zerofier for the terminal quotient has a root in the last
        // value in the cyclical group generated from omicron.
        let zerofier_codeword: Vec<BFieldElement> = fri_domain
            .domain_values()
            .into_iter()
            .map(|x| x - omicron_inverse)
            .collect();

        let zerofier_inverse = BFieldElement::batch_inversion(zerofier_codeword);
        let terminal_constraints = self.ext_terminal_constraints(challenges, terminals);
        let mut quotient_codewords: Vec<Vec<XWord>> = vec![];
        for termc in terminal_constraints.iter() {
            let quotient_codeword: Vec<XWord> = (0..fri_domain.length)
                .into_par_iter()
                .map(|i| {
                    let point: Vec<XWord> =
                        (0..self.full_width()).map(|j| codewords[j][i]).collect();

                    termc.evaluate(&point) * zerofier_inverse[i].lift()
                })
                .collect();
            quotient_codewords.push(quotient_codeword);
        }

        if std::env::var("DEBUG").is_ok() {
            for (i, qc) in quotient_codewords.iter().enumerate() {
                let interpolated = lift_domain(fri_domain).interpolate(qc);
                assert!(
                    interpolated.degree() < fri_domain.length as isize - 1,
                    "Degree of terminal quotient number {} in {} must not be maximal. Got degree {}, and FRI domain length was {}. Unsatisfied constraint: {}", i, self.name(), interpolated.degree(), fri_domain.length, terminal_constraints[i]
                );
            }
        }

        quotient_codewords
    }

    fn boundary_quotients(
        &self,
        fri_domain: &FriDomain<BWord>,
        codewords: &[Vec<XWord>],
        challenges: &AllChallenges,
    ) -> Vec<Vec<XWord>> {
        assert!(!codewords.is_empty(), "Codewords must be non-empty");
        for row in codewords.iter() {
            debug_assert_eq!(
                fri_domain.length,
                row.len(),
                "Codewords have fri_domain.length columns ({}), not {}.",
                fri_domain.length,
                row.len()
            );
        }

        let mut quotient_codewords: Vec<Vec<XWord>> = vec![];

        let boundary_constraints: Vec<MPolynomial<XWord>> =
            self.ext_boundary_constraints(challenges);
        let one = BFieldElement::ring_one();
        let zerofier: Vec<BFieldElement> = (0..fri_domain.length)
            .map(|i| fri_domain.domain_value(i as u32) - one)
            .collect();
        let zerofier_inverse = BFieldElement::batch_inversion(zerofier);

        for bc in boundary_constraints {
            // println!("rows: {}, columns: {}", codewords.len(), codewords[0].len());
            // println!("fri_domain.length = {}", fri_domain.length);
            let quotient_codeword: Vec<XWord> = (0..fri_domain.length)
                .into_iter()
                .map(|fri_dom_i| {
                    // println!(
                    //     "LOOKATME self.base_width() = {}, self.full_width() = {}, self.name() = {}",
                    //     self.base_width(),
                    //     self.full_width(),
                    //     self.name()
                    // );

                    let point: Vec<XWord> = (0..self.full_width())
                        .map(|j| {
                            // println!("i: {}, j: {}", fri_dom_i, j);
                            codewords[j][fri_dom_i]
                        })
                        .collect();

                    // println!(
                    //     "before evaluate, {}: points.len() = {}",
                    //     self.name(),
                    //     self.full_width()
                    // );

                    bc.evaluate(&point) * zerofier_inverse[fri_dom_i].lift()
                })
                .collect();
            quotient_codewords.push(quotient_codeword);
        }

        // If the `DEBUG` environment variable is set, run this extra validity check
        if std::env::var("DEBUG").is_ok() {
            for (i, qc) in quotient_codewords.iter().enumerate() {
                let interpolated = lift_domain(fri_domain).interpolate(qc);
                assert!(
                    interpolated.degree() < fri_domain.length as isize - 1,
                    "Degree of boundary quotient number {} in {} must not be maximal. Got degree {}, and FRI domain length was {}", i, self.name(), interpolated.degree(), fri_domain.length
                );
            }
        }

        quotient_codewords
    }
}
