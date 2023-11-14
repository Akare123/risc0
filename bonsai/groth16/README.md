# Bonsai GROTH16 Verifier

A library to verify Groth16 proofs computed over the BN_254 curve.

## Example Usage

```rust
use anyhow::Result;
use bonsai_groth16_verifier::{
    raw::{RawProof, RawPublic, RawVKey},
    Groth16,
};

fn verification(circom_verification_key: &str, circom_proof: &str, circom_public: &str) -> Result<()> {
    // parse the `verification_key`, `proof` and `public` as generated by Circom/SnarkJS
    let raw_vkey: RawVKey = serde_json::from_str(circom_verification_key)?;
    let raw_proof: RawProof = serde_json::from_str(circom_proof)?;
    let raw_public = RawPublic {
        values: serde_json::from_str(circom_public)?,
    };

    // build a groth16 instance from the raw material collected from Circom/SnarkJS
    let groth16 = Groth16::from_raw(raw_vkey, raw_proof, raw_public)?;

    // groth16 proof verification
    groth16.verify()
}

```