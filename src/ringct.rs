use blstrs::{group::GroupEncoding, G1Affine, G1Projective, Scalar};
use bulletproofs::{BulletproofGens, PedersenGens, RangeProof};
use merlin::Transcript;
use rand_core::RngCore;

use crate::{Error, MlsagMaterial, MlsagSignature, Result, RevealedCommitment};
pub(crate) const RANGE_PROOF_BITS: usize = 64; // note: Range Proof max-bits is 64. allowed are: 8, 16, 32, 64 (only)
                                               //       This limits our amount field to 64 bits also.
pub(crate) const RANGE_PROOF_PARTIES: usize = 1; // The maximum number of parties that can produce an aggregated proof
pub(crate) const MERLIN_TRANSCRIPT_LABEL: &[u8] = b"BLST_RINGCT";

pub struct Output {
    pub public_key: G1Affine,
    pub amount: u64,
}

impl Output {
    pub fn public_key(&self) -> G1Affine {
        self.public_key
    }

    pub fn amount(&self) -> u64 {
        self.amount
    }

    /// Generate a commitment to the input amount
    pub fn random_commitment(&self, rng: impl RngCore) -> RevealedCommitment {
        RevealedCommitment::from_value(self.amount, rng)
    }
}

pub struct RingCtMaterial {
    inputs: Vec<MlsagMaterial>,
    outputs: Vec<Output>,
}

impl RingCtMaterial {
    pub fn sign(
        &self,
        pc_gens: &PedersenGens,
        mut rng: impl RngCore,
    ) -> Result<(Vec<u8>, RingCtTransaction, Vec<RevealedCommitment>)> {
        let bp_gens = BulletproofGens::new(RANGE_PROOF_BITS, RANGE_PROOF_PARTIES);
        let mut prover_ts = Transcript::new(MERLIN_TRANSCRIPT_LABEL);

        // We need to gather a bunch of things for our message to sign.
        //   All public keys in all rings
        //   All key-images,
        //   All PseudoCommitments
        //   All output commitments
        //   All output range proofs

        // All public keys in all rings
        let true_input_public_keys: Vec<G1Projective> = self
            .inputs
            .iter()
            .map(|m| m.true_input.public_key())
            .collect();
        let decoy_inputs_public_keys: Vec<G1Affine> = self
            .inputs
            .iter()
            .flat_map(|m| {
                m.decoy_inputs
                    .iter()
                    .map(|d| d.public_key())
                    .collect::<Vec<G1Affine>>()
            })
            .collect();

        // All key-images,
        let true_input_key_images: Vec<G1Projective> = self
            .inputs
            .iter()
            .map(|m| m.true_input.key_image())
            .collect();

        // All PseudoCommitments.
        let revealed_pseudo_commitments: Vec<RevealedCommitment> = self
            .inputs
            .iter()
            .map(|m| m.true_input.random_pseudo_commitment(&mut rng))
            .collect();

        // All output commitments
        let revealed_output_commitments = {
            let mut output_commitments: Vec<RevealedCommitment> = self
                .outputs
                .iter()
                .map(|out| out.random_commitment(&mut rng))
                .take(self.outputs.len() - 1)
                .collect();

            let input_sum: Scalar = revealed_pseudo_commitments
                .iter()
                .map(RevealedCommitment::blinding)
                .sum();
            let output_sum: Scalar = output_commitments
                .iter()
                .map(RevealedCommitment::blinding)
                .sum();

            let output_blinding_correction = input_sum - output_sum;

            if let Some(last_output) = self.outputs.last() {
                output_commitments.push(RevealedCommitment {
                    value: last_output.amount,
                    blinding: output_blinding_correction,
                });
            } else {
                panic!("Expected at least one output")
            }

            output_commitments
        };

        // All output range proofs
        let outputs: Vec<OutputProof> = revealed_output_commitments
            .iter()
            .map(|revealed_commitment| {
                let (range_proof, commitment) = RangeProof::prove_single(
                    &bp_gens,
                    pc_gens,
                    &mut prover_ts,
                    revealed_commitment.value,
                    &revealed_commitment.blinding,
                    RANGE_PROOF_BITS,
                )?;

                Ok(OutputProof {
                    range_proof,
                    commitment,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        // Generate message to sign.
        let mut msg: Vec<u8> = Default::default();
        for t in true_input_public_keys.iter() {
            msg.extend(t.to_bytes().as_ref());
        }
        for d in decoy_inputs_public_keys.iter() {
            msg.extend(d.to_bytes().as_ref());
        }
        for t in true_input_key_images.iter() {
            msg.extend(t.to_bytes().as_ref());
        }
        for r in revealed_pseudo_commitments.iter() {
            msg.extend(r.to_bytes());
        }
        for o in revealed_output_commitments.iter() {
            msg.extend(o.to_bytes());
        }
        for o in outputs.iter() {
            msg.extend(o.to_bytes());
        }

        // We create a ring signature for each input
        let mlsags: Vec<MlsagSignature> = self
            .inputs
            .iter()
            .zip(revealed_pseudo_commitments.iter())
            .map(|(m, r)| m.sign(&msg, r, pc_gens, &mut rng))
            .collect();

        Ok((
            msg,
            RingCtTransaction { mlsags, outputs },
            revealed_output_commitments,
        ))
    }
}

#[derive(Debug)]
pub struct OutputProof {
    range_proof: RangeProof,
    commitment: G1Affine,
}

impl OutputProof {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v: Vec<u8> = Default::default();
        v.extend(&self.range_proof.to_bytes());
        v.extend(self.commitment.to_bytes().as_ref());
        v
    }

    pub fn range_proof(&self) -> &RangeProof {
        &self.range_proof
    }

    pub fn commitment(&self) -> G1Affine {
        self.commitment
    }
}

#[derive(Debug)]
pub struct RingCtTransaction {
    pub mlsags: Vec<MlsagSignature>,
    pub outputs: Vec<OutputProof>,
}

impl RingCtTransaction {
    pub fn verify(&self, msg: &[u8], public_commitments_per_ring: &[Vec<G1Affine>]) -> Result<()> {
        for (mlsag, public_commitments) in self.mlsags.iter().zip(public_commitments_per_ring) {
            mlsag.verify(msg, public_commitments)?
        }

        let pc_gens = PedersenGens::default();
        let bp_gens = BulletproofGens::new(RANGE_PROOF_BITS, RANGE_PROOF_PARTIES);
        let mut prover_ts = Transcript::new(MERLIN_TRANSCRIPT_LABEL);

        for output in self.outputs.iter() {
            // Verification requires a transcript with identical initial state:
            output.range_proof.verify_single(
                &bp_gens,
                &pc_gens,
                &mut prover_ts,
                &output.commitment,
                RANGE_PROOF_BITS,
            )?;
        }

        let input_sum: G1Projective = self
            .mlsags
            .iter()
            .map(MlsagSignature::pseudo_commitment)
            .map(G1Projective::from)
            .sum();
        let output_sum: G1Projective = self
            .outputs
            .iter()
            .map(OutputProof::commitment)
            .map(G1Projective::from)
            .sum();

        if input_sum != output_sum {
            Err(Error::InputPseudoCommitmentsDoNotSumToOutputCommitments)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use blstrs::group::{ff::Field, Curve, Group};
    use rand_core::OsRng;

    use crate::{DecoyInput, MlsagMaterial, TrueInput};

    use super::*;

    #[derive(Default)]
    struct TestLedger {
        commitments: BTreeMap<[u8; 48], G1Affine>, // Compressed public keys -> Commitments
    }

    impl TestLedger {
        fn log(&mut self, public_key: impl Into<G1Affine>, commitment: impl Into<G1Affine>) {
            self.commitments
                .insert(public_key.into().to_compressed(), commitment.into());
        }

        fn lookup(&self, public_key: impl Into<G1Affine>) -> Option<G1Affine> {
            self.commitments
                .get(&public_key.into().to_compressed())
                .copied()
        }

        fn fetch_decoys(&self, n: usize, exclude: &[G1Projective]) -> Vec<DecoyInput> {
            let exclude_set = BTreeSet::from_iter(exclude.iter().map(G1Projective::to_compressed));

            self.commitments
                .iter()
                .filter(|(pk, _)| !exclude_set.contains(*pk))
                .map(|(pk, c)| DecoyInput {
                    public_key: G1Affine::from_compressed(pk).unwrap(),
                    commitment: *c,
                })
                .take(n)
                .collect()
        }
    }

    #[test]
    fn test_ringct_sign() {
        let mut rng = OsRng::default();
        let pc_gens = PedersenGens::default();

        let true_input = TrueInput {
            secret_key: Scalar::random(&mut rng),
            revealed_commitment: RevealedCommitment {
                value: 3,
                blinding: 5.into(),
            },
        };

        let mut ledger = TestLedger::default();
        ledger.log(
            true_input.public_key(),
            true_input.revealed_commitment.commit(&pc_gens),
        );
        ledger.log(
            G1Projective::random(&mut rng),
            G1Projective::random(&mut rng),
        );
        ledger.log(
            G1Projective::random(&mut rng),
            G1Projective::random(&mut rng),
        );

        let decoy_inputs = ledger.fetch_decoys(2, &[true_input.public_key()]);

        let ring_ct = RingCtMaterial {
            inputs: vec![MlsagMaterial {
                true_input,
                decoy_inputs,
            }],
            outputs: vec![Output {
                public_key: G1Projective::random(&mut rng).to_affine(),
                amount: 3,
            }],
        };

        let (msg, signed_tx, _revealed_output_commitments) = ring_ct
            .sign(&pc_gens, rng)
            .expect("Failed to sign transaction");

        let public_commitments = Vec::from_iter(signed_tx.mlsags.iter().map(|mlsag| {
            Vec::from_iter(
                mlsag
                    .public_keys()
                    .into_iter()
                    .map(|pk| ledger.lookup(pk).unwrap()),
            )
        }));

        assert!(signed_tx.verify(&msg, &public_commitments).is_ok());
    }
}
