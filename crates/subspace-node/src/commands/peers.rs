use crate::commands::shared::{
    derive_keypair, init_logger, store_key_in_keystore, KeystoreOptions,
};
use bip39::Mnemonic;
use clap::Parser;
use sc_cli::{Error, KeystoreParams};
use sc_service::config::KeystoreConfig;
use sp_core::crypto::{ExposeSecret, SecretString};
use sp_core::Pair;
use sp_domains::DomainId;
use std::path::PathBuf;
use tracing::{info, warn};

/// Options for creating domain key
#[derive(Debug, Parser)]
pub struct CreateDomainKeyOptions {
    /// RpcURL .
    #[clap(short, long, default_value = "http://127.0.0.1:9944")]
    pub rpc_url: String,
}

pub fn create_domain_key(options: CreateDomainKeyOptions) -> Result<(), Error> {
    init_logger();

    let CreateDomainKeyOptions {
        rpc_url,
    } = options;
    
    

    Ok(())
}