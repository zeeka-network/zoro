use crate::{circuits, core};
use bellman::groth16;
use bellman::groth16::Parameters;
use bls12_381::Bls12;
use rand::rngs::OsRng;
use zeekit::merkle;

use std::collections::HashMap;

#[derive(Clone, Debug)]
pub enum BankError {
    BalanceInsufficient,
    InvalidNonce,
    InvalidSignature,
    InvalidPublicKey,
}

pub struct Bank {
    update_params: Parameters<Bls12>,
    deposit_withdraw_params: Parameters<Bls12>,
    tree: merkle::SparseTree,
    accounts: HashMap<u64, core::Account>,
}

impl Bank {
    pub fn balances(&self) -> Vec<(u64, u64)> {
        self.accounts.iter().map(|(i, a)| (*i, a.balance)).collect()
    }
    pub fn new(
        update_params: Parameters<Bls12>,
        deposit_withdraw_params: Parameters<Bls12>,
    ) -> Self {
        Self {
            update_params,
            deposit_withdraw_params,
            tree: merkle::SparseTree::new(core::Account::default().hash()),
            accounts: HashMap::new(),
        }
    }
    pub fn get_account(&self, index: u64) -> core::Account {
        self.accounts.get(&index).cloned().unwrap_or_default()
    }
    pub fn deposit_withdraw(&mut self, txs: Vec<core::DepositWithdraw>) -> Result<(), BankError> {
        let mut transitions = Vec::new();
        let state = self.tree.root();
        for tx in txs.iter() {
            let acc = self.get_account(tx.index);
            if acc.address != Default::default() && tx.pub_key != acc.address {
                return Err(BankError::InvalidPublicKey);
            } else if tx.withdraw && acc.balance < tx.amount {
                return Err(BankError::BalanceInsufficient);
            } else {
                let updated_acc = core::Account {
                    address: tx.pub_key.clone(),
                    balance: if tx.withdraw {
                        acc.balance - tx.amount
                    } else {
                        acc.balance + tx.amount
                    },
                    nonce: acc.nonce,
                };
                self.tree.set(tx.index as u64, updated_acc.hash());
                self.accounts.insert(tx.index, updated_acc);

                let proof = self.tree.prove(tx.index);

                transitions.push(circuits::DepositWithdrawTransition {
                    enabled: true,
                    tx: tx.clone(),
                    before: acc,
                    proof,
                });
            }
        }
        let next_state = self.tree.root();

        let circuit = circuits::DepositWithdrawCircuit {
            filled: true,
            state,
            next_state,
            transitions: Box::new(circuits::DepositWithdrawTransitionBatch::new(transitions)),
        };

        let pvk = groth16::prepare_verifying_key(&self.deposit_withdraw_params.vk);

        let start = std::time::Instant::now();
        let proof =
            groth16::create_random_proof(circuit, &self.deposit_withdraw_params, &mut OsRng)
                .unwrap();
        println!(
            "Proving took: {}ms",
            (std::time::Instant::now() - start).as_millis()
        );

        let inputs = vec![state.into(), next_state.into()];

        println!(
            "Verify: {}",
            groth16::verify_proof(&pvk, &proof, &inputs).is_ok()
        );

        Ok(())
    }
    pub fn change_state(&mut self, txs: Vec<core::Transaction>) -> Result<(), BankError> {
        let mut transitions = Vec::new();

        let state = self.tree.root();

        for tx in txs.iter() {
            let src_acc = self.accounts[&tx.src_index].clone();
            if tx.nonce != src_acc.nonce {
                return Err(BankError::InvalidNonce);
            } else if !tx.verify(src_acc.address) {
                return Err(BankError::InvalidSignature);
            } else if src_acc.balance < tx.fee + tx.amount {
                return Err(BankError::BalanceInsufficient);
            } else {
                let src_before = self.get_account(tx.src_index);
                let src_proof = self.tree.prove(tx.src_index);
                self.accounts.get_mut(&tx.src_index).unwrap().nonce += 1;
                self.accounts.get_mut(&tx.src_index).unwrap().balance -= tx.fee + tx.amount;
                self.tree
                    .set(tx.src_index as u64, self.accounts[&tx.src_index].hash());

                self.accounts.entry(tx.dst_index).or_default();
                let dst_before = self.get_account(tx.dst_index);
                let dst_proof = self.tree.prove(tx.dst_index);
                self.accounts.get_mut(&tx.dst_index).unwrap().address = tx.dst_pub_key.clone();
                self.accounts.get_mut(&tx.dst_index).unwrap().balance += tx.amount;
                self.tree
                    .set(tx.dst_index as u64, self.accounts[&tx.dst_index].hash());

                transitions.push(circuits::Transition {
                    enabled: true,
                    tx: tx.clone(),
                    src_before,
                    src_proof,
                    dst_before,
                    dst_proof,
                });
            }
        }

        let next_state = self.tree.root();

        let circuit = circuits::UpdateCircuit {
            filled: true,
            state,
            next_state,
            transitions: Box::new(circuits::TransitionBatch::new(transitions)),
        };

        let pvk = groth16::prepare_verifying_key(&self.update_params.vk);

        let start = std::time::Instant::now();
        let proof = groth16::create_random_proof(circuit, &self.update_params, &mut OsRng).unwrap();
        println!(
            "Proving took: {}ms",
            (std::time::Instant::now() - start).as_millis()
        );

        let inputs = vec![state.into(), next_state.into()];

        println!(
            "Verify: {}",
            groth16::verify_proof(&pvk, &proof, &inputs).is_ok()
        );

        Ok(())
    }
}
