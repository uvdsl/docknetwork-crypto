#![allow(non_snake_case)]
#![allow(non_camel_case_types)]

//! Utilities for batch updates to the accumulators and witnesses.

use crate::setup::SecretKey;
use crate::utils::multiply_field_elems_refs_with_same_group_elem;
use ark_ec::msm::VariableBaseMSM;
use ark_ec::{AffineCurve, ProjectiveCurve};
use ark_ff::{batch_inversion, One, PrimeField, Zero};
use ark_poly::polynomial::{univariate::DensePolynomial, UVPolynomial};
use ark_poly::Polynomial;
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize, SerializationError};
use ark_std::{
    fmt::Debug,
    io::{Read, Write},
    iter::{IntoIterator, Iterator},
    vec,
    vec::Vec,
};
use dock_crypto_utils::serde_utils::*;

use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// Naive multiplication (n^2) of 2 polynomials defined over prime fields
fn multiply_poly<F: PrimeField>(
    left: &DensePolynomial<F>,
    right: &DensePolynomial<F>,
) -> DensePolynomial<F> {
    let mut product = (0..(left.degree() + right.degree() + 1))
        .map(|_| F::zero())
        .collect::<Vec<_>>();
    for i in 0..=left.degree() {
        for j in 0..=right.degree() {
            product[i + j] += left.coeffs[i] * right.coeffs[j];
        }
    }
    DensePolynomial::from_coefficients_vec(product)
}

/// Create a polynomial with given points in `updates` as:
/// (updates[0]-x) * (updates[1]-x) * (updates[2] - x)...(updates[last] - x)
fn poly_from_given_updates<F: PrimeField>(updates: &[F]) -> DensePolynomial<F> {
    if updates.is_empty() {
        return DensePolynomial::zero();
    }

    let minus_one = -F::one();
    // [(updates[0]-x), (updates[1]-x), (updates[2] - x), ..., (updates[last] - x)]
    #[cfg(not(feature = "parallel"))]
    let x_i = updates
        .iter()
        .map(|i| DensePolynomial::from_coefficients_slice(&[*i, minus_one]));

    #[cfg(feature = "parallel")]
    let x_i = updates
        .par_iter()
        .map(|i| DensePolynomial::from_coefficients_slice(&[*i, minus_one]));

    // Product (updates[0]-x) * (updates[1]-x) * (updates[2] - x)...(updates[last] - x)
    #[cfg(not(feature = "parallel"))]
    let r = x_i
        .into_iter()
        .reduce(|a, b| multiply_poly(&a, &b))
        .unwrap();

    #[cfg(feature = "parallel")]
    let r = x_i.into_par_iter().reduce(
        || DensePolynomial::from_coefficients_vec(vec![F::one()]),
        |a, b| multiply_poly(&a, &b),
    );

    r
    // Note: Using multiply operator from ark-poly is orders of magnitude slower than naive multiplication
    // x_i.into_iter().reduce(|a, b| &a * &b).unwrap()
}

pub(crate) fn batch_normalize_projective_into_affine<G: ProjectiveCurve>(
    mut v: Vec<G>,
) -> Vec<G::Affine> {
    G::batch_normalization(&mut v);
    v.into_iter().map(|v| v.into()).collect()
}

// Polynomials as described in section 3 of the paper

/// Polynomial `d_A` and `d_D`. Same polynomial is used for both additions and removals.
#[derive(Clone, Debug, PartialEq, Eq, CanonicalSerialize, CanonicalDeserialize)]
pub struct Poly_d<F: PrimeField>(pub DensePolynomial<F>);

/// Polynomial `v_A`. Used for batch additions
#[derive(Clone, Debug, PartialEq, Eq, CanonicalSerialize, CanonicalDeserialize)]
pub struct Poly_v_A<F: PrimeField>(pub DensePolynomial<F>);

/// Polynomial `v_D`. Used for batch removals
#[derive(Clone, Debug, PartialEq, Eq, CanonicalSerialize, CanonicalDeserialize)]
pub struct Poly_v_D<F: PrimeField>(pub DensePolynomial<F>);

/// Polynomial `v_{A, D}`. Used when doing batch additions and removals in the same call
#[derive(Clone, Debug, PartialEq, Eq, CanonicalSerialize, CanonicalDeserialize)]
pub struct Poly_v_AD<F: PrimeField>(pub DensePolynomial<F>);

// TODO: Convert arguments to following functions to iterators

impl<F> Poly_d<F>
where
    F: PrimeField,
{
    /// Given a list of elements as `updates`, generates a polynomial `(updates[0]-x) * (updates[1]-x) * (updates[2] - x)...(updates[last] - x)`.
    pub fn generate(updates: &[F]) -> Self {
        if updates.is_empty() {
            // Returning constant polynomial with value one as the evaluation of this polynomial is multiplied by the old witness
            Self(DensePolynomial::from_coefficients_slice(&[F::one()]))
        } else {
            Self(poly_from_given_updates(updates))
        }
    }

    /// Evaluate this polynomial at `x`
    pub fn eval(&self, x: &F) -> F {
        self.0.evaluate(x)
    }

    /// Evaluation of polynomial without creating the polynomial as the variable is already known.
    /// Returns (updates[0]-x)*(updates[1]-x)*(updates[2]-x)*...(updates[n]-x)*
    pub fn eval_direct(updates: &[F], x: &F) -> F {
        updates.iter().fold(F::one(), |a, y| (*y - *x) * a)
        // TODO: Figure out the why the following line is about 5 times slower than the sequential one above
        // iter!(updates).map(|y| *y - *x).product()
    }
}

impl<F> Poly_v_A<F>
where
    F: PrimeField,
{
    /// Generate polynomial `v_A(x)` given the list of elements `y_A` as `updates` and the secret
    /// key `alpha`.
    pub fn generate(additions: &[F], alpha: &F) -> Self {
        let n = additions.len();
        if n == 0 {
            return Self(DensePolynomial::zero());
        }
        if n == 1 {
            return Self(DensePolynomial::from_coefficients_vec(vec![F::one()]));
        }

        // Need to compute the sum:
        // (y_1-x)*(y_2-x)*(y_3-x)*..*(y_{n-1}-x) + (y_0+alpha)*(y_2-x)*(y_3-x)*..*(y_{n-1}-x) + (y_0+alpha)*(y_1+alpha)*(y_3-x)*..*(y_{n-1}-x) + (y_0+alpha)*(y_1+alpha)*..*(y_{n-2}+alpha)
        // Compute products by memoization: (y_0+alpha), (y_0+alpha)*(y_1+alpha),...y_0+alpha)*(y_1+alpha)*..*(y_{n-2}+alpha)
        // Compute products by memoization: (y_{n-1}-x), (y_{n-1}-x)*(y_{n-2}-x), ...(y_{n-1}-x)*(y_{n-2}-x)*...*(y_1-x)
        let mut factors = vec![F::one(); n];
        let mut polys = vec![DensePolynomial::from_coefficients_vec(vec![F::one()]); n];
        for s in 1..n {
            factors[s] = factors[s - 1] * (additions[s - 1] + alpha);
            polys[n - 1 - s] = multiply_poly(
                &polys[n - s],
                &DensePolynomial::from_coefficients_vec(vec![additions[n - s], -F::one()]),
            );
        }

        let product = into_iter!(factors)
            .zip(into_iter!(polys))
            .map(|(f, p)| &p * f);

        #[cfg(feature = "parallel")]
        let sum = product.reduce(|| DensePolynomial::zero(), |a, b| a + b);

        #[cfg(not(feature = "parallel"))]
        let sum = product.fold(DensePolynomial::zero(), |a, b| a + b);

        Self(sum)
    }

    /// Generate polynomial `v_A(x)` given the list of elements `y_A` as `updates` and the secret
    /// key `alpha`. Slower than `Self::generate` but uses less memory at the cost of recomputing
    /// products of field elements and polynomials
    pub fn generate_without_memoize(additions: &[F], alpha: &F) -> Self {
        let n = additions.len();
        if n == 0 {
            return Self(DensePolynomial::zero());
        }
        let sum = (0..n)
            .map(|s| {
                let factor = Self::compute_factor(s, additions, alpha);
                let poly = if s < n - 1 {
                    let roots: Vec<F> = iter!(additions).skip(s + 1).map(|a| *a).collect();
                    poly_from_given_updates(&roots)
                } else {
                    DensePolynomial::from_coefficients_vec(vec![F::one()])
                };
                &poly * factor
            })
            .fold(DensePolynomial::zero(), |a, b| a + b);
        Self(sum)
    }

    /// Evaluate this polynomial at `x`
    pub fn eval(&self, x: &F) -> F {
        self.0.evaluate(x)
    }

    /// Evaluation of polynomial without creating the polynomial as the variables are already known.
    pub fn eval_direct(additions: &[F], alpha: &F, x: &F) -> F {
        let n = additions.len();
        if n == 0 {
            return F::zero();
        }
        if n == 1 {
            return F::one();
        }

        // Compute products (y_0+alpha), (y_0+alpha)*(y_1+alpha), .. etc by memoization
        // Compute products (y_{n-1}-x), (y_{n-1}-x)*(y_{n-2}-x), .. etc by memoization
        let mut factors = vec![F::one(); n];
        let mut polys = vec![F::one(); n];
        for s in 1..n {
            factors[s] = factors[s - 1] * (additions[s - 1] + alpha);
            polys[n - 1 - s] = polys[n - s] * (additions[n - s] - *x);
        }
        factors
            .into_iter()
            .zip(polys.into_iter())
            .map(|(f, p)| p * f)
            .fold(F::zero(), |a, b| a + b)

        // TODO: Following is slower by factor of 2 from above but why?
        /*into_iter!(factors)
        .zip(into_iter!(polys))
        .map(|(f, p)| p * f)
        .reduce(F::zero(), |a, b| a + b)*/
    }

    /// Evaluation of polynomial without creating the polynomial as the variables are already known.
    /// Slower than `Self::eval_direct` but uses less memory at the cost of recomputing
    /// products of field elements
    pub fn eval_direct_without_memoize(additions: &[F], alpha: &F, x: &F) -> F {
        let n = additions.len();
        let sum = (0..n)
            .map(|s| {
                let factor = Self::compute_factor(s, additions, alpha);
                let poly = if s < n - 1 {
                    additions
                        .iter()
                        .skip(s + 1)
                        .map(|a| *a - *x)
                        .fold(F::one(), |a, b| a * b)
                } else {
                    F::one()
                };
                poly * factor
            })
            .fold(F::zero(), |a, b| a + b);
        sum
    }

    fn compute_factor(s: usize, additions: &[F], alpha: &F) -> F {
        if s > 0 {
            (0..s)
                .map(|i| additions[i] + *alpha)
                .reduce(|a, b| a * b)
                .unwrap()
        } else {
            F::one()
        }
    }
}

impl<F> Poly_v_D<F>
where
    F: PrimeField,
{
    /// Generate polynomial `v_D(x)` given the list of elements `y_D` as `updates` and the secret key `alpha`
    pub fn generate(removals: &[F], alpha: &F) -> Self {
        let n = removals.len();
        if n == 0 {
            return Self(DensePolynomial::zero());
        }

        // Need products of terms 1/(removals[i]+alpha) for all, so invert them all at once thus
        // `y_plus_alpha_inv` will be 1/(removals[0] + alpha), 1/(removals[1] + alpha), .. etc
        let mut y_plus_alpha_inv = removals.iter().map(|y| *y + *alpha).collect::<Vec<_>>();
        batch_inversion(&mut y_plus_alpha_inv);

        // Compute products by memoization: 1/(y_0+alpha), 1/(y_0+alpha)*1/(y_1+alpha), ...., 1/(y_0+alpha)*1/(y_1+alpha)*...*1/(y_{n-1}+alpha)
        // Compute products by memoization: (y_0-x), (y_0-x)*(y_1-x), ...., (y_0-x)*(y_1-x)*..*(y_{n-2}-x)
        let mut factors = vec![F::one(); n];
        let mut polys = vec![DensePolynomial::from_coefficients_vec(vec![F::one()]); n];
        factors[0] = y_plus_alpha_inv[0];
        for s in 1..n {
            factors[s] = factors[s - 1] * y_plus_alpha_inv[s];
            polys[s] = multiply_poly(
                &polys[s - 1],
                &DensePolynomial::from_coefficients_vec(vec![removals[s - 1], -F::one()]),
            );
        }

        let product = into_iter!(factors)
            .zip(into_iter!(polys))
            .map(|(f, p)| &p * f);

        #[cfg(feature = "parallel")]
        let sum = product.reduce(|| DensePolynomial::zero(), |a, b| a + b);

        #[cfg(not(feature = "parallel"))]
        let sum = product.fold(DensePolynomial::zero(), |a, b| a + b);

        Self(sum)
    }

    /// Generate polynomial `v_D(x)` given the list of elements `y_D` as `updates` and the secret key
    /// `alpha`. Slower than `Self::generate` but uses less memory at the cost of recomputing
    /// products of field elements and polynomials
    pub fn generate_without_memoize(removals: &[F], alpha: &F) -> Self {
        let n = removals.len();
        if n == 0 {
            return Self(DensePolynomial::zero());
        }
        let sum = (0..n)
            .map(|s| {
                let factor = Self::compute_factor(s, removals, alpha);
                let poly = if s > 0 {
                    let roots: Vec<F> = iter!(removals).take(s).map(|a| *a).collect();
                    poly_from_given_updates(&roots)
                } else {
                    DensePolynomial::from_coefficients_vec(vec![F::one()])
                };
                &poly * factor
            })
            .fold(DensePolynomial::zero(), |a, b| a + b);
        Self(sum)
    }

    /// Evaluate this polynomial at `x`
    pub fn eval(&self, x: &F) -> F {
        self.0.evaluate(x)
    }

    /// Evaluation of polynomial without creating the polynomial as the variables are already known.
    pub fn eval_direct(removals: &[F], alpha: &F, x: &F) -> F {
        let n = removals.len();
        if n == 0 {
            return F::zero();
        }

        // Compute 1/(removals[i]+alpha) for all i
        let mut y_plus_alpha_inv = removals.iter().map(|y| *y + *alpha).collect::<Vec<_>>();
        batch_inversion(&mut y_plus_alpha_inv);

        // Compute products by memoization: 1/(y_0+alpha), 1/(y_0+alpha)*1/(y_1+alpha), ...., 1/(y_0+alpha)*1/(y_1+alpha)*...*1/(y_{n-1}+alpha)
        // Compute products by memoization: (y_0-x), (y_0-x)*(y_1-x), ...., (y_0-x)*(y_1-x)*..*(y_{n-2}-x)
        let mut factors = vec![F::one(); n];
        let mut polys = vec![F::one(); n];
        factors[0] = y_plus_alpha_inv[0];
        for s in 1..n {
            factors[s] = factors[s - 1] * y_plus_alpha_inv[s];
            polys[s] = polys[s - 1] * (removals[s - 1] - *x);
        }

        factors
            .into_iter()
            .zip(polys.into_iter())
            .map(|(f, p)| p * f)
            .fold(F::zero(), |a, b| a + b)

        // TODO: Following is slower by factor of ~1.5 from above but why?
        /*into_iter!(factors)
        .zip(into_iter!(polys))
        .map(|(f, p)| p * f)
        .reduce(|| F::zero(), |a, b| a + b)*/
    }

    /// Evaluation of polynomial without creating the polynomial as the variables are already known.
    /// Slower than `Self::eval_direct` but uses less memory at the cost of recomputing
    /// products of field elements
    pub fn eval_direct_without_memoize(removals: &[F], alpha: &F, x: &F) -> F {
        let n = removals.len();
        let sum = (0..n)
            .map(|s| {
                let factor = Self::compute_factor(s, removals, alpha);
                let poly = if s > 0 {
                    removals
                        .iter()
                        .take(s)
                        .map(|a| *a - *x)
                        .fold(F::one(), |a, b| a * b)
                } else {
                    F::one()
                };
                poly * factor
            })
            .fold(F::zero(), |a, b| a + b);
        sum
    }

    fn compute_factor(s: usize, removals: &[F], alpha: &F) -> F {
        (0..s + 1)
            .map(|i| removals[i] + *alpha)
            .fold(F::one(), |a, b| a * b)
            .inverse()
            .unwrap()
    }
}

impl<F> Poly_v_AD<F>
where
    F: PrimeField,
{
    /// Generate polynomial `v_{A,D}(x)`, given `y_A` as `additions`, `y_D` as `removals` and the secret key `alpha`
    pub fn generate(additions: &[F], removals: &[F], alpha: &F) -> Self {
        if additions.is_empty() && removals.is_empty() {
            return Self(DensePolynomial::zero());
        }
        let mut p = Poly_v_A::generate(additions, alpha).0;
        if removals.len() > 0 {
            p = &p
                - &(&Poly_v_D::generate(removals, alpha).0 * Self::compute_factor(additions, alpha))
        }
        Self(p)
    }

    /// Evaluate this polynomial at `x`
    pub fn eval(&self, x: &F) -> F {
        self.0.evaluate(x)
    }

    /// Evaluation of polynomial without creating the polynomial as the variables are already known.
    pub fn eval_direct(additions: &[F], removals: &[F], alpha: &F, x: &F) -> F {
        let mut e = Poly_v_A::eval_direct(additions, alpha, x);
        if removals.len() > 0 {
            e = e
                - (Poly_v_D::eval_direct(removals, alpha, x)
                    * Self::compute_factor(additions, alpha))
        }
        e
    }

    pub fn get_coefficients(&self) -> &[F] {
        &self.0.coeffs
    }

    fn compute_factor(additions: &[F], alpha: &F) -> F {
        additions
            .iter()
            .map(|a| *a + *alpha)
            .fold(F::one(), |a, b| a * b)
    }
}

/// Published by the accumulator manager to allow witness updates without secret info. Defined in section 4.1 of the paper
#[serde_as]
#[derive(
    Clone, PartialEq, Eq, Debug, CanonicalSerialize, CanonicalDeserialize, Serialize, Deserialize,
)]
pub struct Omega<G: AffineCurve>(#[serde_as(as = "Vec<AffineGroupBytes>")] pub Vec<G>);

impl<G> Omega<G>
where
    G: AffineCurve,
{
    /// Create new `Omega` after `additions` are added and `removals` are removed from `old_accumulator`.
    /// Note that `old_accumulator` is the accumulated value before the updates were made.
    pub fn new(
        additions: &[G::ScalarField],
        removals: &[G::ScalarField],
        old_accumulator: &G,
        sk: &SecretKey<G::ScalarField>,
    ) -> Self {
        let poly = Poly_v_AD::generate(additions, removals, &sk.0);
        let coeffs = poly.get_coefficients();
        Omega(batch_normalize_projective_into_affine::<G::Projective>(
            multiply_field_elems_refs_with_same_group_elem(
                4,
                old_accumulator.into_projective(),
                coeffs.iter(),
            ),
        ))
    }

    /// Inner product of powers of `y`, i.e. the element for which witness needs to be updated and `omega`
    /// Used by the (non)member to update its witness without the knowledge of secret key.
    pub fn inner_product_with_powers_of_y(&self, element: &G::ScalarField) -> G::Projective {
        // powers_of_y = [1, element, element^2, element^3, ...]
        let mut powers_of_y = Vec::with_capacity(self.len());
        if self.len() > 0 {
            powers_of_y.push(G::ScalarField::one());
        }
        if self.len() > 1 {
            powers_of_y.push(element.clone());
        }
        for i in 2..self.len() {
            powers_of_y.push(powers_of_y[i - 1] * element);
        }
        let powers_of_y = into_iter!(powers_of_y)
            .map(|y| y.into_repr())
            .collect::<Vec<_>>();

        // <powers_of_y, omega>
        VariableBaseMSM::multi_scalar_mul(&self.0, &powers_of_y)
    }

    /// Scale the omega vector by the given `scalar`
    pub fn scaled(&self, scalar: &G::ScalarField) -> Vec<G::Projective> {
        let scalar_bigint = scalar.into_repr();
        iter!(self.0).map(|o| o.mul(scalar_bigint)).collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Coefficient (`c_i`) at the _ith_ position
    pub fn coefficient(&self, i: usize) -> &G {
        &self.0[i]
    }

    pub fn from(coeffs: Vec<G>) -> Self {
        Self(coeffs)
    }

    #[cfg(test)]
    /// Test function to check if Omega is generated correctly.
    pub(crate) fn check(
        additions: &[G::ScalarField],
        removals: &[G::ScalarField],
        element: &G::ScalarField,
        old_accumulator: &G,
        sk: &SecretKey<G::ScalarField>,
    ) {
        use ark_ff::Field;

        let v_AD = Poly_v_AD::eval_direct(additions, removals, &sk.0, element);
        let d_D_inv = Poly_d::eval_direct(removals, element).inverse().unwrap();

        let mut V_prime = old_accumulator.into_projective();
        V_prime *= v_AD * d_D_inv;

        let omega = Self::new(additions, removals, old_accumulator, sk);
        let mut y_omega_ip = omega.inner_product_with_powers_of_y(element);
        // <powers_of_y, omega> * 1/d_D(x)
        y_omega_ip *= d_D_inv;

        println!("y_omega_ip={}", y_omega_ip);
        println!("V_prime={}", V_prime);
        assert_eq!(V_prime, y_omega_ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ark_bls12_381::Bls12_381;
    use ark_ec::PairingEngine;
    use ark_ff::One;
    use ark_std::{rand::rngs::StdRng, rand::SeedableRng, UniformRand};
    use std::time::Instant;

    type Fr = <Bls12_381 as PairingEngine>::Fr;

    #[test]
    fn polys() {
        // Test evaluation of polynomials defined above
        let mut rng = StdRng::seed_from_u64(0u64);
        let updates = (0..100)
            .into_iter()
            .map(|_| Fr::rand(&mut rng))
            .collect::<Vec<Fr>>();

        let x = Fr::rand(&mut rng);

        let poly_d = Poly_d::generate(&updates);
        assert_eq!(Poly_d::eval_direct(&updates, &x), poly_d.eval(&x));

        // Polynomial with degree 1, i.e. a single update
        let single_update = vec![Fr::rand(&mut rng)];
        let poly_d_single = Poly_d::generate(&single_update);
        assert_eq!(
            Poly_d::eval_direct(&single_update, &x),
            single_update[0] - x
        );
        assert_eq!(poly_d_single.eval(&x), single_update[0] - x);

        assert_eq!(Poly_d::eval_direct(&vec![], &x), Fr::one());
        assert_eq!(Poly_d::generate(&vec![]).eval(&x), Fr::one());

        let alpha = Fr::rand(&mut rng);

        let poly_v_A = Poly_v_A::generate(&updates, &alpha);
        assert_eq!(
            Poly_v_A::eval_direct(&updates, &alpha, &x),
            poly_v_A.eval(&x)
        );
        assert_eq!(
            Poly_v_A::generate_without_memoize(&updates, &alpha),
            Poly_v_A::generate(&updates, &alpha)
        );
        assert_eq!(
            Poly_v_A::eval_direct_without_memoize(&updates, &alpha, &x),
            Poly_v_A::eval_direct(&updates, &alpha, &x)
        );
        assert_eq!(
            Poly_v_A::eval_direct_without_memoize(&vec![], &alpha, &x),
            Fr::zero()
        );
        assert_eq!(Poly_v_A::eval_direct(&vec![], &alpha, &x), Fr::zero());
        assert_eq!(
            Poly_v_A::generate_without_memoize(&vec![], &alpha).eval(&x),
            Fr::zero()
        );
        assert_eq!(Poly_v_A::generate(&vec![], &alpha).eval(&x), Fr::zero());

        let poly_v_D = Poly_v_D::generate(&updates, &alpha);
        assert_eq!(
            Poly_v_D::eval_direct(&updates, &alpha, &x),
            poly_v_D.eval(&x)
        );
        assert_eq!(
            Poly_v_D::generate_without_memoize(&updates, &alpha),
            Poly_v_D::generate(&updates, &alpha)
        );
        assert_eq!(
            Poly_v_D::eval_direct_without_memoize(&updates, &alpha, &x),
            Poly_v_D::eval_direct(&updates, &alpha, &x)
        );
        assert_eq!(
            Poly_v_D::eval_direct_without_memoize(&vec![], &alpha, &x),
            Fr::zero()
        );
        assert_eq!(Poly_v_D::eval_direct(&vec![], &alpha, &x), Fr::zero());
        assert_eq!(
            Poly_v_D::generate_without_memoize(&vec![], &alpha).eval(&x),
            Fr::zero()
        );
        assert_eq!(Poly_v_D::generate(&vec![], &alpha).eval(&x), Fr::zero());

        for i in vec![100, 70, 50, 40, 35, 20, 10, 7, 1, 0] {
            let updates_1 = (0..i)
                .into_iter()
                .map(|_| Fr::rand(&mut rng))
                .collect::<Vec<Fr>>();
            let poly_v_AD = Poly_v_AD::generate(&updates, &updates_1, &alpha);
            assert_eq!(
                Poly_v_AD::eval_direct(&updates, &updates_1, &alpha, &x),
                poly_v_AD.eval(&x)
            );
        }

        macro_rules! test_poly_time {
            ($count:ident, $updates:ident, $alpha:ident, $x:ident, $poly: ident, $name: expr) => {
                let start = Instant::now();
                let poly_m = $poly::generate(&$updates, &$alpha);
                let poly_gen_mem_time = start.elapsed();

                let start = Instant::now();
                let poly = $poly::generate_without_memoize(&$updates, &$alpha);
                let poly_gen_time = start.elapsed();

                assert_eq!(poly, poly_m);

                let start = Instant::now();
                let poly_eval_m = $poly::eval_direct(&$updates, &$alpha, &$x);
                let poly_eval_mem_time = start.elapsed();

                let start = Instant::now();
                let poly_eval = $poly::eval_direct_without_memoize(&$updates, &$alpha, &$x);
                let poly_eval_time = start.elapsed();

                assert_eq!(poly_eval_m, poly_eval);

                println!("For {} updates, {}::generates takes {:?} with memoization and {:?} without memoization", $count, $name, poly_gen_mem_time, poly_gen_time);
                println!("For {} updates, {}::eval_direct takes {:?} with memoization and {:?} without memoization", $count, $name, poly_eval_mem_time, poly_eval_time);
            }
        }

        for count in [100, 200, 400] {
            let updates = (0..count)
                .into_iter()
                .map(|_| Fr::rand(&mut rng))
                .collect::<Vec<Fr>>();
            let x = Fr::rand(&mut rng);

            test_poly_time!(count, updates, alpha, x, Poly_v_A, "Poly_v_A");
            test_poly_time!(count, updates, alpha, x, Poly_v_D, "Poly_v_D");
        }
    }
}
