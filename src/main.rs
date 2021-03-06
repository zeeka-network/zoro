#[macro_use]
extern crate lazy_static;

mod bank;
mod circuits;
mod config;
mod core;

use bazuka::config::blockchain::MPN_CONTRACT_ID;
use bazuka::core::{PaymentDirection, ZkHasher};
use bazuka::crypto::{jubjub, ZkSignatureScheme};
use bazuka::db::ReadOnlyLevelDbKvStore;
use bazuka::zk::{DepositWithdraw, ZeroTransaction};
use bellman::{groth16, Circuit};
use bls12_381::Bls12;
use rand_core::OsRng;
use std::fs::File;
use zeekit::BellmanFr;

fn load_params<C: Circuit<BellmanFr> + Default>(
    path: &str,
    use_cache: bool,
) -> groth16::Parameters<Bls12> {
    if use_cache {
        let param_file = File::open(path).expect("Unable to open parameters file!");
        groth16::Parameters::<Bls12>::read(param_file, false /* false for better performance*/)
            .expect("Unable to read parameters file!")
    } else {
        let c = C::default();

        let p = groth16::generate_random_parameters::<Bls12, _, _>(c, &mut OsRng).unwrap();
        let param_file = File::create(path).expect("Unable to create parameters file!");
        p.write(param_file)
            .expect("Unable to write parameters file!");
        p
    }
}

fn vk_to_hex(vk: &bellman::groth16::VerifyingKey<Bls12>) -> String {
    hex::encode(
        &bincode::serialize(&unsafe {
            std::mem::transmute::<
                bellman::groth16::VerifyingKey<Bls12>,
                bazuka::zk::groth16::Groth16VerifyingKey,
            >(vk.clone())
        })
        .unwrap(),
    )
}

fn db_shutter() -> ReadOnlyLevelDbKvStore {
    ReadOnlyLevelDbKvStore::read_only(std::path::Path::new("/home/keyvan/.bazuka"), 64).unwrap()
}

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ZoroError {
    #[error("io error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("node error: {0}")]
    NodeError(#[from] bazuka::client::NodeError),
}

fn transact(
    node: bazuka::client::PeerAddress,
    tx: bazuka::core::TransactionAndDelta,
) -> Result<bazuka::client::messages::TransactResponse, ZoroError> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let sk =
                <bazuka::core::Signer as bazuka::crypto::SignatureScheme>::generate_keys(b"dummy")
                    .1;
            let (lp, client) = bazuka::client::BazukaClient::connect(sk, node);

            let (res, _) = tokio::join!(
                async move { Ok::<_, bazuka::client::NodeError>(client.transact(tx).await) },
                lp
            );

            res
        })??)
}

fn get_zero_mempool(
    node: bazuka::client::PeerAddress,
) -> Result<bazuka::client::messages::GetZeroMempoolResponse, ZoroError> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let sk =
                <bazuka::core::Signer as bazuka::crypto::SignatureScheme>::generate_keys(b"dummy")
                    .1;
            let (lp, client) = bazuka::client::BazukaClient::connect(sk, node);

            let (res, _) = tokio::join!(
                async move { Ok::<_, bazuka::client::NodeError>(client.get_zero_mempool().await) },
                lp
            );

            res
        })??)
}

fn main() {
    let exec_wallet = bazuka::wallet::Wallet::new(b"Executor".to_vec());
    let use_cache = true;
    let update_params = load_params::<circuits::UpdateCircuit>("groth16_mpn_update.dat", use_cache);
    let deposit_withdraw_params = load_params::<circuits::DepositWithdrawCircuit>(
        "groth16_mpn_deposit_withdraw.dat",
        use_cache,
    );

    let node_addr = bazuka::client::PeerAddress("127.0.0.1:3030".parse().unwrap());

    /*println!("Update: {}", vk_to_hex(&update_params.vk));
    println!(
        "Deposit/Withdraw: {}",
        vk_to_hex(&deposit_withdraw_params.vk)
    );*/

    let b = bank::Bank::new(update_params, deposit_withdraw_params);

    let mut latest_processed = None;
    let db_shutter = db_shutter();
    loop {
        let db = db_shutter.snapshot();

        if latest_processed == Some(b.root(&db)) {
            println!("Block is already processed!");
            std::thread::sleep(std::time::Duration::from_millis(1000));
            continue;
        }

        latest_processed = Some(b.root(&db));

        let mempool = get_zero_mempool(node_addr).unwrap();

        let contract_payments = mempool
            .deposit_withdraws
            .iter()
            .filter(|dw| dw.contract_id == *MPN_CONTRACT_ID)
            .cloned()
            .collect::<Vec<_>>();

        let deposit_withdraws = contract_payments
            .iter()
            .map(|dw| DepositWithdraw {
                index: dw.zk_address_index,
                pub_key: dw.zk_address.clone(),
                amount: match dw.direction {
                    PaymentDirection::Deposit(_) => dw.amount as i64,
                    PaymentDirection::Withdraw(_) => -(dw.amount as i64),
                },
            })
            .collect::<Vec<_>>();
        println!("{:?}", deposit_withdraws);

        if deposit_withdraws.is_empty() {
            println!("No deposit/withdraws!");
            std::thread::sleep(std::time::Duration::from_millis(1000));
            continue;
        }

        let alice_keys = jubjub::JubJub::<ZkHasher>::generate_keys(b"alice");
        let bob_keys = jubjub::JubJub::<ZkHasher>::generate_keys(b"bob");
        let charlie_keys = jubjub::JubJub::<ZkHasher>::generate_keys(b"charlie");
        let alice_index = 0;
        let bob_index = 1;
        let charlie_index = 2;

        let (delta, new_root, proof) = b.deposit_withdraw(&db, deposit_withdraws).unwrap();

        let mut update = bazuka::core::Transaction {
            src: exec_wallet.get_address(),
            nonce: 1,
            fee: 0,
            data: bazuka::core::TransactionData::UpdateContract {
                contract_id: *MPN_CONTRACT_ID,
                updates: vec![bazuka::core::ContractUpdate::DepositWithdraw {
                    deposit_withdraws: contract_payments,
                    next_state: new_root,
                    proof: bazuka::zk::ZkProof::Groth16(Box::new(proof)),
                }],
            },
            sig: bazuka::core::Signature::Unsigned,
        };
        exec_wallet.sign(&mut update);

        let tx_delta = bazuka::core::TransactionAndDelta {
            tx: update,
            state_delta: Some(delta),
        };

        transact(node_addr, tx_delta).unwrap();
    }

    /*println!("{:?}", b.balances(&db));

    let mut tx1 = ZeroTransaction {
        nonce: 0,
        src_index: alice_index,
        dst_index: bob_index,
        dst_pub_key: bob_keys.0.clone(),
        amount: 200,
        fee: 1,
        sig: jubjub::Signature::default(),
    };
    tx1.sign(alice_keys.1.clone());

    let mut tx2 = ZeroTransaction {
        nonce: 0,
        src_index: bob_index,
        dst_index: alice_index,
        dst_pub_key: alice_keys.0.clone(),
        amount: 50,
        fee: 1,
        sig: jubjub::Signature::default(),
    };
    tx2.sign(bob_keys.1.clone());

    let mut tx3 = ZeroTransaction {
        nonce: 1,
        src_index: bob_index,
        dst_index: alice_index,
        dst_pub_key: alice_keys.0.clone(),
        amount: 647,
        fee: 2,
        sig: jubjub::Signature::default(),
    };
    tx3.sign(bob_keys.1);

    let mut tx4 = ZeroTransaction {
        nonce: 1,
        src_index: alice_index,
        dst_index: charlie_index,
        dst_pub_key: charlie_keys.0.clone(),
        amount: 197,
        fee: 2,
        sig: jubjub::Signature::default(),
    };
    tx4.sign(alice_keys.1);

    b.change_state(&db, vec![tx1, tx2, tx3, tx4]).unwrap();
    println!("{:?}", b.balances(&db));*/
}
