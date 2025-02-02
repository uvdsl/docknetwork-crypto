//! Same as CLS range range proof protocol but does Keyed-Verification, i.e the verifies knows the
//! secret key of the BB-sig

use ark_ec::{pairing::Pairing, AffineRepr, CurveGroup};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use ark_std::{cfg_into_iter, format, io::Write, rand::RngCore, vec::Vec, UniformRand};

use crate::{
    ccs_set_membership::setup::SetMembershipCheckParams, common::MemberCommitmentKey,
    error::SmcRangeProofError,
};
use dock_crypto_utils::misc::n_rand;

use crate::prelude::SecretKey;
use dock_crypto_utils::{ff::inner_product, msm::multiply_field_elems_with_same_group_elem};

#[cfg(feature = "parallel")]
use rayon::prelude::*;

use crate::cls_range_proof::util::{
    check_commitment, find_number_of_digits, find_sumset_boundaries,
    get_range_and_randomness_multiple, solve_linear_equations,
};

#[derive(Clone, PartialEq, Eq, Debug, CanonicalSerialize, CanonicalDeserialize)]
pub struct CLSRangeProofWithKVProtocol<E: Pairing> {
    pub base: u16,
    pub digits: Vec<E::ScalarField>,
    pub r: E::ScalarField,
    pub v: Vec<E::ScalarField>,
    pub V: Vec<E::G1Affine>,
    pub a: Vec<E::G1Affine>,
    pub D: E::G1Affine,
    pub m: E::ScalarField,
    pub s: Vec<E::ScalarField>,
    pub t: Vec<E::ScalarField>,
}

#[derive(Clone, PartialEq, Eq, Debug, CanonicalSerialize, CanonicalDeserialize)]
pub struct CLSRangeProofWithKV<E: Pairing> {
    pub base: u16,
    pub V: Vec<E::G1Affine>,
    pub a: Vec<E::G1Affine>,
    pub D: E::G1Affine,
    pub z_v: Vec<E::ScalarField>,
    pub z_sigma: Vec<E::ScalarField>,
    pub z_r: E::ScalarField,
}

impl<E: Pairing> CLSRangeProofWithKVProtocol<E> {
    pub fn init<R: RngCore>(
        rng: &mut R,
        value: u64,
        randomness: E::ScalarField,
        min: u64,
        max: u64,
        comm_key: &MemberCommitmentKey<E::G1Affine>,
        params: &SetMembershipCheckParams<E>,
    ) -> Result<Self, SmcRangeProofError> {
        Self::init_given_base(
            rng,
            value,
            randomness,
            min,
            max,
            params.get_supported_base_for_range_proof(),
            comm_key,
            params,
        )
    }

    pub fn init_given_base<R: RngCore>(
        rng: &mut R,
        mut value: u64,
        randomness: E::ScalarField,
        min: u64,
        max: u64,
        base: u16,
        comm_key: &MemberCommitmentKey<E::G1Affine>,
        params: &SetMembershipCheckParams<E>,
    ) -> Result<Self, SmcRangeProofError> {
        if min > value {
            return Err(SmcRangeProofError::IncorrectBounds(format!(
                "value={} should be >= min={}",
                value, min
            )));
        }
        if value >= max {
            return Err(SmcRangeProofError::IncorrectBounds(format!(
                "value={} should be < max={}",
                value, max
            )));
        }

        params.validate_base(base)?;

        let (range, randomness_multiple) = get_range_and_randomness_multiple(base, min, max);
        value = value - min;
        if randomness_multiple != 1 {
            value = value * (base - 1) as u64;
        }

        let l = find_number_of_digits(range, base);
        let G = find_sumset_boundaries(range, base, l);

        // Note: This is different from the paper as only a single `m` needs to be created.
        let m = E::ScalarField::rand(rng);
        let s = n_rand(rng, l).collect::<Vec<E::ScalarField>>();
        let D = comm_key.commit(
            &inner_product(
                &s,
                &cfg_into_iter!(G.clone())
                    .map(|G_i| E::ScalarField::from(G_i))
                    .collect::<Vec<_>>(),
            ),
            &(m * E::ScalarField::from(randomness_multiple)),
        );

        if let Some(digits) = solve_linear_equations(value, &G, base) {
            // Following is only for debugging
            // let mut expected = 0_u64;
            // for j in 0..digits.len() {
            //     assert!(digits[j] < base);
            //     expected += digits[j] as u64 * G[j];
            // }
            // assert_eq!(expected, value);

            let digits = cfg_into_iter!(digits)
                .map(|d| E::ScalarField::from(d))
                .collect::<Vec<_>>();
            let t = n_rand(rng, l).collect::<Vec<_>>();
            let v = n_rand(rng, l).collect::<Vec<_>>();
            let V = randomize_sigs!(&digits, &v, &params);

            let g1t =
                multiply_field_elems_with_same_group_elem(params.bb_sig_params.g1.into_group(), &t);
            let a = cfg_into_iter!(0..l as usize)
                .map(|i| (V[i] * -s[i]) + g1t[i])
                .collect::<Vec<_>>();
            Ok(Self {
                base,
                digits,
                r: randomness,
                v,
                V: E::G1::normalize_batch(&V),
                a: E::G1::normalize_batch(&a),
                D,
                m,
                s,
                t,
            })
        } else {
            Err(SmcRangeProofError::InvalidRange(range, base))
        }
    }

    pub fn challenge_contribution<W: Write>(
        &self,
        commitment: &E::G1Affine,
        comm_key: &MemberCommitmentKey<E::G1Affine>,
        params: &SetMembershipCheckParams<E>,
        writer: W,
    ) -> Result<(), SmcRangeProofError> {
        Self::compute_challenge_contribution(
            &self.V, &self.a, &self.D, commitment, comm_key, params, writer,
        )
    }

    pub fn gen_proof(self, challenge: &E::ScalarField) -> CLSRangeProofWithKV<E> {
        gen_proof!(self, challenge, CLSRangeProofWithKV)
    }

    pub fn compute_challenge_contribution<W: Write>(
        V: &[E::G1Affine],
        a: &[E::G1Affine],
        D: &E::G1Affine,
        commitment: &E::G1Affine,
        comm_key: &MemberCommitmentKey<E::G1Affine>,
        params: &SetMembershipCheckParams<E>,
        mut writer: W,
    ) -> Result<(), SmcRangeProofError> {
        params.serialize_for_schnorr_protocol_for_kv(&mut writer)?;
        comm_key.serialize_compressed(&mut writer)?;
        for sig in &params.sigs {
            sig.0.serialize_compressed(&mut writer)?;
        }
        commitment.serialize_compressed(&mut writer)?;
        for V_i in V {
            V_i.serialize_compressed(&mut writer)?;
        }
        for a_i in a {
            a_i.serialize_compressed(&mut writer)?;
        }
        D.serialize_compressed(&mut writer)?;
        Ok(())
    }
}

impl<E: Pairing> CLSRangeProofWithKV<E> {
    pub fn verify(
        &self,
        commitment: &E::G1Affine,
        challenge: &E::ScalarField,
        min: u64,
        max: u64,
        comm_key: &MemberCommitmentKey<E::G1Affine>,
        params: &SetMembershipCheckParams<E>,
        secret_key: &SecretKey<E::ScalarField>,
    ) -> Result<(), SmcRangeProofError> {
        self.non_crypto_validate(min, max, &params)?;

        check_commitment::<E>(
            self.base,
            &self.z_sigma,
            &self.z_r,
            &self.D,
            min,
            max,
            commitment,
            challenge,
            comm_key,
        )?;

        let g1v = multiply_field_elems_with_same_group_elem(
            params.bb_sig_params.g1.into_group(),
            &self.z_v,
        );

        let sk_c = secret_key.0 * challenge;

        for i in 0..self.V.len() {
            if self.a[i] != (self.V[i] * (sk_c - self.z_sigma[i]) + g1v[i]).into_affine() {
                return Err(SmcRangeProofError::InvalidRangeProof);
            }
        }
        Ok(())
    }

    pub fn challenge_contribution<W: Write>(
        &self,
        commitment: &E::G1Affine,
        comm_key: &MemberCommitmentKey<E::G1Affine>,
        params: &SetMembershipCheckParams<E>,
        writer: W,
    ) -> Result<(), SmcRangeProofError> {
        CLSRangeProofWithKVProtocol::compute_challenge_contribution(
            &self.V, &self.a, &self.D, commitment, comm_key, params, writer,
        )
    }

    fn non_crypto_validate(
        &self,
        min: u64,
        max: u64,
        params: &SetMembershipCheckParams<E>,
    ) -> Result<(), SmcRangeProofError> {
        params.validate_base(self.base)?;
        if min >= max {
            return Err(SmcRangeProofError::IncorrectBounds(format!(
                "min={} should be < max={}",
                min, max
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ccs_set_membership::setup::SetMembershipCheckParams;
    use ark_bls12_381::{Bls12_381, Fr, G1Affine};
    use ark_std::{
        rand::{rngs::StdRng, SeedableRng},
        UniformRand,
    };
    use blake2::Blake2b512;
    use schnorr_pok::compute_random_oracle_challenge;
    use std::time::{Duration, Instant};

    #[test]
    fn sumsets_check() {
        let mut rng = StdRng::seed_from_u64(0u64);

        fn check(max: u64, g: &[u64], base: u16) {
            for i in max..=max {
                let sigma = solve_linear_equations(max, g, base).unwrap();
                assert_eq!(sigma.len(), g.len());
                let mut expected = 0_u64;
                for j in 0..sigma.len() {
                    assert!(sigma[j] < base);
                    expected += sigma[j] as u64 * g[j];
                }
                assert_eq!(expected, i);
            }
        }

        let mut runs = 0;
        let start = Instant::now();
        for base in [3, 4, 5, 8, 10, 11, 14, 16] {
            for _ in 0..10 {
                // let max = ((u16::rand(&mut rng) as u64)) * (base as u64 - 1);
                // let max = ((u16::rand(&mut rng) as u64) << 4) * (base as u64 - 1);
                let max = ((u16::rand(&mut rng) as u64) >> 4) * (base as u64 - 1);
                // while (max % (base as u64 - 1)) != 0 {
                //     max = u64::rand(&mut rng);
                // }
                let l = find_number_of_digits(max, base);
                let G = find_sumset_boundaries(max, base, l);
                println!("Starting for base={} and max={}", base, max);
                let start_check = Instant::now();
                check(max, &G, base);
                println!(
                    "Check done for base={} and max={} in {:?}",
                    base,
                    max,
                    start_check.elapsed()
                );
                runs += 1;
            }
        }
        println!("Time for {} runs: {:?}", runs, start.elapsed());
    }

    #[test]
    fn cls_range_proof() {
        let mut rng = StdRng::seed_from_u64(0u64);
        let mut proving_time = Duration::default();
        let mut verifying_time = Duration::default();
        let mut num_proofs = 0;

        for base in [2, 4, 8, 16] {
            let (params, sk) = SetMembershipCheckParams::<Bls12_381>::new_for_range_proof::<
                _,
                Blake2b512,
            >(&mut rng, b"test", base);
            params.verify().unwrap();

            let comm_key = MemberCommitmentKey::<G1Affine>::generate_using_rng(&mut rng);

            for _ in 0..5 {
                let mut a = [
                    u16::rand(&mut rng) as u64,
                    u16::rand(&mut rng) as u64,
                    u16::rand(&mut rng) as u64,
                ];
                a.sort();
                let min = a[0];
                let max = a[2];
                let value = a[1];
                assert!(value > min);
                assert!(value < max);
                let randomness = Fr::rand(&mut rng);
                let commitment = comm_key.commit(&Fr::from(value), &randomness);

                // Params with incorrect base should fail
                let params_with_smaller_base = {
                    let (params, _) = SetMembershipCheckParams::<Bls12_381>::new_for_range_proof::<
                        _,
                        Blake2b512,
                    >(&mut rng, b"test", base - 1);
                    params.verify().unwrap();
                    params
                };
                assert!(CLSRangeProofWithKVProtocol::init_given_base(
                    &mut rng,
                    value,
                    randomness,
                    min,
                    max,
                    base,
                    &comm_key,
                    &params_with_smaller_base,
                )
                .is_err());

                // min > max should fail
                assert!(CLSRangeProofWithKVProtocol::init_given_base(
                    &mut rng, value, randomness, max, min, base, &comm_key, &params,
                )
                .is_err());

                // Params with larger base should work
                let (params_with_larger_base, sk_larger) = {
                    let (params, sk) = SetMembershipCheckParams::<Bls12_381>::new_for_range_proof::<
                        _,
                        Blake2b512,
                    >(&mut rng, b"test", base + 1);
                    params.verify().unwrap();
                    (params, sk)
                };

                for (p, sk) in [(&params, &sk), (&params_with_larger_base, &sk_larger)] {
                    let start = Instant::now();
                    let protocol = CLSRangeProofWithKVProtocol::init_given_base(
                        &mut rng, value, randomness, min, max, base, &comm_key, p,
                    )
                    .unwrap();

                    let mut chal_bytes_prover = vec![];
                    protocol
                        .challenge_contribution(&commitment, &comm_key, p, &mut chal_bytes_prover)
                        .unwrap();
                    let challenge_prover =
                        compute_random_oracle_challenge::<Fr, Blake2b512>(&chal_bytes_prover);

                    let proof = protocol.gen_proof(&challenge_prover);
                    proving_time += start.elapsed();

                    let start = Instant::now();
                    let mut chal_bytes_verifier = vec![];
                    proof
                        .challenge_contribution(&commitment, &comm_key, p, &mut chal_bytes_verifier)
                        .unwrap();
                    let challenge_verifier =
                        compute_random_oracle_challenge::<Fr, Blake2b512>(&chal_bytes_verifier);
                    assert_eq!(challenge_prover, challenge_verifier);

                    // assert_eq!(proof.V.len(), l as usize);
                    proof
                        .verify(&commitment, &challenge_verifier, min, max, &comm_key, p, sk)
                        .unwrap();
                    verifying_time += start.elapsed();

                    num_proofs += 1;
                }
            }
        }

        println!(
            "For {} proofs, proving_time={:?} and verifying_time={:?}",
            num_proofs, proving_time, verifying_time
        );
    }
}
