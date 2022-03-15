use ark_ec::{AffineCurve, PairingEngine, ProjectiveCurve};
use ark_ff::{One, PrimeField, SquareRootField};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize, SerializationError};
use ark_std::{
    cfg_iter,
    io::{Read, Write},
    ops::AddAssign,
    rand::RngCore,
    vec::Vec,
    UniformRand,
};

#[cfg(feature = "parallel")]
use rayon::prelude::*;

use crate::error::Error;
use crate::setup::EncryptionGens;
use crate::utils::chunks_count;
use dock_crypto_utils::{
    ec::batch_normalize_projective_into_affine, msm::multiply_field_elems_with_same_group_elem,
};

/// Used to decrypt
#[derive(Clone, PartialEq, Eq, Debug, CanonicalSerialize, CanonicalDeserialize)]
pub struct SecretKey<F: PrimeField + SquareRootField>(pub F);

/// Used to encrypt, rerandomize and verify the encryption. Called "PK" in the paper.
#[derive(Clone, PartialEq, Eq, Debug, CanonicalSerialize, CanonicalDeserialize)]
pub struct EncryptionKey<E: PairingEngine> {
    /// `G * delta`
    pub X_0: E::G1Affine,
    /// `G * delta*s_i`
    pub X: Vec<E::G1Affine>,
    /// `G_i * t_{i+1}`
    pub Y: Vec<E::G1Affine>,
    /// `H * t_i`
    pub Z: Vec<E::G2Affine>,
    /// `(G*delta) * t_0 + (G*delta) * t_1*s_0 + (G*delta) * t_2*s_1 + .. (G*delta) * t_n*s_{n-1}`
    pub P_1: E::G1Affine,
    /// `(G*-gamma) * (1 + s_0 + s_1 + .. s_{n-1})`
    pub P_2: E::G1Affine,
}

/// Used to decrypt and verify decryption. Called "VK" in the paper.
#[derive(Clone, PartialEq, Eq, Debug, CanonicalSerialize, CanonicalDeserialize)]
pub struct DecryptionKey<E: PairingEngine> {
    /// `H * rho`
    pub V_0: E::G2Affine,
    /// `H * s_i*v_i`
    pub V_1: Vec<E::G2Affine>,
    /// `H * rho*v_i`
    pub V_2: Vec<E::G2Affine>,
}

impl<E: PairingEngine> EncryptionKey<E> {
    pub fn supported_chunks_count(&self) -> crate::Result<u8> {
        let n = self.X.len();
        if self.Y.len() != n {
            return Err(Error::MalformedEncryptionKey(self.Y.len(), n));
        }
        if self.Z.len() != (n + 1) {
            return Err(Error::MalformedEncryptionKey(self.Z.len(), n));
        }
        Ok(n as u8)
    }

    pub fn validate(&self) -> crate::Result<()> {
        self.supported_chunks_count()?;
        Ok(())
    }
}

impl<E: PairingEngine> DecryptionKey<E> {
    pub fn supported_chunks_count(&self) -> crate::Result<u8> {
        let n = self.V_1.len();
        if self.V_2.len() != n {
            return Err(Error::MalformedDecryptionKey(self.V_2.len(), n));
        }
        Ok(n as u8)
    }

    pub fn validate(&self) -> crate::Result<()> {
        self.supported_chunks_count()?;
        Ok(())
    }
}

/// Generate keys for encryption and decryption. The parameters `g_i`, `delta_g` and `gamma_g` are
/// shared with the SNARK SRS.
pub fn keygen<R: RngCore, E: PairingEngine>(
    rng: &mut R,
    chunk_bit_size: u8,
    gens: &EncryptionGens<E>,
    g_i: &[E::G1Affine],
    delta_g: &E::G1Affine,
    gamma_g: &E::G1Affine,
) -> crate::Result<(SecretKey<E::Fr>, EncryptionKey<E>, DecryptionKey<E>)> {
    let n = chunks_count::<E::Fr>(chunk_bit_size) as usize;
    if n > g_i.len() {
        return Err(Error::VectorShorterThanExpected(g_i.len(), n));
    }

    let rho = E::Fr::rand(rng);
    let s = (0..n).map(|_| E::Fr::rand(rng)).collect::<Vec<_>>();
    let t = (0..=n).map(|_| E::Fr::rand(rng)).collect::<Vec<_>>();
    let v = (0..n).map(|_| E::Fr::rand(rng)).collect::<Vec<_>>();

    let delta_g_proj = delta_g.into_projective();
    let t_repr = cfg_iter!(t).map(|t| t.into_repr()).collect::<Vec<_>>();

    let X = multiply_field_elems_with_same_group_elem(delta_g_proj.clone(), &s);
    let Y = (0..n).map(|i| g_i[i].mul(t_repr[i + 1])).collect();
    let Z = multiply_field_elems_with_same_group_elem(gens.H.into_projective(), &t);

    // P_1 = G*delta * (t_0 + \sum_{j in 0..n}(s_j * t_{j+1}))
    let P_1 = delta_g_proj.mul((t[0] + (0..n).map(|j| s[j] * t[j + 1]).sum::<E::Fr>()).into_repr());

    let ek = EncryptionKey {
        X_0: delta_g.clone(),
        X: batch_normalize_projective_into_affine(X),
        Y: batch_normalize_projective_into_affine(Y),
        Z: batch_normalize_projective_into_affine(Z),
        P_1: P_1.into_affine(),
        P_2: gamma_g
            .mul((E::Fr::one() + s.iter().sum::<E::Fr>()).into_repr())
            .into_affine(),
    };
    let V_0 = gens.H.mul(rho.into_repr());
    let V_2 = multiply_field_elems_with_same_group_elem(V_0.clone(), &v);
    let V_1 = multiply_field_elems_with_same_group_elem(
        gens.H.into_projective(),
        &s.into_iter()
            .zip(v.into_iter())
            .map(|(s_i, v_i)| s_i * v_i)
            .collect::<Vec<_>>(),
    );
    let dk = DecryptionKey {
        V_0: V_0.into_affine(),
        V_1: batch_normalize_projective_into_affine(V_1),
        V_2: batch_normalize_projective_into_affine(V_2),
    };
    Ok((SecretKey(rho), ek, dk))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use ark_bls12_381::Bls12_381;
    use ark_std::rand::prelude::StdRng;
    use ark_std::rand::SeedableRng;

    type Fr = <Bls12_381 as PairingEngine>::Fr;

    #[test]
    fn keygen_works() {
        fn check_keygen(chunk_bit_size: u8) {
            let mut rng = StdRng::seed_from_u64(0u64);
            let n = chunks_count::<Fr>(chunk_bit_size) as usize;
            let gens = EncryptionGens::<Bls12_381>::new_using_rng(&mut rng);
            let g_i = (0..n)
                .map(|_| <Bls12_381 as PairingEngine>::G1Projective::rand(&mut rng).into_affine())
                .collect::<Vec<_>>();
            let delta = Fr::rand(&mut rng);
            let gamma = Fr::rand(&mut rng);
            let g_delta = gens.G.mul(delta.into_repr()).into_affine();
            let g_gamma = gens.G.mul(gamma.into_repr()).into_affine();
            let (_, ek, dk) =
                keygen(&mut rng, chunk_bit_size, &gens, &g_i, &g_delta, &g_gamma).unwrap();
            assert_eq!(ek.X.len(), n);
            assert_eq!(ek.Y.len(), n);
            assert_eq!(ek.Z.len(), n + 1);
            assert_eq!(dk.V_1.len(), n);
            assert_eq!(dk.V_2.len(), n);
            assert_eq!(ek.supported_chunks_count().unwrap(), n as u8);
            assert_eq!(dk.supported_chunks_count().unwrap(), n as u8);
        }

        check_keygen(4);
        check_keygen(8);
    }
}
