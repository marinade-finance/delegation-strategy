#![cfg_attr(not(debug_assertions), deny(warnings))]

use std::{
    env::VarError,
    fmt::Display,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use anyhow::anyhow;
use anyhow::Result;

use shellexpand::LookupError;

use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    signature::{write_keypair_file, Signer},
};

pub mod rpc_client_helpers;
pub mod rpc_marinade;

pub use solana_client;

#[derive(Debug)]
pub enum Cluster {
    Testnet,
    Mainnet,
    Devnet,
    Local,
    Other,
}

impl Cluster {
    pub fn from_url(url: &str) -> Cluster {
        if url.contains("testnet") {
            Cluster::Testnet
        } else if url.contains("mainnet") {
            Cluster::Mainnet
        } else if url.contains("devnet") {
            Cluster::Devnet
        } else if url.contains("localhost") {
            Cluster::Local
        } else {
            Cluster::Other
        }
    }
    pub fn default_instance(&self) -> Pubkey {
        Pubkey::from_str(match self {
            //Cluster::Devnet => "9tA9pzAZWimw2EMZgMjmUwzB2qPKrHhFNaC2ZvCrReeh",
            Cluster::Devnet => "8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC",
            Cluster::Testnet => "8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC",
            Cluster::Mainnet => "8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC",
            Cluster::Local => "8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC",
            Cluster::Other => "8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC",
        })
        .unwrap()
    }
    pub fn to_string(&self) -> String {
        String::from_str(match self {
            Cluster::Devnet => "devnet",
            Cluster::Testnet => "testnet",
            Cluster::Mainnet => "mainnet-beta",
            Cluster::Local => "local",
            Cluster::Other => "other",
        })
        .unwrap()
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ExpandedPath(PathBuf);

impl Deref for ExpandedPath {
    type Target = PathBuf;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ExpandedPath {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl FromStr for ExpandedPath {
    type Err = LookupError<VarError>;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        shellexpand::full(s).map(|expanded| ExpandedPath(PathBuf::from(expanded.as_ref())))
    }
}

impl AsRef<Path> for ExpandedPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Display for ExpandedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.display().fmt(f)
    }
}

#[derive(Clone)]
pub struct InputKeypair {
    path: ExpandedPath,
    keypair: Arc<Keypair>,
}

impl InputKeypair {
    pub fn as_path(&self) -> &ExpandedPath {
        &self.path
    }

    pub fn as_keypair(&self) -> Arc<Keypair> {
        self.keypair.clone()
    }

    pub fn as_pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }
}

impl std::fmt::Debug for InputKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "InputKeypair({}, {})", self.path, self.as_pubkey())
    }
}

impl FromStr for InputKeypair {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let path = ExpandedPath::from_str(s)?;
        let keypair = if path.exists() {
            if path.is_dir() {
                let keypair = Keypair::new();
                write_keypair_file(&keypair, &path)
                    .map_err(|_| anyhow!("error writing keypair file {}", path))?;
                keypair
            } else {
                read_keypair_file(&path).map_err(|e| anyhow!("Error reading keypair file {}", e))?
            }
        } else {
            let keypair = Keypair::new();
            write_keypair_file(&keypair, &path)
                .map_err(|_| anyhow!("error writing keypair file {}", path))?;
            keypair
        };
        Ok(Self {
            path,
            keypair: Arc::new(keypair),
        })
    }
}

impl Display for InputKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({})", self.keypair.pubkey(), &self.path)
    }
}

#[derive(Debug, Clone)]
pub enum InputPubkey {
    Pubkey(Pubkey),
    Keypair(InputKeypair),
    Auto,
}

impl InputPubkey {
    pub fn try_as_path(&self) -> Option<&ExpandedPath> {
        match self {
            InputPubkey::Pubkey(_) => None,
            InputPubkey::Auto => None,
            InputPubkey::Keypair(input_keypair) => Some(input_keypair.as_path()),
        }
    }

    pub fn try_as_keypair(&self) -> Option<Arc<Keypair>> {
        match self {
            InputPubkey::Pubkey(_) => None,
            InputPubkey::Auto => None,
            InputPubkey::Keypair(input_keypair) => Some(input_keypair.as_keypair()),
        }
    }

    pub fn as_pubkey(&self) -> Pubkey {
        match self {
            InputPubkey::Auto => panic!("auto pubkey not set"),
            InputPubkey::Pubkey(pubkey) => *pubkey,
            InputPubkey::Keypair(input_keypair) => input_keypair.as_pubkey(),
        }
    }
}

impl FromStr for InputPubkey {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(if let Ok(pubkey) = Pubkey::from_str(s) {
            Self::Pubkey(pubkey)
        } else if s == "auto" {
            Self::Auto
        } else {
            Self::Keypair(InputKeypair::from_str(s)?)
        })
    }
}

impl Display for InputPubkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputPubkey::Auto => Display::fmt("auto", f),
            InputPubkey::Pubkey(pubkey) => Display::fmt(pubkey, f),
            InputPubkey::Keypair(input_keypair) => Display::fmt(input_keypair, f),
        }
    }
}
