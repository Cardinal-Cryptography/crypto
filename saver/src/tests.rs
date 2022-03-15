use crate::circuit::BitsizeCheckCircuit;
use crate::commitment::ChunkedCommitment;
use crate::encryption::Encryption;
use crate::saver_groth16::{create_proof, verify_proof};
use crate::setup::{setup_for_groth16, ChunkedCommitmentGens, EncryptionGens};
use crate::utils::decompose;
use ark_bls12_381::{Bls12_381, G1Affine};
use ark_ec::{AffineCurve, PairingEngine, ProjectiveCurve};
use ark_ff::PrimeField;
use ark_groth16::prepare_verifying_key;
use ark_std::rand::prelude::StdRng;
use ark_std::rand::{RngCore, SeedableRng};
use ark_std::UniformRand;
use bbs_plus::setup::{KeypairG2, SignatureParamsG1};
use bbs_plus::signature::SignatureG1;
use blake2::Blake2b;
use proof_system::prelude::{
    EqualWitnesses, MetaStatement, MetaStatements, Proof, ProofSpec, Statement, Statements,
    Witness, WitnessRef, Witnesses,
};
use proof_system::statement::{
    PedersenCommitment as PedersenCommitmentStmt, PoKBBSSignatureG1 as PoKSignatureBBSG1Stmt,
};
use proof_system::witness::PoKBBSSignatureG1 as PoKSignatureBBSG1Wit;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Add;
use std::time::Instant;

type Fr = <Bls12_381 as PairingEngine>::Fr;
type ProofG1 = Proof<Bls12_381, G1Affine, Fr, Blake2b>;

fn sig_setup<R: RngCore>(
    rng: &mut R,
    message_count: usize,
) -> (
    Vec<Fr>,
    SignatureParamsG1<Bls12_381>,
    KeypairG2<Bls12_381>,
    SignatureG1<Bls12_381>,
) {
    let messages: Vec<Fr> = (0..message_count)
        .into_iter()
        .map(|_| Fr::rand(rng))
        .collect();
    let params = SignatureParamsG1::<Bls12_381>::generate_using_rng(rng, message_count);
    let keypair = KeypairG2::<Bls12_381>::generate_using_rng(rng, &params);
    let sig = SignatureG1::<Bls12_381>::new(rng, &messages, &keypair.secret_key, &params).unwrap();
    sig.verify(&messages, &keypair.public_key, &params).unwrap();
    (messages, params, keypair, sig)
}

#[test]
fn bbs_plus_verifiably_encrypt_message() {
    // Given a BBS+ signature with one of the messages as user id, verifiably encrypt the user id for an entity
    // called decryptor which can decrypt the user id. But the verifier can't decrypt, only verify

    fn check(chunk_bit_size: u8) {
        let mut rng = StdRng::seed_from_u64(0u64);
        // Prover has the BBS+ signature
        let message_count = 10;
        let (messages, sig_params, keypair, sig) = sig_setup(&mut rng, message_count);
        sig.verify(&messages, &keypair.public_key, &sig_params)
            .unwrap();

        // User id at message index `user_id_idx`
        let user_id_idx = 1;

        // Decryptor creates public parameters
        let enc_gens = EncryptionGens::<Bls12_381>::new_using_rng(&mut rng);

        // For transformed commitment to the message
        let chunked_comm_gens =
            ChunkedCommitmentGens::<<Bls12_381 as PairingEngine>::G1Affine>::new_using_rng(
                &mut rng,
            );

        let (snark_srs, sk, ek, dk) =
            setup_for_groth16(&mut rng, chunk_bit_size, &enc_gens).unwrap();
        let chunks_count = ek.supported_chunks_count().unwrap();

        // User encrypts
        let (ct, r) = Encryption::encrypt_given_snark_vk(
            &mut rng,
            &messages[user_id_idx],
            &ek,
            &snark_srs.pk.vk,
            chunk_bit_size,
        )
        .unwrap();

        // User creates proof
        let decomposed_message = decompose(&messages[user_id_idx], chunk_bit_size)
            .unwrap()
            .into_iter()
            .map(|m| Fr::from(m as u64))
            .collect::<Vec<_>>();

        let circuit =
            BitsizeCheckCircuit::new(chunk_bit_size, None, Some(decomposed_message.clone()), true);

        let start = Instant::now();

        let blinding = Fr::rand(&mut rng);
        let comm_single = chunked_comm_gens
            .G
            .mul(messages[user_id_idx].into_repr())
            .add(&(chunked_comm_gens.H.mul(blinding.into_repr())));
        let comm_chunks = ChunkedCommitment::<<Bls12_381 as PairingEngine>::G1Affine>::new(
            &messages[user_id_idx],
            &blinding,
            chunk_bit_size,
            &chunked_comm_gens,
        )
        .unwrap()
        .0;

        let bases_comm_chunks =
            ChunkedCommitment::<<Bls12_381 as PairingEngine>::G1Affine>::commitment_key(
                &chunked_comm_gens,
                chunk_bit_size,
                1 << chunk_bit_size,
            );
        let mut wit_comm_chunks = decomposed_message.clone();
        wit_comm_chunks.push(blinding.clone());

        let mut bases_comm_ct = ek.Y.clone();
        bases_comm_ct.push(ek.P_1.clone());
        let mut wit_comm_ct = decomposed_message.clone();
        wit_comm_ct.push(r.clone());

        let mut statements = Statements::new();
        statements.add(Statement::PoKBBSSignatureG1(PoKSignatureBBSG1Stmt {
            params: sig_params.clone(),
            public_key: keypair.public_key.clone(),
            revealed_messages: BTreeMap::new(),
        }));
        statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
            bases: vec![chunked_comm_gens.G, chunked_comm_gens.H],
            commitment: comm_single.into_affine(),
        }));
        statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
            bases: bases_comm_chunks.clone(),
            commitment: comm_chunks.clone(),
        }));
        statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
            bases: bases_comm_ct.clone(),
            commitment: ct.commitment.clone(),
        }));

        let mut meta_statements = MetaStatements::new();
        meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
            vec![(0, user_id_idx), (1, 0)] // 0th statement's `user_id_idx`th witness is equal to 1st statement's 0th witness
                .into_iter()
                .collect::<BTreeSet<WitnessRef>>(),
        )));
        for i in 0..chunks_count as usize {
            meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
                vec![(2, i), (3, i)]
                    .into_iter()
                    .collect::<BTreeSet<WitnessRef>>(),
            )));
        }

        let proof_spec = ProofSpec {
            statements: statements.clone(),
            meta_statements: meta_statements.clone(),
            context: None,
        };

        let mut witnesses = Witnesses::new();
        witnesses.add(PoKSignatureBBSG1Wit::new_as_witness(
            sig.clone(),
            messages
                .clone()
                .into_iter()
                .enumerate()
                .map(|t| t)
                .collect(),
        ));
        witnesses.add(Witness::PedersenCommitment(vec![
            messages[user_id_idx].clone(),
            blinding,
        ]));
        witnesses.add(Witness::PedersenCommitment(wit_comm_chunks));
        witnesses.add(Witness::PedersenCommitment(wit_comm_ct));

        println!("Timing for {}-bit chunks", chunk_bit_size);
        let proof = ProofG1::new(&mut rng, proof_spec.clone(), witnesses.clone(), None).unwrap();
        println!("Time taken to create proof {:?}", start.elapsed());

        // Verifies the proof
        let start = Instant::now();
        proof.verify(proof_spec, None).unwrap();
        println!("Time taken to verify proof {:?}", start.elapsed());

        let start = Instant::now();
        let proof = create_proof(circuit, r, &snark_srs, &ek, &mut rng).unwrap();
        println!("Time taken to create Groth16 proof {:?}", start.elapsed());

        let start = Instant::now();
        ct.verify_commitment(&ek, &enc_gens).unwrap();
        println!(
            "Time taken to verify ciphertext commitment {:?}",
            start.elapsed()
        );

        let start = Instant::now();
        let pvk = prepare_verifying_key::<Bls12_381>(&snark_srs.pk.vk);
        assert!(verify_proof(&pvk, &proof, &ct).unwrap());
        println!("Time taken to verify Groth16 proof {:?}", start.elapsed());

        // Decryptor decrypts
        let (decrypted_message, nu) = ct
            .decrypt_given_groth16_vk(&sk, &dk, &snark_srs.pk.vk, chunk_bit_size)
            .unwrap();
        assert_eq!(decrypted_message, messages[user_id_idx]);
        ct.verify_decryption_given_groth16_vk(
            &decrypted_message,
            &nu,
            chunk_bit_size,
            &dk,
            &snark_srs.pk.vk,
            &enc_gens,
        )
        .unwrap();
    }
    check(4);
    check(8);
}

#[test]
fn bbs_plus_verifiably_encrypt_many_messages() {
    let mut rng = StdRng::seed_from_u64(0u64);
    // Prover has the BBS+ signature
    let message_count = 10;
    let (messages, sig_params, keypair, sig) = sig_setup(&mut rng, message_count);
    sig.verify(&messages, &keypair.public_key, &sig_params)
        .unwrap();

    let m_idx_1 = 1;
    let m_idx_2 = 3;
    let m_idx_3 = 7;

    // Decryptor creates public parameters
    let enc_gens = EncryptionGens::<Bls12_381>::new_using_rng(&mut rng);

    // For transformed commitment to the message
    let chunked_comm_gens =
        ChunkedCommitmentGens::<<Bls12_381 as PairingEngine>::G1Affine>::new_using_rng(&mut rng);

    let chunk_bit_size = 8;

    let (snark_srs, sk, ek, dk) = setup_for_groth16(&mut rng, chunk_bit_size, &enc_gens).unwrap();
    let chunks_count = ek.supported_chunks_count().unwrap();

    // User encrypts
    let (ct_1, r_1, proof_1) = Encryption::encrypt_with_proof(
        &mut rng,
        &messages[m_idx_1],
        &ek,
        &snark_srs,
        chunk_bit_size,
    )
    .unwrap();
    let (ct_2, r_2, proof_2) = Encryption::encrypt_with_proof(
        &mut rng,
        &messages[m_idx_2],
        &ek,
        &snark_srs,
        chunk_bit_size,
    )
    .unwrap();
    let (ct_3, r_3, proof_3) = Encryption::encrypt_with_proof(
        &mut rng,
        &messages[m_idx_3],
        &ek,
        &snark_srs,
        chunk_bit_size,
    )
    .unwrap();

    let decomposed_message_1 = decompose(&messages[m_idx_1], chunk_bit_size)
        .unwrap()
        .into_iter()
        .map(|m| Fr::from(m as u64))
        .collect::<Vec<_>>();
    let blinding_1 = Fr::rand(&mut rng);
    let comm_single_1 = chunked_comm_gens
        .G
        .mul(messages[m_idx_1].into_repr())
        .add(&(chunked_comm_gens.H.mul(blinding_1.into_repr())));
    let comm_chunks_1 = ChunkedCommitment::<<Bls12_381 as PairingEngine>::G1Affine>::new(
        &messages[m_idx_1],
        &blinding_1,
        chunk_bit_size,
        &chunked_comm_gens,
    )
    .unwrap()
    .0;

    let decomposed_message_2 = decompose(&messages[m_idx_2], chunk_bit_size)
        .unwrap()
        .into_iter()
        .map(|m| Fr::from(m as u64))
        .collect::<Vec<_>>();
    let blinding_2 = Fr::rand(&mut rng);
    let comm_single_2 = chunked_comm_gens
        .G
        .mul(messages[m_idx_2].into_repr())
        .add(&(chunked_comm_gens.H.mul(blinding_2.into_repr())));
    let comm_chunks_2 = ChunkedCommitment::<<Bls12_381 as PairingEngine>::G1Affine>::new(
        &messages[m_idx_2],
        &blinding_2,
        chunk_bit_size,
        &chunked_comm_gens,
    )
    .unwrap()
    .0;

    let decomposed_message_3 = decompose(&messages[m_idx_3], chunk_bit_size)
        .unwrap()
        .into_iter()
        .map(|m| Fr::from(m as u64))
        .collect::<Vec<_>>();
    let blinding_3 = Fr::rand(&mut rng);
    let comm_single_3 = chunked_comm_gens
        .G
        .mul(messages[m_idx_3].into_repr())
        .add(&(chunked_comm_gens.H.mul(blinding_3.into_repr())));
    let comm_chunks_3 = ChunkedCommitment::<<Bls12_381 as PairingEngine>::G1Affine>::new(
        &messages[m_idx_3],
        &blinding_3,
        chunk_bit_size,
        &chunked_comm_gens,
    )
    .unwrap()
    .0;

    let bases_comm_chunks =
        ChunkedCommitment::<<Bls12_381 as PairingEngine>::G1Affine>::commitment_key(
            &chunked_comm_gens,
            chunk_bit_size,
            1 << chunk_bit_size,
        );
    let mut bases_comm_ct = ek.Y.clone();
    bases_comm_ct.push(ek.P_1.clone());

    let mut wit_comm_chunks_1 = decomposed_message_1.clone();
    wit_comm_chunks_1.push(blinding_1.clone());
    let mut wit_comm_ct_1 = decomposed_message_1.clone();
    wit_comm_ct_1.push(r_1.clone());

    let mut wit_comm_chunks_2 = decomposed_message_2.clone();
    wit_comm_chunks_2.push(blinding_2.clone());
    let mut wit_comm_ct_2 = decomposed_message_2.clone();
    wit_comm_ct_2.push(r_2.clone());

    let mut wit_comm_chunks_3 = decomposed_message_3.clone();
    wit_comm_chunks_3.push(blinding_3.clone());
    let mut wit_comm_ct_3 = decomposed_message_3.clone();
    wit_comm_ct_3.push(r_3.clone());

    let mut statements = Statements::new();
    statements.add(Statement::PoKBBSSignatureG1(PoKSignatureBBSG1Stmt {
        params: sig_params.clone(),
        public_key: keypair.public_key.clone(),
        revealed_messages: BTreeMap::new(),
    }));

    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: vec![chunked_comm_gens.G, chunked_comm_gens.H],
        commitment: comm_single_1.into_affine(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: bases_comm_chunks.clone(),
        commitment: comm_chunks_1.clone(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: bases_comm_ct.clone(),
        commitment: ct_1.commitment.clone(),
    }));

    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: vec![chunked_comm_gens.G, chunked_comm_gens.H],
        commitment: comm_single_2.into_affine(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: bases_comm_chunks.clone(),
        commitment: comm_chunks_2.clone(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: bases_comm_ct.clone(),
        commitment: ct_2.commitment.clone(),
    }));

    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: vec![chunked_comm_gens.G, chunked_comm_gens.H],
        commitment: comm_single_3.into_affine(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: bases_comm_chunks.clone(),
        commitment: comm_chunks_3.clone(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: bases_comm_ct.clone(),
        commitment: ct_3.commitment.clone(),
    }));

    let mut meta_statements = MetaStatements::new();
    meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
        vec![(0, m_idx_1), (1, 0)]
            .into_iter()
            .collect::<BTreeSet<WitnessRef>>(),
    )));
    meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
        vec![(0, m_idx_2), (4, 0)]
            .into_iter()
            .collect::<BTreeSet<WitnessRef>>(),
    )));
    meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
        vec![(0, m_idx_3), (7, 0)]
            .into_iter()
            .collect::<BTreeSet<WitnessRef>>(),
    )));

    for i in 0..chunks_count as usize {
        meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
            vec![(2, i), (3, i)]
                .into_iter()
                .collect::<BTreeSet<WitnessRef>>(),
        )));
        meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
            vec![(5, i), (6, i)]
                .into_iter()
                .collect::<BTreeSet<WitnessRef>>(),
        )));
        meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
            vec![(8, i), (9, i)]
                .into_iter()
                .collect::<BTreeSet<WitnessRef>>(),
        )));
    }

    let proof_spec = ProofSpec {
        statements: statements.clone(),
        meta_statements: meta_statements.clone(),
        context: None,
    };

    let mut witnesses = Witnesses::new();
    witnesses.add(PoKSignatureBBSG1Wit::new_as_witness(
        sig.clone(),
        messages
            .clone()
            .into_iter()
            .enumerate()
            .map(|t| t)
            .collect(),
    ));
    witnesses.add(Witness::PedersenCommitment(vec![
        messages[m_idx_1].clone(),
        blinding_1,
    ]));
    witnesses.add(Witness::PedersenCommitment(wit_comm_chunks_1));
    witnesses.add(Witness::PedersenCommitment(wit_comm_ct_1));

    witnesses.add(Witness::PedersenCommitment(vec![
        messages[m_idx_2].clone(),
        blinding_2,
    ]));
    witnesses.add(Witness::PedersenCommitment(wit_comm_chunks_2));
    witnesses.add(Witness::PedersenCommitment(wit_comm_ct_2));

    witnesses.add(Witness::PedersenCommitment(vec![
        messages[m_idx_3].clone(),
        blinding_3,
    ]));
    witnesses.add(Witness::PedersenCommitment(wit_comm_chunks_3));
    witnesses.add(Witness::PedersenCommitment(wit_comm_ct_3));

    let proof = ProofG1::new(&mut rng, proof_spec.clone(), witnesses.clone(), None).unwrap();

    let pvk = prepare_verifying_key::<Bls12_381>(&snark_srs.pk.vk);
    proof.verify(proof_spec, None).unwrap();
    ct_1.verify_commitment_and_proof(&proof_1, &pvk, &ek, &enc_gens)
        .unwrap();
    ct_2.verify_commitment_and_proof(&proof_2, &pvk, &ek, &enc_gens)
        .unwrap();
    ct_3.verify_commitment_and_proof(&proof_3, &pvk, &ek, &enc_gens)
        .unwrap();

    let (decrypted_message_1, nu_1) = ct_1
        .decrypt_given_groth16_vk(&sk, &dk, &snark_srs.pk.vk, chunk_bit_size)
        .unwrap();
    assert_eq!(decrypted_message_1, messages[m_idx_1]);
    ct_1.verify_decryption_given_groth16_vk(
        &decrypted_message_1,
        &nu_1,
        chunk_bit_size,
        &dk,
        &snark_srs.pk.vk,
        &enc_gens,
    )
    .unwrap();

    let (decrypted_message_2, nu_2) = ct_2
        .decrypt_given_groth16_vk(&sk, &dk, &snark_srs.pk.vk, chunk_bit_size)
        .unwrap();
    assert_eq!(decrypted_message_2, messages[m_idx_2]);
    ct_2.verify_decryption_given_groth16_vk(
        &decrypted_message_2,
        &nu_2,
        chunk_bit_size,
        &dk,
        &snark_srs.pk.vk,
        &enc_gens,
    )
    .unwrap();

    let (decrypted_message_3, nu_3) = ct_3
        .decrypt_given_groth16_vk(&sk, &dk, &snark_srs.pk.vk, chunk_bit_size)
        .unwrap();
    assert_eq!(decrypted_message_3, messages[m_idx_3]);
    ct_3.verify_decryption_given_groth16_vk(
        &decrypted_message_3,
        &nu_3,
        chunk_bit_size,
        &dk,
        &snark_srs.pk.vk,
        &enc_gens,
    )
    .unwrap();
}

#[test]
fn bbs_plus_verifiably_encrypt_message_from_2_sigs() {
    // Given 2 BBS+ signatures with one of the message as user id, verifiably encrypt the user ids for an entity
    // called decryptor which can decrypt the user id. But the verifier can't decrypt, only verify

    let mut rng = StdRng::seed_from_u64(0u64);

    // Prover has the BBS+ signatures
    let message_count_1 = 5;
    let (messages_1, sig_params_1, keypair_1, sig_1) = sig_setup(&mut rng, message_count_1);
    sig_1
        .verify(&messages_1, &keypair_1.public_key, &sig_params_1)
        .unwrap();

    let message_count_2 = 8;
    let (messages_2, sig_params_2, keypair_2, sig_2) = sig_setup(&mut rng, message_count_2);
    sig_2
        .verify(&messages_2, &keypair_2.public_key, &sig_params_2)
        .unwrap();

    // User id at message index `user_id_idx`
    let user_id_idx = 1;

    let enc_gens = EncryptionGens::<Bls12_381>::new_using_rng(&mut rng);

    // For transformed commitment to the message
    let chunked_comm_gens =
        ChunkedCommitmentGens::<<Bls12_381 as PairingEngine>::G1Affine>::new_using_rng(&mut rng);

    let chunk_bit_size = 8;

    let (snark_srs, sk, ek, dk) = setup_for_groth16(&mut rng, chunk_bit_size, &enc_gens).unwrap();
    let chunks_count = ek.supported_chunks_count().unwrap();

    // User encrypts 1st user id
    let (ct_1, r_1) = Encryption::encrypt_given_snark_vk(
        &mut rng,
        &messages_1[user_id_idx],
        &ek,
        &snark_srs.pk.vk,
        chunk_bit_size,
    )
    .unwrap();

    // User encrypts 2nd user id
    let (ct_2, r_2) = Encryption::encrypt_given_snark_vk(
        &mut rng,
        &messages_2[user_id_idx],
        &ek,
        &&snark_srs.pk.vk,
        chunk_bit_size,
    )
    .unwrap();

    // User creates proof
    let decomposed_message_1 = decompose(&messages_1[user_id_idx], chunk_bit_size)
        .unwrap()
        .into_iter()
        .map(|m| Fr::from(m as u64))
        .collect::<Vec<_>>();

    // User creates proof
    let decomposed_message_2 = decompose(&messages_2[user_id_idx], chunk_bit_size)
        .unwrap()
        .into_iter()
        .map(|m| Fr::from(m as u64))
        .collect::<Vec<_>>();

    let circuit_1 = BitsizeCheckCircuit::new(
        chunk_bit_size,
        None,
        Some(decomposed_message_1.clone()),
        true,
    );
    let circuit_2 = BitsizeCheckCircuit::new(
        chunk_bit_size,
        None,
        Some(decomposed_message_2.clone()),
        true,
    );

    let start = Instant::now();
    let blinding_1 = Fr::rand(&mut rng);
    let blinding_2 = Fr::rand(&mut rng);

    let comm_single_1 = chunked_comm_gens
        .G
        .mul(messages_1[user_id_idx].into_repr())
        .add(&(chunked_comm_gens.H.mul(blinding_1.into_repr())));
    let comm_chunks_1 = ChunkedCommitment::<<Bls12_381 as PairingEngine>::G1Affine>::new(
        &messages_1[user_id_idx],
        &blinding_1,
        chunk_bit_size,
        &chunked_comm_gens,
    )
    .unwrap()
    .0;

    let comm_single_2 = chunked_comm_gens
        .G
        .mul(messages_2[user_id_idx].into_repr())
        .add(&(chunked_comm_gens.H.mul(blinding_2.into_repr())));
    let comm_chunks_2 = ChunkedCommitment::<<Bls12_381 as PairingEngine>::G1Affine>::new(
        &messages_2[user_id_idx],
        &blinding_2,
        chunk_bit_size,
        &chunked_comm_gens,
    )
    .unwrap()
    .0;

    let bases_comm_chunks =
        ChunkedCommitment::<<Bls12_381 as PairingEngine>::G1Affine>::commitment_key(
            &chunked_comm_gens,
            chunk_bit_size,
            1 << chunk_bit_size,
        );

    let mut wit_comm_chunks_1 = decomposed_message_1.clone();
    wit_comm_chunks_1.push(blinding_1.clone());

    let mut wit_comm_chunks_2 = decomposed_message_2.clone();
    wit_comm_chunks_2.push(blinding_2.clone());

    let mut bases_comm_ct = ek.Y.clone();
    bases_comm_ct.push(ek.P_1.clone());

    let mut wit_comm_ct_1 = decomposed_message_1.clone();
    wit_comm_ct_1.push(r_1.clone());

    let mut wit_comm_ct_2 = decomposed_message_2.clone();
    wit_comm_ct_2.push(r_2.clone());

    let mut statements = Statements::new();
    // For 1st sig
    statements.add(Statement::PoKBBSSignatureG1(PoKSignatureBBSG1Stmt {
        params: sig_params_1.clone(),
        public_key: keypair_1.public_key.clone(),
        revealed_messages: BTreeMap::new(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: vec![chunked_comm_gens.G, chunked_comm_gens.H],
        commitment: comm_single_1.into_affine(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: bases_comm_chunks.clone(),
        commitment: comm_chunks_1.clone(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: bases_comm_ct.clone(),
        commitment: ct_1.commitment.clone(),
    }));

    // For 2nd sig
    statements.add(Statement::PoKBBSSignatureG1(PoKSignatureBBSG1Stmt {
        params: sig_params_2.clone(),
        public_key: keypair_2.public_key.clone(),
        revealed_messages: BTreeMap::new(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: vec![chunked_comm_gens.G, chunked_comm_gens.H],
        commitment: comm_single_2.into_affine(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: bases_comm_chunks.clone(),
        commitment: comm_chunks_2.clone(),
    }));
    statements.add(Statement::PedersenCommitment(PedersenCommitmentStmt {
        bases: bases_comm_ct.clone(),
        commitment: ct_2.commitment.clone(),
    }));

    let mut meta_statements = MetaStatements::new();

    meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
        vec![(0, user_id_idx), (1, 0)] // 0th statement's `user_id_idx`th witness is equal to 1st statement's 0th witness
            .into_iter()
            .collect::<BTreeSet<WitnessRef>>(),
    )));
    for i in 0..chunks_count as usize {
        meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
            vec![(2, i), (3, i)]
                .into_iter()
                .collect::<BTreeSet<WitnessRef>>(),
        )));
    }

    meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
        vec![(4, user_id_idx), (5, 0)] // 0th statement's `user_id_idx`th witness is equal to 4th statement's 0th witness
            .into_iter()
            .collect::<BTreeSet<WitnessRef>>(),
    )));
    for i in 0..chunks_count as usize {
        meta_statements.add(MetaStatement::WitnessEquality(EqualWitnesses(
            vec![(6, i), (7, i)]
                .into_iter()
                .collect::<BTreeSet<WitnessRef>>(),
        )));
    }

    let proof_spec = ProofSpec {
        statements: statements.clone(),
        meta_statements: meta_statements.clone(),
        context: None,
    };

    let mut witnesses = Witnesses::new();
    witnesses.add(PoKSignatureBBSG1Wit::new_as_witness(
        sig_1.clone(),
        messages_1
            .clone()
            .into_iter()
            .enumerate()
            .map(|t| t)
            .collect(),
    ));
    witnesses.add(Witness::PedersenCommitment(vec![
        messages_1[user_id_idx].clone(),
        blinding_1,
    ]));
    witnesses.add(Witness::PedersenCommitment(wit_comm_chunks_1));
    witnesses.add(Witness::PedersenCommitment(wit_comm_ct_1));

    witnesses.add(PoKSignatureBBSG1Wit::new_as_witness(
        sig_2.clone(),
        messages_2
            .clone()
            .into_iter()
            .enumerate()
            .map(|t| t)
            .collect(),
    ));
    witnesses.add(Witness::PedersenCommitment(vec![
        messages_2[user_id_idx].clone(),
        blinding_2,
    ]));
    witnesses.add(Witness::PedersenCommitment(wit_comm_chunks_2));
    witnesses.add(Witness::PedersenCommitment(wit_comm_ct_2));

    let proof = ProofG1::new(&mut rng, proof_spec.clone(), witnesses.clone(), None).unwrap();
    println!("Time taken to create proof {:?}", start.elapsed());

    // Verifies the proof
    let start = Instant::now();
    proof.verify(proof_spec, None).unwrap();
    println!("Time taken to verify proof {:?}", start.elapsed());

    let start = Instant::now();
    let proof_1 = create_proof(circuit_1, r_1, &snark_srs, &ek, &mut rng).unwrap();
    let proof_2 = create_proof(circuit_2, r_2, &snark_srs, &ek, &mut rng).unwrap();
    println!(
        "Time taken to create 2 Groth16 proofs {:?}",
        start.elapsed()
    );

    let pvk = prepare_verifying_key::<Bls12_381>(&snark_srs.pk.vk);

    let start = Instant::now();
    ct_1.verify_commitment(&ek, &enc_gens).unwrap();
    ct_2.verify_commitment(&ek, &enc_gens).unwrap();
    println!(
        "Time taken to verify ciphertext commitment {:?}",
        start.elapsed()
    );

    let start = Instant::now();
    assert!(verify_proof(&pvk, &proof_1, &ct_1).unwrap());
    assert!(verify_proof(&pvk, &proof_2, &ct_2).unwrap());
    println!(
        "Time taken to verify 2 Groth16 proofs {:?}",
        start.elapsed()
    );

    // Decryptor decrypts
    let (decrypted_message_1, nu_1) = ct_1
        .decrypt_given_groth16_vk(&sk, &dk, &snark_srs.pk.vk, chunk_bit_size)
        .unwrap();
    assert_eq!(decrypted_message_1, messages_1[user_id_idx]);

    let (decrypted_message_2, nu_2) = ct_2
        .decrypt_given_groth16_vk(&sk, &dk, &snark_srs.pk.vk, chunk_bit_size)
        .unwrap();
    assert_eq!(decrypted_message_2, messages_2[user_id_idx]);

    ct_1.verify_decryption_given_groth16_vk(
        &decrypted_message_1,
        &nu_1,
        chunk_bit_size,
        &dk,
        &snark_srs.pk.vk,
        &enc_gens,
    )
    .unwrap();

    ct_2.verify_decryption_given_groth16_vk(
        &decrypted_message_2,
        &nu_2,
        chunk_bit_size,
        &dk,
        &snark_srs.pk.vk,
        &enc_gens,
    )
    .unwrap();
}
