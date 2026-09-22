use anyhow::Result;
use ethers::types::{Address, Bytes, U256};

use crate::graph::Edge;
use crate::plan::StepData;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct VenueSnapshot {
    pub edges: Vec<Edge>,
}

#[allow(dead_code)]
pub trait VenueAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn kind(&self) -> &str;

    fn identify_pools(&self) -> Result<()>;

    fn snapshot_edges(&self) -> Result<VenueSnapshot>;

    fn exact_quote(&self, token_in: Address, token_out: Address, amount_in: U256) -> Result<U256>;

    fn build_steps(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<Vec<StepData>>;

    fn router_call(&self, _data: Bytes) -> Result<Vec<StepData>> {
        Ok(Vec::new())
    }
}
