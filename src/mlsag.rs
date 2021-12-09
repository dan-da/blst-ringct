use blstrs::{
    group::{ff::Field, Curve, Group},
    G1Affine, G1Projective, Scalar,
};
use bulletproofs::PedersenGens;
use rand_core::RngCore;
use tiny_keccak::{Hasher, Sha3};

use crate::{Error, Result, RevealedCommitment};

pub struct TrueInput {
    pub secret_key: Scalar,
    pub revealed_commitment: RevealedCommitment,
}

impl TrueInput {
    pub fn public_key(&self) -> G1Projective {
        G1Projective::generator() * self.secret_key
    }

    pub fn revealed_commitment(&self) -> &RevealedCommitment {
        &self.revealed_commitment
    }

    /// Computes the Key Image for this inputs keypair
    /// A key image is defined to be I = x * Hp(P)
    pub fn key_image(&self) -> G1Projective {
        crate::hash_to_curve(self.public_key()) * self.secret_key
    }

    /// Generate a pseudo-commitment to the input amount
    pub fn random_pseudo_commitment(&self, rng: impl RngCore) -> RevealedCommitment {
        RevealedCommitment::from_value(self.revealed_commitment.value, rng)
    }
}

#[derive(Clone, Copy)]
pub struct DecoyInput {
    pub public_key: G1Affine,
    pub commitment: G1Affine,
}

impl DecoyInput {
    pub fn public_key(&self) -> G1Affine {
        self.public_key
    }

    pub fn commitment(&self) -> G1Affine {
        self.commitment
    }
}

pub struct MlsagMaterial {
    pub true_input: TrueInput,
    pub decoy_inputs: Vec<DecoyInput>,
}

impl MlsagMaterial {
    pub fn count_inputs(&self) -> usize {
        self.decoy_inputs.len() + 1 // + 1 for the true_input
    }

    pub fn public_keys(&self, pi: usize) -> Vec<G1Affine> {
        let mut keys = Vec::from_iter(self.decoy_inputs.iter().map(DecoyInput::public_key));
        keys.insert(pi, self.true_input.public_key().to_affine());
        keys
    }

    pub fn commitments(&self, pi: usize, pc_gens: &PedersenGens) -> Vec<G1Affine> {
        let mut cs = Vec::from_iter(self.decoy_inputs.iter().map(DecoyInput::commitment));
        let true_commitment = self.true_input.revealed_commitment.commit(pc_gens);
        cs.insert(pi, true_commitment.to_affine());
        cs
    }

    pub fn sign(
        &self,
        msg: &[u8],
        revealed_pseudo_commitment: &RevealedCommitment,
        pc_gens: &PedersenGens,
        mut rng: impl RngCore,
    ) -> MlsagSignature {
        #[allow(non_snake_case)]
        let G1 = G1Projective::generator(); // TAI: should we use pedersen.G instead?

        // The position of the true input will be randomply placed amongst the decoys
        let pi = rng.next_u32() as usize % (self.decoy_inputs.len() + 1);

        let public_keys = self.public_keys(pi);
        let commitments = self.commitments(pi, pc_gens);

        let pseudo_commitment = revealed_pseudo_commitment.commit(pc_gens);

        // commitment = r G + a H -- a is the amount, r is the blinding factor
        // pseudo_commitment = v G + a H
        // commitment - pseudo_commitment = (r G + a H) - (v G + a H)
        //                                = (r - v) G + 0 H = (r - v) G

        let ring: Vec<(G1Affine, G1Affine)> = public_keys
            .into_iter()
            .zip(commitments)
            .map(|(pk, commitment)| (pk, (commitment - pseudo_commitment).to_affine()))
            .collect();

        let key_image = self.true_input.key_image();

        let alpha = (Scalar::random(&mut rng), Scalar::random(&mut rng));
        let mut r: Vec<(Scalar, Scalar)> = (0..ring.len())
            .map(|_| (Scalar::random(&mut rng), Scalar::random(&mut rng)))
            .collect();
        let mut c: Vec<Scalar> = (0..ring.len()).map(|_| Scalar::zero()).collect();

        c[(pi + 1) % ring.len()] = c_hash(
            msg,
            G1 * alpha.0,
            G1 * alpha.1,
            crate::hash_to_curve(ring[pi].0.into()) * alpha.0,
        );

        for offset in 1..ring.len() {
            let n = (pi + offset) % ring.len();
            c[(n + 1) % ring.len()] = c_hash(
                msg,
                G1 * r[n].0 + ring[n].0 * c[n],
                G1 * r[n].1 + ring[n].1 * c[n],
                crate::hash_to_curve(ring[n].0.into()) * r[n].0 + key_image * c[n],
            );
        }

        let secret_keys = (
            self.true_input.secret_key,
            self.true_input.revealed_commitment.blinding - revealed_pseudo_commitment.blinding,
        );

        r[pi] = (
            alpha.0 - c[pi] * secret_keys.0,
            alpha.1 - c[pi] * secret_keys.1,
        );

        #[cfg(test)]
        {
            // For our sanity, check a few identities
            assert_eq!(G1 * secret_keys.0, ring[pi].0.into());
            assert_eq!(G1 * secret_keys.1, ring[pi].1.into());
            assert_eq!(
                G1 * (alpha.0 - c[pi] * secret_keys.0),
                G1 * alpha.0 - G1 * (c[pi] * secret_keys.0)
            );
            assert_eq!(
                G1 * (alpha.1 - c[pi] * secret_keys.1),
                G1 * alpha.1 - G1 * (c[pi] * secret_keys.1)
            );
            assert_eq!(
                G1 * (alpha.0 - c[pi] * secret_keys.0) + ring[pi].0 * c[pi],
                G1 * alpha.0
            );
            assert_eq!(
                G1 * (alpha.1 - c[pi] * secret_keys.1) + ring[pi].1 * c[pi],
                G1 * alpha.1
            );
            assert_eq!(
                G1 * r[pi].0 + ring[pi].0 * c[pi],
                G1 * (alpha.0 - c[pi] * secret_keys.0) + ring[pi].0 * c[pi]
            );
            assert_eq!(
                G1 * r[pi].1 + ring[pi].1 * c[pi],
                G1 * (alpha.1 - c[pi] * secret_keys.1) + ring[pi].1 * c[pi]
            );
            assert_eq!(
                crate::hash_to_curve(ring[pi].0.into()) * r[pi].0 + key_image * c[pi],
                crate::hash_to_curve(ring[pi].0.into()) * (alpha.0 - c[pi] * secret_keys.0)
                    + key_image * c[pi]
            );
            assert_eq!(
                crate::hash_to_curve(ring[pi].1.into()) * r[pi].1 + key_image * c[pi],
                crate::hash_to_curve(ring[pi].1.into()) * (alpha.1 - c[pi] * secret_keys.1)
                    + key_image * c[pi]
            );

            assert_eq!(
                crate::hash_to_curve(ring[pi].0.into()) * secret_keys.0,
                key_image
            );
            assert_eq!(
                crate::hash_to_curve(ring[pi].0.into()) * r[pi].0 + key_image * c[pi],
                crate::hash_to_curve(ring[pi].0.into()) * (alpha.0 - c[pi] * secret_keys.0)
                    + key_image * c[pi]
            );
            assert_eq!(
                crate::hash_to_curve(ring[pi].1.into()) * r[pi].1 + key_image * c[pi],
                crate::hash_to_curve(ring[pi].1.into()) * (alpha.1 - c[pi] * secret_keys.1)
                    + key_image * c[pi]
            );
        }

        MlsagSignature {
            c0: c[0],
            r,
            key_image: key_image.to_affine(),
            ring,
            pseudo_commitment: pseudo_commitment.to_affine(),
        }
    }
}

#[derive(Debug)]
pub struct MlsagSignature {
    pub c0: Scalar,
    pub r: Vec<(Scalar, Scalar)>,
    pub key_image: G1Affine,
    pub ring: Vec<(G1Affine, G1Affine)>,
    pub pseudo_commitment: G1Affine,
}

impl MlsagSignature {
    pub fn pseudo_commitment(&self) -> G1Affine {
        self.pseudo_commitment
    }

    pub fn public_keys(&self) -> Vec<G1Affine> {
        self.ring.iter().map(|(pk, _)| *pk).collect()
    }

    pub fn verify(&self, msg: &[u8], public_commitments: &[G1Affine]) -> Result<()> {
        if self.ring.len() != public_commitments.len() {
            return Err(Error::ExpectedAPublicCommitmentsForEachRingEntry);
        }
        // Check that hidden commitments in the ring where computed with: C - C'
        for ((_, hidden_commitment), public_commitment) in self.ring.iter().zip(public_commitments)
        {
            if G1Projective::from(hidden_commitment)
                != public_commitment - G1Projective::from(self.pseudo_commitment)
            {
                return Err(Error::InvalidHiddenCommitmentInRing);
            }
        }

        #[allow(non_snake_case)]
        let G1 = G1Projective::generator();

        // Verify key image is in G
        if !bool::from(self.key_image.is_on_curve()) {
            // TODO: I don't think this is enough, we need to check that key_image is in the group as well
            println!("Key images not on curve");
            return Err(Error::KeyImageNotOnCurve);
        }

        let mut cprime = Vec::from_iter((0..self.ring.len()).map(|_| Scalar::zero()));
        cprime[0] = self.c0;

        for (n, keys) in self.ring.iter().enumerate() {
            cprime[(n + 1) % self.ring.len()] = c_hash(
                msg,
                G1 * self.r[n].0 + keys.0 * cprime[n],
                G1 * self.r[n].1 + keys.1 * cprime[n],
                crate::hash_to_curve(keys.0.into()) * self.r[n].0 + self.key_image * cprime[n],
            );
        }

        println!("c': {:#?}", cprime);
        if self.c0 != cprime[0] {
            Err(Error::InvalidRingSignature)
        } else {
            Ok(())
        }
    }
}

fn c_hash(msg: &[u8], l1: G1Projective, l2: G1Projective, r1: G1Projective) -> Scalar {
    hash_to_scalar(&[
        msg,
        &l1.to_compressed(),
        &l2.to_compressed(),
        &r1.to_compressed(),
    ])
}

/// Hashes given material to a Scalar, repeated hashing is used if a hash can not be interpreted as a Scalar
fn hash_to_scalar(material: &[&[u8]]) -> Scalar {
    let mut sha3 = Sha3::v256();
    for chunk in material {
        sha3.update(chunk);
    }
    let mut hash = [0u8; 32];
    sha3.finalize(&mut hash);
    loop {
        let s_opt = Scalar::from_bytes_le(&hash);
        if bool::from(s_opt.is_some()) {
            return s_opt.unwrap();
        }

        let mut sha3 = Sha3::v256();
        sha3.update(&hash);
        sha3.finalize(&mut hash);
    }
}
